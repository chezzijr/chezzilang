// checker::setup — split out of checker/mod.rs. `super::*` == the `checker` module.
// Checker construction; stdlib/struct/enum/newtype seeding; signature harvesting.

use super::*;

impl Checker {
    pub(super) fn new() -> Self {
        let mut c = Checker {
            errors: Vec::new(),
            scopes: Vec::new(),
            const_decls: Vec::new(),
            loop_vars: Vec::new(),
            capture_table: Vec::new(),
            module_global_lets: std::collections::HashSet::new(),
            functions: HashMap::new(),
            local_fn_names: std::collections::HashSet::new(),
            structs: HashMap::new(),
            protocols: prebuilt_protocols(),
            type_params: HashMap::new(),
            enums: HashMap::new(),
            enum_type_params: HashMap::new(),
            enum_methods: HashMap::new(),
            variants: HashMap::new(),
            variant_owners: HashMap::new(),
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            newtype_names: std::collections::HashSet::new(),
            newtype_defs: HashMap::new(),
            newtype_type_params: HashMap::new(),
            aliases: HashMap::new(),
            alias_resolving: Vec::new(),
            ffi_alias_ok: std::collections::HashSet::new(),
            current_ret: Ty::Nil,
            current_self_ty: None,
            yield_ty: None,
            recover_depth: 0,
            generic_arg_prepass: false,
            expected_hint: None,
            float_elem_hint: None,
            inferring_ret: false,
            collected_rets: Vec::new(),
            in_generator: false,
            in_fn_body: false,
            in_defer_block: false,
            collected_yields: Vec::new(),
            module_sigs: HashMap::new(),
            imported_modules: HashMap::new(),
            import_path_heads: HashMap::new(),
            module_prefix2: HashMap::new(),
            import_binds: HashMap::new(),
            imported_alias_tys: HashMap::new(),
            imported_alias_ctypes: HashMap::new(),
            extern_sigs: ExternTable::new(),
            extern_module_idx: None,
            keyword_calls: KeywordTable::new(),
            harvest_keywords: false,
            keyword_module_idx: 0,
            kw_frag_ctx: Span::default(),
            kw_frag_ord: 0,
            in_extern_sig: false,
            struct_field_asts: HashMap::new(),
            struct_ctypes: HashMap::new(),
            types_by_name: HashMap::new(),
            imported_poly: std::collections::HashSet::new(),
            imported_values: HashMap::new(),
            imported_consts: std::collections::HashSet::new(),
            imported_ffi_types: std::collections::HashSet::new(),
            imported_concurrency: std::collections::HashSet::new(),
            imported_time: std::collections::HashSet::new(),
            imported_net: std::collections::HashSet::new(),
            imported_io: std::collections::HashSet::new(),
            imported_builtin_types: std::collections::HashSet::new(),
            current_module_label: None,
            loop_depth: 0,
            capture_floors: Vec::new(),
            current_module_is_stdlib: false,
            net_socket_seed: None,
            net_listener_seed: None,
            io_writer_seed: None,
            io_reader_seed: None,
            concurrency_seeds: HashMap::new(),
            time_timer_sig: None,
            container_seeds: HashMap::new(),
            native_prelude_sigs: HashMap::new(),
            type_keys: HashMap::new(),
            current_module_id: None,
            bare_types: HashMap::new(),
            hover_probe: None,
            hover_entry: None,
            hover_result: None,
            name_docs: HashMap::new(),
            empty_coll_sites: Vec::new(),
            hover_pending: None,
        };
        c.seed_stdlib_structs();
        c
    }

    /// The runtime key for a bare-written type name in the CURRENT module: its `bare_types` entry when
    /// bare-visible (local / `from`-imported / std), else the name itself — which covers the reserved
    /// built-ins (`Result`/`Option`/`Ref`/…) and a not-bare-visible name (resolution then misses, as
    /// before). Mirrors the compiler's `enum_bare_key` (shared for structs + enums here).
    pub(super) fn bare_key(&self, name: &str) -> String {
        self.bare_types
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// The module-scoped runtime key for a type `name` declared in module `mid` (bare unless a genuine
    /// cross-module clash disambiguated it in [`check_graph`]). Mirrors the compiler's `type_key`.
    pub(super) fn type_key(&self, mid: &ModuleId, name: &str) -> String {
        self.type_keys
            .get(&(mid.clone(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    // --- member-resolution fallback by the value's OWN module-scoped identity key ---
    //
    // User types are MODULE-SCOPED: their per-module shape tables (`self.structs` / `self.enums` +
    // `enum_methods`/`enum_type_params` / `newtype_defs`+`newtype_type_params`) are populated ONLY
    // when the WHOLE module OR the type NAME is imported. A named FUNCTION import injects nothing, so a
    // factory result's value — correctly typed `Ty::Struct`/`Enum`/`NewType` carrying its owning
    // module's IDENTITY KEY (`type_key(mid, name)`) — misses the local table and its fields/methods
    // wrongly fail to resolve (gap #4). These helpers add a LAZY, MISS-ONLY fallback: on a local-table
    // miss, resolve the shape from the OWNING module's `ModuleSig` by scanning `module_sigs` for the
    // def whose `type_key(mid, name)` equals the value's key. The deps-first graph pass inserts every
    // dependency's sig into `module_sigs` before an importer's bodies are checked, so any legitimately
    // produced value's owning type is present (including transitive deps). Purely additive: fires only
    // on a MISS, so it never shadows the globally-seeded `Match`/`Response`/`ProcResult` (always in
    // `self.structs`) and costs nothing on the local-hit path; it reads `module_sigs` only — it does
    // NOT touch `struct_names`/`bare_types`, so NAMING/CONSTRUCTING an un-imported type still errors
    // (`resolve_type` stays gated) and a same-named LOCAL type is unaffected (distinct keys; the local
    // table hits first). Aligns the impl with docs/spec.md's "reading their fields off a returned value
    // works import-free".
    //
    // ASSUMPTION (identity-key uniqueness): the scan resolves the FIRST `ModuleSig` def whose
    // `type_key(mid, name)` matches. User types are always module-qualified, so their keys are unique.
    // Native/std-owned types keep the BARE name as their key, so two DISTINCT std modules each exporting
    // a bare-keyed type of the SAME name would resolve by iteration order — a latent edge that does not
    // arise today (no two std modules share a bare type name) and is inherited unchanged from the
    // member-access fix these helpers back; the protocol-satisfaction path adds no new exposure.

    /// A struct's shape looked up by identity key in the OWNING module's `ModuleSig` (miss-only
    /// fallback for [`Checker::struct_shape`]).
    pub(super) fn owning_struct_def(&self, key: &str) -> Option<&StructInfo> {
        self.module_sigs.iter().find_map(|(mid, sig)| {
            sig.struct_defs
                .iter()
                .find_map(|(name, info)| (self.type_key(mid, name) == key).then_some(info))
        })
    }

    /// A struct's shape by its value's identity key: the local per-module table first, else the
    /// owning module's `ModuleSig` (so a named-factory-import result resolves its fields/methods).
    pub(super) fn struct_shape(&self, key: &str) -> Option<&StructInfo> {
        self.structs
            .get(key)
            .or_else(|| self.owning_struct_def(key))
    }

    /// An enum's shape looked up by identity key in the owning module's `ModuleSig` (miss-only).
    pub(super) fn owning_enum_def(&self, key: &str) -> Option<&EnumSigInfo> {
        self.module_sigs.iter().find_map(|(mid, sig)| {
            sig.enum_defs
                .iter()
                .find_map(|(name, info)| (self.type_key(mid, name) == key).then_some(info))
        })
    }

    /// An enum's method table by identity key: local table first, else the owning `ModuleSig`.
    pub(super) fn enum_methods_of(&self, key: &str) -> Option<&HashMap<String, FnSig>> {
        self.enum_methods
            .get(key)
            .or_else(|| self.owning_enum_def(key).map(|e| &e.methods))
    }

    /// An enum's type params by identity key: local table first, else the owning `ModuleSig`.
    pub(super) fn enum_type_params_of(&self, key: &str) -> Option<&Vec<TypeParam>> {
        self.enum_type_params
            .get(key)
            .or_else(|| self.owning_enum_def(key).map(|e| &e.type_params))
    }

    /// A newtype's shape looked up by identity key in the owning module's `ModuleSig` (miss-only).
    pub(super) fn owning_newtype_def(&self, key: &str) -> Option<&NewTypeSigInfo> {
        self.module_sigs.iter().find_map(|(mid, sig)| {
            sig.newtype_defs
                .iter()
                .find_map(|(name, info)| (self.type_key(mid, name) == key).then_some(info))
        })
    }

    /// A newtype's method table by identity key: local table first, else the owning `ModuleSig`.
    pub(super) fn newtype_methods_of(&self, key: &str) -> Option<&HashMap<String, FnSig>> {
        self.newtype_defs
            .get(key)
            .map(|(_, ms)| ms)
            .or_else(|| self.owning_newtype_def(key).map(|nt| &nt.methods))
    }

    /// A newtype's type params by identity key: local table first, else the owning `ModuleSig`.
    pub(super) fn newtype_type_params_of(&self, key: &str) -> Option<&Vec<TypeParam>> {
        self.newtype_type_params
            .get(key)
            .or_else(|| self.owning_newtype_def(key).map(|nt| &nt.type_params))
    }

    /// Register the synthetic struct shapes that native std modules return (M9): `Match`
    /// (`std.regex`), `Response` (`std.request`), and `ProcResult` (`std.process`). They have no
    /// AST, so their field layouts are seeded here; `infer_field` then types `m.text`, `resp.status`,
    /// `r.code`, etc. — IMPORT-FREE, since the layout lookup is keyed by the name the native return's
    /// `Ty::Struct(...)` carries. These are MODULE-OWNED, NOT program-global: only the LAYOUT is
    /// seeded (so field access on a return works without an import); the BARE TYPE NAME for
    /// annotation/construction is licensed ONLY by importing the owning module (whose
    /// `native_module_sig` exports the same shape via `struct_defs`). So a user `struct Response`
    /// (without `import std.request`) is their OWN type — the `Builtin`-origin seed is overwritten.
    pub(super) fn seed_stdlib_structs(&mut self) {
        let mk = |fields: Vec<(&str, Ty)>| StructInfo {
            type_params: Vec::new(),
            fields: fields
                .into_iter()
                .map(|(n, t)| (n.to_string(), t))
                .collect(),
            methods: HashMap::new(),
            origin: StructOrigin::Builtin,
            doc: None,
        };
        // The LAYOUT stays globally present (so field access on a native return — `regex.find(...)
        // .text`, `request.get(...).status`, `process.run(...).code` — resolves import-free via
        // `infer_field`'s `self.structs[sname]` lookup keyed by the name the native return's
        // `Ty::Struct("Match",...)` carries). The BARE-NAME reservation (`struct_names`) is NOT
        // seeded: the bare type name (`m: Match` / `Match(...)`) is licensed ONLY by importing the
        // owning module (`import std.regex` / `from std.regex import Match`), whose `struct_defs`
        // flow into `struct_names`/`bare_types` on import. This frees the names for a user `struct
        // Response` — the always-present `Builtin`-origin seed is overwritten by a user `User`-origin
        // declaration (see the already-defined gate in the hoist pass).
        self.structs.insert(
            "Match".into(),
            mk(vec![
                ("text", Ty::Str),
                ("start", Ty::Int),
                ("end", Ty::Int),
                ("groups", Ty::list(Ty::Str)),
            ]),
        );
        self.structs.insert(
            "Response".into(),
            mk(vec![
                ("status", Ty::Int),
                ("body", Ty::Str),
                ("headers", Ty::map(Ty::Str, Ty::Str)),
            ]),
        );
        self.structs.insert(
            "ProcResult".into(),
            mk(vec![
                ("stdout", Ty::Str),
                ("stderr", Ty::Str),
                ("code", Ty::Int),
            ]),
        );
        // gaps §6 — `FileInfo` from `std.fs` (returned by `fs.stat`). Field order load-bearing
        // (matches `native/fs.rs` stat builder + compiler `Compiler::new` layout).
        self.structs.insert(
            "FileInfo".into(),
            mk(vec![
                ("size", Ty::Int),
                ("mtime", Ty::Int),
                ("mode", Ty::Int),
                ("is_dir", Ty::Bool),
                ("is_file", Ty::Bool),
                ("is_symlink", Ty::Bool),
            ]),
        );
        // Phase 4c-net — re-seed std.net's `Socket`/`Listener` METHOD tables (harvested from
        // `std/net.chz`) under their bare names so `socket.read(...)`/`listener.accept(...)` resolve via
        // the normal method path (the `Ty::Socket`/`Ty::Listener` method arms look the table up here).
        // NO `struct_names`/`bare_types` licensing is added: `Socket`/`Listener`
        // resolve to the RESERVED `Ty::Socket`/`Ty::Listener` (opaque handles, not nominal structs) via
        // `resolve_type`'s reserved arm, which stays import-gated by `imported_net`. The method table is
        // reached only from a value whose `Ty` is already `Ty::Socket`/`Ty::Listener`, so a bare
        // unimported annotation still errors while a licensed `socket.read(...)` still resolves.
        if let Some(info) = self.net_socket_seed.clone() {
            self.structs.insert("Socket".into(), info);
        }
        if let Some(info) = self.net_listener_seed.clone() {
            self.structs.insert("Listener".into(), info);
        }
        // R2 — re-seed std.io's `Writer` METHOD table (harvested from `std/io.chz`) under its bare name
        // so `w.write(...)`/`w.close(...)` resolve via the normal method path (the `Ty::Writer` method
        // arm looks the table up here). Like `Socket`/`Listener` — NO `struct_names`/`bare_types`
        // licensing: `Writer` resolves to the RESERVED `Ty::Writer` (opaque handle) via `resolve_type`'s
        // reserved arm, import-gated by `imported_io`. The method table is reached only from a value
        // whose `Ty` is already `Ty::Writer`, so a bare unimported annotation still errors.
        if let Some(info) = self.io_writer_seed.clone() {
            self.structs.insert("Writer".into(), info);
        }
        // R2b — re-seed std.io's `Reader` METHOD table (harvested from `std/io.chz`) under its bare name
        // so `r.read_line()`/`r.read_bytes(..)`/`r.close()` resolve via the normal method path (the
        // `Ty::Reader` method arm looks the table up here). Same as `Writer` above.
        if let Some(info) = self.io_reader_seed.clone() {
            self.structs.insert("Reader".into(), info);
        }
        // Phase 4c-concurrency — re-seed std.concurrency's `Shared`/`RwShared`/`Atomic`/`Executor`
        // METHOD tables (harvested from `std/concurrency.chz`) under their bare names so `s.set(...)`/
        // `r.read(...)`/`a.cas(...)`/`ex.submit(...)` resolve via the normal method path (the
        // `Ty::Shared`/etc method arms look the table up here, substituting the box's element type for
        // the sig's `Ty::Param`). Like `Socket`/`Listener` above — and UNLIKE `Ref` — NO
        // `struct_names`/`bare_types` licensing is added: the four resolve to the RESERVED
        // `Ty::Shared`/`Ty::RwShared`/`Ty::Atomic`/`Ty::Executor` via `resolve_type`'s reserved arm,
        // which stays import-gated by `imported_concurrency`. The method table is reached only from a
        // value whose `Ty` is already one of those, so a bare unimported annotation still errors.
        for (name, info) in &self.concurrency_seeds {
            self.structs.insert(name.clone(), info.clone());
        }
        // Phase 5a-containers — re-seed the always-linked prelude's `List`/`Map`/`Set` METHOD tables
        // (harvested from `std/prelude.chz`) under their bare names so `xs.push(...)`/`m.get(...)`/
        // `s.add(...)` resolve via the normal method path (the `Ty::List`/`Ty::Map`/`Ty::Set` method arms
        // look the table up here, substituting the value's element/key/value type for the sig's
        // `Ty::Param`). Like `Socket`/`Shared` above — and UNLIKE `Ref` — NO `struct_names`/`bare_types`
        // licensing is added: the three resolve to the RESERVED `Ty::List`/`Ty::Map`/`Ty::Set` via
        // `resolve_type`'s reserved arms (universe types — always in scope). The `Builtin` origin means
        // `unique_member_owner`'s owner scan skips them (no mis-pin). The method table is reached only
        // from a value whose `Ty` is already a container, and the literal/ctor stay compiler-wired.
        for (name, info) in &self.container_seeds {
            self.structs.insert(name.clone(), info.clone());
        }
    }

    /// Phase 4b — harvest a FILE-BACKED native std module's whole SIGNATURE from its parsed in-module
    /// `native struct` / `native fn` decls into its [`ModuleSig`], REPLACING both the retired phase-4a
    /// companion stub and the hand-built `native_module_sig` regex arm. Used only for
    /// `std.regex` today (the resolver loads its real `std/regex.chz` while keeping the `native` marker;
    /// runtime member VALUES stay name-keyed via `native_members` — this supplies only the checker sig).
    ///
    /// Two passes so a `native fn`'s return type can reference the module's own `native struct` (regex's
    /// `find -> Result[Option[Match]]`): pass 1 harvests every `native struct` (into `sig.struct_defs`/
    /// `sig.types`, origin FORCED [`StructOrigin::Builtin`] — load-bearing for `imported_builtin_types`
    /// → both engines' pure-type `bind_import` skip) and TRANSIENTLY inserts each name into
    /// `self.struct_names` (so pass 2's `resolve_type` sees the bare type) AND transiently installs THIS
    /// module's harvested layout into `self.structs` (save+restore). The latter is load-bearing: this arm
    /// runs OUTSIDE `begin_module`, so `self.structs` is LEFTOVER from the previously-checked module — a
    /// sibling user module may legally declare a generic `struct Match[T]` (Match is import-gated, not
    /// reserved), overwriting the seeded nparams-0 native layout; without the transient install, pass 2's
    /// `resolve_type` would key off that generic arity and spuriously reject with `type 'Match' expects 1
    /// type argument(s), got 0`. With it, the name resolves against its own (nparams-0) shape — byte-
    /// identical to the old hand-built `Ty::Struct("Match", vec![])` and immune to sibling-module /
    /// graph-traversal-order state. Pass 2 harvests every `native fn` sig via the native-decl dynamic
    /// convention (unannotated param → `Ty::Unknown`; no `-> ret` → `Ty::Unknown`). The transient
    /// `struct_names` inserts and `self.structs` overwrites are then REVERTED so IMPORT-GATING is
    /// preserved (a bare unimported `m: Match` still errors) and this arm leaves NO residue.
    pub(super) fn harvest_native_module(&mut self, ast: &crate::ast::Module, sig: &mut ModuleSig) {
        // PASS 1 — native structs.
        let mut transient: Vec<String> = Vec::new();
        // Saved `self.structs` / `self.bare_types` entries so PASS 1's transient overwrites leave NO
        // residue.
        let mut saved_structs: Vec<(String, Option<StructInfo>)> = Vec::new();
        let mut saved_bare: Vec<(String, Option<String>)> = Vec::new();
        for s in &ast.stmts {
            if let StmtKind::NativeStruct {
                name,
                type_params,
                fields,
                span,
                ..
            } = &s.kind
            {
                let saved = self.enter_type_params(type_params);
                let harvested_fields: Vec<(String, Ty)> = fields
                    .iter()
                    .map(|f| (f.name.clone(), self.resolve_type(&f.ty, *span)))
                    .collect();
                self.exit_type_params(saved);
                let info = StructInfo {
                    type_params: type_params.clone(),
                    fields: harvested_fields,
                    // Methods are harvested in PASS 1b below (after every native struct name is
                    // transiently visible so a method return type can reference a sibling native
                    // struct, e.g. `Listener.accept -> Result[Socket]`).
                    methods: HashMap::new(),
                    origin: StructOrigin::Builtin,
                    doc: None,
                };
                sig.struct_defs.insert(name.clone(), info.clone());
                sig.types.insert(name.clone());
                // This harvest runs in the native-module arm WITHOUT `begin_module`, so `self.structs`
                // holds LEFTOVER state from the previously-checked module. A sibling user module may
                // legally declare a generic `struct Match[T]` (Match is import-gated, not reserved),
                // overwriting the seeded nparams-0 native layout. PASS 2 below resolves this type's own
                // name in the fns' return types (`find -> Result[Option[Match]]`) via `resolve_type`,
                // whose struct arm keys on the layout's `type_params.len()` — so it would spuriously
                // reject with `type 'Match' expects 1 type argument(s), got 0`. Transiently install THIS
                // module's native layout under the BARE name AND point `bare_types[name]` at the bare
                // name, so PASS 2's `resolve_type` (which looks the layout up via `bare_key`) resolves
                // the name against its own (nparams-0) shape — immune to sibling-module / graph-order
                // state. Both are restored after PASS 2 (this arm mutates no committed table — the sig
                // is the source of truth). A sibling `struct Match[T]` disambiguates to a module-keyed
                // `bare_types` entry, so the bare-name override is load-bearing, not just the layout.
                saved_structs.push((name.clone(), self.structs.insert(name.clone(), info)));
                saved_bare.push((
                    name.clone(),
                    self.bare_types.insert(name.clone(), name.clone()),
                ));
                // Make the native type's bare name resolvable while harvesting the fn sigs below (so a
                // return type like `Result[Option[Match]]` resolves). Removed after pass 2.
                if self.struct_names.insert(name.clone()) {
                    transient.push(name.clone());
                }
            }
        }
        // Transiently license the import-gated C-ABI TYPE names that std.ffi's `native fn` sigs
        // reference (phase 4c). `native fn null() -> ptr` resolves its `ptr` return via `resolve_type`,
        // whose `ptr` (and fixed-width `int8..uint64`) arms require the name to be in
        // `self.imported_ffi_types`. This harvest runs WITHOUT `begin_module`, so that set is empty/stale
        // and the resolve would spuriously error `unknown type 'ptr'`. Insert every `sig.types` name
        // that is `ptr` or a fixed-width FFI name (the only names carrying such an alias — driven off the
        // sig, so module-agnostic: no non-ffi module's sig carries them), tracking the NEWLY-inserted
        // ones, and remove exactly those after PASS 2. The direct analog of the `struct_names` transient
        // above — a pure sig computation that leaves no residue (so a later unrelated module still
        // rejects a bare unimported `ptr`). See the `native_module_sig("std.ffi")` type-license tail.
        let mut ffi_type_transient: Vec<String> = Vec::new();
        for tn in &sig.types {
            if (tn == "ptr" || crate::native::ffi::TYPE_NAMES.contains(&tn.as_str()))
                && self.imported_ffi_types.insert(tn.clone())
            {
                ffi_type_transient.push(tn.clone());
            }
        }
        // PASS 1b — native METHODS (phase 4c; self added 4c-followup). A `native fn` inside a `native
        // struct` body is an INSTANCE method declaring a leading bare `self` (mirroring user structs);
        // `harvest_native_fn_sig(_, true)` STRIPS that `self` so the harvested method-table sig is
        // byte-identical to the pre-`self` spelling, checked via the NORMAL method-resolution path
        // (retiring the bespoke `socket_method_sig`/`listener_method_sig` arms). Runs
        // after PASS 1 so every native struct name is transiently visible (a method return type can
        // reference a sibling native struct — `Listener.accept -> Result[Socket]`). For a generic native
        // struct the type params are in scope while resolving its method sigs.
        for s in &ast.stmts {
            if let StmtKind::NativeStruct {
                name,
                type_params,
                methods,
                bodied_methods,
                ..
            } = &s.kind
                && (!methods.is_empty() || !bodied_methods.is_empty())
            {
                let saved = self.enter_type_params(type_params);
                let mut table: HashMap<String, FnSig> = methods
                    .iter()
                    .map(|m| (m.name.clone(), self.harvest_native_fn_sig(m, true)))
                    .collect();
                // Phase 4c-followup — a BODIED method (`fn lines(self) -> …: <body>`) is checked via
                // the same method-resolution path as a bodyless `native fn`, so its sig must land in the
                // SAME table with the SAME shape (leading `self` stripped). `fn_sig` leaves `self` on;
                // drop the first param to match `harvest_native_fn_sig(_, true)`. The body itself is
                // type-checked back in the graph loop's native arm (which now runs `check_fn_body` on a
                // FRESH self-carrying `fn_sig` — NOT this stripped table sig); this harvest only supplies
                // the call-arg method-table shape.
                for m in bodied_methods {
                    let mut fsig = self.fn_sig(m, m.name_span);
                    if !fsig.params.is_empty() {
                        fsig.params.remove(0);
                        fsig.min_params = fsig.min_params.saturating_sub(1);
                    }
                    if !fsig.labels.is_empty() {
                        fsig.labels.remove(0);
                    }
                    table.insert(m.name.clone(), fsig);
                }
                self.exit_type_params(saved);
                if let Some(info) = sig.struct_defs.get_mut(name) {
                    info.methods = table;
                }
            }
        }
        // PASS 2 — native fns (module members, sig from the parsed decl; runtime value name-keyed).
        for s in &ast.stmts {
            if let StmtKind::Native(decl) = &s.kind {
                let fsig = self.harvest_native_fn_sig(decl, false);
                // `timer` is an opcode-backed BARE-callable builtin (lowers to `Op::NewTimer`, no runtime
                // value): keep it OUT of `sig.functions` (else the From-import arm binds it as a normal
                // callable, breaking bare-callability). Stash its sig for the bare `timer(...)` expr arm;
                // the license stays in the `native_module_sig` `sig.types` insert. `timer` is a reserved
                // name declared in exactly one `.chz`, so this name match is unambiguous and self-scoping.
                if decl.name == "timer" {
                    self.time_timer_sig = Some(fsig);
                } else {
                    sig.functions.insert(decl.name.clone(), fsig);
                }
            }
        }
        // PASS 2b — module-level BODIED fns (the hybrid native+Chezzi module form). A `native` std
        // file may carry ordinary `fn foo(): <body>` alongside its bodyless `native fn` decls; harvest
        // its sig as a module member so `mod.foo()` / `from mod import foo` resolve. Runtime binding
        // comes from RUNNING the native module's toplevel (see `run_module`); the BODY is type-checked
        // back in the graph loop (the native arm now runs `check_fn_body` — this harvest only supplies
        // the member sig). Runs while PASS 1's transient struct visibility is still installed, so a
        // bodied fn may name a sibling native struct in its signature (`-> Reader`).
        for s in &ast.stmts {
            if let StmtKind::Fn(decl) = &s.kind {
                sig.functions
                    .insert(decl.name.clone(), self.fn_sig(decl, decl.name_span));
            }
        }
        // Preserve import-gating: drop the transient bare-name visibility.
        for name in transient {
            self.struct_names.remove(&name);
        }
        // Drop the transient FFI type-license (phase 4c) — restore exactly the names this harvest
        // inserted, so `ptr`/`int8..uint64` do NOT leak a license into a later unrelated module.
        for name in ffi_type_transient {
            self.imported_ffi_types.remove(&name);
        }
        // Restore `self.bare_types` to its pre-harvest state (paired with the layout restore below).
        for (name, prev) in saved_bare {
            match prev {
                Some(key) => {
                    self.bare_types.insert(name, key);
                }
                None => {
                    self.bare_types.remove(&name);
                }
            }
        }
        // Restore `self.structs` to its pre-harvest state so this arm leaves no residue (the next
        // module re-seeds via `begin_module` anyway; this keeps the harvest a pure sig computation).
        for (name, prev) in saved_structs {
            match prev {
                Some(info) => {
                    self.structs.insert(name, info);
                }
                None => {
                    self.structs.remove(&name);
                }
            }
        }
    }

    /// Lower one body-less `native fn`/`native ctor` (a free module member OR a native-struct method) to
    /// its [`FnSig`], per the native-decl dynamic convention: an unannotated param → `Ty::Unknown`; no
    /// `-> ret` → `Ty::Unknown` (a native-controlled return). A trailing `= default` marker lowers to an
    /// OPTIONAL tail (`min_params = len - trailing-defaults`) — the file-backed spelling of the net
    /// socket ops' optional `timeout_ms` (and std.request's `get`/`post`). `parse_params` guarantees any
    /// default is trailing, so a plain count is correct; the default EXPR is inert (desugar ignores
    /// `StmtKind::Native`, so it is never injected at a call site). Callers set any needed type-param
    /// scope (native-struct methods enter the struct's `type_params`).
    ///
    /// `skip_self` (true only for a native-struct INSTANCE method, PASS 1b) drops the leading bare
    /// `self` receiver BEFORE the param→`Ty` map (so `self` is never typed as a dynamic `Ty::Unknown`)
    /// AND before the optional-tail count — the resulting method-table sig (params/min_params/ret) is
    /// byte-identical to the pre-`self` (phase-4c) spelling, the behavior-preserving invariant.
    /// Module-level (free) native fns pass `skip_self=false` (the parser already forbids `self` there).
    pub(super) fn harvest_native_fn_sig(&mut self, decl: &NativeDecl, skip_self: bool) -> FnSig {
        let skip = if skip_self { 1 } else { 0 };
        // A native METHOD may declare its OWN `[U]` params (`map[U]`); enter them into scope (NESTED
        // inside the enclosing native struct's `[T]` scope, already entered by the caller) so `U` in
        // the method's params/ret resolves to `Ty::Param("U")` and does not read as an unknown type.
        // No-op when the decl has no type params (the common `native fn`/`native ctor`).
        let saved_tps = self.enter_type_params(&decl.type_params);
        let params: Vec<Ty> = decl
            .params
            .iter()
            .skip(skip)
            .map(|p| match &p.ty {
                // A variadic `...xs: T` collapses to the slot type `List[T]` (same as user `fn_sig`).
                Some(t) if p.is_variadic => Ty::List(Box::new(self.resolve_type(t, decl.span))),
                Some(t) => self.resolve_type(t, decl.span),
                None => Ty::Unknown,
            })
            .collect();
        let ret = match &decl.ret {
            Some(t) => self.resolve_type(t, decl.span),
            None => Ty::Unknown,
        };
        self.exit_type_params(saved_tps);
        let variadic = decl.params.iter().skip(skip).position(|p| p.is_variadic);
        let optional = decl
            .params
            .iter()
            .skip(skip)
            .filter(|p| p.default.is_some())
            .count();
        let mut sig = if optional > 0 {
            FnSig::optional_tail(params, ret, optional)
        } else {
            FnSig::plain(params, ret)
        };
        // Carry the `where T: Bound` clause onto the harvested sig — for a native METHOD (e.g.
        // `List.sort`'s `where T: Comparable`) it is enforced at each call site by the container
        // method-dispatch arm (the `T` names the enclosing native struct's type param, in scope here).
        sig.where_bounds = decl.where_bounds.clone();
        // A native method's OWN `[U]` params land on the sig so it routes through the generic-method
        // inference path (`infer_generic_method`), not the fixed-arity path (empty for the common case).
        sig.type_params = decl.type_params.clone();
        sig.variadic = variadic;
        sig
    }

    /// Phase 5a-containers — harvest the METHOD table of one `native struct` (by bare name) from a parsed
    /// module AST into a `StructInfo`. Mirrors [`harvest_native_module`]'s PASS 1b, used for the always-
    /// linked `std/prelude.chz`'s reserved UNIVERSE containers (`List`/`Map`/`Set`): their identity is the
    /// reserved `Ty::List`/`Ty::Map`/`Ty::Set` (NOT a nominal struct), so `fields` are empty and only the
    /// method table is kept. The leading bare `self` is STRIPPED by `harvest_native_fn_sig(_, true)`, so
    /// the harvested sigs BYTE-MATCH the retired bespoke `list_method_sig`/`map_method_sig`/
    /// `set_method_sig` arms. Type params (incl. `Map`/`Set`'s `Hashable`-bounded key/elem) are in scope
    /// while resolving each method sig so the internal `Map[K, V]`/`List[T]`/`Set[T]` return types resolve
    /// past the hashable gate. Returns `None` if the named native struct is not present in the AST.
    pub(super) fn harvest_native_struct_table(
        &mut self,
        ast: &crate::ast::Module,
        name: &str,
    ) -> Option<StructInfo> {
        for s in &ast.stmts {
            if let StmtKind::NativeStruct {
                name: sn,
                type_params,
                methods,
                ..
            } = &s.kind
                && sn == name
            {
                let saved = self.enter_type_params(type_params);
                let table: HashMap<String, FnSig> = methods
                    .iter()
                    .map(|m| (m.name.clone(), self.harvest_native_fn_sig(m, true)))
                    .collect();
                self.exit_type_params(saved);
                return Some(StructInfo {
                    type_params: type_params.clone(),
                    fields: Vec::new(),
                    methods: table,
                    origin: StructOrigin::Builtin,
                    doc: None,
                });
            }
        }
        None
    }

    /// Phase 5b-native-enum — harvest the VARIANT SHAPE (+ any leading-`self` methods) of one
    /// `native enum` (by bare name) from a parsed module AST. The ENUM analog of
    /// [`harvest_native_struct_table`], used for the always-linked `std/prelude.chz`'s reserved
    /// `Option`/`Result`: their identity stays the reserved `Ty::Option`/`Ty::Result` (NOT a nominal
    /// enum), and their `?`/match/construction wiring stays Rust-inline — so this harvest is a
    /// DRIFT GUARD ONLY (the parsed variant set must byte-match the inline `variants_of` maps), never a
    /// runtime-consumed table. Type params are in scope while resolving each variant payload so
    /// `Some(T)`/`Ok(T)`/`Err(E)` resolve to `Ty::Param`. Returns `(variant_map, method_table)` — the
    /// variant map keyed by variant name to its resolved payload types — or `None` if the named native
    /// enum is absent. The leading bare `self` on any method is STRIPPED by
    /// `harvest_native_fn_sig(_, true)`, exactly like native-struct methods.
    pub(super) fn harvest_native_enum_table(
        &mut self,
        ast: &crate::ast::Module,
        name: &str,
    ) -> Option<NativeEnumShape> {
        for s in &ast.stmts {
            if let StmtKind::NativeEnum {
                name: en,
                type_params,
                variants,
                methods,
                ..
            } = &s.kind
                && en == name
            {
                let saved = self.enter_type_params(type_params);
                let vmap: HashMap<String, Vec<Ty>> = variants
                    .iter()
                    .map(|v| {
                        let payload = v
                            .payload
                            .iter()
                            .map(|t| self.resolve_type(t, v.name_span))
                            .collect();
                        (v.name.clone(), payload)
                    })
                    .collect();
                let mtable: HashMap<String, FnSig> = methods
                    .iter()
                    .map(|m| (m.name.clone(), self.harvest_native_fn_sig(m, true)))
                    .collect();
                self.exit_type_params(saved);
                return Some((vmap, mtable));
            }
        }
        None
    }

    /// Phase 5b-native-enum DRIFT GUARD — assert the `Option`/`Result` variant SHAPE declared in
    /// `std/prelude.chz`'s `native enum` decls byte-matches the reserved-type shape synthesized INLINE
    /// by [`variants_of`]. This is the behavior-preservation contract: the file-backed shape is an
    /// ADDITIVE mirror, so a change to either the `.chz` decl or the inline map that makes them disagree
    /// is a bug. Compared with EXPLICIT `E` (the `Result[T]` → `Error`-protocol surface default is
    /// injected by `resolve_type`, not encoded in the variant), and asserts NO ported methods
    /// (Option/Result carry zero bespoke method arms). Called only on the always-linked prelude module;
    /// the body is guarded on `cfg!(debug_assertions)` so it is a NO-OP at runtime in release yet stays
    /// COMPILED (so `harvest_native_enum_table` is never dead code).
    pub(super) fn assert_native_enum_shape_matches(&mut self, ast: &crate::ast::Module) {
        if !cfg!(debug_assertions) {
            return;
        }
        if let Some((vmap, methods)) = self.harvest_native_enum_table(ast, "Option") {
            debug_assert!(
                methods.is_empty(),
                "native enum Option must have no methods"
            );
            let inline = self
                .variants_of(&Ty::option(Ty::Param("T".to_string())))
                .expect("inline Option variants_of");
            debug_assert_eq!(
                vmap, inline,
                "native enum Option in std/prelude.chz drifted from inline variants_of"
            );
        }
        if let Some((vmap, methods)) = self.harvest_native_enum_table(ast, "Result") {
            debug_assert!(
                methods.is_empty(),
                "native enum Result must have no methods"
            );
            let inline = self
                .variants_of(&Ty::result_e(
                    Ty::Param("T".to_string()),
                    Ty::Param("E".to_string()),
                ))
                .expect("inline Result variants_of");
            debug_assert_eq!(
                vmap, inline,
                "native enum Result in std/prelude.chz drifted from inline variants_of"
            );
        }
    }

    /// Phase 5c-protocols — harvest the SHAPE of a reserved protocol declared in `std/prelude.chz` as a
    /// plain `protocol` decl (its `type_params`, `embeds`, and method `FnSig`s), WITHOUT inserting it
    /// into `self.protocols` or emitting any error. Mirrors [`harvest_native_enum_table`] but for a
    /// `StmtKind::Protocol`, replicating the exact `Self` + own-type-param scope [`hoist_protocol`] uses
    /// to resolve the method sigs (`self` → `Ty::Unknown`, `Self`/own params → `Ty::Param`). Used ONLY by
    /// the always-on debug DRIFT GUARD [`assert_native_protocol_shape_matches`] to prove the file-backed
    /// SHAPE byte-matches the live Rust seed [`prebuilt_protocols`] (which stays the runtime source — the
    /// `.chz` decl is an ADDITIVE mirror, never registered). Returns `None` if `name` isn't declared in `ast`.
    pub(super) fn harvest_protocol_shape(
        &mut self,
        ast: &crate::ast::Module,
        name: &str,
    ) -> Option<ProtocolInfo> {
        for s in &ast.stmts {
            if let StmtKind::Protocol {
                name: pn,
                type_params,
                methods,
                embeds,
                ..
            } = &s.kind
                && pn == name
            {
                // Same scope hoist_protocol builds: swap in a clean map with only `Self` + the
                // protocol's own type params visible, resolve, then restore.
                let mut saved = self.type_params.clone();
                std::mem::swap(&mut self.type_params, &mut saved);
                self.type_params.insert("Self".to_string(), Vec::new());
                for tp in type_params {
                    self.type_params.insert(tp.name.clone(), tp.bounds.clone());
                }
                let sigs: Vec<(String, FnSig)> = methods
                    .iter()
                    .map(|m| {
                        let params = m
                            .params
                            .iter()
                            .map(|p| match &p.ty {
                                Some(t) => self.resolve_type(t, s.span),
                                None => Ty::Unknown, // leading bare `self`
                            })
                            .collect();
                        let ret = m
                            .ret
                            .as_ref()
                            .map(|t| self.resolve_type(t, s.span))
                            .unwrap_or(Ty::Nil);
                        (m.name.clone(), FnSig::plain(params, ret))
                    })
                    .collect();
                self.type_params = saved;
                return Some(ProtocolInfo {
                    type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                    methods: sigs,
                    embeds: embeds.clone(),
                });
            }
        }
        None
    }

    /// Phase 5c-protocols DRIFT GUARD — assert each reserved protocol's SHAPE declared in
    /// `std/prelude.chz` (as a plain `protocol` decl) byte-matches the live Rust seed
    /// [`prebuilt_protocols`], which stays the RUNTIME source of truth. The file-backed decls are an
    /// ADDITIVE mirror — never inserted into `self.protocols` (the `hoist_protocol` stdlib gate no-ops
    /// them) — so nothing at runtime consults them; this guard is the only thing that reads them, keeping
    /// the two source expressions from silently drifting. All 18 reserved protocols are mirrored;
    /// `Iterable`'s `iter(self) -> Iterator[Elem]` return type resolves (via `resolve_type`'s dedicated
    /// `Iterator[T]` value arm) to the same `Ty::Struct("Iterator",[Elem])` the seed uses, so its shape
    /// byte-matches too. Called only on the always-linked prelude module; the body is
    /// `cfg!(debug_assertions)`-guarded so it is a NO-OP in release yet stays COMPILED (so
    /// `harvest_protocol_shape` is never dead code).
    pub(super) fn assert_native_protocol_shape_matches(&mut self, ast: &crate::ast::Module) {
        if !cfg!(debug_assertions) {
            return;
        }
        let seed = prebuilt_protocols();
        for name in [
            // `Any` — the empty (zero-method, zero-embed) accept-all top type. Now expressible in
            // Chezzi as `protocol Any:\n    pass`, so it is mirrored + drift-guarded like the rest.
            "Any",
            "Comparable",
            "Stringable",
            "Error",
            "Hashable",
            "Add",
            "Sub",
            "Mul",
            "Div",
            "Mod",
            "Neg",
            "Arithmetic",
            "Iterator",
            "Iterable",
            "Index",
            "IndexSet",
            "Slice",
            "Convert",
            "Contains",
        ] {
            let got = self.harvest_protocol_shape(ast, name).unwrap_or_else(|| {
                panic!("reserved protocol '{name}' missing from std/prelude.chz")
            });
            let want = seed
                .get(name)
                .unwrap_or_else(|| panic!("reserved protocol '{name}' missing from prebuilt seed"));
            debug_assert_eq!(
                got.type_params, want.type_params,
                "protocol '{name}' type_params drifted from prebuilt_protocols"
            );
            debug_assert_eq!(
                got.embeds, want.embeds,
                "protocol '{name}' embeds drifted from prebuilt_protocols"
            );
            debug_assert_eq!(
                got.methods.len(),
                want.methods.len(),
                "protocol '{name}' method count drifted from prebuilt_protocols"
            );
            for ((gn, gs), (wn, ws)) in got.methods.iter().zip(&want.methods) {
                debug_assert_eq!(gn, wn, "protocol '{name}' method name/order drifted");
                debug_assert!(
                    fn_sig_eq(gs, ws),
                    "protocol '{name}' method '{gn}' sig drifted from prebuilt_protocols"
                );
            }
        }
    }

    /// Phase 4c — look up a method sig on a reserved native handle type in the harvested method table
    /// `seed_stdlib_structs` re-seeded into `self.structs[type]`. Replaces the retired bespoke
    /// `socket_method_sig`/`listener_method_sig` (net, non-generic) AND `shared_method_sig`/
    /// `rwshared_method_sig`/`atomic_method_sig`/`executor_method_sig` (concurrency); the sigs
    /// (params/min_params/ret) come verbatim from the `.chz` `native fn` decls. `targs` are the reserved
    /// GENERIC handle's element types (`&[elem]` for `Shared[T]`/`RwShared[T]`/`Atomic[T]`; `&[]` for the
    /// non-generic `Socket`/`Listener`/`Executor`), substituted into the sig's `Ty::Param`s — the same
    /// per-type param subst the generic-struct machinery uses, so `Shared[int].set` expects `int`. The
    /// harvested method sigs carry NO `self` receiver (the `.chz` decls declare a leading bare `self`
    /// that PASS 1b's `harvest_native_fn_sig(_, true)` STRIPS), so `params` are the call args directly —
    /// the arms pass them to `check_args_range` unchanged.
    pub(super) fn native_handle_method(
        &self,
        ty_name: &str,
        method: &str,
        targs: &[Ty],
    ) -> Option<FnSig> {
        let info = self.structs.get(ty_name)?;
        let sig = info.methods.get(method)?;
        if targs.is_empty() || info.type_params.is_empty() {
            // Non-generic handle (Socket/Listener/Executor), or no element type to substitute — the
            // stored sig is already concrete (identity subst; behavior-preserving for net).
            return Some(sig.clone());
        }
        let map = struct_param_map(info, targs);
        Some(FnSig {
            params: sig.params.iter().map(|p| subst(p, &map)).collect(),
            ret: subst(&sig.ret, &map),
            ..sig.clone()
        })
    }

    pub(super) fn error(&mut self, span: Span, message: impl Into<String>) {
        let message = match &self.current_module_label {
            Some(label) => format!("in module '{label}': {}", message.into()),
            None => message.into(),
        };
        self.errors.push(CheckError { message, span });
    }

    /// Report each name that repeats within `items` as a "`<kind>` '`<name>`' is already defined"
    /// error at its decl-site span. Fires once per REPEAT occurrence (the 2nd, 3rd, … copy), so the
    /// first occurrence stays the surviving definition (mirrors the dup-variant precedent). Shared by
    /// the struct/enum/newtype method-hoist arms and the struct field-hoist arm so the seen-set loop
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
        self.loop_vars.clear();
        self.functions.clear();
        self.local_fn_names.clear();
        self.name_docs.clear();
        self.type_params.clear();
        self.imported_modules.clear();
        self.import_path_heads.clear();
        self.module_prefix2.clear();
        self.import_binds.clear();
        self.imported_alias_tys.clear();
        self.imported_alias_ctypes.clear();
        self.imported_poly.clear();
        self.imported_values.clear();
        self.imported_consts.clear();
        self.imported_ffi_types.clear();
        self.imported_concurrency.clear();
        self.imported_time.clear();
        self.imported_net.clear();
        self.imported_io.clear(); // R2/R2b — Writer + Reader licensing
        self.imported_builtin_types.clear();
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
        self.newtype_names.clear();
        self.newtype_defs.clear();
        self.newtype_type_params.clear();
        self.aliases.clear();
        self.bare_types.clear();
        self.seed_stdlib_structs();
        self.current_ret = Ty::Nil;
        self.in_fn_body = false;
        self.inferring_ret = false;
        self.collected_rets.clear();
        self.current_module_label = label;
    }

    /// Check one module's statements with its imports bound first; returns its public signature.
    /// `id` is `Some` for a graph module (enables import binding), `None` for a lone `check`.
    pub(super) fn check_module(
        &mut self,
        stmts: &[Stmt],
        id: Option<&ModuleId>,
        imports: &[ResolvedImport],
    ) -> ModuleSig {
        self.push_scope();
        // Record every top-level `let`/`:=` name in THIS module (rebuilt per module — `check_module`
        // is called once per module, never nested). Lets `infer_ident` tell a genuine first-class
        // builtin (`f := print`) from a same-named module global used before its definition line (a
        // use-before-def error). Mirrors the compiler's `collect_globals` top-level `Let` sweep.
        self.module_global_lets.clear();
        for s in stmts {
            if let StmtKind::Let { names, .. } = &s.kind {
                for n in names {
                    self.module_global_lets.insert(n.clone());
                }
            }
        }
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
                | StmtKind::NewType { name, .. }
                | StmtKind::TypeAlias { name, .. } = &s.kind
                {
                    let key = self.type_key(mid, name);
                    self.bare_types.insert(name.clone(), key);
                }
            }
        }
        for imp in imports {
            self.bind_import(imp);
        }
        self.collect_names(stmts);
        self.collect_docs(stmts);
        self.hoist(stmts);
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
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        let sig = self.capture_sig(stmts);
        self.finalize_empty_coll_sites();
        self.finalize_hover_pending();
        self.pop_scope();
        sig
    }

    /// Record that `bind` is bound by an import at `span`. Returns `true` if this name was ALREADY
    /// bound by an earlier import in this module — the caller then emits the duplicate-import error
    /// and skips re-binding. Spans across ALL import namespaces (values/functions/modules/types) so a
    /// value-then-fn or fn-then-fn collision is caught (which the separate tables otherwise miss).
    pub(super) fn note_import_bind(&mut self, bind: &str, span: Span) -> bool {
        if self.import_binds.contains_key(bind) {
            self.error(span, format!("'{bind}' is already imported"));
            return true;
        }
        self.import_binds.insert(bind.to_string(), span);
        false
    }

    /// Bind an import into the current module: a whole-module import becomes a `Ty::Module` name;
    /// a `from` import injects each member (function/value) into scope, validating it exists.
    pub(super) fn bind_import(&mut self, imp: &ResolvedImport) {
        match &imp.import {
            Import::Module {
                path,
                alias,
                name_span,
            } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                // A whole-module bind lands in the VALUE namespace, where it BEATS a same-named
                // builtin/ctor in EXPRESSION position (`import std.str` used to make `str(5)` fail with
                // the confusing `module str is not callable`; `import lib.geo as Ok` kills the `Ok(...)`
                // variant ctor). So a RESERVED bound name — the alias, or the last path segment when
                // un-aliased — is REJECTED here; the module stays usable under a non-reserved alias,
                // which the un-aliased diagnostic names. `is_reserved_module_bind` = reserved CALLABLE +
                // reserved TYPE names + `nil` + the builtin variant ctors (`Ok`/`Err`/`Some`/`None`).
                // (A collision with a USER-declared ctor of the same name is a separate, unhandled
                // residual — see `is_reserved_module_bind`'s doc.)
                if crate::checker::is_reserved_module_bind(&name) {
                    let msg = if alias.is_some() {
                        format!("import alias '{name}' is reserved (builtin)")
                    } else {
                        let path_str = path.join(".");
                        format!(
                            "module name '{name}' is reserved (builtin) — alias it: import {path_str} as {name}s"
                        )
                    };
                    self.error(imp.span, msg);
                    return;
                }
                if self.note_import_bind(&name, imp.span) {
                    return;
                }
                // Editor hover (decl-site): record the bound module name's type at the bound-name
                // token (`math` / the `as` alias). Probe-gated no-op off the probe / outside entry.
                if self.hover_probe.is_some() {
                    self.hover_record_at(
                        *name_span,
                        &Ty::Module(name.clone()),
                        HoverKind::Other,
                        None,
                    );
                }
                self.imported_modules
                    .insert(name.clone(), imp.target.clone());
                // Record the first TWO path segments → bound name (`(std,net)` → `net`,
                // `(std,concurrency)` → `collection` for `import std.concurrency.collection`) so a
                // too-deep-path mistake — which fires with only head + next segment visible — names
                // the EXACT module's bound name. `None` marks a genuine ambiguity (two imports sharing
                // both segments) so we fall back to a generic hint rather than guess.
                if path.len() >= 2 {
                    let key = (path[0].clone(), path[1].clone());
                    match self.module_prefix2.get(&key) {
                        Some(Some((_, prev))) if prev != &name => {
                            self.module_prefix2.insert(key, None);
                        }
                        None => {
                            self.module_prefix2
                                .insert(key, Some((path.join("."), name.clone())));
                        }
                        _ => {}
                    }
                }
                // Record the path HEAD (`std` of `import std.concurrency`) so a multi-level mistake
                // (`std.concurrency.Shared(...)`) gets the two-level hint, not "unknown name 'std'".
                // Only for a genuine dotted path whose head isn't itself the bound name (first wins).
                if path.len() >= 2 {
                    let head = path[0].clone();
                    if head != name {
                        self.import_path_heads
                            .entry(head)
                            .or_insert_with(|| (path.join("."), name.clone()));
                    }
                }
                self.declare(&name, Ty::Module(name.clone()));
                // Register the imported module's struct/enum LAYOUTS into the per-module shape tables
                // (so `geo.Point(1,2).x` and `geo`'s enum methods resolve), but NOT into the bare
                // *_names sets — a bare `Point` must still error. The bare-name gate (`struct_names`/
                // `enum_names`) stays cleared; `infer_field`/`infer_method_call` consult `self.structs`/
                // `self.enums` for the layout, which is what these provide. A same-named layout already
                // present (a local decl or another import) is NOT overwritten (first/local wins; the
                // compiler disambiguates any genuine runtime collision).
                //
                // EXCEPTION — a STDLIB module (`import std.ref`/`std.iter`/…) ALSO exposes its types
                // BARE (`struct_names`/`enum_names`), like the reserved/native surface (`Ref`/`Result`).
                // The `ref T` syntax lowers to a bare `Ref[T]` annotation that has no module prefix, so
                // `Ref` must resolve bare wherever `std.ref` is imported. This keeps the std type
                // surface globally usable on import, as before, without leaking USER module types.
                let is_std = path.first().map(String::as_str) == Some("std");
                if let Some(sig) = self.module_sigs.get(&imp.target).cloned() {
                    for (sname, info) in &sig.struct_defs {
                        // A RESERVED native handle (std.net's `Socket`/`Listener`) has a `sig.struct_defs`
                        // entry only for its harvested METHOD table — it is NOT a constructible/nominal
                        // struct (it resolves to the opaque `Ty::Socket`/`Ty::Listener` via
                        // `resolve_type`'s reserved arm, and its method table is seeded independently by
                        // `seed_stdlib_structs`). Skip it entirely here so a whole-module `import std.net`
                        // does NOT make bare `Socket(...)` a constructor or `Socket` a nominal-struct
                        // annotation (both must stay gated by the reserved arm / rejected as a ctor).
                        if self.qualified_builtin_ty(sname, &[]).is_some() {
                            continue;
                        }
                        // Register the LAYOUT under the DECLARING module's runtime key (bare unless a
                        // genuine cross-module clash). Register BOTH colliding layouts (no first-wins),
                        // so a value of either — whose `Ty` carries the matching key — resolves its
                        // own fields/methods. A std module ALSO exposes its types bare.
                        let key = self.type_key(&imp.target, sname);
                        self.structs.insert(key.clone(), info.clone());
                        if is_std {
                            self.struct_names.insert(sname.clone());
                            self.bare_types.entry(sname.clone()).or_insert(key);
                            // A Builtin-origin std struct (Match/Response/ProcResult, Token/Heap/
                            // …) licensed bare by THIS import: record the name so a same-named user
                            // `struct` decl below is a clean `reserved (builtin)` error, not an
                            // accept-then-trap (the user layout would shadow the native shape).
                            if info.origin == StructOrigin::Builtin {
                                self.imported_builtin_types.insert(sname.clone());
                            }
                        }
                    }
                    for (ename, edef) in &sig.enum_defs {
                        let key = self.type_key(&imp.target, ename);
                        self.enums.insert(key.clone(), edef.variant_names.clone());
                        self.enum_type_params
                            .insert(key.clone(), edef.type_params.clone());
                        self.enum_methods.insert(key.clone(), edef.methods.clone());
                        if is_std {
                            self.enum_names.insert(ename.clone());
                            self.bare_types.entry(ename.clone()).or_insert(key.clone());
                        }
                        for (vname, vinfo) in edef.variant_names.iter().zip(&edef.variants) {
                            let mut vi = vinfo.clone();
                            vi.enum_name = key.clone();
                            self.variants.insert((key.clone(), vname.clone()), vi);
                            if is_std {
                                self.variant_owners
                                    .entry(vname.clone())
                                    .or_default()
                                    .push(ename.clone());
                            }
                        }
                    }
                    for (ntname, ntdef) in &sig.newtype_defs {
                        // Register the newtype's underlying + methods under the declaring module's
                        // runtime key (so a value whose `Ty::NewType(key)` matches resolves its
                        // methods/construct/cast). A std module also exposes it bare.
                        let key = self.type_key(&imp.target, ntname);
                        self.newtype_defs.insert(
                            key.clone(),
                            (ntdef.underlying.clone(), ntdef.methods.clone()),
                        );
                        self.newtype_type_params
                            .insert(key.clone(), ntdef.type_params.clone());
                        if is_std {
                            self.newtype_names.insert(ntname.clone());
                            self.bare_types.entry(ntname.clone()).or_insert(key);
                        }
                    }
                }
                // A whole-module `import std.ffi` licenses the bare opaque `ptr` type (extern blocks
                // use it pervasively, so whole-module licensing is the ergonomic default — UNLIKE the
                // width types int8..uint64, which stay per-name-only). Keyed on the EXACT path, NOT
                // `is_std`, so `import std.ref`/`std.iter`/… do NOT license `ptr`.
                if path.as_slice() == ["std".to_string(), "ffi".to_string()] {
                    self.imported_ffi_types.insert("ptr".to_string());
                }
                // A whole-module `import std.concurrency` licenses ALL FOUR runtime concurrency ctor/
                // TYPE names (the ergonomic default). Keyed on the EXACT len-2 path, so the real file
                // submodule `import std.concurrency.collection` (len-3) does NOT license them.
                if path.as_slice() == ["std".to_string(), "concurrency".to_string()] {
                    for tn in ["Shared", "RwShared", "Atomic", "Executor"] {
                        self.imported_concurrency.insert(tn.to_string());
                    }
                }
                // A whole-module `import std.time` licenses the opcode-backed `timer(ms)` builtin.
                // Keyed on the EXACT len-2 path. std.time is a REAL native module, so `Import::Plain`
                // already binds the module object (and its now/monotonic/sleep_ms/format members); we
                // only ADD the `timer` licensing insert here.
                if path.as_slice() == ["std".to_string(), "time".to_string()] {
                    self.imported_time.insert("timer".to_string());
                }
                // A whole-module `import std.net` licenses BOTH bare TCP handle TYPE names. Keyed on
                // the EXACT len-2 path. std.net is a REAL native module, so `Import::Plain` already
                // binds the module object (connect/listen members); we only ADD the type licensing.
                if path.as_slice() == ["std".to_string(), "net".to_string()] {
                    for tn in ["Socket", "Listener"] {
                        self.imported_net.insert(tn.to_string());
                    }
                }
                // R2 — a whole-module `import std.io` licenses the bare `Writer` TYPE name. Keyed on the
                // EXACT len-2 path. std.io is a REAL native module (its fns bind via `Import::Plain`); we
                // only ADD the type licensing.
                if path.as_slice() == ["std".to_string(), "io".to_string()] {
                    self.imported_io.insert("Writer".to_string());
                    // R2b — a whole-module `import std.io` also licenses the bare `Reader` TYPE name.
                    self.imported_io.insert("Reader".to_string());
                }
            }
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
                    // Aliasing a `from` import TO a reserved builtin name (`import sqrt as int from
                    // std.math`, or a reserved TYPE `import who as Result from lib`) silently rebinds
                    // it: the builtin `int()` conversion / `Result` type wins at call/type sites and
                    // the `as int` binding is DEAD — a silent wrong result with no diagnostic. Reject
                    // it as `reserved (builtin)`, like `fn int()`. `is_reserved_alias_target` covers
                    // BOTH reserved CALLABLE and reserved TYPE names (mirrors the decl-site guard). The `a != member`
                    // guard is CRITICAL: reserved members that are themselves importable
                    // (`Shared`/`Executor`/`timer`/…) must still import UN-aliased (bind == member) and
                    // via a redundant self-rename (`as Shared`), so only a genuine RENAME to a reserved
                    // name is gated here. Fresh non-reserved aliases (`import timer as t2`) are inert to
                    // this check and fall through to the specialized rename-rejection arms below.
                    // A from-import alias binds a VALUE, so `nil` stays legal here (a value still works
                    // as a value — `is_reserved_alias_target`'s carve-out); only the MODULE bind adds
                    // it. The builtin variant ctors ARE rejected (an alias to `Ok` would kill the
                    // ctor in expression position).
                    // The SAME hazard reaches the VALUE namespace through the UN-aliased MEMBER
                    // spelling too: a module GLOBAL or FUNCTION named `str`/`int`/`Ok` (all legal at
                    // their decl site) bind here and beat the builtin ctor in EXPRESSION position —
                    // `import str from lib.sh` made `str(5)` fail with `int is not callable`. So the
                    // reserved-name reject covers BOTH doors: a genuine RENAME to a reserved name (any
                    // member kind), and a bare reserved member that BINDS A VALUE/FUNCTION. The
                    // value/function scoping is what keeps a reserved TYPE member — the LICENSING
                    // import of the builtin itself (`import Shared from std.concurrency`, `import ptr
                    // from std.ffi`) — legal un-aliased; it binds no value.
                    let reserved_bind = crate::checker::is_reserved_alias_target(bind)
                        || crate::checker::is_builtin_variant(bind);
                    let binds_value =
                        sig.functions.contains_key(member) || sig.values.contains_key(member);
                    if reserved_bind && (alias.as_ref().is_some_and(|a| a != member) || binds_value)
                    {
                        let msg = if alias.is_some() {
                            format!("import alias '{bind}' is reserved (builtin)")
                        } else {
                            format!(
                                "imported name '{bind}' is reserved (builtin) — alias it: import {bind} as {bind}_ from {}",
                                path.join(".")
                            )
                        };
                        self.error(imp.span, msg);
                        continue;
                    }
                    // Reject a second import binding the same name (across ALL namespaces), but only
                    // when the member actually exists — a missing member is its own error below, and
                    // shouldn't also claim the name. The bind-name (alias wins) is the collision key,
                    // so `import x as y` + `import z as y` collides while distinct names don't.
                    let member_exists = sig.functions.contains_key(member)
                        || sig.values.contains_key(member)
                        || sig.types.contains(member);
                    if member_exists && self.note_import_bind(bind, imp.span) {
                        continue;
                    }
                    if let Some(fsig) = sig.functions.get(member) {
                        self.functions.insert(bind.clone(), fsig.clone());
                        // Carry the numeric-polymorphism marker onto the imported name (gap #12).
                        if sig.numeric_poly.contains(member) {
                            self.imported_poly.insert(bind.clone());
                        }
                        // Editor hover (decl-site): record the imported function's signature at the
                        // bound-name token (probe-gated no-op off the probe / outside the entry module).
                        if self.hover_probe.is_some() {
                            let fty = Ty::Func {
                                params: fsig.params.clone(),
                                ret: Box::new(fsig.ret.clone()),
                                labels: crate::checker::FnLabels::default(),
                            };
                            self.hover_record_at(
                                *name_span,
                                &fty,
                                HoverKind::Func,
                                fsig.doc.clone(),
                            );
                        }
                    } else if let Some(vty) = sig.values.get(member) {
                        // Editor hover (decl-site): record the imported value's type at the bound name.
                        if self.hover_probe.is_some() {
                            self.hover_record_at(*name_span, vty, HoverKind::Other, None);
                        }
                        self.declare(bind, vty.clone());
                        // The bind is a SNAPSHOT copy of the module global — rebinding it is rejected
                        // in `check_assign` (see `imported_values`).
                        self.imported_values.insert(bind.clone(), path.join("."));
                        // Carry the source's const-ness so the rebind guard names it const, not just
                        // "a snapshot copy" (whose "call a mutator fn" advice is wrong for a const).
                        if sig.const_values.contains(member) {
                            self.imported_consts.insert(bind.clone());
                        }
                    } else if sig.types.contains(member) {
                        // A type name imported from a module. For `std.ffi`'s exported FFI marshalling
                        // TYPE names — the fixed-width integers (`int32`) AND the opaque `ptr` handle —
                        // this is a special case: record it into the per-module `imported_ffi_types`
                        // set so `resolve_type` accepts the bare name in THIS module (it's a type, not
                        // a callable value). Only `std.ffi` lists these in `sig.types`, so the check is
                        // already scoped to it.
                        if member == "timer" {
                            // A selective `import timer from std.time` licenses the opcode-backed
                            // `timer(ms)` builtin in THIS module. `timer` is a reserved name, so ONLY
                            // `std.time`'s `sig.types` can carry it (no user module can export a member
                            // named `timer`) — matching the member name alone is unambiguous. Like the
                            // concurrency ctors, `timer` carries no runtime value (it lowers via
                            // name→opcode), so an alias would bind nothing usable AND the runtime
                            // `bind_import` skip keys on the original member name — reject the rename.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    "timer cannot be renamed on import — \
                                     write `import timer from std.time`"
                                        .to_string(),
                                );
                            } else {
                                self.imported_time.insert(member.clone());
                                // Editor hover: `timer` is a reserved FUNCTION (`timer(ms) ->
                                // Channel[bool]`), not a type — record a function-style import-line
                                // hover (probe-gated no-op off the probe).
                                if self.hover_probe.is_some() {
                                    let fty = Ty::Func {
                                        params: vec![Ty::Int],
                                        ret: Box::new(Ty::channel(Ty::Bool)),
                                        labels: crate::checker::FnLabels::default(),
                                    };
                                    self.hover_record_at(
                                        *name_span,
                                        &fty,
                                        HoverKind::Func,
                                        Some(
                                            "one-shot timeout channel — timer(ms) delivers `true` \
                                             once after ms milliseconds (import std.time)"
                                                .to_string(),
                                        ),
                                    );
                                }
                            }
                        } else if matches!(
                            member.as_str(),
                            "Shared" | "RwShared" | "Atomic" | "Executor"
                        ) {
                            // A selective `import Shared from std.concurrency` licenses just the named
                            // ctor/TYPE in THIS module (mirrors the per-name FFI width imports). Like
                            // the FFI types these carry no runtime value (ctor lowers via name→opcode),
                            // so an alias would bind nothing usable AND the runtime `bind_import` skip
                            // keys on the original member name — reject the rename to keep it honest.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "concurrency type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.concurrency`"
                                    ),
                                );
                            } else {
                                self.imported_concurrency.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if crate::native::ffi::TYPE_NAMES.contains(&member.as_str())
                            || member == "ptr"
                        {
                            // An FFI marshalling type CANNOT be RENAMED on import: the backends'
                            // `ctype_of` keys off the literal surface name (`int32`/`ptr`), so an alias
                            // would resolve to a type the marshaller can't lower. Reject `import int32
                            // as W` / `import ptr as P` (name unusable) and `import int8 as int32`
                            // (silently the wrong width). A redundant identical self-rename (`import
                            // ptr as ptr`) is harmless — the as-name equals the member, no wrong-type
                            // risk — so it falls through to the normal no-op import of `ptr`.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "FFI type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.ffi`"
                                    ),
                                );
                            } else {
                                self.imported_ffi_types.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if matches!(member.as_str(), "Socket" | "Listener") {
                            // A selective `import Socket from std.net` licenses just the named TCP
                            // handle TYPE in THIS module (mirrors the per-name concurrency imports).
                            // Like those, a net handle carries no runtime value (the type resolves
                            // directly to `Ty::Socket`; a value comes from `connect`/`listen`) AND the
                            // runtime `bind_import` skip keys on the original member name — so an alias
                            // would bind nothing usable: reject the rename. Only `std.net`'s `sig.types`
                            // carries these names (they're reserved, so no user module can export them).
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "net type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.net`"
                                    ),
                                );
                            } else {
                                self.imported_net.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if (member == "Writer" || member == "Reader")
                            && path.as_slice() == ["std".to_string(), "io".to_string()]
                        {
                            // R2/R2b — a selective `import Writer from std.io` / `import Reader from
                            // std.io` licenses just that TYPE in THIS module. Like the net handles, it
                            // carries no runtime value (the type resolves directly to `Ty::Writer`/
                            // `Ty::Reader`; a value comes from `create`/`open`/…) AND the runtime
                            // `bind_import` skip keys on the original member name — so an alias would
                            // bind nothing usable: reject the rename.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "io type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.io`"
                                    ),
                                );
                            } else {
                                self.imported_io.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if let Some(info) = sig.struct_defs.get(member) {
                            // A user struct imported by name: inject its resolved shape under the
                            // DECLARING module's runtime key (so it unifies with that module's
                            // signatures + a value's `Ty`), and make it BARE-VISIBLE under the bind
                            // name via `struct_names`/`bare_types` so `S(...)`/`x: S` resolve here.
                            let key = self.type_key(&imp.target, member);
                            self.structs.insert(key.clone(), info.clone());
                            self.struct_names.insert(bind.clone());
                            self.bare_types.insert(bind.clone(), key.clone());
                            // Same soundness gate as the whole-module path: a Builtin-origin std
                            // struct imported by name reserves its BIND name against a same-named user
                            // `struct` decl (else accept-then-trap on the native shape mismatch).
                            if info.origin == StructOrigin::Builtin {
                                self.imported_builtin_types.insert(bind.clone());
                            }
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
                        } else if let Some(ntdef) = sig.newtype_defs.get(member) {
                            // A user newtype imported by name: inject its underlying + methods under
                            // the declaring module's runtime key; expose it bare under the bind name.
                            let key = self.type_key(&imp.target, member);
                            self.newtype_defs.insert(
                                key.clone(),
                                (ntdef.underlying.clone(), ntdef.methods.clone()),
                            );
                            self.newtype_type_params
                                .insert(key.clone(), ntdef.type_params.clone());
                            self.newtype_names.insert(bind.clone());
                            self.bare_types.insert(bind.clone(), key.clone());
                            self.record_imported_type_hover(
                                bind,
                                *name_span,
                                &Ty::NewType(key, vec![]),
                                ntdef.doc.as_deref(),
                                "newtype",
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
                                // Carry the alias's width-bearing CType (computed in its DEFINING
                                // module's scope) so an extern boundary in THIS module marshals the
                                // real width through the named-import hop — not the bare flat map.
                                self.imported_alias_ctypes
                                    .insert(bind.clone(), asig.ctype.clone());
                                if asig.licensed {
                                    self.ffi_alias_ok.insert(bind.clone());
                                }
                            }
                        }
                    } else {
                        self.error(
                            imp.span,
                            format!(
                                "module '{}' has no member '{member}'",
                                module_label(&imp.import)
                            ),
                        );
                    }
                }
            }
        }
    }

    /// Capture this module's public surface (own top-level fns/values/types) after checking.
    pub(super) fn capture_sig(&self, stmts: &[Stmt]) -> ModuleSig {
        let mut sig = ModuleSig::default();
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    if let Some(fsig) = self.functions.get(&decl.name) {
                        sig.functions.insert(decl.name.clone(), fsig.clone());
                    }
                }
                StmtKind::Let {
                    names, is_const, ..
                } => {
                    for name in names {
                        if let Some(ty) = self.lookup(name) {
                            sig.values.insert(name.clone(), ty);
                            // Export const-ness so an importer's rebind names it const (a `const` let
                            // is single-name, so this only ever marks the one binding).
                            if *is_const {
                                sig.const_values.insert(name.clone());
                            }
                        }
                    }
                }
                StmtKind::Struct { name, .. } => {
                    sig.types.insert(name.clone());
                    // The LAYOUT lives under the runtime key (bare unless disambiguated); the sig is
                    // keyed by the BARE name (importers look up by bare member name + their own
                    // `type_key`).
                    let key = self.bare_key(name);
                    if let Some(info) = self.structs.get(&key) {
                        let mut info = info.clone();
                        // Carry the decl docstring onto the EXPORTED sig (editor hover) so an
                        // importer's `from M import S` / `x: S` hover surfaces it across the boundary.
                        info.doc = self.name_docs.get(name).cloned();
                        sig.struct_defs.insert(name.clone(), info);
                    }
                }
                StmtKind::Enum { name, .. } => {
                    sig.types.insert(name.clone());
                    let key = self.bare_key(name);
                    if let Some(variant_names) = self.enums.get(&key) {
                        let type_params =
                            self.enum_type_params.get(&key).cloned().unwrap_or_default();
                        let variants = variant_names
                            .iter()
                            .filter_map(|v| self.variants.get(&(key.clone(), v.clone())).cloned())
                            .collect();
                        sig.enum_defs.insert(
                            name.clone(),
                            EnumSigInfo {
                                variant_names: variant_names.clone(),
                                type_params,
                                variants,
                                methods: self.enum_methods.get(&key).cloned().unwrap_or_default(),
                                doc: self.name_docs.get(name).cloned(),
                            },
                        );
                    }
                }
                StmtKind::NewType { name, .. } => {
                    sig.types.insert(name.clone());
                    let key = self.bare_key(name);
                    if let Some((underlying, methods)) = self.newtype_defs.get(&key) {
                        sig.newtype_defs.insert(
                            name.clone(),
                            NewTypeSigInfo {
                                underlying: underlying.clone(),
                                type_params: self
                                    .newtype_type_params
                                    .get(&key)
                                    .cloned()
                                    .unwrap_or_default(),
                                methods: methods.clone(),
                                doc: self.name_docs.get(name).cloned(),
                            },
                        );
                    }
                }
                StmtKind::TypeAlias { name, .. } => {
                    sig.types.insert(name.clone());
                    if let Some(body) = self.aliases.get(name) {
                        // Resolve the alias body in THIS (the defining) module's scope so an
                        // importer carries the right underlying type (incl. an FFI width license).
                        let resolved = self.resolve_type_ro_pub(body);
                        // Also resolve the body to a WIDTH-BEARING CType in this defining scope, so a
                        // cross-module `type Len = int32` exports `int32` (not `Ty::Int`). This is the
                        // channel the real width travels through a `from`-import / `module.Alias` hop.
                        let ctype = self.resolve_ctype(body);
                        let licensed = self.ffi_alias_ok.contains(name);
                        // If the alias embeds FFI widths but is NOT licensed, find the first width
                        // the defining module did not import — an importer must reject the alias
                        // rather than launder the un-imported width.
                        let unlicensed_width = if licensed {
                            None
                        } else {
                            let mut widths = Vec::new();
                            Self::collect_width_names(body, &mut widths);
                            widths
                                .into_iter()
                                .find(|w| !self.imported_ffi_types.contains(w))
                        };
                        sig.type_aliases.insert(
                            name.clone(),
                            AliasSig {
                                body: resolved,
                                licensed,
                                unlicensed_width,
                                ctype,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        sig
    }

    /// `resolve_ty_ro` wrapped for use inside `capture_sig` (which holds `&self`). Resolves an alias
    /// body to a `Ty` in the current (defining) module's scope without emitting errors.
    pub(super) fn resolve_type_ro_pub(&self, t: &Type) -> Ty {
        self.resolve_ty_ro(t)
    }

    // ===== scopes =====

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.loop_vars.push(std::collections::HashSet::new());
        self.const_decls.push(std::collections::HashSet::new());
        self.capture_table.push(HashMap::new());
    }
    pub(super) fn pop_scope(&mut self) {
        self.scopes.pop();
        self.loop_vars.pop();
        self.const_decls.pop();
        self.capture_table.pop();
    }
    /// Record `name` (already declared in the current scope) as a `const T` binding.
    pub(super) fn declare_const(&mut self, name: &str) {
        if let Some(set) = self.const_decls.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name` an in-scope `const T` binding (innermost binding wins, shadowing-aware)? A `const`
    /// captured by a nested fn stays const inside the closure body — the enclosing scope is still on
    /// the stack, so this resolves through it.
    pub(super) fn is_const_decl(&self, name: &str) -> bool {
        for (vars, consts) in self.scopes.iter().zip(self.const_decls.iter()).rev() {
            if vars.contains_key(name) {
                return consts.contains(name);
            }
        }
        false
    }
    pub(super) fn declare(&mut self, name: &str, ty: Ty) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
        // Re-declaring a name (e.g. `:=` shadowing a loop var in the same scope) yields a fresh,
        // mutable binding — clear any loop-var mark so assignment to it isn't wrongly rejected.
        if let Some(set) = self.loop_vars.last_mut() {
            set.remove(name);
        }
        // Same rule for a `const` binding: a re-declaration in the same scope is a fresh binding
        // (mutable unless `declare_const` re-marks it), so clear any stale const mark first.
        if let Some(set) = self.const_decls.last_mut() {
            set.remove(name);
        }
        // Same rule for a `from`-imported global: re-declaring it at MODULE scope (`COUNT := COUNT + 1`)
        // hands the name back to this module, so the from-import rebind gate (`imported_values`, keyed
        // by bare name) must stop firing — the binding it names is gone. Module scope only (the sole
        // scope the gate consults); a fn-local shadow leaves the module binding intact. `bind_import`
        // inserts AFTER its own `declare`, so its entry survives.
        if self.scopes.len() == 1 {
            self.imported_values.remove(name);
        }
    }
    pub(super) fn lookup(&self, name: &str) -> Option<Ty> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }
    /// Re-pin `name`'s binding to `ty` **in its OWNING scope** (the same scope `lookup` resolves),
    /// not the innermost one. Used by refine-on-first-use to narrow an empty-collection's `Unknown`
    /// element/key/value slot to the concrete type the first mutating op supplies. `declare` always
    /// writes the last scope — wrong for an outer-scope receiver refined inside an `if`/`for` block
    /// (it would shadow-create a bogus inner binding that leaks on pop), so we walk innermost-first
    /// and overwrite the first scope that owns `name`. Returns the scope index written (so the
    /// flow-sensitivity snapshot/restore barrier can revert THIS scope's binding precisely).
    pub(super) fn repin(&mut self, name: &str, ty: Ty) -> Option<usize> {
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                self.scopes[i].insert(name.to_string(), ty);
                return Some(i);
            }
        }
        None
    }
    /// Snapshot every in-scope binding whose type still carries an `Unknown` in a slot position (a
    /// refinable empty-collection / nullary-variant / None producer), recording its OWNING scope
    /// index, name, and current type. Paired with [`Self::restore_refinable`]. Refine-on-first-use is
    /// now PERSISTENT scope-wide first-use pinning, so the STATEMENT-position sites
    /// (`check_block`/for-loop/`check_match`) no longer snapshot/restore — a pin there persists. These
    /// helpers remain in use by the EXPRESSION-position arms (`infer_if_else`/`infer_match`): a value-
    /// arm produces a VALUE, so a pin in one value-arm must not leak to a sibling value-arm or it
    /// would corrupt branch value inference. We snapshot the OWNING scope index — not the innermost
    /// block scope — so restoring reverts the exact binding `repin` wrote, even when the receiver was
    /// declared in an outer scope.
    pub(super) fn snapshot_refinable(&self) -> Vec<(usize, String, Ty)> {
        let mut snap = Vec::new();
        for (i, scope) in self.scopes.iter().enumerate() {
            for (name, ty) in scope {
                if contains_unknown_in_slot(ty) {
                    snap.push((i, name.clone(), ty.clone()));
                }
            }
        }
        snap
    }
    /// Restore the bindings captured by [`Self::snapshot_refinable`], reverting any in-arm refinement
    /// so each EXPRESSION-position value-arm refines independently from the pre-arm type (kept only
    /// at `infer_if_else`/`infer_match`; statement-position pins now persist). Writes back by (scope
    /// index, name); a snapshotted scope that was already popped is skipped (binding gone, nothing to
    /// revert).
    pub(super) fn restore_refinable(&mut self, snap: Vec<(usize, String, Ty)>) {
        for (i, name, ty) in snap {
            if let Some(scope) = self.scopes.get_mut(i)
                && scope.contains_key(&name)
            {
                scope.insert(name, ty);
            }
        }
    }
    /// PART A — is `t` an empty collection type whose own DIRECT element/key/value slot is still a bare
    /// `Unknown` (the shape produced by an un-constrained empty literal: `[]`→`List[Unknown]`,
    /// `{}`→`Map[Unknown,Unknown]`, `Set()`→`Set[Unknown]`)? The slot must be DIRECTLY `Unknown`, not
    /// merely nested-Unknown: `[[]]` is `List[List[Unknown]]` (a NON-empty list whose element is an
    /// empty list) — its direct slot is `List[Unknown]`, so it is NOT an empty collection and must not
    /// be flagged. This also DELIBERATELY excludes `Option[Unknown]` (`x := None`) and nullary-enum
    /// producers (`Box[Unknown]`): the requirement is scoped to the three literal container kinds.
    pub(super) fn is_unrefined_empty_coll(t: &Ty) -> bool {
        match t {
            Ty::List(e) | Ty::Set(e) => e.is_unknown(),
            Ty::Map(k, v) => k.is_unknown() || v.is_unknown(),
            _ => false,
        }
    }
    /// PART A — a constraining op (`push`/`add`/`insert`/`extend`, `m[k]=v`) targeted `name`, so clear
    /// its pending empty-collection requirement. Resolves `name`'s OWNING scope (reverse walk, like
    /// `repin`) and drops only that binding's site, so an inner-scope shadow of the same name does not
    /// clear an outer binding's requirement.
    pub(super) fn drop_empty_site(&mut self, name: &str) {
        let owner = (0..self.scopes.len())
            .rev()
            .find(|&i| self.scopes[i].contains_key(name));
        if let Some(owner) = owner {
            self.empty_coll_sites
                .retain(|(o, n, _)| !(*o == owner && n == name));
        }
    }
    /// PART A — an empty-collection binding READ AS A VALUE that ESCAPES into another binding or
    /// structure (the RHS of `:=`/`=`/field-/index-/tuple-assign, or an element of a list/set/map/tuple
    /// literal) is no longer provably-unconstrained: drop its pending requirement. Without this,
    /// `c = b` / `c := b` / `c := [b]` (with `b := []`) spuriously errored on `b` even though the
    /// program is type-sound (b aliases / flows into a typed-or-later-refined slot) — the drop-guard
    /// otherwise covers only typed sinks (annotation/param/return) and the LHS target of reassign,
    /// never the RHS source. Scans the value one level. A bare-ident read that is NOT a value-escape
    /// (a call arg like `print(b)`) is intentionally NOT covered — that case must still require the
    /// annotation. The alias binding itself, if left unrefined, records its own site, so the
    /// requirement moves rather than vanishes (no new false-negative).
    pub(super) fn drop_value_escape_sites(&mut self, value: &Expr) {
        match &value.kind {
            ExprKind::Ident(name) => self.drop_empty_site(name),
            ExprKind::List(elems) | ExprKind::Set(elems) | ExprKind::Tuple(elems) => {
                for e in elems {
                    if let ExprKind::Ident(n) = &e.kind {
                        self.drop_empty_site(n);
                    }
                }
            }
            ExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    if let ExprKind::Ident(n) = &k.kind {
                        self.drop_empty_site(n);
                    }
                    if let ExprKind::Ident(n) = &v.kind {
                        self.drop_empty_site(n);
                    }
                }
            }
            _ => {}
        }
    }
    /// PART A — at end-of-scope (the fn-body / module seam, called BEFORE `pop_scope`), error on every
    /// pending empty-collection site owned by the scope being popped whose binding is STILL an
    /// unrefined empty collection (never constrained → no element type → require an annotation). Sites
    /// owned by an enclosing scope (`owning < idx`) are kept for that scope's own finalize; sites whose
    /// owning scope was already popped (a block/for/match-body residual — `get(owning)` is `None`) are
    /// silently drained without erroring, matching the refine machinery's block-local limits.
    pub(super) fn finalize_empty_coll_sites(&mut self) {
        let idx = self.scopes.len() - 1;
        let mut to_error: Vec<Span> = Vec::new();
        self.empty_coll_sites.retain(|(owning, name, span)| {
            if *owning < idx {
                return true; // an enclosing scope owns it — its finalize handles it
            }
            if self
                .scopes
                .get(*owning)
                .and_then(|s| s.get(name))
                .is_some_and(Self::is_unrefined_empty_coll)
            {
                to_error.push(*span);
            }
            false // drained: either errored now, or its scope is already gone
        });
        for span in to_error {
            self.error(
                span,
                "cannot infer element type of empty collection; add a type annotation".to_string(),
            );
        }
    }
    /// Is `name` bound *below* the module-global scope (scope 0) — i.e. a local, parameter, or
    /// captured binding? The qualified enum-variant form `Enum.Variant` yields to such a binding but
    /// NOT to a module global or function, mirroring both engines' locals-only precedence gate (VM
    /// `resolve_local`/`captures`, interp `get_local`). Using full [`Self::lookup`] here would let a
    /// top-level global named like the enum shadow in the checker but not the engines — a soundness
    /// hole (the checker would validate a different program than the one that runs).
    pub(super) fn is_local_binding(&self, name: &str) -> bool {
        self.scopes.iter().skip(1).any(|s| s.contains_key(name))
    }
    /// Mark `name` (already declared in the current scope) as an immutable `for`-loop variable.
    pub(super) fn mark_loop_var(&mut self, name: &str) {
        if let Some(set) = self.loop_vars.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name`'s nearest binding a `for`-loop variable? Resolves to the binding's defining scope
    /// so an inner `:=` shadow (a fresh local) is correctly reported as not-a-loop-var.
    pub(super) fn is_loop_var(&self, name: &str) -> bool {
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return self.loop_vars[i].contains(name);
            }
        }
        false
    }
    /// Is `name` a binding **captured** by an enclosing `spawn:` task — i.e. defined in a local
    /// scope below the innermost task's floor? Such bindings are read-only inside the task body
    /// (the airlock: a task gets its own copy, so reassigning the capture can't leak out). A
    /// task-local binding (declared inside the task) and a global/function are not captures.
    pub(super) fn is_captured(&self, name: &str) -> bool {
        let Some(&floor) = self.capture_floors.last() else {
            return false;
        };
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return i < floor;
            }
        }
        false
    }
    /// Like [`is_captured`], but excludes module-level (scope 0) bindings — imports and top-level
    /// declarations are globals resolvable identically in every task (like free functions), not
    /// per-task value captures. Used by the *read* sendability gate so reading an imported module or
    /// a top-level closure inside a `spawn:` block isn't flagged; the *reassign* gate keeps the
    /// broader [`is_captured`] (writing a copy of any capture, global or not, can't leak out).
    pub(super) fn is_local_capture(&self, name: &str) -> bool {
        let Some(&floor) = self.capture_floors.last() else {
            return false;
        };
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return i > 0 && i < floor;
            }
        }
        false
    }

    /// Does `name` resolve to the MODULE scope (index 0) — i.e. is it a module-level binding rather
    /// than a local/param shadow? Used by the from-imported-global rebind gate, so a fn-local `:=`
    /// shadow of an imported name stays assignable.
    pub(super) fn resolves_at_module_scope(&self, name: &str) -> bool {
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return i == 0;
            }
        }
        false
    }

    // ===== B3.3 (Task 2a): capture-sendability gate at spawn callee/arg sites =====

    /// The non-sendable LOCAL captures among the free-variable set `free`, resolved against the
    /// CURRENT scope stack. A name resolving at scope index **0** (a module global — a read-only
    /// namespace resolvable in every task, like a free fn) is EXCLUDED: this is the invariant that
    /// keeps a MODULE-GLOBAL `ref` out of the gate. A name that resolves in no scope (a free fn /
    /// import / builtin) is likewise excluded. Only a name at scope index `> 0` (an enclosing local —
    /// the closure's own param scope is already popped by the time this runs, mirroring
    /// [`is_local_capture`]) whose type is `!sendable` is a capture. Sorted by name for deterministic
    /// diagnostics (HashSet iteration order is nondeterministic).
    pub(super) fn local_captures_of(
        &self,
        free: &std::collections::HashSet<String>,
    ) -> Vec<Capture> {
        let mut caps = Vec::new();
        for name in free {
            let mut resolved = None;
            for i in (0..self.scopes.len()).rev() {
                if let Some(ty) = self.scopes[i].get(name) {
                    resolved = Some((i, ty.clone()));
                    break;
                }
            }
            let Some((i, ty)) = resolved else { continue };
            // Scope 0 = module globals: read-only across tasks, NOT a per-task capture (PITFALL —
            // a module-global `ref` must never be gated).
            if i == 0 || self.sendable(&ty) {
                continue;
            }
            caps.push(Capture {
                name: name.clone(),
                ty,
            });
        }
        caps.sort_by(|a, b| a.name.cmp(&b.name));
        caps
    }

    /// Record (keyed by the bound `name`) the non-sendable local captures of a just-declared
    /// closure/nested-fn whose free-variable set is `free`. No-op when there are none.
    pub(super) fn record_closure_captures(
        &mut self,
        name: &str,
        free: &std::collections::HashSet<String>,
    ) {
        let caps = self.local_captures_of(free);
        if !caps.is_empty()
            && let Some(tbl) = self.capture_table.last_mut()
        {
            tbl.insert(name.to_string(), caps);
        }
    }

    /// The recorded non-sendable captures of a named closure/nested-fn binding (innermost scope wins,
    /// mirroring `lookup`). Empty when `name` is not a recorded capturing value (a sendable closure, a
    /// free fn, a builtin, …).
    pub(super) fn lookup_captures(&self, name: &str) -> Vec<Capture> {
        for tbl in self.capture_table.iter().rev() {
            if let Some(caps) = tbl.get(name) {
                return caps.clone();
            }
        }
        Vec::new()
    }

    /// The non-sendable local captures of a closure VALUE appearing at a spawn callee/arg site: an
    /// `Ident` resolves through the recorded side-table; an INLINE `Closure` is analyzed on the spot
    /// (its free-var set minus its params). Any other expression carries no capturing closure value.
    pub(super) fn spawn_value_captures(&self, ex: &crate::ast::Expr) -> Vec<Capture> {
        match &ex.kind {
            ExprKind::Ident(name) => self.lookup_captures(name),
            ExprKind::Closure { params, body, .. } => {
                let bound: std::collections::HashSet<String> =
                    params.iter().map(|p| p.name.clone()).collect();
                let mut free = std::collections::HashSet::new();
                crate::compiler::free_names_expr(body, &bound, &mut free);
                self.local_captures_of(&free)
            }
            _ => Vec::new(),
        }
    }

    /// Emit the block-form diagnostic (verbatim, so `spawn:` block ≡ callee/arg) at `span` for each
    /// non-sendable capture, rendering the captured binding's type.
    pub(super) fn emit_capture_errors(&mut self, caps: &[Capture], span: Span) {
        for c in caps {
            let disp = c.ty.to_string();
            self.error(
                span,
                format!(
                    "cannot use non-sendable captured binding '{name}' of type {disp} inside a \
                     spawned task (captures cross the airlock — communicate via a Channel or Shared)",
                    name = c.name
                ),
            );
        }
    }

    // ===== pass 1: hoist declarations =====

    /// First sub-pass: learn every struct/enum *name* so `resolve_type` can recognize them even
    /// when used before their definition (or inside each other).
    pub(super) fn collect_names(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Struct { name, .. } => {
                    // Cross-kind name clash: a struct and an enum can't share a name (they'd both
                    // register, the enum silently shadowed, and — sharing a `Name[args]` Display —
                    // produce nonsense like "cannot assign Foo[int] to … Foo[int]"). Same-kind dups
                    // are caught later in the resolve pass.
                    if self.enum_names.contains(name) || self.newtype_names.contains(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.struct_names.insert(name.clone());
                }
                StmtKind::Enum { name, .. } => {
                    if self.struct_names.contains(name) || self.newtype_names.contains(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.enum_names.insert(name.clone());
                }
                // A file-backed native std module (`std/regex.chz`) checked STANDALONE as the ENTRY
                // (`chezzi check std/regex.chz` / editor/LSP) goes through this normal module path
                // (native:None), unlike the import path (native:Some → `harvest_native_module`). Register
                // the `native struct`'s bare name so a later same-file `native fn` return type that
                // references it (e.g. `find -> Result[Option[Match]]`) resolves — its layout is already
                // seeded in `self.structs` by `seed_stdlib_structs`. Stdlib-only (a user-file `native
                // struct` is rejected downstream in the hoist pass); the import path never reaches here.
                StmtKind::NativeStruct { name, .. } if self.current_module_is_stdlib => {
                    self.struct_names.insert(name.clone());
                }
                StmtKind::NewType { name, .. } => {
                    if matches!(
                        name.as_str(),
                        "int" | "float" | "bool" | "str" | "bytes" | "bytearray" | "nil"
                    ) || is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    } else if self.struct_names.contains(name)
                        || self.enum_names.contains(name)
                        || self.newtype_names.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.newtype_names.insert(name.clone());
                }
                StmtKind::TypeAlias { name, ty, .. } => {
                    if matches!(
                        name.as_str(),
                        "int" | "float" | "bool" | "str" | "bytes" | "bytearray" | "nil"
                    ) || is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    } else if self.aliases.contains_key(name)
                        || self.struct_names.contains(name)
                        || self.enum_names.contains(name)
                        || self.newtype_names.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    } else {
                        self.aliases.insert(name.clone(), ty.clone());
                        // PRECISE width-alias opt-in: if this alias's body references fixed-width FFI
                        // type names (`int8`..`uint64`) — directly (`type Len = int32`) or embedded in
                        // a composite (`type Pair = (int32, int32)`, `type Buf = list[uint8]`) — and
                        // EVERY such width was imported per-name from `std.ffi` by THIS (the defining)
                        // module, record the alias as licensed. `resolve_type` then lets those widths
                        // resolve through the alias anywhere, including cross-module with no re-import.
                        // A `type Len = int32` whose module never imported int32 is NOT licensed, so it
                        // can't launder the bare width past the import gate. Requiring ALL embedded
                        // widths imported keeps it precise: a `type Mixed = (int32, int64)` that imported
                        // only int32 stays unlicensed, so int64 can't ride in on int32's opt-in.
                        // `collect_names` runs after `bind_import`, so `imported_ffi_types` is populated.
                        let mut widths = Vec::new();
                        Self::collect_width_names(ty, &mut widths);
                        if !widths.is_empty()
                            && widths.iter().all(|w| self.imported_ffi_types.contains(w))
                        {
                            self.ffi_alias_ok.insert(name.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect doc-comments for the module's top-level NON-fn declarations (struct/enum/protocol/
    /// newtype/type-alias names + top-level `let` bindings) into `name_docs`, keyed by simple name.
    /// Free fns and methods carry their doc on `FnSig::doc` instead (handled in `fn_sig`). Purely for
    /// LSP hover — never consulted by checking/codegen, so it is behavior- and parity-neutral.
    pub(super) fn collect_docs(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Struct {
                    name, doc: Some(d), ..
                }
                | StmtKind::Enum {
                    name, doc: Some(d), ..
                }
                | StmtKind::Protocol {
                    name, doc: Some(d), ..
                }
                | StmtKind::NewType {
                    name, doc: Some(d), ..
                }
                | StmtKind::TypeAlias {
                    name, doc: Some(d), ..
                } => {
                    self.name_docs.insert(name.clone(), d.clone());
                }
                // A top-level `let` binds (possibly multiple) names; surface the doc on each.
                StmtKind::Let {
                    names,
                    doc: Some(d),
                    ..
                } => {
                    for n in names {
                        self.name_docs.insert(n.clone(), d.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect every fixed-width FFI type name (`int8`..`uint64`) referenced anywhere in `ty`,
    /// recursing through composites (`Generic` args, `Func` params/return, `Tuple` elements). Used
    /// to license a width-alias only when its defining module imported all the widths it embeds.
    pub(super) fn collect_width_names(ty: &Type, out: &mut Vec<String>) {
        match ty {
            Type::Named { name: n, .. } => {
                if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) {
                    out.push(n.clone());
                }
            }
            Type::Generic(_, args, ..) => {
                for a in args {
                    Self::collect_width_names(a, out);
                }
            }
            Type::Func { params, ret, .. } => {
                for p in params {
                    Self::collect_width_names(p, out);
                }
                Self::collect_width_names(ret, out);
            }
            Type::Tuple(elems) => {
                for e in elems {
                    Self::collect_width_names(e, out);
                }
            }
            // A module-qualified type's head is a user type (never a bare width); only its type
            // arguments could carry a width name written in THIS module.
            Type::Qualified { args, .. } => {
                for a in args {
                    Self::collect_width_names(a, out);
                }
            }
        }
    }

    /// Second sub-pass: resolve and register signatures, fields, and variants. Redeclarations
    /// (a name defined twice) are reported here — otherwise "last write wins" would silently
    /// mis-type or, for struct methods, panic in pass 2 on a key that no longer exists.
    pub(super) fn hoist(&mut self, stmts: &[Stmt]) {
        // Protocols first: function/struct signatures may reference them in type-parameter bounds.
        for s in stmts {
            if let StmtKind::Protocol {
                name,
                type_params,
                methods,
                embeds,
                ..
            } = &s.kind
            {
                self.hoist_protocol(name, type_params, methods, embeds, s.span);
            }
        }
        // M22 — validate embeds in a SECOND pass, now that every protocol is registered, so a forward
        // (or cyclic) embed reference resolves. Collision/cycle detection lives here (declare-time,
        // authoritative — before any satisfaction check can recurse into a cycle).
        for s in stmts {
            if let StmtKind::Protocol {
                name,
                methods,
                embeds,
                ..
            } = &s.kind
            {
                self.validate_protocol_embeds(name, methods, embeds, s.span);
            }
        }
        // extern fn (name, span) pairs, collected during the hoist loop and checked against the
        // fully-built struct/variant/enum registries AFTER the loop (so a `struct S` declared *after*
        // an `extern fn S` still collides — the check is order-independent).
        // `mut` + the post-loop sweep are unix-only (only the `#[cfg(unix)]` arm pushes); on other
        // targets extern is rejected wholesale, leaving this empty.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut extern_names: Vec<(String, Span)> = Vec::new();
        // Extern param/return marshallability is validated AFTER this loop (collected here), so a
        // struct passed/returned BY VALUE may be DECLARED AFTER the extern block: `self.structs`
        // (field info, which `assert_marshallable` inspects for a flat-scalar struct) is only fully
        // populated once every struct in the module has been hoisted. `collect_names` already
        // pre-registered struct *names*, so `resolve_type` accepts the forward reference inline.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut extern_marshal_checks: Vec<(Ty, String, Span, bool)> = Vec::new();
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    // A user `fn` named after a runtime constructor / builtin op (`timer`, `Channel`,
                    // `range`, …) is silently SHADOWED: `infer_named_call` / the backends resolve the
                    // builtin arm before a plain named call, so the user fn is dead code. Reject the
                    // collision with a clear `reserved` error (mirrors the extern-name guard).
                    if is_reserved_name(&decl.name) {
                        self.error(
                            s.span,
                            format!("function name '{}' is reserved (builtin)", decl.name),
                        );
                    } else if self.functions.contains_key(&decl.name) {
                        self.error(
                            s.span,
                            format!("function '{}' is already defined", decl.name),
                        );
                    }
                    let sig = self.fn_sig(decl, s.span);
                    self.functions.insert(decl.name.clone(), sig);
                    // Same-module fn name (top-level `fn` only, NOT an import) — licenses the
                    // generic-fn-as-value turbofish B-path in `infer_index`, in lockstep with the
                    // compiler's `fn_names` erase set.
                    self.local_fn_names.insert(decl.name.clone());
                }
                StmtKind::Struct {
                    name,
                    type_params,
                    fields,
                    methods,
                    ..
                } => {
                    // `imported_builtin_types`: a name THIS module imported as a Builtin-origin std
                    // struct (Match/Response/ProcResult, …) is reserved against a same-named user
                    // `struct` decl — declaring it would overwrite the native seed yet the runtime
                    // still constructs/returns the native shape, so the check-clean program would trap
                    // at runtime on a field mismatch. (A bare unimported `struct Match` stays legal —
                    // the name isn't in the set without the import event.)
                    if is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                        || self.imported_builtin_types.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    // A pre-seeded synthetic stdlib struct (`Match`/`Response`/`ProcResult`, always
                    // present in `self.structs` under its bare key tagged `StructOrigin::Builtin` for
                    // import-free field access) is NOT a real prior definition — a user `struct
                    // Response` shadows it (the hoist insert below overwrites the seed with the user's
                    // `User`-origin layout). Only a genuine User-origin entry is "already defined".
                    let already_defined = self
                        .structs
                        .get(&self.bare_key(name))
                        .is_some_and(|i| i.origin != StructOrigin::Builtin);
                    if already_defined {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // A type PARAMETER may not be named after a reserved builtin type (`struct
                    // Box[int]`/`[List]`/`[Result]`) — same one-way-ratchet rule as the decl NAME.
                    self.reject_reserved_type_params(type_params);
                    // The struct's type parameters are in scope across its field and method
                    // signatures (so `first: A` and `fn push(self, x: T)` resolve `A`/`T`).
                    let saved = self.enter_type_params(type_params);
                    // A repeated field name adds a dead-but-still-positionally-required ctor slot
                    // (ctor is positional over the field Vec; field lookup is first-wins) — reject it.
                    self.report_dup_names(
                        fields.iter().map(|f| (f.name.as_str(), f.name_span)),
                        "field",
                    );
                    // A repeated method name silently last-wins (the HashMap collapses it) — reject at
                    // hoist so the dup is the headline error, not a downstream return-type mismatch.
                    self.report_dup_names(
                        methods.iter().map(|m| (m.name.as_str(), m.name_span)),
                        "method",
                    );
                    // A field and a method may not share a name on the same struct (mirrors the enum
                    // variant/static disjointness below): `p.f` would be ambiguous between the two.
                    let field_names: std::collections::HashSet<&str> =
                        fields.iter().map(|f| f.name.as_str()).collect();
                    for m in methods {
                        if field_names.contains(m.name.as_str()) {
                            self.error(
                                m.name_span,
                                format!(
                                    "'{}' is declared as both a field and a method of '{name}'",
                                    m.name
                                ),
                            );
                        }
                    }
                    let fields: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| (f.name.clone(), self.resolve_type(&f.ty, s.span)))
                        .collect();
                    // `Self` in a method sig resolves to this concrete struct (parameterized by its
                    // own type params — the layout isn't inserted yet, so build the self-ty directly
                    // from the in-scope `type_params`, matching `struct_self_ty`'s shape).
                    let self_ty = Ty::Struct(
                        self.bare_key(name),
                        type_params
                            .iter()
                            .map(|tp| Ty::Param(tp.name.clone()))
                            .collect(),
                    );
                    let saved_self = self.current_self_ty.replace(self_ty);
                    let methods = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.current_self_ty = saved_self;
                    self.exit_type_params(saved);
                    let origin = if self.current_module_is_stdlib {
                        StructOrigin::Builtin
                    } else {
                        StructOrigin::User
                    };
                    // Register the LAYOUT under this module's runtime key (bare unless a genuine
                    // cross-module clash disambiguated it), so a value of this type — whose `Ty` also
                    // carries the key — resolves its fields/methods here and across the module
                    // boundary. `struct_names` (bare-visibility) stays bare; only the layout is keyed.
                    let key = self.bare_key(name);
                    self.structs.insert(
                        key,
                        StructInfo {
                            type_params: type_params.clone(),
                            fields,
                            methods,
                            origin,
                            // Decl docstring is attached later in `capture_sig` (from `name_docs`),
                            // only for the module's exported sig; the in-checker layout doesn't need it.
                            doc: None,
                        },
                    );
                }
                StmtKind::Enum {
                    name,
                    type_params,
                    variants,
                    methods,
                    ..
                } => {
                    if is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    // The LAYOUT tables (`enums`/`variants`/`enum_type_params`) are keyed by this
                    // module's runtime key (bare unless disambiguated), so a value's `Ty::Enum(key)`
                    // resolves its variants here and across module boundaries. `enum_names`/
                    // `variant_owners` (bare-visibility + qualify-hint) stay bare.
                    let key = self.bare_key(name);
                    if self.enums.contains_key(&key) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // A type PARAMETER may not be named after a reserved builtin type (`enum E[int]`).
                    self.reject_reserved_type_params(type_params);
                    // The enum's type parameters are in scope across its variant payloads (so a
                    // `Node(T, Tree[T])` resolves `T`). Validate each bound names a known protocol.
                    let saved = self.enter_type_params(type_params);
                    for tp in type_params {
                        self.check_bounds(&tp.bounds, &tp.name, s.span);
                    }
                    let mut names = Vec::new();
                    for v in variants {
                        // Variants are scoped under their enum, so two *different* enums may share a
                        // variant name. A repeat *within the same* enum is still a collision.
                        if self.variants.contains_key(&(key.clone(), v.name.clone())) {
                            self.error(
                                s.span,
                                format!("variant '{}' is already defined in enum '{name}'", v.name),
                            );
                        }
                        names.push(v.name.clone());
                        let payload = v
                            .payload
                            .iter()
                            .map(|t| self.resolve_type(t, s.span))
                            .collect();
                        self.variants.insert(
                            (key.clone(), v.name.clone()),
                            VariantInfo {
                                enum_name: key.clone(),
                                payload,
                            },
                        );
                        self.variant_owners
                            .entry(v.name.clone())
                            .or_default()
                            .push(name.clone());
                    }
                    // Methods see the enum's type parameters in scope (like the struct path), so a
                    // generic `fn get(self) -> T` resolves `T`. Name-keyed exactly like struct methods.
                    // A repeated method name silently last-wins (the HashMap collapses it) — reject.
                    self.report_dup_names(
                        methods.iter().map(|m| (m.name.as_str(), m.name_span)),
                        "method",
                    );
                    // `Self` in a method sig resolves to this concrete enum (parameterized by its own
                    // type params — the `enum_type_params` table isn't inserted yet, so build the
                    // self-ty from the in-scope `type_params`, matching `enum_self_ty`'s shape).
                    let self_ty = Ty::Enum(
                        key.clone(),
                        type_params
                            .iter()
                            .map(|tp| Ty::Param(tp.name.clone()))
                            .collect(),
                    );
                    let saved_self = self.current_self_ty.replace(self_ty);
                    let method_sigs: HashMap<String, FnSig> = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.current_self_ty = saved_self;
                    // Variant names and STATIC-method names must be DISJOINT: `Enum.name` always
                    // resolves the variant first (see `infer_call`), so a static method named after a
                    // variant could never be reached — a collision is an error, not a silent shadow.
                    for m in methods {
                        if names.contains(&m.name) {
                            self.error(
                                s.span,
                                format!("'{}' is already a variant of enum '{name}'", m.name),
                            );
                        }
                    }
                    self.exit_type_params(saved);
                    self.enums.insert(key.clone(), names);
                    self.enum_type_params
                        .insert(key.clone(), type_params.clone());
                    self.enum_methods.insert(key, method_sigs);
                }
                StmtKind::NewType {
                    name,
                    type_params,
                    underlying,
                    methods,
                    ..
                } => {
                    if is_reserved_type(name) || is_reserved_protocol(name) {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    let key = self.bare_key(name);
                    if self.newtype_defs.contains_key(&key) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // A type PARAMETER may not be named after a reserved builtin type (`newtype
                    // N[List] = int`).
                    self.reject_reserved_type_params(type_params);
                    // A type-parameterized newtype puts its params in scope across the underlying type
                    // + method signatures (so `newtype Stack[T] = list[T]` and `fn push(self, x: T)`
                    // resolve `T`), exactly like the struct/enum generic path. Validate each bound.
                    let saved = self.enter_type_params(type_params);
                    for tp in type_params {
                        self.check_bounds(&tp.bounds, &tp.name, s.span);
                    }
                    let under_ty = self.resolve_type(underlying, s.span);
                    // A newtype cannot wrap itself or another newtype's identity that's still a bare
                    // newtype value — but a newtype OF a newtype is simply nominal nesting (allowed:
                    // construct/unwrap one level at a time). No special rejection needed here.
                    // A repeated method name silently last-wins (the HashMap collapses it) — reject.
                    self.report_dup_names(
                        methods.iter().map(|m| (m.name.as_str(), m.name_span)),
                        "method",
                    );
                    // Static (associated) methods on a newtype (`fn zero()` — no `self`) parse but are
                    // unreachable: the call site (`Meters.zero()`) has no newtype static-dispatch path
                    // and falls through to a cryptic "unknown name 'Meters'". Reject at the decl site
                    // with a clear not-supported message (deferred v1 limit; struct/enum only).
                    for m in methods {
                        if m.params.first().is_none_or(|p| p.name != "self") {
                            self.error(
                                m.name_span,
                                format!(
                                    "static (associated) method '{}' on a newtype is not supported yet (only struct and enum have them)",
                                    m.name
                                ),
                            );
                        }
                    }
                    // `Self` in a method sig resolves to this concrete newtype (parameterized by its
                    // own type params — `newtype_type_params` isn't inserted yet, so build the self-ty
                    // from the in-scope `type_params`, matching `newtype_self_ty`'s shape).
                    let self_ty = Ty::NewType(
                        key.clone(),
                        type_params
                            .iter()
                            .map(|tp| Ty::Param(tp.name.clone()))
                            .collect(),
                    );
                    let saved_self = self.current_self_ty.replace(self_ty);
                    let method_sigs: HashMap<String, FnSig> = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.current_self_ty = saved_self;
                    self.exit_type_params(saved);
                    self.newtype_type_params
                        .insert(key.clone(), type_params.clone());
                    self.newtype_defs.insert(key, (under_ty, method_sigs));
                }
                StmtKind::Extern { fns, .. } => {
                    // Dynamic C-ABI FFI (`dlopen`/libffi) is unix-only — `int` marshals as C `long`,
                    // which is 64-bit on every supported (LP64) unix target. On a non-unix target
                    // (e.g. LLP64 Windows, where C `long` is 32-bit) `extern` is unavailable; reject
                    // it here so the `MakeCffi`/`dlopen` + `as c_long` truncation path is statically
                    // unreachable off-unix.
                    #[cfg(not(unix))]
                    for ef in fns {
                        self.error(
                            ef.span,
                            format!(
                                "extern FFI is only supported on unix targets ('{}')",
                                ef.name
                            ),
                        );
                    }
                    // Each extern C fn becomes a plain module-global signature, hoisted exactly like
                    // a top-level `fn` so calls type-check through the normal `infer_named_call` path.
                    // v1 marshals scalars only — every resolved param + return type must be
                    // C-marshallable (int/float/bool/str, or void return).
                    #[cfg(unix)]
                    for ef in fns {
                        // An extern fn may not take a builtin/print/constructor name — both backends
                        // resolve those to a special op before a plain call, so the extern would be
                        // dead code (and the compiler's eager `MakeCffi` would `dlsym` a symbol it can
                        // never reach). Struct/variant collisions are checked after the loop.
                        if is_reserved_name(&ef.name) {
                            self.error(
                                ef.span,
                                format!(
                                    "'{}' is a builtin/reserved name and cannot be an extern fn",
                                    ef.name
                                ),
                            );
                        }
                        extern_names.push((ef.name.clone(), ef.span));
                        if self.functions.contains_key(&ef.name) {
                            self.error(
                                ef.span,
                                format!("function '{}' is already defined", ef.name),
                            );
                        }
                        // `owned_str` is a return-only extern marshalling form — license its
                        // `resolve_type` arm only while resolving THIS signature's params + return
                        // (and the harvested C signature below), then reset so a bare non-extern use
                        // is rejected.
                        self.in_extern_sig = true;
                        let params: Vec<Ty> = ef
                            .params
                            .iter()
                            .map(|p| match &p.ty {
                                Some(t) => {
                                    // RETURN-ONLY surface forms (`owned_str`, `str?`/`owned_str?`)
                                    // must be rejected as PARAMS on the SURFACE Type, before
                                    // `resolve_type` collapses `owned_str` to a plain `Str` (which
                                    // would otherwise sail past `assert_marshallable`).
                                    if self.is_return_only_extern_type(t) {
                                        self.error(
                                            ef.span,
                                            format!(
                                                "type '{}' is not C-marshallable in extern fn '{}' \
                                                 (owned_str / str? are return-only)",
                                                describe_extern_type(t),
                                                ef.name
                                            ),
                                        );
                                    }
                                    let ty = self.resolve_type(t, ef.span);
                                    // A parameter must be a real C scalar — `nil` (void) is a
                                    // return-only sentinel and would panic the backend's `ctype_of`.
                                    // Deferred to the post-loop sweep (a by-value struct param may be
                                    // declared after this extern block).
                                    extern_marshal_checks.push((
                                        ty.clone(),
                                        ef.name.clone(),
                                        ef.span,
                                        false,
                                    ));
                                    ty
                                }
                                None => {
                                    self.error(
                                        ef.span,
                                        format!(
                                            "extern parameter '{}' needs a type annotation",
                                            p.name
                                        ),
                                    );
                                    Ty::Unknown
                                }
                            })
                            .collect();
                        let ret = match &ef.ret {
                            Some(t) => {
                                let ty = self.resolve_type(t, ef.span);
                                // The return slot may be `nil` (void) in addition to the C scalars.
                                // Deferred to the post-loop sweep (a by-value struct return may be
                                // declared after this extern block).
                                extern_marshal_checks.push((
                                    ty.clone(),
                                    ef.name.clone(),
                                    ef.span,
                                    true,
                                ));
                                ty
                            }
                            // A void extern returns nothing observable; model it as `Nil`.
                            None => Ty::Nil,
                        };
                        self.functions
                            .insert(ef.name.clone(), FnSig::plain(params, ret));
                        // ROOT FIX (fix4): harvest the FULLY-RESOLVED, width-bearing C signature for
                        // each extern fn, resolved here in THIS module's import/alias scope (the same
                        // scope `resolve_type` used to accept it). Both backends consume this instead
                        // of re-resolving alias names themselves — closing every spelling at once.
                        // Keyed by `(graph module index, fn name)`, the index both backends derive.
                        // Only built when `resolve_extern_signatures` drives the pass.
                        if let Some(midx) = self.extern_module_idx {
                            let cparams: Vec<Option<CType>> = ef
                                .params
                                .iter()
                                .map(|p| p.ty.as_ref().and_then(|t| self.resolve_ctype(t)))
                                .collect();
                            let cret = ef.ret.as_ref().and_then(|t| self.resolve_ctype(t));
                            self.extern_sigs.insert(
                                (midx, ef.name.clone()),
                                ExternCSig {
                                    params: cparams,
                                    ret: cret,
                                },
                            );
                        }
                        self.in_extern_sig = false;
                    }
                }
                StmtKind::Native(decl) => {
                    // `native fn`/`native ctor` is PRELUDE/STD-ONLY (a user program can't bind a name to
                    // a nonexistent intrinsic — a footgun). Reject it in a non-stdlib module; register
                    // its signature into `native_prelude_sigs` (the single source of truth for the eight
                    // migrated universe builtins) in a stdlib module. The name/first-classness metadata
                    // still lives in the hollow Rust `PRELUDE` table (read by the backends, which have no
                    // graph); this only moves the FnSig into the `.chz` decl (drift-guarded).
                    if !self.current_module_is_stdlib {
                        self.error(
                            decl.span,
                            "native fn/ctor declarations are only allowed in standard-library modules"
                                .to_string(),
                        );
                    } else {
                        self.register_native_decl(decl);
                    }
                }
                StmtKind::NativeStruct { span, .. } => {
                    // `native struct` is PRELUDE/STD-ONLY, the type-level analog of `native fn`/`native
                    // ctor` (a user program can't declare a native type whose layout the runtime doesn't
                    // know). Reject it in a non-stdlib module. In a stdlib module it is a no-op here: the
                    // ONLY native struct is regex.Match (in the file-backed `std/regex.chz`), whose
                    // SIGNATURE is harvested via `harvest_native_module` off the native-module arm — this
                    // hoist arm is reached only by non-native modules, so a native struct here is a user
                    // file → the guard always fires there.
                    if !self.current_module_is_stdlib {
                        self.error(
                            *span,
                            "native struct declarations are only allowed in standard-library modules"
                                .to_string(),
                        );
                    }
                }
                // `native enum` is PRELUDE/STD-ONLY, the ENUM analog of `native struct` (a user
                // program can't declare a reserved builtin enum's variant shape). Reject it in a
                // non-stdlib module (the guard). In a stdlib module it is a NO-OP (falls to `_`): the
                // ONLY native enums are `std/prelude.chz`'s `Option`/`Result`, whose variant SHAPE is
                // harvested as a DRIFT-GUARD MIRROR (see `harvest_native_enum_table`) and whose identity
                // stays the reserved `Ty::Option`/`Ty::Result`. Crucially it must NOT register into
                // `self.enums`/`enum_names` — that would mint a colliding nominal `Ty::Enum` and
                // silently break `?`/match; type identity stays 100% in `resolve_type`.
                StmtKind::NativeEnum { span, .. } if !self.current_module_is_stdlib => {
                    self.error(
                        *span,
                        "native enum declarations are only allowed in standard-library modules"
                            .to_string(),
                    );
                }
                _ => {}
            }
        }
        // Order-independent extern/registry collision sweep: a struct or enum variant registers a
        // same-named constructor the backends resolve before a plain call, so an extern sharing that
        // name is unreachable. Done after the loop so a `struct S`/`enum {Leaf}` declared *after* an
        // `extern fn S`/`fn Leaf` still collides (the maps are fully built by now). `extern_names` is
        // only populated on unix (the `#[cfg(unix)]` arm above); on other targets the extern was
        // already rejected wholesale, so the sweep is a no-op.
        for (name, span) in &extern_names {
            // Struct names and enum *variant* names register backend-resolved constructors; an
            // enum *type* name does not (it is not callable in either engine), so it is NOT a
            // collision — `extern fn Foo` alongside `enum Foo` resolves to the extern, exactly as
            // a plain `fn Foo` alongside `enum Foo` does.
            if self.structs.contains_key(name) || self.variant_owners.contains_key(name) {
                self.error(
                    *span,
                    format!("'{name}' is a builtin/reserved name and cannot be an extern fn"),
                );
            }
        }
        // Now every struct's field info is registered, so a by-value-struct param/return resolves its
        // fields regardless of whether the struct was declared before or after the extern block.
        for (ty, name, span, allow_void) in &extern_marshal_checks {
            self.assert_marshallable(ty, name, *span, *allow_void);
        }
    }

    /// Register one `native fn`/`native ctor` decl's signature into [`Checker::native_prelude_sigs`]
    /// (the single source of truth for the eight migrated universe builtins). Shared by the `hoist`
    /// pass (graph path) and [`seed_native_prelude_sigs`] (single-module `check` path). Applies the
    /// native-decl dynamic convention: an UNANNOTATED param → `Ty::Unknown`; no `-> ret` → `Ty::Unknown`
    /// return (native/never, how `panic` is spelled).
    pub(super) fn register_native_decl(&mut self, decl: &NativeDecl) {
        let params: Vec<Ty> = decl
            .params
            .iter()
            .map(|p| match &p.ty {
                // A variadic `...args: T` collapses to the slot type `List[T]` (mirrors `fn_sig` /
                // `harvest_native_fn_sig`), so `print`'s harvested sig reads as `fn(List[Any], str, str)`.
                Some(t) if p.is_variadic => Ty::List(Box::new(self.resolve_type(t, decl.span))),
                Some(t) => self.resolve_type(t, decl.span),
                None => Ty::Unknown,
            })
            .collect();
        let ret = match &decl.ret {
            Some(t) => self.resolve_type(t, decl.span),
            None => Ty::Unknown,
        };
        // A defaulted trailing param (`sep`/`end`) is optional; count how many so `min_params` is right.
        let optional = decl.params.iter().filter(|p| p.default.is_some()).count();
        let mut sig = if optional > 0 {
            FnSig::optional_tail(params, ret, optional)
        } else {
            FnSig::plain(params, ret)
        };
        sig.variadic = decl.params.iter().position(|p| p.is_variadic);
        self.native_prelude_sigs.insert(decl.name.clone(), sig);
    }

    /// Populate [`Checker::native_prelude_sigs`] from the always-linked `std/prelude.chz` on the
    /// SINGLE-MODULE `check` path (test-only — the graph path hoists the prelude module directly, which
    /// registers these during `hoist`). Without this, a single-module `check` (`ok()`/`check_src`) has
    /// no prelude in scope, so `ord`/`panic` value-position typing + hover would silently regress.
    /// Reads, parses, and registers only the file's `native` decls; the prelude imports nothing and its
    /// param/return types are reserved primitives (`str`/`int`/`bytes`/…), so no module scope is needed.
    #[cfg(test)]
    pub(super) fn seed_native_prelude_sigs(&mut self) {
        if !self.native_prelude_sigs.is_empty() {
            return;
        }
        // Same source chain as the resolver ($CHEZZI_STD → embedded) — no reader bypasses it.
        let Ok(src) = crate::resolver::std_source(&["std".to_string(), "prelude".to_string()])
        else {
            return;
        };
        let Ok(toks) = crate::lexer::tokenize(&src) else {
            return;
        };
        let Ok(module) = crate::parser::parse(toks) else {
            return;
        };
        for s in &module.stmts {
            if let StmtKind::Native(decl) = &s.kind {
                self.register_native_decl(decl);
            }
        }
        // Phase 5a-containers — the single-module `check` path never builds a graph, so the graph-capture
        // of the prelude's `List`/`Map`/`Set` method tables (into `container_seeds`, re-seeded by
        // `seed_stdlib_structs`) never runs here. Harvest them straight into `self.structs` (the same
        // consumption site) so `xs.push(...)`/`m.get(...)`/`s.add(...)` resolve on the graph-less path
        // exactly as on the graph path — the precise mirror of how this helper backfills the native-fn
        // sigs for the graph-less path. `begin_module` (which would re-clear+re-seed) is never called on
        // this path, so a direct insert survives to `check_module`.
        for tn in ["List", "Map", "Set"] {
            if let Some(info) = self.harvest_native_struct_table(&module, tn) {
                self.structs.insert(tn.to_string(), info);
            }
        }
    }

    /// Is this surface extern `Type` a RETURN-ONLY marshalling form (`owned_str`, `str?`, or
    /// `owned_str?`)? Checked on the SURFACE `Type` (pre-`resolve_type`) because `owned_str` collapses
    /// to a plain `Str` once resolved, losing its return-only-ness. (A plain `str?` param is also
    /// caught by `assert_marshallable`, but this gives it a clearer "return-only" message.)
    ///
    /// Transparent type aliases are resolved here, mirroring the backends' alias-resolving `ctype_of`:
    /// `type O = owned_str` makes a param `s: O` whose surface name is `O` (not `owned_str`) yet whose
    /// `ctype_of` is `CType::OwnedStr` — without alias resolution it would slip past this guard,
    /// type-check as a plain `Str`, then hit the return-only `unreachable!` param arm at runtime.
    pub(super) fn is_return_only_extern_type(&self, t: &Type) -> bool {
        self.is_return_only_extern_type_seen(t, &mut Vec::new())
    }

    /// `is_return_only_extern_type` with a shared `seen` set of alias names that spans the WHOLE
    /// recursion — including the `Named`→`Option`→`Named` re-entry. A single per-loop guard is not
    /// enough: a cyclic alias routed through an `Option`/`?` form (e.g. `type A = A?`) crosses the
    /// arm boundary, and without shared state each frame restarts with an empty set and recurses
    /// forever (stack overflow). The cycle itself is reported separately by `resolve_type`; here we
    /// just terminate cleanly and report "not return-only".
    pub(super) fn is_return_only_extern_type_seen(&self, t: &Type, seen: &mut Vec<String>) -> bool {
        match t {
            Type::Named { name: n, .. } => {
                if n == "owned_str" {
                    return true;
                }
                if seen.iter().any(|s| s == n) {
                    return false; // cycle — terminate; `resolve_type` diagnoses it
                }
                if let Some(aliased) = self.aliases.get(n) {
                    seen.push(n.clone());
                    return self.is_return_only_extern_type_seen(aliased, seen);
                }
                false
            }
            // `str?` / `owned_str?` parse to `Option[inner]`; the inner may itself be an alias.
            Type::Generic(n, args, ..) if n == "Option" => args.first().is_some_and(|inner| {
                matches!(inner, Type::Named { name: s, .. } if s == "str" || s == "owned_str")
                    || self.is_return_only_extern_type_seen(inner, seen)
            }),
            _ => false,
        }
    }

    /// v1 C-ABI marshallability: an extern fn's param/return types must be C-scalar — `int`, `float`,
    /// `bool`, or `str` (`char*`). `Nil` (void) is accepted ONLY for the return slot (`allow_void`),
    /// never for a parameter: a `nil` param has no `CType` lowering and would panic the backend's
    /// `ctype_of`, while a void-returning extern's `Nil` value would otherwise satisfy it. Everything
    /// else (list/map/set/tuple/struct/enum/func/option/result/protocol/channel/…) is rejected with a
    /// single uniform error. Called on the **resolved** `Ty` (after `resolve_type`), so a transparent
    /// alias to a scalar is accepted. `Unknown` is already-errored and silently allowed (no cascade).
    ///
    /// RETURN-ONLY (`allow_void`) additionally accepts `Option[str]` (surface `str?`): the nullable
    /// opt-in where a NULL `char*` lowers to `None` instead of faulting. (`owned_str` resolves to a
    /// plain `Str` and so needs no special case here — its return-only-ness is guarded on the surface
    /// `Type` in the extern param loop, before `resolve_type` collapses it.)
    pub(super) fn assert_marshallable(
        &mut self,
        ty: &Ty,
        fn_name: &str,
        span: Span,
        allow_void: bool,
    ) {
        let scalar = matches!(
            ty,
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Ptr | Ty::Unknown
        );
        let ok = scalar
            || (allow_void
                && (matches!(ty, Ty::Nil)
                    || matches!(ty, Ty::Option(inner) if matches!(**inner, Ty::Str))));
        if ok {
            return;
        }
        // A sync scalar callback (callbacks #4): a function-typed PARAM whose every param and its
        // return is a C scalar (`int`/`float`/`bool`/`ptr`; widths resolve to those). PARAM-ONLY — a
        // function-typed RETURN (`allow_void`) is rejected (no C marshalling for a returned function
        // pointer in v1). A non-scalar part (str/struct/nested callback/void return) falls through to
        // the uniform error below, which names the offending function type.
        if let Ty::Func { params, ret, .. } = ty
            && !allow_void
        {
            let part_ok =
                |t: &Ty| matches!(t, Ty::Int | Ty::Float | Ty::Bool | Ty::Ptr | Ty::Unknown);
            if params.iter().all(part_ok) && part_ok(ret) {
                return;
            }
        }
        // A flat-scalar struct BY VALUE: every field must itself be a marshallable C *scalar* (no
        // nested struct, no str/owned_str). Generic structs (non-empty type args) have no fixed C
        // layout — reject them. `visited` guards a struct cycling back through a field (defensive; a
        // struct field that is itself a struct is already rejected as nested). `Iterator` is a
        // built-in existential `Struct`, not a real POD — never marshallable.
        if let Ty::Struct(name, args) = ty
            && args.is_empty()
            && name != "Iterator"
            && self.structs.contains_key(name)
        {
            let mut visited = std::collections::HashSet::new();
            // The recursion emits field-level errors itself; either way return (no generic error).
            self.struct_fields_marshallable(name, fn_name, span, &mut visited);
            return;
        }
        self.error(
            span,
            format!(
                "type '{ty}' is not C-marshallable in extern fn '{fn_name}' \
                 (v1 supports only int, float, bool, str, ptr, and a flat struct of those)"
            ),
        );
    }

    /// Whether every field of struct `name` is a marshallable C *scalar* — the v1 by-value-struct
    /// rule (flat scalar fields only). On a non-scalar field (str/owned_str, a nested struct, a
    /// generic `Ty::Param`, a list/map/…) emits a clear error naming the struct AND the offending
    /// field, and returns `false`. `visited` breaks a (defensive) field-type cycle without overflow.
    pub(super) fn struct_fields_marshallable(
        &mut self,
        name: &str,
        fn_name: &str,
        span: Span,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if !visited.insert(name.to_string()) {
            self.error(
                span,
                format!(
                    "struct '{name}' is recursively defined and cannot be C-marshallable in extern \
                     fn '{fn_name}'"
                ),
            );
            return false;
        }
        // Clone the field list to drop the immutable borrow on `self` before emitting errors.
        let fields = match self.structs.get(name) {
            Some(info) => info.fields.clone(),
            None => return false,
        };
        let mut all_ok = true;
        for (fname, fty) in &fields {
            // Only true C *scalars* are valid struct fields (NOT `Str` — str by value is deferred).
            let ok = matches!(fty, Ty::Int | Ty::Float | Ty::Bool | Ty::Ptr | Ty::Unknown);
            if !ok {
                all_ok = false;
                self.error(
                    span,
                    format!(
                        "struct '{name}' field '{fname}' of type '{fty}' is not C-marshallable \
                         (extern structs require flat scalar fields; nested structs and str are not \
                         supported in v1) in extern fn '{fn_name}'"
                    ),
                );
            }
        }
        visited.remove(name);
        all_ok
    }
}
