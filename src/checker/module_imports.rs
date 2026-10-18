//! Checker: diagnostics, module entry, import declarations and import binding.
//! Split out of `src/checker/setup.rs`. `super::*` is the parent module.

use super::setup::TypeHeadKind;
use super::setup::variant_import_binds;
use super::*;
use crate::resolver::GlobalSource;

impl Checker {
    /// A std-internal name (`is_std_internal_name`) read from a module that is not std.
    pub(super) fn std_internal_hidden(&self, name: &str) -> bool {
        crate::checker::is_std_internal_name(name) && !self.current_module_is_stdlib
    }

    pub(super) fn error(&mut self, span: Span, message: impl Into<String>) {
        self.error_help(span, message, None);
    }

    /// Like `error`, plus a near-miss suggestion for a "did you mean" `help` line. `help` is never
    /// module-attributed — only `message` passes through `attribute`.
    pub(super) fn error_help(
        &mut self,
        span: Span,
        message: impl Into<String>,
        help: Option<String>,
    ) {
        // TICKET-238 -- a use of a rejected binding reports nothing more: one error per open
        // binding. The silence requires an error already in `errors`.
        if self.after_hole_error() {
            return;
        }
        let message = self.resolve_var_tokens(self.attribute(message));
        let mut e = CheckError::error(message, span);
        e.help = help;
        self.errors.push(e);
    }

    /// A NON-FATAL diagnostic: reported alongside the errors but never in the `Err` arm, so the exit
    /// code is unchanged. Shares `error`'s module attribution — a warning raised while checking an
    /// imported module must name it, exactly like an error from the same position.
    pub(super) fn warn(&mut self, span: Span, message: impl Into<String>) {
        let message = self.resolve_var_tokens(self.attribute(message));
        self.warnings.push(CheckError::warning(message, span));
    }

    /// TICKET-238 -- replace every pending-variable token `Ty`'s formatter printed with
    /// `var_text`, until none is left. The two diagnostic funnels call it; a new sink that renders
    /// a type for a user must zonk first or call this.
    fn resolve_var_tokens(&self, mut message: String) -> String {
        use super::ty::{VAR_CLOSE, VAR_OPEN};
        while let Some(a) = message.find(VAR_OPEN) {
            let Some(len) = message[a..].find(VAR_CLOSE) else {
                break;
            };
            let digits = &message[a + VAR_OPEN.len_utf8()..a + len];
            let text = digits.parse().map_or("_".to_string(), |v| self.var_text(v));
            message.replace_range(a..a + len + VAR_CLOSE.len_utf8(), &text);
        }
        message
    }

    /// Snapshot BOTH diagnostic channels for the speculative-inference idiom: mark, run a probe
    /// re-inference, [`Checker::diag_rollback`] everything the probe emitted so the REAL path reports
    /// it exactly once. Pair them — never truncate a channel by hand — because the pair is what keeps
    /// the two vectors in step. `warnings` is the reason this exists: a rule that warns inside a fn
    /// body would otherwise fire twice (return-inference pass, then pass 2) and would also fire from
    /// speculative branches whose errors are correctly discarded.
    pub(super) fn diag_mark(&self) -> DiagMark {
        DiagMark {
            errors: self.errors.len(),
            warnings: self.warnings.len(),
            ret_coerce: self.ret_coerce.clone(),
            wraps: self.wraps.clone(),
            const_overflow_seen: self.const_overflow_seen.clone(),
            tyvars: self.tyvars.borrow().mark(),
            hover_result: self.hover_result.clone(),
            poison_reads: self.poison_reads,
            stmt_poison: self.stmt_mark.poison,
            table_conflicts: self.table_conflicts.clone(),
            kw_certain: self.kw_certain.clone(),
            kw_written: self.kw_written.clone(),
            kw_pending: self.kw_pending.clone(),
            fn_write_scopes: self.fn_write_scopes.clone(),
        }
    }

    /// Discard every diagnostic — error AND warning — recorded since `m`, and restore the speculative
    /// state a discarded diagnostic consumed. See [`Checker::diag_mark`].
    pub(super) fn diag_rollback(&mut self, m: DiagMark) {
        self.errors.truncate(m.errors);
        self.warnings.truncate(m.warnings);
        self.ret_coerce = m.ret_coerce;
        self.wraps = m.wraps;
        self.const_overflow_seen = m.const_overflow_seen;
        self.tyvars.borrow_mut().rollback(m.tyvars);
        self.hover_result = m.hover_result;
        self.poison_reads = m.poison_reads;
        self.stmt_mark.poison = m.stmt_poison;
        self.table_conflicts = m.table_conflicts;
        self.kw_certain = m.kw_certain;
        self.kw_written = m.kw_written;
        self.kw_pending = m.kw_pending;
        self.fn_write_scopes = m.fn_write_scopes;
    }

    /// Attribute a diagnostic to the module currently being checked (graph path only). Shared by
    /// `error` and `warn` so the two can never drift on how a cross-module diagnostic reads.
    pub(super) fn attribute(&self, message: impl Into<String>) -> String {
        match &self.current_module_label {
            Some(label) => format!("in module '{label}': {}", message.into()),
            None => message.into(),
        }
    }

    /// Report each name that repeats within `items` as a "`<kind>` '`<name>`' is already defined"
    /// error at its decl-site span. Fires once per REPEAT occurrence (the 2nd, 3rd, … copy), so the
    /// first occurrence stays the surviving definition (mirrors the dup-variant precedent). Shared by
    /// the struct/enum method-hoist arms and the struct field-hoist arm so the seen-set loop
    /// is written once, not inlined four times.
    pub(super) fn report_dup_names<'a>(
        &mut self,
        items: impl IntoIterator<Item = (&'a str, Span)>,
        kind: &str,
    ) {
        let mut seen: std::collections::HashSet<&'a str> = std::collections::HashSet::new();
        for (name, span) in items {
            if !seen.insert(name) {
                self.error(span, format!("{kind} '{name}' is already defined"));
            }
        }
    }

    /// Reset per-module state (functions, scopes, imports, current fn) before checking the next
    /// module of a multi-file program. Program-global tables (structs/enums/variants/their names,
    /// `module_sigs`) and accumulated `errors` are kept.
    pub(super) fn begin_module(&mut self, label: Option<String>) {
        self.scopes.clear();
        self.hole_rejected.clear();
        // `push_scope` pushes both stacks; index `i` of one is scope `i` of the other (TICKET-190).
        self.fn_write_scopes.clear();
        self.loop_vars.clear();
        self.functions.clear();
        self.fn_homes.clear();
        self.local_fn_names.clear();
        self.name_docs.clear();
        self.type_params.clear();
        self.imported_modules.clear();
        self.import_paths.clear();
        self.import_binds.clear();
        self.imported_alias_tys.clear();
        self.imported_alias_ctypes.clear();
        self.imported_values.clear();
        self.builtin_binds.clear();
        if let Some(m) = label.as_deref() {
            self.bind_own_builtins(m);
        }
        // Types are MODULE-SCOPED: a type declared in module A is NOT visible bare in module B (it
        // must be imported). Clear the per-module type tables so a prior module's types don't leak.
        // The synthetic stdlib structs (`Match`/`Response`) and pre-seeded protocols are global, so
        // they're re-seeded after the clear.
        self.structs.clear();
        self.enums.clear();
        self.enum_type_params.clear();
        self.enum_methods.clear();
        self.variants.clear();
        self.variant_owners.clear();
        self.struct_names.clear();
        self.enum_names.clear();
        self.aliases.clear();
        self.bare_types.clear();
        self.imported_variants.clear();
        self.seed_stdlib_structs();
        self.current_ret = Ty::Nil;
        self.in_fn_body = false;
        self.in_default_provider = false;
        self.ret_declared = false;
        self.inferring_ret = false;
        self.current_module_label = label;
    }

    /// Check one module's statements with its imports bound first; returns its public signature.
    /// `id` is `Some` for a graph module (enables import binding), `None` for a lone `check`.
    pub(super) fn check_module(
        &mut self,
        stmts: &[Stmt],
        id: Option<&ModuleId>,
        imports: &[ResolvedImport],
        binds: &[(String, GlobalSource)],
    ) -> ModuleSig {
        self.push_scope();
        // The one record per module slot, built before any body is walked (TICKET-186).
        // `seed_module_globals` and the fn-writes pass read it.
        self.collect_module_globals(stmts, imports, binds);
        // Module-scoped types: record THIS module's id and seed its locally-declared type names into
        // `bare_types` under their runtime key (bare unless disambiguated), so a bare annotation /
        // constructor resolves to the same key the layout is registered under. `bind_import` then adds
        // `from`-imported + std-whole-module type names. Done before `bind_import`/`hoist` so type
        // resolution during hoisting sees the keys.
        self.current_module_id = id.cloned();
        if let Some(mid) = id {
            for s in stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::TypeAlias { name, .. }
                | StmtKind::Protocol { name, .. } = &s.kind
                {
                    // A std protocol keeps its bare name as its key, in its declaring module too.
                    let key = if matches!(s.kind, StmtKind::Protocol { .. })
                        && is_std_protocol(name)
                        && self.current_module_is_stdlib
                    {
                        name.clone()
                    } else {
                        self.type_key(mid, name)
                    };
                    self.bare_types.insert(name.clone(), key);
                }
            }
        }
        for imp in imports {
            self.bind_import(imp, binds);
        }
        self.note_variant_import_binds(stmts);
        self.collect_names(stmts);
        if self.current_module_is_stdlib && self.protocols.is_empty() {
            // The first std module is the prelude. Its signatures name `Hashable` and `Any`, so
            // the shapes from the `protocol` declarations in `std/*.chz` exist before it is hoisted.
            self.seed_std_protocols();
        }
        self.collect_docs(stmts);
        self.hoist(stmts);
        self.bind_variant_imports(stmts);
        self.infer_self_writers(stmts);
        self.infer_fn_writers(stmts);
        // SINGLE-RESOLVER FFI fix: cache every struct declared in THIS module under its identity key,
        // its by-value `CType::Struct` computed HERE — in this (the DEFINING) module's import/alias
        // scope (extends the `AliasSig::ctype` precedent to structs). Done only when harvesting
        // externs, after `hoist` (all of this module's aliases/`from`-imports are live) and BEFORE the
        // check_stmt loop (so a same-module extern harvested in the loop reads the cache). Modules are
        // checked deps-first, so a downstream importer's extern returning `mod.Struct` reads this
        // cached, defining-scope CType verbatim — its own (colliding/invisible) scope is never used.
        if self.extern_module_idx.is_some() {
            self.populate_struct_ctypes(stmts, id);
        }
        self.infer_returns(stmts);
        self.record_hook_table(stmts);
        self.pretype_place_subscripts(stmts);
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        if !self.current_module_is_stdlib {
            for (name, span) in super::unused::unused_locals(stmts) {
                self.warn(
                    span,
                    format!(
                        "unused variable '{name}': it is never read; prefix it with '_' if that is intended"
                    ),
                );
            }
        }
        let sig = self.capture_sig(stmts);
        self.pop_scope();
        sig
    }

    /// Record that `bind` is bound by an import at `span`. Returns `true` if this name was ALREADY
    /// bound by an earlier import in this module: the caller then skips the re-bind. It reports
    /// nothing: the registry keeps the first import's span for messages, and
    /// `GlobalBinding::clashes` reports a second import.
    ///
    /// An import no one wrote (`Span::NOWHERE`) has no entry: every message that names an import
    /// reads this registry, and that import has no line to name.
    pub(super) fn note_import_bind(&mut self, bind: &str, span: Span) -> bool {
        if span.is_nowhere() {
            return false;
        }
        if self.import_binds.contains_key(bind) {
            return true;
        }
        self.import_binds.insert(bind.to_string(), span);
        false
    }

    /// Enter every variant import of this module into the import registry (TICKET-229). A
    /// declaration or an import in a user module shadows a name the prelude binds, as a builtin; a
    /// reserved bound name is rejected like a from-import alias; `GlobalBinding::clashes` reports a
    /// second import of one name. Adds no collision test of its own.
    fn note_variant_import_binds(&mut self, stmts: &[Stmt]) {
        for (bind, _, _, span) in variant_import_binds(stmts) {
            self.note_import_bind(&bind, span);
        }
    }

    /// Bind the variant imports `note_variant_import_binds` accepted, once `hoist` has declared
    /// this module's enums: `imported_variants` is the only table that makes a bare name a variant.
    fn bind_variant_imports(&mut self, stmts: &[Stmt]) {
        for (bind, en, variant, span) in variant_import_binds(stmts) {
            if self.import_binds.get(&bind) != Some(&span) {
                continue;
            }
            let Some(head) = self
                .bare_type_head(&en)
                .filter(|h| h.kind == TypeHeadKind::Enum)
            else {
                self.error(span, format!("'{en}' is not an enum"));
                continue;
            };
            if !self
                .variants
                .contains_key(&(head.key.clone(), variant.clone()))
            {
                let names = self.variant_names(&head.key);
                self.error_help(
                    span,
                    format!("enum '{en}' has no variant '{variant}'"),
                    suggest::did_you_mean(&variant, &names),
                );
                continue;
            }
            self.imported_variants.insert(
                bind,
                ImportedVariant {
                    head,
                    variant,
                    prelude: false,
                },
            );
        }
    }

    /// The use-site diagnostic for a bare name that two un-aliased imports both bind (TICKET-175),
    /// or `None` when `name` is not ambiguous. The suggested alias is the initials of the first
    /// path's segments (`a.math` gives `am`).
    pub(super) fn ambiguous_bind_msg(&self, name: &str) -> Option<String> {
        let (a, b) = self.ambiguous_binds.get(name)?;
        let short = a
            .split('.')
            .filter_map(|s| s.chars().next())
            .collect::<String>();
        Some(format!(
            "'{name}' is ambiguous: `{a}` and `{b}` both bind it — write the full path (`{a}.<Name>`), or bind one under its own name with `import {a} as {short}`"
        ))
    }

    /// TICKET-187 — bind the TYPE a from-import names (`import Q from lib`): a struct, enum, alias,
    /// protocol or `std` type name. The one type binder for a from-import.
    #[allow(clippy::too_many_arguments, clippy::ptr_arg)] // the branch body, moved verbatim
    fn bind_imported_type(
        &mut self,
        bind: &String,
        member: &String,
        sig: &ModuleSig,
        imp: &ResolvedImport,
        path: &[String],
        name_span: &Span,
    ) {
        // A module-owned builtin type (`Shared`, `Socket`, `ptr`, `int32`, …) imported from its
        // owning module: bind it under the bind name, an alias included.
        if super::builtin_type::builtin_owner(member) == Some(path.join(".").as_str()) {
            self.builtin_binds.insert(bind.clone(), member.clone());
            self.record_native_type_import_hover(member, *name_span, path);
        } else if let Some(info) = sig.struct_defs.get(member) {
            // A user struct imported by name: inject its resolved shape under the
            // DECLARING module's runtime key (so it unifies with that module's
            // signatures + a value's `Ty`), and make it BARE-VISIBLE under the bind
            // name via `struct_names`/`bare_types` so `S(...)`/`x: S` resolve here.
            let key = self.type_key(&imp.target, member);
            self.bind_imported_struct_name(bind, &key, info, imp.span.is_nowhere());
            // Editor hover (Tier C): the imported type's doc — its own decl docstring
            // carried across the boundary, else a `kind (from module)` fallback. Seed
            // `name_docs[bind]` so a later bare/annotation/generic-head use surfaces it
            // (the `Type::Named`/`Type::Generic` hover arms read `name_docs`), AND record
            // the import-line token hover here. Both are probe-gated no-ops off-probe.
            self.record_imported_type_hover(
                bind,
                *name_span,
                &Ty::strukt(key),
                info.doc.as_deref(),
                "struct",
                path,
            );
        } else if let Some(edef) = sig.enum_defs.get(member) {
            // A user enum imported by name: inject its variant names, type params, and
            // each variant's payload under the declaring module's runtime key; expose
            // it bare under the bind name.
            let key = self.type_key(&imp.target, member);
            self.enums.insert(key.clone(), edef.variant_names.clone());
            self.enum_names.insert(bind.clone());
            self.bare_types.insert(bind.clone(), key.clone());
            self.enum_type_params
                .insert(key.clone(), edef.type_params.clone());
            self.enum_methods.insert(key.clone(), edef.methods.clone());
            for (vname, vinfo) in edef.variant_names.iter().zip(&edef.variants) {
                let mut vi = vinfo.clone();
                vi.enum_name = key.clone();
                self.variants.insert((key.clone(), vname.clone()), vi);
                self.variant_owners
                    .entry(vname.clone())
                    .or_default()
                    .push(bind.clone());
            }
            self.record_imported_type_hover(
                bind,
                *name_span,
                &Ty::Enum(key, vec![]),
                edef.doc.as_deref(),
                "enum",
                path,
            );
        } else if let Some(asig) = sig.type_aliases.get(member) {
            // A user type alias imported by name. An unlicensed alias embedding an
            // un-imported FFI width cannot be laundered — reject it here, mirroring the
            // old use-site "unknown type" error.
            if let Some(w) = &asig.unlicensed_width {
                self.error(
                    imp.span,
                    format!(
                        "unknown type '{w}' (import it from std.ffi: `import {w} from std.ffi`)"
                    ),
                );
            } else {
                // Inject the alias's RESOLVED body so bare use (`x: Len`) resolves to
                // the underlying type. A licensed FFI-width alias re-seeds
                // `ffi_alias_ok` under the bind name (defensive; the body is already
                // a concrete `Ty`, so no width re-check is hit).
                self.imported_alias_tys
                    .insert(bind.clone(), asig.body.clone());
                self.hydrate_alias_target(&asig.body);
                // Carry the alias's width-bearing CType (computed in its DEFINING
                // module's scope) so an extern boundary in THIS module marshals the
                // real width through the named-import hop — not the bare flat map.
                self.imported_alias_ctypes
                    .insert(bind.clone(), asig.ctype.clone());
                if asig.licensed {
                    self.ffi_alias_ok.insert(bind.clone());
                }
            }
        } else if let Some(pdef) = sig.protocol_defs.get(member) {
            // A protocol carries no runtime value and no layout, so registering the
            // exported shape under its declaring-module KEY (TICKET-027) plus a
            // bare-visible entry under the BIND name is the whole binding.
            let key = self.type_key(&imp.target, member);
            self.protocols.insert(key.clone(), pdef.info.clone());
            self.bare_types.insert(bind.clone(), key.clone());
            self.record_imported_type_hover(
                bind,
                *name_span,
                &Ty::Protocol(key, Vec::new()),
                pdef.doc.as_deref(),
                "protocol",
                path,
            );
        }
    }

    /// THE one decision of which module imports bind their bare name (TICKET-175): when two
    /// un-aliased dotted imports share a last segment (`import a.math` + `import b.math`), neither
    /// binds the bare name, both full paths stay bound, and a bare use reports the ambiguity. It
    /// writes `ambiguous_binds` and `nameless_imports`.
    pub(super) fn decide_bare_imports(&mut self, imports: &[ResolvedImport]) {
        self.ambiguous_binds.clear();
        self.nameless_imports.clear();
        let mut first: HashMap<String, (String, Span)> = HashMap::new();
        for imp in imports {
            let Import::Module {
                path, alias: None, ..
            } = &imp.import
            else {
                continue;
            };
            let Some(name) = path.last().filter(|_| path.len() >= 2) else {
                continue;
            };
            let dotted = path.join(".");
            let Some((prev, prev_span)) = first.get(name).cloned() else {
                first.insert(name.clone(), (dotted, imp.span));
                continue;
            };
            if prev != dotted {
                self.ambiguous_binds
                    .entry(name.clone())
                    .or_insert((prev, dotted));
                self.nameless_imports.insert((prev_span, name.clone()));
                self.nameless_imports.insert((imp.span, name.clone()));
            }
        }
    }

    /// Bind an import into the current module: a whole-module import becomes a `Ty::Module` name;
    /// a `from` import injects each member (function/value) into scope, validating it exists.
    /// `binds` is the module's slot list: a `from` member is bound as a value exactly when it holds
    /// a slot there.
    pub(super) fn bind_import(&mut self, imp: &ResolvedImport, binds: &[(String, GlobalSource)]) {
        match &imp.import {
            Import::Module {
                path,
                alias,
                name_span,
            } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                // Only keywords are reserved: a module bound under a builtin's name (`import
                // std.math as int`) shadows the builtin in this module, as a local binding does.
                // The resolver's synthetic full-path bind (`a.b` for `import a.b`, TICKET-175).
                let full = crate::ast::is_full_path_bind(&name);
                // `decide_bare_imports` decided whether this import binds its bare name.
                let bare = !self.nameless_imports.contains(&(imp.span, name.clone()));
                if bare && self.note_import_bind(&name, imp.span) {
                    return;
                }
                // Editor hover (decl-site): record the bound module name's type at the bound-name
                // token (`math` / the `as` alias). Probe-gated no-op off the probe / outside entry.
                // The synthetic full-path bind shares that token, so it records nothing.
                if bare && self.hover_probe.is_some() && !full {
                    self.hover_record_at(
                        *name_span,
                        &Ty::Module(name.clone()),
                        HoverKind::Other,
                        None,
                    );
                }
                if bare {
                    self.imported_modules
                        .insert(name.clone(), imp.target.clone());
                }
                // Record each dotted import for the diagnostics of a full path that does not resolve
                // (`pkg.deep.X` after `import pkg.deep as d`, or `pkg.other.X` never imported).
                if !full && path.len() >= 2 {
                    self.import_paths
                        .push((path.clone(), name.clone(), alias.is_none()));
                }
                if bare {
                    self.declare(&name, Ty::Module(name.clone()));
                }
                // Register the imported module's struct/enum LAYOUTS into the per-module shape tables
                // (so `geo.Point(1,2).x` and `geo`'s enum methods resolve), but NOT into the bare
                // *_names sets — a bare `Point` must still error. The bare-name gate (`struct_names`/
                // `enum_names`) stays cleared; `infer_field`/`infer_method_call` consult `self.structs`/
                // `self.enums` for the layout, which is what these provide. A same-named layout already
                // present (a local decl or another import) is NOT overwritten (first/local wins; the
                // compiler disambiguates any genuine runtime collision). A std module is no
                // different: `import std.regex` gives `regex.Match`, never a bare `Match`.
                if let Some(sig) = self.module_sigs.get(&imp.target).cloned() {
                    for (sname, info) in &sig.struct_defs {
                        // A RESERVED native handle (std.net's `Socket`/`Listener`) has a `sig.struct_defs`
                        // entry only for its harvested METHOD table — it is NOT a constructible/nominal
                        // struct (it resolves to the opaque `Ty::Socket`/`Ty::Listener` via
                        // `resolve_type`'s reserved arm, and its method table is seeded independently by
                        // `seed_stdlib_structs`). Skip it entirely here so a whole-module `import std.net`
                        // does NOT make bare `Socket(...)` a constructor or `Socket` a nominal-struct
                        // annotation (both must stay gated by the reserved arm / rejected as a ctor).
                        if sig.builtins.contains(sname) {
                            continue;
                        }
                        // Register the LAYOUT under the DECLARING module's runtime key (bare unless a
                        // genuine cross-module clash). Register BOTH colliding layouts (no first-wins),
                        // so a value of either — whose `Ty` carries the matching key — resolves its
                        // own fields/methods.
                        let key = self.type_key(&imp.target, sname);
                        self.structs.insert(key.clone(), info.clone());
                    }
                    for (ename, edef) in &sig.enum_defs {
                        let key = self.type_key(&imp.target, ename);
                        self.enums.insert(key.clone(), edef.variant_names.clone());
                        self.enum_type_params
                            .insert(key.clone(), edef.type_params.clone());
                        self.enum_methods.insert(key.clone(), edef.methods.clone());
                        for (vname, vinfo) in edef.variant_names.iter().zip(&edef.variants) {
                            let mut vi = vinfo.clone();
                            vi.enum_name = key.clone();
                            self.variants.insert((key.clone(), vname.clone()), vi);
                        }
                    }
                    // A `module.Alias` needs its target's shape too — the target may live in a
                    // module this one never imports (TICKET-172).
                    for asig in sig.type_aliases.values() {
                        self.hydrate_alias_target(&asig.body);
                    }
                }
            }
            // The resolver never resolves a variant import; `bind_variant_imports` binds it.
            Import::Variants { .. } => {}
            Import::From {
                path,
                names,
                name_spans,
            } => {
                let sig = self
                    .module_sigs
                    .get(&imp.target)
                    .cloned()
                    .unwrap_or_default();
                // `name_spans` is parallel to `names`; zip truncates safely if they ever diverge (a
                // bound name's hover is dropped, never a panic). Diagnostic-only — the per-name span
                // anchors the decl-site hover at the bound import name.
                for ((member, alias), name_span) in names.iter().zip(name_spans.iter()) {
                    let bind = alias.as_ref().unwrap_or(member);
                    if self.note_import_bind(bind, imp.span) {
                        continue;
                    }
                    let holds = binds
                        .iter()
                        .any(|(n, src)| n == bind && matches!(src, GlobalSource::Import));
                    let no_member = |this: &mut Self| {
                        let candidates = Self::sig_member_names(&sig);
                        this.error_help(
                            *name_span,
                            format!(
                                "module '{}' has no member '{member}'",
                                module_label(&imp.import)
                            ),
                            suggest::did_you_mean(member, &candidates),
                        );
                    };
                    if !holds {
                        if sig.types.contains(member) {
                            self.bind_imported_type(bind, member, &sig, imp, path, name_span);
                        } else {
                            no_member(self);
                        }
                    } else if let Some(fsig) = sig.certain_fn(member) {
                        self.functions.insert(bind.clone(), fsig.clone());
                        if let Some(&home) = self.module_idx_of.get(&imp.target) {
                            self.fn_homes.insert(bind.clone(), (home, member.clone()));
                        }
                        // Editor hover (decl-site): record the imported function's signature at the
                        // bound-name token (probe-gated no-op off the probe / outside the entry module).
                        if self.hover_probe.is_some() {
                            let fty = crate::checker::fn_value_ty(fsig);
                            self.hover_record_at(
                                *name_span,
                                &fty,
                                HoverKind::Func,
                                fsig.doc.clone(),
                            );
                        }
                    } else if let Some(m) = sig.member(member) {
                        // Editor hover (decl-site): record the imported value's type at the bound name.
                        if self.hover_probe.is_some() {
                            self.hover_record_at(*name_span, &m.ty, HoverKind::Other, None);
                        }
                        self.declare(bind, m.ty.clone());
                        // The bind is a SNAPSHOT copy of the module global — rebinding it is rejected
                        // in `check_assign` (see `imported_values`). Its const-ness arrives through
                        // its `GlobalBinding` (`DeclKind::Import`).
                        self.imported_values.insert(bind.clone(), path.join("."));
                    } else {
                        no_member(self);
                    }
                }
            }
        }
    }
}

/// A snapshot of the lengths of BOTH diagnostic channels, taken by [`Checker::diag_mark`] and undone
/// by [`Checker::diag_rollback`]. `errors` is public within the checker because several sites also
/// ask "did the probe ERROR?" (a cascade-suppression signal — a warning is not an error and must not
/// answer it); `warnings` is only ever compared by the rollback itself.
#[derive(Clone)]
pub(super) struct DiagMark {
    pub(super) errors: usize,
    warnings: usize,
    /// W8-21 — a nested ANNOTATED fn inside an un-annotated one is body-checked for real
    /// (`check_fn_body:4065`'s comment) on every speculative `infer_inline_fn_ret` pass, and a forward-declared
    /// callee can be `Unknown` on an early pass and concrete on a later one — so the SAME return span
    /// can record two different verdicts across passes, which `record_call_table_entry` turns into a
    /// hard `internal:` error on a valid program. `ret_coerce` is DECIDED state exactly like
    /// diagnostic buffers, not a diagnostic itself, so it needs the same snapshot-and-restore: the one true
    /// recording happens on the real (non-speculative) `check_stmt` walk, once every callee sig is
    /// settled.
    ret_coerce: crate::checker::RetCoerceTable,
    /// TICKET-227 -- the implicit wraps; decided state, same reason as `ret_coerce`.
    wraps: crate::checker::WrapTable,
    /// TICKET-142 (W14-33) — the constant-overflow dedupe set is speculative state too: a
    /// speculative pass (return inference of an un-annotated fn, a generic-arg
    /// prepass) records a span, then the rollback discards its error. A rollback that kept the span
    /// would leave the real pass silent. Empty unless the program overflows, so the clone is free.
    const_overflow_seen: std::collections::HashSet<Span>,
    /// TICKET-157 — a speculative walk's NON-diagnostic effects must be erased too, or the nested-fn
    /// return memo (`Checker::ret_memo`) is unsound: the memo replays only a walk's type and its
    /// post-rollback diagnostics, so everything the skipped walk did BEFORE its rollback has to be
    /// something the rollback already undoes. Each field below is written during a body walk and is
    /// restored wholesale, never re-derived.
    /// A NEW `Checker` field written during a body walk must be added here in the same commit.
    /// TICKET-225 — the type-variable store (DEC-157): a walk binds vars and creates them.
    tyvars: super::tyvar::TyVarMark,
    hover_result: Option<(Ty, HoverKind, Option<String>)>,
    poison_reads: usize,
    /// TICKET-238 -- a nested statement inside the walk raises `stmt_mark.poison` by its reads.
    /// The rollback takes the reads back, so it takes the raise back too.
    stmt_poison: usize,
    table_conflicts: Vec<(Span, String)>,
    kw_certain: HashMap<(usize, String), Vec<(usize, String)>>,
    kw_written: std::collections::HashSet<(usize, String)>,
    kw_pending: Vec<(Vec<(usize, String)>, super::globals::KwUse)>,
    fn_write_scopes: Vec<HashMap<String, fn_writes::FnSummary>>,
}
