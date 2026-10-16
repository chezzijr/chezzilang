//! Checker: collecting names, docs and widths, hoisting declarations, native decls, the native
//! prelude, marshalling checks.
//! Split out of `src/checker/setup.rs`. `super::*` is the parent module.

use super::sig::TypePos;
use super::*;

impl Checker {
    /// First sub-pass: learn every struct/enum *name* so `resolve_type` can recognize them even
    /// when used before their definition (or inside each other).
    pub(super) fn collect_names(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                // A second declaration of a type name is reported by `GlobalBinding::clashes`.
                StmtKind::Struct { name, .. } => {
                    self.struct_names.insert(name.clone());
                }
                StmtKind::Enum { name, .. } => {
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
                StmtKind::TypeAlias { name, ty, .. } => {
                    // Only keywords are reserved: an alias named after a builtin type shadows it in
                    // this module (`tests/shadowing_grid.rs`).
                    if !self.aliases.contains_key(name) {
                        self.aliases.insert(name.clone(), ty.clone());
                        // TICKET-262: a width type describes the C boundary only, and an alias body
                        // expands at any use, so an alias body never carries one.
                        let mut widths = Vec::new();
                        Self::collect_width_names(ty, &mut widths);
                        for w in widths {
                            self.error(s.span, super::sig::width_outside_c_msg(&w));
                        }
                    }
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Protocol { .. }
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
                | StmtKind::Expr(_) => {}
            }
        }
    }

    /// Collect doc-comments for the module's top-level NON-fn declarations (struct/enum/protocol/
    /// type-alias names + top-level `let` bindings) into `name_docs`, keyed by simple name.
    /// Free fns and methods carry their doc on `FnSig::doc` instead (handled in `fn_sig`). Purely for
    /// LSP hover — never consulted by checking/codegen, so it is behavior-neutral.
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
                | StmtKind::TypeAlias {
                    name, doc: Some(d), ..
                } => {
                    self.name_docs.insert(name.clone(), d.clone());
                }
                // A top-level `let` binds (possibly multiple) names; surface the doc on each.
                StmtKind::Let {
                    target,
                    doc: Some(d),
                    ..
                } => {
                    for n in target.binder_names() {
                        self.name_docs.insert(n, d.clone());
                    }
                }
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
                | StmtKind::Expr(_) => {}
            }
        }
    }

    /// Collect every fixed-width FFI type name (`int8`..`uint64`) referenced anywhere in `ty`,
    /// recursing through composites (`Generic` args, `Func` params/return, `Tuple` elements). Used
    /// to license a width-alias only when its defining module imported all the widths it embeds.
    pub(super) fn collect_width_names(ty: &Type, out: &mut Vec<String>) {
        match ty {
            Type::Nil(_) => {}
            Type::Named { name: n, .. } => {
                if crate::native::ffi::is_width(n) {
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
            // A module-qualified head can be a width (`ffi.int8`); its type arguments are walked too.
            Type::Qualified { name, args, .. } => {
                if crate::native::ffi::is_width(name) {
                    out.push(name.clone());
                }
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
        // TICKET-202 — resolve every embed's args once, in its own protocol's scope, now that a
        // forward protocol name resolves, and before any reader (validation included) runs.
        for s in stmts {
            if let StmtKind::Protocol {
                name,
                type_params,
                embeds,
                ..
            } = &s.kind
            {
                self.resolve_protocol_embeds(name, type_params, embeds, s.span);
            }
        }
        // M22 — validate embeds in a SECOND pass, now that every protocol is registered, so a forward
        // (or cyclic) embed reference resolves. Collision/cycle detection lives here (declare-time,
        // authoritative — before any satisfaction check can recurse into a cycle).
        for s in stmts {
            if let StmtKind::Protocol {
                name,
                type_params,
                methods,
                embeds,
                ..
            } = &s.kind
            {
                self.validate_protocol_embeds(name, type_params, methods, embeds, s.span);
            }
        }
        // Extern param/return marshallability is validated AFTER this loop (collected here), so a
        // struct passed/returned BY VALUE may be DECLARED AFTER the extern block: `self.structs`
        // (field info, which `assert_marshallable` inspects for a flat-scalar struct) is only fully
        // populated once every struct in the module has been hoisted. `collect_names` already
        // pre-registered struct *names*, so `resolve_type` accepts the forward reference inline.
        // TICKET-250 (D4): the same holds for every conformance question a type spelling asks
        // (`Set[Node]` inside `Node`, `Map[R, _]` before `R`, `Box[A]` with a bounded `Box`).
        // Each is deferred here and discharged at the end of this function.
        let outer_obligations = self.deferred_obligations.replace(Vec::new());
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    // Only keywords are reserved: a fn named after a builtin fn or constructor (`ord`,
                    // `range`, `Range`) shadows it in this module; the name resolves to the user fn
                    // first (`resolve::classify_ident`). A second declaration of the name is
                    // reported by `GlobalBinding::clashes`.
                    let sig = self.fn_sig(decl, s.span, None);
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
                    // Only keywords are reserved: a struct named after a builtin type shadows it in
                    // this module (the hoist insert below overwrites a pre-seeded stdlib layout). A
                    // second declaration of the name is reported by `GlobalBinding::clashes`.
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
                    let field_slots =
                        crate::desugar::field_slots(fields, s.span.file, name, type_params);
                    let defaulted_fields: Vec<String> = fields
                        .iter()
                        .filter(|f| f.default.is_some())
                        .map(|f| f.name.clone())
                        .collect();
                    let fields: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                self.resolve_type_at(&f.ty, s.span, TypePos::Field),
                            )
                        })
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
                    let saved_self = self.current_self_ty.replace(self_ty.clone());
                    let methods = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span, Some(&self_ty))))
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
                    self.note_field_widths(&key, &fields);
                    let rtps = self.resolve_bounds(type_params, s.span);
                    self.structs.insert(
                        key,
                        StructInfo {
                            // TICKET-027 + TICKET-202: each stored bound is keyed and its args
                            // resolved in the declaring scope.
                            type_params: rtps,
                            fields,
                            methods,
                            origin,
                            // Decl docstring is attached later in `capture_sig` (from `name_docs`),
                            // only for the module's exported sig; the in-checker layout doesn't need it.
                            doc: None,
                            defaulted_fields,
                            field_slots: Some(field_slots),
                            self_writers: HashSet::new(),
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
                    // The LAYOUT tables (`enums`/`variants`/`enum_type_params`) are keyed by this
                    // module's runtime key (bare unless disambiguated), so a value's `Ty::Enum(key)`
                    // resolves its variants here and across module boundaries. `enum_names`/
                    // `variant_owners` (bare-visibility + qualify-hint) stay bare.
                    let key = self.bare_key(name);
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
                                v.name_span,
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
                    let saved_self = self.current_self_ty.replace(self_ty.clone());
                    let method_sigs: HashMap<String, FnSig> = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span, Some(&self_ty))))
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
                    // TICKET-027 + TICKET-202: keyed bounds, args resolved in the declaring scope.
                    let rtps = self.resolve_bounds(type_params, s.span);
                    self.enum_type_params.insert(key.clone(), rtps);
                    self.enum_methods.insert(key, method_sigs);
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
                        // An extern fn named after a builtin shadows it, as a `fn` does; a second
                        // declaration of the name is reported by `GlobalBinding::clashes`.
                        // `owned_str` is a return-only extern marshalling form — license its
                        // `resolve_type` arm only while resolving THIS signature's params + return
                        // (and the harvested C signature below), then reset so a bare non-extern use
                        // is rejected.
                        self.in_extern_sig = true;
                        self.receiver_or_report(&ef.params, None, ef.span);
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
                                    let ty = self.resolve_type_at(t, ef.span, TypePos::CParam);
                                    // A parameter must be a real C scalar — `nil` (void) is a
                                    // return-only sentinel and would panic the backend's `ctype_of`.
                                    // Deferred to the post-loop sweep (a by-value struct param may be
                                    // declared after this extern block).
                                    self.conformance_obligation(Obligation::ExternMarshal {
                                        ty: ty.clone(),
                                        name: ef.name.clone(),
                                        span: ef.span,
                                        allow_void: false,
                                    });
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
                                let ty = self.resolve_type_at(t, ef.span, TypePos::CRet);
                                // The return slot may be `nil` (void) in addition to the C scalars.
                                // Deferred to the post-loop sweep (a by-value struct return may be
                                // declared after this extern block).
                                self.conformance_obligation(Obligation::ExternMarshal {
                                    ty: ty.clone(),
                                    name: ef.name.clone(),
                                    span: ef.span,
                                    allow_void: true,
                                });
                                ty
                            }
                            // A void extern returns nothing observable; model it as `Nil`.
                            None => Ty::Nil,
                        };
                        let mut sig = FnSig::plain(params, ret);
                        sig.c_variadic = ef.c_variadic;
                        self.functions.insert(ef.name.clone(), sig);
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
                                    c_variadic: ef.c_variadic,
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
                    // migrated universe builtins) in a stdlib module. The id, lowering and first-classness
                    // live in the `Builtin` table (`base/builtin.rs`, read by the backends, which have
                    // no graph); this only moves the FnSig into the `.chz` decl (drift-guarded).
                    if !self.current_module_is_stdlib {
                        self.error(
                            decl.span,
                            "native fn/ctor declarations are only allowed in standard-library modules"
                                .to_string(),
                        );
                    } else {
                        self.register_native_decl(decl);
                        // W7-8 — a HYBRID std module's bodied fn may CALL a `native fn` sibling
                        // (`std.fs`'s `exists` wrapper calls `_exists`). The graph path repopulates
                        // `functions` from the harvested `ModuleSig`, but the SINGLE-MODULE path
                        // (standalone `chezzi check <std file>`) only had `native_prelude_sigs`, which
                        // bare-name resolution does not consult — the call read `unknown name`.
                        // Register the sig as an ordinary module function so both paths agree.
                        // Skipped for a `native ctor` (not first-class), for a universe builtin (its
                        // dispatch is its `Builtin` id's, not a module function's).
                        if decl.kind == crate::ast::NativeKind::Fn
                            && crate::checker::prelude_fn(&decl.name).is_none()
                        {
                            let fsig = self.harvest_native_fn_sig(decl, false);
                            self.functions.insert(decl.name.clone(), fsig);
                        }
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
                // `native enum` is PRELUDE/STD-ONLY: reject it in a non-stdlib module. In a stdlib
                // module this arm does nothing; the prelude's `Option` / `Result` decls ARE the
                // registered carrier enums, harvested by `harvest_native_enums` and put into the
                // enum tables of every module by `seed_carrier_enums`.
                StmtKind::NativeEnum { span, .. } if !self.current_module_is_stdlib => {
                    self.error(
                        *span,
                        "native enum declarations are only allowed in standard-library modules"
                            .to_string(),
                    );
                }
                // TICKET-218 — `native type` is STD-ONLY. In a std module its underlying type is
                // `int`/`float` (a C width) or absent (an opaque handle), and the declaring file sees
                // its own type (the single-module `chezzi check std/ffi.chz` path; the graph path
                // licenses it in `harvest_native_module`).
                StmtKind::NativeType {
                    name,
                    underlying,
                    span,
                    ..
                } => {
                    if !self.current_module_is_stdlib {
                        self.error(
                            *span,
                            "native type declarations are only allowed in standard-library modules"
                                .to_string(),
                        );
                    } else {
                        let scalar = match underlying {
                            None => true,
                            Some(Type::Named { name, .. }) => name == "int" || name == "float",
                            Some(_) => false,
                        };
                        if !scalar {
                            self.error(
                                *span,
                                "a native type's underlying type must be int or float".to_string(),
                            );
                        }
                        self.builtin_binds.insert(name.clone(), name.clone());
                    }
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Protocol { .. }
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
                | StmtKind::NativeEnum { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
            }
        }
        // M24-2 — index the MEMBER names some declaration takes a witness for BEFORE either loop
        // below. It is the NECESSARY half of the member-forward charge (`witness_member_names`): a
        // `recv.m(x)` whose name is nowhere declared witness-taking is a builtin or a plain method
        // and can forward nothing. Reading the DECLARATION rather than the derived `witness_params`
        // is what makes it legal here — it needs no fixpoint of its own, so the free-fn loop can
        // consult it while method charges are still unfinished.
        for s in stmts {
            let methods = match &s.kind {
                StmtKind::Struct { methods, .. } | StmtKind::Enum { methods, .. } => methods,
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Protocol { .. }
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
                | StmtKind::Expr(_) => continue,
            };
            for m in methods {
                if !self.static_bounded_type_params(m).is_empty() {
                    self.witness_member_names.insert(m.name.clone());
                }
            }
        }
        // M24 slice 2 — witness FORWARDING makes "does this fn need hidden witness params" depend on
        // its CALLEES' answers, and a fn is hoisted before them. Re-run the ONE derivation
        // (`witness_params_of`) over this module's free fns to a fixpoint, so the answer is
        // order-independent (`a` → `b` → `c` charges `a` even with `c` declared last, and mutual
        // recursion converges).
        //
        // A pass ADDS and REMOVES charges — it is NOT monotone, and the seed is not a lower bound.
        // `fn_sig` ran mid-hoist against tables that still lacked every declaration below the fn, so
        // it is wrong in both directions: a later-declared callee was not yet in `self.functions`
        // (under-charge), and a later-declared struct was not yet in `self.structs`, so
        // `Checker::concrete_ctor_head`'s shape lookup missed and a concrete-argument forward read as
        // a real one (over-charge). Pass 1 sees the complete tables and DROPS that over-charge —
        // measured: `fn a[T: Default](x: T) -> int: return b(Later(1))` beside a witness-taking
        // `fn b[T: Default]`, with `struct Later` declared LAST, is accepted as a function value,
        // which happens only because the charge `fn_sig` installed is removed here.
        //
        // What makes it settle: from pass 1 on, every input `witness_params_of` reads EXCEPT the
        // witness sets is frozen (structs, protocols, imports, and each signature's params/ret), and
        // in the witness sets alone the derivation only grows — a callee that gains a charge can make
        // `call_forwards_a_witness` answer yes for its callers, never stop it — over a finite lattice
        // (each fn's charge ⊆ its own static-bounded type params). The `fn_decls.len()` bound is the
        // unconditional backstop, not the argument: were some future edit to break that one-way
        // property too, this loop would still EXIT — with the last pass's state, which need not be a
        // fixpoint — rather than spin.
        let fn_decls: Vec<&crate::ast::FnDecl> = stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Fn(d) => Some(d),
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
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
            })
            .collect();
        for _ in 0..fn_decls.len() {
            let mut changed = false;
            for d in &fn_decls {
                let w = self.witness_params_of(d);
                if let Some(sig) = self.functions.get_mut(&d.name)
                    && sig.witness_params != w
                {
                    sig.witness_params = w;
                    // `min_params` is DERIVED from the witness set, so it is re-derived with it. The
                    // seed `fn_sig` computed is mid-hoist, and this loop adds AND removes charges, so
                    // leaving it behind means the checker's accepted arity and the compiler's
                    // `Proto::min_arity` — which reads the FINAL set through the same
                    // `min_callable_params` — disagree by construction. That is the divergence the
                    // one-source-of-truth predicate exists to prevent.
                    sig.min_params =
                        crate::ast::min_callable_params(&d.params, !sig.witness_params.is_empty());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // M24 Task 5 — a MEMBER's own `[T]` can be witnessed too, and its forwarding charge reads the
        // free fns' answers, which the fixpoint above just finalized. ONE re-pass is enough (no second
        // fixpoint): a method is never a forwarding TARGET — it cannot be called by bare name — so no
        // member's answer depends on another member's.
        for s in stmts {
            let (methods, key) = match &s.kind {
                StmtKind::Struct { name, methods, .. } | StmtKind::Enum { name, methods, .. } => {
                    (methods, self.bare_key(name))
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Protocol { .. }
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
                | StmtKind::Expr(_) => continue,
            };
            for m in methods {
                let w = self.witness_params_of(m);
                let slot = match &s.kind {
                    StmtKind::Struct { .. } => self
                        .structs
                        .get_mut(&key)
                        .and_then(|s| s.methods.get_mut(&m.name)),
                    StmtKind::Let { .. }
                    | StmtKind::Assign { .. }
                    | StmtKind::Fn(_)
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
                    | StmtKind::Expr(_) => self
                        .enum_methods
                        .get_mut(&key)
                        .and_then(|ms| ms.get_mut(&m.name)),
                };
                if let Some(sig) = slot {
                    sig.witness_params = w;
                    // …and its derived arity floor with it (see the free-fn loop above).
                    sig.min_params =
                        crate::ast::min_callable_params(&m.params, !sig.witness_params.is_empty());
                }
            }
        }
        // Now every type of the module is inserted: discharge what the loop deferred, each in the
        // type-parameter scope it was asked in.
        let deferred = std::mem::replace(&mut self.deferred_obligations, outer_obligations);
        for (ob, scope) in deferred.unwrap_or_default() {
            let saved = std::mem::replace(&mut self.type_params, scope);
            self.discharge_obligation(ob);
            self.type_params = saved;
        }
    }

    /// Register one `native fn`/`native ctor` decl's signature into [`Checker::native_prelude_sigs`]
    /// (the single source of truth for the eight migrated universe builtins). Shared by the `hoist`
    /// pass (graph path) and [`seed_native_prelude_sigs`] (single-module `check` path). Applies the
    /// native-decl dynamic convention: an UNANNOTATED param → `Ty::Unknown`; no `-> ret` → `Ty::Unknown`
    /// return (native/never, how `panic` is spelled).
    pub(super) fn register_native_decl(&mut self, decl: &NativeDecl) {
        let sig = self.harvest_native_fn_sig(decl, false);
        self.native_prelude_sigs.insert(decl.name.clone(), sig);
    }

    /// The parsed `std/prelude.chz`, for the test-only paths that check a program with no prelude
    /// module in a graph; `seed_native_prelude_sigs` is their one seed.
    /// Same source chain as the resolver ($CHEZZI_STD → embedded) — no reader bypasses it.
    #[cfg(any(test, feature = "test-api"))]
    fn parse_prelude_source() -> Option<crate::ast::Module> {
        let src = crate::resolver::std_source(&["std".to_string(), "prelude".to_string()]).ok()?;
        let toks = crate::lexer::tokenize(&src).ok()?;
        let mut module = crate::parser::parse(toks).ok()?;
        // A std file never probes the project root (as `Resolver::parse` classifies it).
        crate::ast::classify_variant_imports(&mut module, &|_| false);
        Some(module)
    }

    /// Populate [`Checker::native_prelude_sigs`] from the always-linked `std/prelude.chz` on the
    /// SINGLE-MODULE `check` path (test-only — the graph path hoists the prelude module directly, which
    /// registers these during `hoist`). Without this, a single-module `check` (`ok()`/`check_src`) has
    /// no prelude in scope, so `ord`/`panic` value-position typing + hover would silently regress.
    /// Reads, parses, and registers only the file's `native` decls; the prelude imports nothing and its
    /// param/return types are reserved primitives (`str`/`int`/`bytes`/…), so no module scope is needed.
    #[cfg(any(test, feature = "test-api"))]
    pub(super) fn seed_native_prelude_sigs(&mut self) {
        if !self.native_prelude_sigs.is_empty() {
            return;
        }
        let Some(module) = Self::parse_prelude_source() else {
            return;
        };
        // The carriers come first: the native sigs and container tables below name `Option[T]` /
        // `Result[T, E]`, and those resolve through the enum tables. This reads the prelude, a std
        // module, so the carriers' names are in scope for it.
        let outer_std = std::mem::replace(&mut self.current_module_is_stdlib, true);
        self.carrier_seeds = self.harvest_native_enums(&module);
        self.seed_carrier_enums();
        self.seed_range_struct(&module);
        for s in &module.stmts {
            if let StmtKind::Native(decl) = &s.kind {
                self.register_native_decl(decl);
            }
        }
        // Phase 5a-containers — the single-module `check` path never builds a graph, so the graph-capture
        // of the prelude's `List`/`Map`/`Set`/`Channel`/`str`/`bytes`/`bytearray` method tables (into
        // `container_seeds`, re-seeded by `seed_stdlib_structs`) never runs here. Harvest them straight
        // into `self.structs` (the same consumption site) so `xs.push(...)`/`m.get(...)`/`s.add(...)`/
        // `ch.send(...)`/`s.upper(...)`/`b.decode(...)` resolve on the graph-less path exactly as on the
        // graph path — the precise mirror of how this helper backfills the native-fn sigs for the
        // graph-less path. `begin_module` (which would re-clear+re-seed) is never called on this path, so
        // a direct insert survives to `check_module`.
        for tn in ["List", "Map", "Set", "Channel", "str", "bytes", "bytearray"] {
            if let Some(info) = self.harvest_native_struct_table(&module, tn) {
                // `begin_module` re-seeds from `container_seeds`, so the test-only graph with no
                // prelude module (`standalone_graph`) keeps these tables too.
                self.container_seeds.insert(tn.to_string(), info.clone());
                self.structs.insert(tn.to_string(), info);
            }
        }
        self.adopt_reserved_protocol_labels(&module);
        self.current_module_is_stdlib = outer_std;
        // Re-seed under the caller's module, which may be a user module.
        self.seed_carrier_enums();
    }

    /// TICKET-253 — `Range` is a struct of the prelude that every module sees with no `import`
    /// (the resolver adds `import Range from std.prelude` on the graph path). The graph-less path
    /// hoists its declaration straight into this checker so `Range(0, 3)` resolves here too.
    #[cfg(any(test, feature = "test-api"))]
    pub(super) fn seed_range_struct(&mut self, module: &crate::ast::Module) {
        let range_decl: Vec<Stmt> = module
            .stmts
            .iter()
            .filter(|s| matches!(&s.kind, StmtKind::Struct { name, .. } if name == "Range"))
            .cloned()
            .collect();
        let outer = std::mem::replace(&mut self.current_module_is_stdlib, true);
        self.hoist(&range_decl);
        self.current_module_is_stdlib = outer;
        if let Some(info) = self.structs.get("Range").cloned() {
            self.bind_imported_struct_name("Range", "Range", &info, true);
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
                if self.bound_builtin(n) == Some("owned_str") {
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
            Type::Generic(n, args, ..) if crate::ast::unprivate(n) == "Option" => {
                args.first().is_some_and(|inner| {
                    matches!(inner, Type::Named { name: s, .. }
                        if s == "str" || self.bound_builtin(s) == Some("owned_str"))
                        || self.is_return_only_extern_type_seen(inner, seen)
                })
            }
            Type::Qualified { module, name, .. } => {
                name == "owned_str" && self.module_owns_builtin(module, name)
            }
            Type::Generic(..) | Type::Func { .. } | Type::Tuple(_) | Type::Nil(_) => false,
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
            ty.scalar(),
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Ptr | Ty::Unknown
        );
        let ok = scalar
            || (allow_void
                && (matches!(ty, Ty::Nil)
                    || matches!(ty, Ty::Option(inner) if matches!(**inner, Ty::Str))));
        if ok {
            return;
        }
        // A sync scalar callback (callbacks #4): a function-typed PARAM whose every param is a C
        // scalar (`int`/`float`/`bool`/`ptr`; widths resolve to those). PARAM-ONLY — a function-typed
        // RETURN (`allow_void`) is rejected (no C marshalling for a returned function pointer in v1).
        // A non-scalar part (str/struct/nested callback) falls through to the uniform error below,
        // which names the offending function type.
        //
        // The callback's own RETURN slot additionally accepts `nil` (void) — mirroring the top-level
        // `allow_void` split above, but scoped to the return only: a `nil` callback PARAM has no
        // `CType` lowering and would panic the backend's `ctype_of`, so it stays rejected.
        // A variadic fn type (`fn(...int)`, a C `...`) has no C callback shape, so it falls through too.
        if let Ty::Func {
            params,
            ret,
            variadic: crate::ast::Variadic::No,
            ..
        } = ty
            && !allow_void
        {
            let part_ok = |t: &Ty| {
                matches!(
                    t.scalar(),
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Ptr | Ty::Unknown
                )
            };
            if params.iter().all(part_ok) && (part_ok(ret) || matches!(**ret, Ty::Nil)) {
                return;
            }
        }
        // A struct BY VALUE: every field must be a C scalar or a C-marshallable struct, recursively
        // (no str/owned_str). Generic structs (non-empty type args) have no fixed C layout — reject
        // them. `visited` rejects a struct cycling back through its own fields. `Iterator` is a
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
                 (v1 supports only int, float, bool, str, ptr, and a struct of those)"
            ),
        );
    }

    /// Whether struct `name` is C-marshallable by value — the one recursive rule: every field is a C
    /// scalar or itself a C-marshallable (non-generic) struct, to any depth. On any other field
    /// (str/owned_str, a generic `Ty::Param`, a list/map/…) emits a clear error naming the struct AND
    /// the offending field, and returns `false`. A ZERO-field struct is rejected outright (libffi
    /// cannot build a CIF for an empty aggregate — it panics). `visited` is the current struct path:
    /// a struct reached again through its own fields is recursively defined and rejected.
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
        // A ZERO-field struct has no C counterpart — C has no empty struct (GCC/Clang give it size 1
        // as an extension) and libffi's `prep_cif` errors `Typedef` on an empty element list, which
        // libffi-rs `unwrap`s into a Rust PANIC (unrecoverable — `recover:` can't catch it). Reject it
        // here, alongside the other marshalling rejects, so the panic path is statically unreachable.
        if fields.is_empty() {
            self.error(
                span,
                format!(
                    "struct '{}' has no fields and cannot be C-marshallable (an extern struct \
                     requires at least one C-marshallable field) in extern fn '{fn_name}'",
                    crate::base::words::bare_display(name)
                ),
            );
            visited.remove(name);
            return false;
        }
        let mut all_ok = true;
        for (fname, fty) in &fields {
            // A nested non-generic struct recurses with the same `visited` path, so it reports its
            // own offending field, or the recursion if it cycles back (no by-value C layout exists).
            if let Ty::Struct(inner, args) = fty
                && args.is_empty()
                && inner != "Iterator"
                && self.structs.contains_key(inner)
            {
                all_ok &= self.struct_fields_marshallable(inner, fn_name, span, visited);
                continue;
            }
            // Otherwise only true C *scalars* are valid struct fields (NOT `Str`: whether C may hold
            // a `char*` into Chezzi memory is an ownership question, out of scope).
            let ok = matches!(
                fty.scalar(),
                Ty::Int | Ty::Float | Ty::Bool | Ty::Ptr | Ty::Unknown
            );
            if !ok {
                all_ok = false;
                self.error(
                    span,
                    format!(
                        "struct '{name}' field '{fname}' of type '{fty}' is not C-marshallable \
                         (extern struct fields must be C scalars or C-marshallable structs; str \
                         fields are not supported) in extern fn '{fn_name}'"
                    ),
                );
            }
        }
        visited.remove(name);
        all_ok
    }
}
