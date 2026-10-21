//! Checker: the graph pass over modules, provider cycles, native module metadata and docs.
//! Split out of `src/checker/mod.rs`. `super::*` is the parent module.

use super::*;

impl Checker {
    /// TICKET-265 — `stmt` is a top-level fn of this module that the program does not reach
    /// ([`ModuleGraph::skips_fn_body`]): no pass reads its body.
    pub(super) fn skips_body(&self, stmt: &Stmt) -> bool {
        if let StmtKind::Fn(decl) = &stmt.kind {
            self.skip_fn_bodies.skips(&decl.name)
        } else {
            false
        }
    }

    /// The shared deps-first module-checking pass behind both [`check_graph`] and
    /// [`resolve_extern_signatures`]. When `harvest_externs` is set, gathers every struct's AST field
    /// types up front and stamps each module's graph index so the extern loop records resolved C
    /// signatures into `self.extern_sigs`.
    pub(super) fn run_graph_pass(&mut self, graph: &ModuleGraph, harvest_externs: bool) {
        let c = self;
        #[cfg(any(test, feature = "test-api"))]
        if !graph.modules.iter().any(|m| m.dotted == ["std", "prelude"]) {
            c.seed_native_prelude_sigs();
        }
        // Every file's index is known before any body is checked.
        for (idx, lm) in graph.modules.iter().enumerate() {
            c.module_idx_of_file.insert(lm.file, idx);
            if lm.dotted == crate::resolver::JSON_MODULE {
                c.json_module = Some(lm.id.clone());
            }
        }
        c.range_key = crate::resolver::range_key(graph);
        // ROOT REDESIGN — module-scoped IDENTITY KEYS: scan every non-native module's struct/enum/alias
        // names and key EACH one `<module-key>::Name` (via the shared `resolver::module_keys`, the SAME
        // derivation the compiler uses), so the checker, compiler, and VM agree on every key (parity) and
        // every user type is unique by construction (a cross-module name clash is just two distinct keys).
        // Std modules' types are RESERVED/NATIVE and keep their BARE name, so `Iterator`/FFI widths
        // resolve bare, like the synthetic natives (`user_type_keys` skips them).
        for k in crate::resolver::user_type_keys(graph) {
            c.type_keys
                .insert((graph.modules[k.module].id.clone(), k.name), k.key);
        }
        // FFI ROOT FIX (fix4): when harvesting extern signatures, gather every user struct's AST field
        // types up front, keyed by IDENTITY KEY (`<module-key>::Name`, the SAME derivation), so
        // `resolve_ctype` can build a by-value extern struct's `CType::Struct` with each field's real C
        // width — even when the struct is declared after the extern block, or in another module.
        if harvest_externs {
            let mkeys = crate::resolver::module_keys(graph);
            for (idx, lm) in graph.modules.iter().enumerate() {
                if lm.native.is_some() {
                    continue;
                }
                let is_std = lm.is_std();
                for s in &lm.ast.stmts {
                    if let StmtKind::Struct { name, fields, .. } = &s.kind {
                        let key = if is_std {
                            name.clone()
                        } else {
                            format!("{}::{name}", mkeys[idx])
                        };
                        c.struct_field_asts.insert(
                            key,
                            fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect(),
                        );
                    }
                }
            }
        }
        // Build the reverse index name → declaring USER module label(s), in graph (deps-first)
        // order, so a bare unimported type can hint "import it from <module>". The entry module is
        // excluded (its types are local, never imported). A std module's types come from
        // `std_type_owners`, whether or not this program loads the module.
        for lm in &graph.modules {
            if lm.id == graph.entry || lm.is_std() {
                continue;
            }
            let label = lm.label();
            for s in &lm.ast.stmts {
                if let Some(name) = declared_type_name(&s.kind) {
                    c.types_by_name
                        .entry(name.to_string())
                        .or_default()
                        .push(label.clone());
                }
            }
        }
        let binds = std::rc::Rc::new(crate::resolver::graph_global_binds(graph));
        c.graph_binds = std::rc::Rc::clone(&binds);
        for (idx, lm) in graph.modules.iter().enumerate() {
            c.module_idx_of.insert(lm.id.clone(), idx);
            c.skip_range_bodies = graph.skips_range_methods(lm);
            c.skip_fn_bodies = graph.fn_skips(lm);
            // Keys every checker->compiler table this module's bodies record, the bodied fns of a native
            // std module included (they are checked below, before the AST-module branch sets it).
            c.graph_module_idx = idx;
            // A native std module (std.math/io/os) has no AST: its public surface is a static table.
            if let Some(name) = lm.native {
                // A native std module is always stdlib. The native branch runs `begin_module` once at
                // its entry, for the clash pass; set stdlib here too. Its own builtin types
                // (std.net's `Socket` in `connect -> Socket!`) resolve through `bind_own_builtins`
                // below.
                c.current_module_is_stdlib = true;
                // The one clash pass and the one bare-import decision, as `check_module` runs them: a
                // native module never reaches `check_module`. A later `begin_module` keeps `globals`
                // and the decision.
                c.begin_module(Some(lm.label()));
                c.collect_module_globals(&lm.ast.stmts, &lm.imports, &binds[idx]);
                // FILE-BACKED native modules harvest their whole SIGNATURE (fns, native structs,
                // `native type` exports, std.time's `timer` license) from the real `std/<M>.chz` AST
                // the resolver loaded; no Rust-side sig exists. Same predicate as the resolver's
                // `visit_native_file` gate — lockstep by construction.
                let mut sig = ModuleSig::default();
                // W7-8 — a native `.chz` may `import` a sibling std module and NAME ITS TYPES in the
                // harvested SIGNATURES (`std.fs`'s `list_dir(p: PathLike) -> Result[List[path.Path]]`).
                // The harvest RESOLVES those types, so the imports must be bound BEFORE it — the
                // `has_bodied` bind below (which exists for bodied BODIES) runs far too late and the
                // signature errored `unknown module 'path'`. `begin_module` first, so the harvest sees
                // the same clean, stdlib-seeded env `check_module` gives an AST module.
                // Gated on having an import: only then does the harvest need bound imports.
                if !lm.imports.is_empty() {
                    c.current_module_is_stdlib = true;
                    c.begin_module(Some(lm.label()));
                    c.push_scope(); // `bind_import` declares the bound name — it needs a live scope
                    for imp in &lm.imports {
                        c.bind_import(imp);
                    }
                }
                // The module's own sigs name the builtin types it owns (`std/ffi.chz`'s `ptr`).
                if lm.imports.is_empty() {
                    c.builtin_binds.clear();
                }
                c.bind_own_builtins(name);
                sig.builtins = super::builtin_type::module_builtins(name)
                    .into_iter()
                    .collect();
                if crate::native::is_file_backed_native(name) {
                    c.harvest_native_module(&lm.ast, &mut sig);
                }
                // Phase 4c-net — cache std.net's harvested `Socket`/`Listener` method tables so
                // `seed_stdlib_structs` can re-seed them (bare, method-table only) into `self.structs`
                // for every subsequent module, letting `socket.read(...)`/`listener.accept(...)` resolve
                // via the normal method path (the retired bespoke `socket_method_sig` arm's replacement).
                if name == "std.net" {
                    c.net_socket_seed = sig.struct_defs.get("Socket").cloned();
                    c.net_listener_seed = sig.struct_defs.get("Listener").cloned();
                }
                // R2 — cache std.io's harvested `Writer` method table so `seed_stdlib_structs` re-seeds
                // it (bare, method-table only) for every subsequent module, letting `w.write(...)`/
                // `w.close(...)` resolve via the normal method path.
                if name == "std.io" {
                    c.io_writer_seed = sig.struct_defs.get("Writer").cloned();
                    // R2b — cache std.io's harvested `Reader` method table (parallel to `io_writer_seed`;
                    // a SEPARATE field so neither type's table clobbers the other).
                    c.io_reader_seed = sig.struct_defs.get("Reader").cloned();
                }
                // Re-attach the checker-side metadata that native decls can't express (hover docs,
                // module constants like math.pi/e, std.concurrency's
                // `RwShared.read`/`Executor.submit` closure-param sigs). Run for EVERY native module
                // (idempotent for those it doesn't cover) so the doc/const/poly attach no longer lives
                // in the deleted per-module arms.
                attach_native_module_metadata(name, &mut sig);
                // Phase 4c-concurrency — cache std.concurrency's harvested `Shared`/`RwShared`/`Atomic`/
                // `Executor` method tables so `seed_stdlib_structs` can re-seed them (bare, method-table
                // only) into `self.structs` for every subsequent module, letting `s.set(...)`/`a.cas(...)`
                // resolve via the normal method path (the retired bespoke `shared_method_sig`/etc arms'
                // replacement). Cached AFTER `attach_native_module_metadata` (unlike net's before-attach
                // cache) because that step mutates `RwShared.read`/`Executor.submit`'s closure-param sigs
                // — caching before would seed the pre-port (unannotated) tables.
                if name == "std.concurrency" {
                    for tn in ["Shared", "RwShared", "Atomic", "AtomicInt", "Executor"] {
                        if let Some(info) = sig.struct_defs.get(tn) {
                            c.concurrency_seeds.insert(tn.to_string(), info.clone());
                        }
                    }
                }
                // A HYBRID native module carries BODIED Chezzi decls alongside its bodyless native
                // ones: module-level `fn`s (PASS 2b harvested them into `sig.members`) and
                // native-struct `bodied_methods` (`Reader.lines`, in the struct method table). The
                // native arm skips `check_module`, so those bodies would go UNCHECKED — a `str`
                // returned under an `int` sig would slip straight through. Type-check them here via the
                // same `check_fn_body` a normal module uses, on a clean `begin_module` env. Gated on
                // actually having a bodied decl so a pure-native module (os/fs/regex/…) pays no second
                // `begin_module` and its live-table state is left exactly as the harvest above produced it.
                let has_bodied = lm.ast.stmts.iter().any(|s| {
                    matches!(&s.kind, StmtKind::Fn(_) | StmtKind::Let { .. })
                        || matches!(&s.kind, StmtKind::NativeStruct { bodied_methods, .. } if !bodied_methods.is_empty())
                });
                if has_bodied {
                    c.current_module_is_stdlib = true;
                    c.begin_module(Some(lm.label()));
                    c.push_scope();
                    // Bind this native module's own imports so a bodied fn body can use them (a native
                    // `.chz` may `import` like any other module; deps are checked earlier in graph order,
                    // so their sigs are already in `module_sigs`).
                    for imp in &lm.imports {
                        c.bind_import(imp);
                    }
                    // `begin_module` cleared the live tables; repopulate the callables/types the
                    // harvested `sig` holds so a bodied body can call a sibling fn or name a sibling
                    // native struct (`-> Reader`).
                    for (n, f) in sig.fns() {
                        c.functions.insert(n.clone(), f.clone());
                    }
                    for (n, info) in &sig.struct_defs {
                        c.structs.insert(n.clone(), info.clone());
                        c.bare_types.insert(n.clone(), n.clone());
                        c.struct_names.insert(n.clone());
                    }
                    // TICKET-218 — module constants (`pi: const float = …`): check each value against
                    // its declared type, before the bodies that may read it.
                    for s in &lm.ast.stmts {
                        if matches!(&s.kind, StmtKind::Let { .. }) {
                            c.check_stmt(s);
                        }
                    }
                    // Module-level bodied fns (`divmod`): free-fn sig, no `self`, decl↔sig params
                    // align by index exactly as `check_module`'s top-level-fn path.
                    for s in &lm.ast.stmts {
                        if let StmtKind::Fn(decl) = &s.kind
                            && let Some(fsig) = sig.certain_fn(&decl.name).cloned()
                        {
                            c.check_fn_body(decl, None, fsig);
                        }
                    }
                    // Native-struct bodied methods (`Reader.lines`): use a FRESH `fn_sig` (which keeps
                    // the leading `self`), NOT the leading-`self`-STRIPPED method-table sig — else
                    // `check_fn_body`'s positional decl↔sig param map (self at decl index 0) shifts
                    // every real param to `Unknown`.
                    for s in &lm.ast.stmts {
                        if let StmtKind::NativeStruct {
                            name,
                            type_params,
                            bodied_methods,
                            ..
                        } = &s.kind
                            && !bodied_methods.is_empty()
                        {
                            let saved = c.enter_type_params(type_params);
                            // A native struct's `self` is usually a RESERVED opaque handle
                            // (`Reader`→`Ty::Reader`, `Socket`→`Ty::Socket`, …) whose method dispatch
                            // goes through the reserved-`Ty` arm (which reads the leading-`self`-stripped
                            // method table correctly). Use that Ty for `self` so `self.read_line()`
                            // resolves — `Ty::Struct("Reader")` would route to the generic struct arm and
                            // wrongly demand a receiver slot. Fall back to the nominal struct type for a
                            // plain (non-reserved) native struct.
                            let args: Vec<Ty> = type_params
                                .iter()
                                .map(|tp| Ty::Param(tp.name.clone()))
                                .collect();
                            let self_ty = c
                                .qualified_builtin_ty(name, &args)
                                .unwrap_or_else(|| c.struct_self_ty(name));
                            for m in bodied_methods {
                                let msig = c.fn_sig(m, m.name_span, Some(&self_ty));
                                c.check_fn_body(m, Some(self_ty.clone()), msig);
                            }
                            c.exit_type_params(saved);
                        }
                    }
                }
                // The native arm is the ONE module path that never ends on a `begin_module`, so drop
                // this module's import bindings explicitly: a leaked `imported_modules`/`structs` entry
                // would make an UNIMPORTED type resolve by accident in the NEXT native module harvested
                // (an AST module clears them itself). Same `!imports.is_empty()` gate, so a pure-native
                // module's post-state is untouched.
                if !lm.imports.is_empty() {
                    c.begin_module(None);
                }
                sig.seal_std_internals();
                c.module_sigs.insert(lm.id.clone(), sig);
                continue;
            }
            let label = if lm.id == graph.entry {
                None
            } else {
                Some(lm.label())
            };
            // The prelude is graph module 0. Harvest its carrier enums before its own
            // `begin_module`, so `seed_stdlib_structs` registers them for every module, the
            // prelude included.
            if c.carrier_seeds.enums.is_empty() && lm.dotted == ["std", "prelude"] {
                c.carrier_seeds = c.harvest_native_enums(&lm.ast);
            }
            // Set before `begin_module`: its carrier seed reads it (a user module sees the carriers
            // only through `T?` / `T!E`).
            c.current_module_is_stdlib = lm.is_std();
            c.begin_module(label);
            // Stamp the current module's graph index so the extern loop keys its resolved C signatures
            // the SAME way both backends look them up. `None` outside the harvesting pass.
            c.extern_module_idx = if harvest_externs { Some(idx) } else { None };
            // Maintained on every graph pass (cheap) so a recorded value+keyword call is keyed under
            // the SAME module index the backends derive — mirrors the extern-sig keying.
            c.graph_module_idx = idx;
            let mut sig = c.check_module(&lm.ast.stmts, Some(&lm.id), &lm.imports, &binds[idx]);
            // The builtin types this std module owns (`std.iter`'s `Iterator`): a qualified
            // `iter.Iterator[int]` resolves to the builtin type, as the bare name does.
            if lm.is_std() {
                sig.builtins = super::builtin_type::module_builtins(&lm.label())
                    .into_iter()
                    .collect();
            }
            // Phase 5a-containers — capture the always-linked prelude's `List`/`Map`/`Set`/`Channel`
            // native-struct METHOD tables so `seed_stdlib_structs` can re-seed them (bare, method-table
            // only) into `self.structs` for every subsequent module, letting `xs.push(...)`/`m.get(...)`/
            // `s.add(...)`/`ch.send(...)` resolve via the normal method path (the retired bespoke
            // `list_method_sig`/`map_method_sig`/`set_method_sig`/`channel_method_sig` arms' replacement).
            // Harvested here (NOT from `sig.struct_defs` — the prelude
            // is a normal AST module whose `check_module` no-ops native structs) by resolving the decls
            // over `lm.ast` while still in the prelude's stdlib module context (type params + `Hashable`
            // in scope). The prelude is order[0] (always-linked, no deps), so this is populated before
            // entry/all others. The `List`/`Map`/`Set` names still resolve
            // to the RESERVED `Ty::List`/`Ty::Map`/`Ty::Set` via `resolve_type`'s reserved arms; the
            // ctors/literals stay compiler-wired.
            if c.container_seeds.is_empty() && lm.dotted == ["std", "prelude"] {
                for tn in ["List", "Map", "Set", "Channel", "str", "bytes", "bytearray"] {
                    if let Some(info) = c.harvest_native_struct_table(&lm.ast, tn) {
                        c.container_seeds.insert(tn.to_string(), info);
                    }
                }
            }
            // M24 — the manifest's `[project] entrypoint = "mod:fn"` names a function the runtime
            // invokes BY NAME at a fixed arity of ZERO, exactly like a `test fn`. A hidden witness
            // parameter has no call site to come from there, so a bare `chezzi run` would fault at
            // startup after a green check with the hidden arity leaked into the message
            // ("expects 1 argument(s), got 0"). Rejected here, where the name is known.
            if lm.id == graph.entry
                && let Some(f) = c.entry_fn.clone()
            {
                let decl = lm.ast.stmts.iter().find_map(|s| match &s.kind {
                    StmtKind::Fn(d) if d.name == f => Some((d, s.span)),
                    StmtKind::Let { .. }
                    | StmtKind::Assign { .. }
                    | StmtKind::Fn(_)
                    | StmtKind::Struct { .. }
                    | StmtKind::Protocol { .. }
                    | StmtKind::Enum { .. }
                    | StmtKind::TypeAlias { .. }
                    | StmtKind::If { .. }
                    | StmtKind::For { .. }
                    | StmtKind::While { .. }
                    | StmtKind::Match { .. }
                    | StmtKind::Return(_)
                    | StmtKind::Yield(_)
                    | StmtKind::Defer(_)
                    | StmtKind::Parallel { .. }
                    | StmtKind::Spawn(_)
                    | StmtKind::Wait { .. }
                    | StmtKind::Break
                    | StmtKind::Continue
                    | StmtKind::Pass
                    | StmtKind::Import(_)
                    | StmtKind::Extern { .. }
                    | StmtKind::Native(_)
                    | StmtKind::NativeStruct { .. }
                    | StmtKind::NativeEnum { .. }
                    | StmtKind::NativeType { .. }
                    | StmtKind::Assert { .. }
                    | StmtKind::Expr(_) => None,
                });
                let span = decl.map_or(Span::RUNTIME, |(_, sp)| sp);
                // Two ways the zero-argument call cannot be satisfied, one message. The DECLARED
                // params are the plain one — `fn main(a: int)` used to check green and then die at
                // startup with "function 'main' expects 1 argument(s), got 0", which reads as a
                // caller's mistake when the caller is the runtime. The hidden witness params are the
                // M24 one: no call site exists to pin `T`.
                let declared = decl.map_or(0, |(d, _)| d.params.len());
                let w = c.stored_witness_params(None, &f);
                let cause = if declared > 0 {
                    Some(format!(
                        "it declares {declared} parameter(s), which nothing can supply. Give the entrypoint a nullary signature and read its inputs inside it (e.g. `std.os.args`)"
                    ))
                } else if !w.is_empty() {
                    Some(format!(
                        "it cannot construct through its static-protocol bound ({}) — the hidden type witness has no call site to come from. Give the entrypoint a non-generic signature and move the construction into a helper it calls with a concrete type",
                        w.join(", ")
                    ))
                } else {
                    None
                };
                if let Some(cause) = cause {
                    c.error(
                        span,
                        format!(
                            "the manifest entrypoint '{f}' is invoked with no arguments, so {cause}"
                        ),
                    );
                }
                // TICKET-150 (W14-35c) — a name nothing binds is refused here, before any user code
                // runs; it used to surface only after the module's top level. `Span::RUNTIME`: there
                // is no declaration to point at (DEC-048). A binding that exists but is not callable
                // stays a run-time error — its checker type may be `Any`, so the checker declines.
                if decl.is_none() && !entry_module_binds(&binds[idx], &f) {
                    c.error(
                        Span::RUNTIME,
                        format!(
                            "chezzi.toml's [project] entrypoint function '{f}' not found: the entry module has no top-level fn, value or import called '{f}'"
                        ),
                    );
                }
            }
            if lm.is_std() {
                sig.seal_std_internals();
            }
            c.module_sigs.insert(lm.id.clone(), sig);
        }
        c.check_provider_cycles();
    }

    /// A default whose provider, directly or through other providers, fills its own slot again is
    /// unbounded recursion: report it (`fn f(x: int = f())`).
    pub(super) fn check_provider_cycles(&mut self) {
        let edges = std::mem::take(&mut self.provider_edges);
        if let Err(e) = crate::desugar::check_provider_cycles_in(&edges) {
            self.error(e.span, e.message);
        }
    }
}

/// Re-attach the checker-side metadata a `native fn` decl CANNOT express, on top of a native module's
/// `ModuleSig` (harvested from its file-backed `std/M.chz`).
/// Runs in the graph loop for EVERY native module (idempotent for those it doesn't cover). It attaches
/// editor hover docs (`MODULE_FN_DOCS`) — a concise one-line blurb on each authored fn's `FnSig.doc`
/// (excluded from `fn_sig_eq`, so purely informational; `record_method_hover` forwards it at the
/// `module.fn` hover site). Drift-guarded by `module_fn_docs_all_resolve`. Module constants are
/// `NAME: const T = value` lets in the std file, harvested by `harvest_native_module`.
/// Moved out of the (now-deleted) per-module `native_module_sig` arms so the file-backed migration
/// (phase 4d) keeps hover/const byte-identical. The synthetic struct LAYOUTS (Match/Response/
/// ProcResult) + request's optional-tail fns are NOT here either — they are file-backed (4b/4f) and
/// harvested from the parsed `.chz` by `harvest_native_module`.
/// The type or protocol name a top-level declaration declares, if any.
pub(super) fn declared_type_name(kind: &StmtKind) -> Option<&str> {
    match kind {
        StmtKind::Struct { name, .. }
        | StmtKind::Enum { name, .. }
        | StmtKind::TypeAlias { name, .. }
        | StmtKind::Protocol { name, .. }
        | StmtKind::NativeStruct { name, .. }
        | StmtKind::NativeEnum { name, .. }
        | StmtKind::NativeType { name, .. } => Some(name),
        StmtKind::Let { .. }
        | StmtKind::Assign { .. }
        | StmtKind::Fn(_)
        | StmtKind::If { .. }
        | StmtKind::For { .. }
        | StmtKind::While { .. }
        | StmtKind::Match { .. }
        | StmtKind::Return(_)
        | StmtKind::Yield(_)
        | StmtKind::Defer(_)
        | StmtKind::Parallel { .. }
        | StmtKind::Spawn(_)
        | StmtKind::Wait { .. }
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_)
        | StmtKind::Extern { .. }
        | StmtKind::Native(_)
        | StmtKind::Assert { .. }
        | StmtKind::Expr(_) => None,
    }
}

/// One `protocol` declaration of an embedded std file: the owner of that protocol's name, module
/// and shape. `Checker::seed_std_protocols` harvests the shape; `is_std_protocol` and `import_hint`
/// read the name and module.
pub(super) struct StdProtocol {
    pub name: String,
    pub module: String,
    pub stmt: crate::ast::Stmt,
}

/// The std module of an embedded file: `std.ops` for `ops.chz`.
fn std_module_of(rel: &str) -> String {
    format!("std.{}", rel.trim_end_matches(".chz").replace('/', "."))
}

/// Every public type name a std module declares → its modules (`std.regex` for `Match`), read once
/// from the shared std parse (`resolver::std_module_ast`) on the first diagnostic that needs an import hint, never at
/// startup. The prelude is left out: its types are in scope everywhere. A protocol is left out
/// too: `std_protocol_owner` answers for it.
pub(super) fn std_type_owners() -> &'static HashMap<String, Vec<String>> {
    static OWNERS: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
    OWNERS.get_or_init(|| {
        let mut type_owners: HashMap<String, Vec<String>> = HashMap::new();
        for (rel, _) in crate::native::std_embed::STD_FILES {
            if *rel == "prelude.chz" {
                continue;
            }
            let Some(ast) = crate::resolver::std_module_ast(rel) else {
                continue;
            };
            for s in &ast.stmts {
                if let StmtKind::Protocol { .. } = &s.kind {
                    continue;
                }
                if let Some(name) = declared_type_name(&s.kind)
                    && !is_std_internal_name(name)
                {
                    type_owners
                        .entry(name.to_string())
                        .or_default()
                        .push(std_module_of(rel));
                }
            }
        }
        type_owners
    })
}

/// Every `protocol` declaration in the embedded std sources, in file order: the one source of which
/// protocols exist, their shapes and the module that owns each. The build script extracts them
/// from the same parse as the module ASTs (`std_embed::STD_PROTOCOLS`), so their spans carry the
/// file's id.
pub(super) fn std_protocol_decls() -> &'static [StdProtocol] {
    static PROTOCOLS: std::sync::OnceLock<Vec<StdProtocol>> = std::sync::OnceLock::new();
    PROTOCOLS.get_or_init(|| {
        let decoded = crate::ast::codec::decode::<Vec<(u32, crate::ast::Stmt)>>(
            crate::native::std_embed::STD_PROTOCOLS,
        )
        .expect("the std protocol artifact decodes");
        let mut protocols = Vec::new();
        for (index, stmt) in decoded {
            let module = std_module_of(crate::native::std_embed::STD_FILES[index as usize].0);
            if let StmtKind::Protocol { name, .. } = &stmt.kind {
                protocols.push(StdProtocol {
                    name: name.clone(),
                    module,
                    stmt,
                });
            }
        }
        protocols
    })
}

/// The std module that declares the protocol `name` (`std.prelude` for `Any` and `Error`), or `None`
/// when no std file declares it.
pub fn std_protocol_owner(name: &str) -> Option<&'static str> {
    std_protocol_decls()
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.module.as_str())
}

pub(super) fn attach_native_module_metadata(name: &str, sig: &mut ModuleSig) {
    if let Some((_, docs)) = MODULE_FN_DOCS.iter().find(|(m, _)| *m == name) {
        for (fname, doc) in *docs {
            if let Some(f) = sig
                .members
                .get_mut(*fname)
                .and_then(|m| m.certain_fn.as_mut())
            {
                f.doc = Some((*doc).to_string());
            }
        }
    }
    // Phase 4c-concurrency — two method sigs a plain harvested sig CANNOT express, re-attached here.
    // Both are closure params whose expressible constraint is arity + (for `read`) the box's element
    // type, but whose RETURN must be `?` (any R) — a shape `std/concurrency.chz` declares as an
    // UNANNOTATED param (`Ty::Unknown`) and this step retypes.
    if name == "std.concurrency" {
        // `RwShared[T].read(f)` — `read(fn(T) -> R) -> R` is R-polymorphic: the param is retyped to
        // `fn(T) -> ?` (any closure return accepted; the real R is recovered at the call site by the
        // `Ty::RwShared` dispatch arm). `T` is RwShared's own type param (kept as `Ty::Param` in the
        // seeded table; the dispatch arm substitutes the concrete element type).
        // TICKET-246: `Shared[T].read(f)` has the same shape, so both boxes take one retype.
        for owner in ["RwShared", "Shared"] {
            let Some(rw) = sig.struct_defs.get_mut(owner) else {
                continue;
            };
            let t = rw
                .type_params
                .first()
                .map(|tp| Ty::Param(tp.name.clone()))
                .unwrap_or(Ty::Unknown);
            if let Some(read) = rw.methods.get_mut("read")
                && let Some(p0) = read.params.first_mut()
            {
                *p0 = Ty::Func {
                    params: vec![t],
                    ret: Box::new(Ty::Unknown),
                    labels: FnLabels::default(),
                    variadic: crate::ast::Variadic::No,
                };
            }
        }
        // `Executor.submit(f)` — a detached zero-arg task closure whose return is discarded: the param
        // is retyped to `fn() -> ?` so any-return closures are accepted while a wrong-ARITY closure is
        // still rejected (Executor is non-generic — no `Ty::Param` involved).
        if let Some(ex) = sig.struct_defs.get_mut("Executor")
            && let Some(submit) = ex.methods.get_mut("submit")
            && let Some(p0) = submit.params.first_mut()
        {
            *p0 = Ty::Func {
                params: vec![],
                ret: Box::new(Ty::Unknown),
                labels: FnLabels::default(),
                variadic: crate::ast::Variadic::No,
            };
        }
    }
}

/// Editor hover (Tier C): concise one-line `(module, [(fn, doc)])` blurbs for native stdlib module
/// functions, paraphrased from `docs/reference/stdlib/ §4`. Applied to `FnSig.doc` in `native_module_sig`
/// and surfaced verbatim at the `module.fn` hover site. Drift-guarded by `module_fn_docs_all_resolve`
/// (every listed fn must exist). Coverage is `std.math` / `std.io` / `std.os` for v1; other native
/// modules are a follow-up (noted in PROGRESS.md) — their fns simply hover doc-less, as before.
#[allow(clippy::type_complexity)]
pub(super) const MODULE_FN_DOCS: &[(&str, &[(&str, &str)])] = &[
    (
        "std.math",
        &[
            (
                "abs",
                "absolute value (numeric-polymorphic: int→int, float→float)",
            ),
            ("floor", "round down to the nearest integer (toward -inf)"),
            ("ceil", "round up to the nearest integer (toward +inf)"),
            (
                "round",
                "round to the nearest integer (half away from zero)",
            ),
            ("pow", "pow(base, exp): base raised to exp"),
            ("sqrt", "square root (NaN for a negative argument)"),
            ("sin", "sine of an angle in radians"),
            ("cos", "cosine of an angle in radians"),
            ("tan", "tangent of an angle in radians"),
            ("asin", "arcsine in radians (NaN outside [-1, 1])"),
            ("acos", "arccosine in radians (NaN outside [-1, 1])"),
            ("atan", "arctangent in radians"),
            (
                "atan2",
                "atan2(y, x): angle of the vector (x, y) in radians",
            ),
            ("exp", "e raised to the power x"),
            ("ln", "natural (base-e) logarithm (-inf at 0, NaN below)"),
            ("log2", "base-2 logarithm"),
            ("log10", "base-10 logarithm"),
            (
                "log",
                "log(value, base): logarithm of value in the given base",
            ),
            ("is_nan", "true if x is NaN"),
            ("is_inf", "true if x is +inf or -inf"),
            ("is_finite", "true if x is neither NaN nor infinite"),
        ],
    ),
    (
        "std.io",
        &[
            ("print", "write a line to stdout (with a trailing newline)"),
            ("eprint", "write a line to stderr (with a trailing newline)"),
            (
                "read_line",
                "read one line from stdin, newline stripped (None at EOF)",
            ),
            (
                "flush",
                "flush this process's stdout (a no-op: streamed output is unbuffered)",
            ),
            (
                "input",
                "input(prompt): print the prompt (no newline), flush, read one line (None at EOF)",
            ),
            ("read_file", "read a whole file as text (may fail)"),
            (
                "write_file",
                "write/overwrite a file with the given text (may fail)",
            ),
        ],
    ),
    (
        "std.os",
        &[
            (
                "args",
                "program arguments (positionals after the script path)",
            ),
            ("env", "look up an environment variable (None if unset)"),
            ("getcwd", "the current working directory (may fail)"),
            (
                "exit",
                "halt the program with an exit code (does NOT run defers)",
            ),
        ],
    ),
    (
        "std.time",
        &[(
            "timer",
            "one-shot timeout channel — timer(ms) delivers `true` once after ms milliseconds",
        )],
    ),
];
