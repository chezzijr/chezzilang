//! Checker: seeding the stdlib structs and harvesting native modules, enums and protocol shapes.
//! Split out of `src/checker/setup.rs`. `super::*` is the parent module.

use super::setup::variant_import_binds;
use super::setup::{TypeHead, TypeHeadKind};
use super::sig::TypePos;
use super::*;

impl Checker {
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
            defaulted_fields: Vec::new(),
            field_slots: None,
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
        // `resolve_type`'s reserved arm, which stays bound by an import (`builtin_binds`). The method table is
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
        // reserved arm, bound by an import (`builtin_binds`). The method table is reached only from a value
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
        // which stays bound by an import (`builtin_binds`). The method table is reached only from a
        // value whose `Ty` is already one of those, so a bare unimported annotation still errors.
        for (name, info) in &self.concurrency_seeds {
            self.structs.insert(name.clone(), info.clone());
        }
        // Phase 5a-containers — re-seed the always-linked prelude's `List`/`Map`/`Set` METHOD tables
        // (harvested from `std/prelude.chz`) under their bare names so `xs.push(...)`/`m.get(...)`/
        // `s.insert(...)` resolve via the normal method path (the `Ty::List`/`Ty::Map`/`Ty::Set` method arms
        // look the table up here, substituting the value's element/key/value type for the sig's
        // `Ty::Param`). Like `Socket`/`Shared` above — and UNLIKE `Ref` — NO `struct_names`/`bare_types`
        // licensing is added: the three resolve to the RESERVED `Ty::List`/`Ty::Map`/`Ty::Set` via
        // `resolve_type`'s reserved arms (universe types — always in scope). The `Builtin` origin means
        // `unique_member_owner`'s owner scan skips them (no mis-pin). The method table is reached only
        // from a value whose `Ty` is already a container, and the literal/ctor stay compiler-wired.
        for (name, info) in &self.container_seeds {
            self.structs.insert(name.clone(), info.clone());
        }
        self.seed_carrier_enums();
    }

    /// Phase 4b — harvest a FILE-BACKED native std module's whole SIGNATURE from its parsed in-module
    /// `native struct` / `native fn` decls into its [`ModuleSig`], REPLACING both the retired phase-4a
    /// companion stub and the hand-built `native_module_sig` regex arm. Used only for
    /// `std.regex` today (the resolver loads its real `std/regex.chz` while keeping the `native` marker;
    /// runtime member VALUES stay name-keyed via `native_members` — this supplies only the checker sig).
    ///
    /// Two passes so a `native fn`'s return type can reference the module's own `native struct` (regex's
    /// `find -> Result[Option[Match]]`): pass 1 harvests every `native struct` (into `sig.struct_defs`/
    /// `sig.types`, origin FORCED [`StructOrigin::Builtin`]) and TRANSIENTLY inserts each name into
    /// `self.struct_names` (so pass 2's `resolve_type` sees the bare type) AND transiently installs THIS
    /// module's harvested layout into `self.structs` (save+restore). The native branch's entry `begin_module` cleared `self.structs` and re-seeded the
    /// stdlib structs; the transient install puts THIS module's harvested layout in place of the seeded
    /// one. Without it, pass 2's
    /// `resolve_type` would key off that generic arity and spuriously reject with `type 'Match' expects 1
    /// type argument(s), got 0`. With it, the name resolves against its own (nparams-0) shape — byte-
    /// identical to the old hand-built `Ty::Struct("Match", vec![])` and immune to sibling-module /
    /// graph-traversal-order state. Pass 2 harvests every `native fn` sig via the native-decl dynamic
    /// convention (unannotated param → `Ty::Unknown`; no `-> ret` → `Ty::Unknown`). The transient
    /// `struct_names` inserts and `self.structs` overwrites are then REVERTED so IMPORT-GATING is
    /// preserved (a bare unimported `m: Match` still errors) and this arm leaves NO residue.
    /// Export a protocol's shape from its module: the name joins `sig.types` and the shape joins
    /// `sig.protocol_defs`. A file module and a native module both export through here.
    pub(super) fn export_protocol(
        sig: &mut ModuleSig,
        name: &str,
        info: ProtocolInfo,
        doc: Option<String>,
    ) {
        sig.types.insert(name.to_string());
        sig.protocol_defs
            .insert(name.to_string(), ProtocolSigInfo { info, doc });
    }

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
                    .map(|f| {
                        (
                            f.name.clone(),
                            self.resolve_type_at(&f.ty, *span, TypePos::Field),
                        )
                    })
                    .collect();
                self.exit_type_params(saved);
                self.note_field_widths(name, &harvested_fields);
                let info = StructInfo {
                    type_params: self.resolve_bounds(type_params, *span),
                    fields: harvested_fields,
                    // Methods are harvested in PASS 1b below (after every native struct name is
                    // transiently visible so a method return type can reference a sibling native
                    // struct, e.g. `Listener.accept -> Result[Socket]`).
                    methods: HashMap::new(),
                    origin: StructOrigin::Builtin,
                    doc: None,
                    defaulted_fields: Vec::new(),
                    field_slots: None,
                };
                sig.struct_defs.insert(name.clone(), info.clone());
                sig.types.insert(name.clone());
                // The native branch's entry `begin_module` cleared `self.structs` and re-seeded the stdlib
                // structs; the transient install puts THIS module's harvested layout in place of the
                // seeded one. PASS 2 below resolves this type's own name in the fns' return types
                // (`find -> Result[Option[Match]]`) via `resolve_type`,
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
        // PASS 1d — plain `struct` exports (TICKET-289; std.concurrency's `Task[T]`). A file-backed
        // native module may declare an ordinary struct with Chezzi methods beside its native ones.
        // It joins `sig.struct_defs` and `sig.types`, so `import Task from std.concurrency`
        // validates. The same transient installs as PASS 1 make its name resolvable in the native
        // structs' method sigs (`Executor.submit -> Task[T]`); PASS 3 below undoes them.
        let plain: Vec<crate::ast::Stmt> = ast
            .stmts
            .iter()
            .filter(|s| matches!(&s.kind, StmtKind::Struct { .. }))
            .cloned()
            .collect();
        let plain_names: Vec<String> = plain
            .iter()
            .filter_map(|s| {
                if let StmtKind::Struct { name, .. } = &s.kind {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();
        for name in &plain_names {
            saved_structs.push((name.clone(), self.structs.get(name).cloned()));
            saved_bare.push((
                name.clone(),
                self.bare_types.insert(name.clone(), name.clone()),
            ));
            if self.struct_names.insert(name.clone()) {
                transient.push(name.clone());
            }
        }
        self.hoist(&plain);
        for name in &plain_names {
            if let Some(info) = self.structs.get(name) {
                sig.struct_defs.insert(name.clone(), info.clone());
                sig.types.insert(name.clone());
            }
        }
        // PASS 1a — `native type` exports (TICKET-218; std.ffi's `ptr` and C widths). Each declared
        // name joins `sig.types`, so `import int32 from std.ffi` validates. The file's own sigs
        // (`native fn null() -> ptr`) resolve the name through the module's own builtin binds.
        for s in &ast.stmts {
            if let StmtKind::NativeType { name, .. } = &s.kind {
                sig.types.insert(name.clone());
            }
        }
        // PASS 1c — protocol exports (TICKET-250; std.concurrency's `StructuralEq`). A std protocol's
        // shape is already seeded from its declaration (`seed_std_protocols`), so the sig takes a
        // copy of that shape.
        for s in &ast.stmts {
            if let StmtKind::Protocol { name, .. } = &s.kind
                && let Some(info) = self.protocols.get(name).cloned()
            {
                Self::export_protocol(sig, name, info, None);
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
                let owner_args: Vec<Ty> = type_params
                    .iter()
                    .map(|tp| Ty::Param(tp.name.clone()))
                    .collect();
                let owner = self
                    .qualified_builtin_ty(name, &owner_args)
                    .unwrap_or_else(|| self.struct_self_ty(name));
                for m in bodied_methods {
                    let mut fsig = self.fn_sig(m, m.name_span, Some(&owner));
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
                sig.insert_fn(decl.name.clone(), fsig);
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
                sig.insert_fn(decl.name.clone(), self.fn_sig(decl, decl.name_span, None));
            }
        }
        // PASS 2c — module CONSTANTS (TICKET-218): a top-level `NAME: const T = value` let is a const
        // member of type `T`. Its value is bound by running the module's toplevel, and type-checked
        // against `T` in the graph loop's bodied branch. Any other top-level binding is rejected: a
        // std module exports no mutable global.
        for s in &ast.stmts {
            if let StmtKind::Let {
                target,
                ty,
                is_const,
                ..
            } = &s.kind
            {
                match (target.target_ident(), ty, is_const) {
                    (Some(name), Some(t), true) => {
                        let ty = self.resolve_type(t, s.span);
                        sig.members.insert(
                            name.to_string(),
                            MemberSig {
                                ty,
                                certain_fn: None,
                                is_const: true,
                            },
                        );
                    }
                    _ => self.error(
                        s.span,
                        "a standard-library module's top-level binding must be NAME: const T = value"
                            .to_string(),
                    ),
                }
            }
        }
        // Preserve import-gating: drop the transient bare-name visibility.
        for name in transient {
            self.struct_names.remove(&name);
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
                Some(t) if p.is_variadic => Ty::List(Box::new(self.resolve_type_at(
                    t,
                    decl.span,
                    TypePos::CParam,
                ))),
                Some(t) => self.resolve_type_at(t, decl.span, TypePos::CParam),
                None => Ty::Unknown,
            })
            .collect();
        let ret = match &decl.ret {
            Some(t) => self.resolve_type_at(t, decl.span, TypePos::CRet),
            None => Ty::Unknown,
        };
        // TICKET-202: resolve the bounds BEFORE the scope closes, while the enclosing native
        // struct's `[T]` is still entered (`List.sort`'s `where T: Comparable`).
        let where_bounds = self.resolve_bounds(&decl.where_bounds, decl.span);
        let own_tps = self.resolve_bounds(&decl.type_params, decl.span);
        self.exit_type_params(saved_tps);
        let variadic = decl.params.iter().skip(skip).position(|p| p.is_variadic);
        let mut sig = FnSig::plain(params, ret);
        sig.min_params = crate::ast::min_callable_params(&decl.params[skip..], false);
        // Carry the `where T: Bound` clause onto the harvested sig — for a native METHOD (e.g.
        // `List.sort`'s `where T: Comparable`) it is enforced at each call site by the container
        // method-dispatch arm (the `T` names the enclosing native struct's type param, in scope here).
        sig.where_bounds = where_bounds;
        // A native method's OWN `[U]` params land on the sig so it routes through the generic-method
        // inference path (`infer_generic_method`), not the fixed-arity path (empty for the common case).
        sig.type_params = own_tps;
        sig.variadic = variadic;
        // A native takes no named arguments (DEC-182) unless a parameter follows its variadic one
        // (`print(...args, sep, end)`): then `slots` carries the keyword-only names and defaults.
        sig.slots = crate::desugar::native_param_slots(decl, skip);
        sig.labels = decl
            .params
            .iter()
            .skip(skip)
            .map(|p| Some(p.name.clone()))
            .collect();
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
                    type_params: self.resolve_bounds(type_params, s.span),
                    fields: Vec::new(),
                    methods: table,
                    origin: StructOrigin::Builtin,
                    doc: None,
                    defaulted_fields: Vec::new(),
                    field_slots: None,
                });
            }
        }
        None
    }

    /// Harvest every `native enum` of a parsed module (the prelude's `Option[T]` / `Result[T, E]`):
    /// its name, its type parameters and its variants in declaration order, each payload resolved
    /// with the enum's type parameters in scope (`Some(T)` holds `Ty::Param("T")`). These decls ARE
    /// the registered carrier enums: [`Checker::seed_carrier_enums`] puts them into the ordinary
    /// enum tables, and every variant, pattern and exhaustiveness reader answers from there.
    pub(super) fn harvest_native_enums(&mut self, ast: &crate::ast::Module) -> CarrierSeeds {
        let mut out = Vec::new();
        for s in &ast.stmts {
            if let StmtKind::NativeEnum {
                name,
                type_params,
                variants,
                ..
            } = &s.kind
            {
                let saved = self.enter_type_params(type_params);
                let variants = variants
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
                self.exit_type_params(saved);
                let type_params = self.resolve_bounds(type_params, s.span);
                out.push(NativeEnumSeed {
                    name: name.clone(),
                    type_params,
                    variants,
                });
            }
        }
        CarrierSeeds {
            enums: out,
            imports: variant_import_binds(&ast.stmts)
                .into_iter()
                .map(|(bind, en, variant, _)| (bind, en, variant))
                .collect(),
        }
    }

    /// Register the prelude's carrier enums (`carrier_seeds`) in the ordinary enum tables of the
    /// current module, under their bare names, and bind the variant names the prelude imports
    /// from them. A carrier declares no method, so its method table is empty.
    pub(super) fn seed_carrier_enums(&mut self) {
        let seeds = std::mem::take(&mut self.carrier_seeds);
        self.seed_carrier_enums_from(&seeds);
        self.carrier_seeds = seeds;
    }

    fn seed_carrier_enums_from(&mut self, seeds: &CarrierSeeds) {
        // The carriers behind `T?` and `T!E` are the prelude's own. A user module reaches them only
        // through that syntax: it gets no bare enum name, no bare variant owner, and the variant binds
        // only under their private spelling (`ast::private_name`, which desugar writes), except a
        // keyword (`None`). A std module (the prelude's own signatures) spells them by name.
        let public = self.current_module_is_stdlib;
        for seed in &seeds.enums {
            let key = seed.name.clone();
            let mut names = Vec::new();
            for (vname, payload) in &seed.variants {
                names.push(vname.clone());
                self.variants.insert(
                    (key.clone(), vname.clone()),
                    VariantInfo {
                        enum_name: key.clone(),
                        payload: payload.clone(),
                    },
                );
                let owners = self.variant_owners.entry(vname.clone()).or_default();
                owners.retain(|k| *k != key);
                if public {
                    owners.push(key.clone());
                }
                if owners.is_empty() {
                    self.variant_owners.remove(vname);
                }
            }
            self.enums.insert(key.clone(), names);
            self.enum_type_params
                .insert(key.clone(), seed.type_params.clone());
            self.enum_methods.insert(key.clone(), HashMap::new());
            if public {
                self.enum_names.insert(key);
            } else {
                self.enum_names.remove(&key);
            }
        }
        for (bind, en, variant) in &seeds.imports {
            // The carrier's key is its bare name (a std type keeps it); the head is built from it,
            // since a user module does not see the bare name.
            let head = TypeHead {
                kind: TypeHeadKind::Enum,
                key: en.clone(),
                name: en.clone(),
                spelled: en.clone(),
                pinned: None,
                native_handle: false,
            };
            let mut binds = vec![crate::ast::private_name(bind)];
            if public || crate::lexer::is_keyword(bind) {
                binds.push(bind.clone());
            } else if self
                .imported_variants
                .get(bind)
                .is_some_and(|iv| iv.prelude)
            {
                // A seed for a std module left the public bind; a user module does not see it.
                self.imported_variants.remove(bind);
            }
            for b in binds {
                self.imported_variants.insert(
                    b,
                    ImportedVariant {
                        head: head.clone(),
                        variant: variant.clone(),
                        prelude: true,
                    },
                );
            }
        }
    }

    /// Harvest the SHAPE of a `protocol` declaration (its `type_params`, `embeds` and method `FnSig`s)
    /// into a value, with the same `Self` and own-type-param scope [`Checker::hoist_protocol`] uses.
    /// `None` for a statement that is not a protocol. The caller inserts the result.
    pub(super) fn harvest_protocol_stmt(&mut self, s: &crate::ast::Stmt) -> Option<ProtocolInfo> {
        let StmtKind::Protocol {
            type_params,
            methods,
            embeds,
            ..
        } = &s.kind
        else {
            return None;
        };
        // Same scope hoist_protocol builds: swap in a clean map with only `Self` + the
        // protocol's own type params visible, resolve, then restore.
        let mut saved = self.type_params.clone();
        std::mem::swap(&mut self.type_params, &mut saved);
        self.type_params.insert("Self".to_string(), Vec::new());
        let resolved = self.resolve_bounds(type_params, s.span);
        for rtp in resolved {
            self.type_params.insert(rtp.name, rtp.bounds);
        }
        let sigs = self.protocol_method_sigs(methods, s.span);
        // TICKET-202: embeds resolve here, while the protocol's own params are in scope.
        let embeds: Vec<TyBound> = embeds
            .iter()
            .map(|b| self.resolve_bound(b, s.span))
            .collect();
        self.type_params = saved;
        Some(ProtocolInfo {
            type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
            methods: sigs,
            embeds,
        })
    }

    /// Fill `self.protocols` from the `protocol` declarations in `std/*.chz` (`std_protocol_decls`),
    /// the one source of which protocols exist and their shapes. Two passes: the first inserts every
    /// shape, the second re-harvests so an embed declared later in the file order (`Num` embeds
    /// `Comparable`) resolves. Runs as stdlib, so the declarations resolve names no user module sees.
    pub(super) fn seed_std_protocols(&mut self) {
        let outer_std = std::mem::replace(&mut self.current_module_is_stdlib, true);
        for pass in 0..2 {
            // The first pass meets an embed whose protocol is not inserted yet; the second pass
            // re-resolves it, so the first pass's diagnostics are dropped.
            let mark = self.diag_mark();
            for d in super::graph_pass::std_protocol_decls() {
                if let Some(info) = self.harvest_protocol_stmt(&d.stmt) {
                    self.protocols.insert(d.name.clone(), info);
                }
            }
            if pass == 0 {
                self.diag_rollback(mark);
            }
        }
        self.current_module_is_stdlib = outer_std;
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
        if self.std_internal_hidden(method) {
            return None;
        }
        let info = self.structs.get(ty_name)?;
        let sig = info.methods.get(method)?;
        if targs.is_empty() || info.type_params.is_empty() {
            // Non-generic handle (Socket/Listener/Executor), or no element type to substitute — the
            // stored sig is already concrete (identity subst; behavior-preserving for net).
            return Some(sig.clone());
        }
        let map = struct_param_map(info, targs);
        Some(FnSig {
            ..instantiate_method(sig, &map)
        })
    }
}
