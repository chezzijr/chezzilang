//! Checker: read-only type resolution and C types of structs.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::*;

/// The C type of a module-owned builtin type, by its own name (`ptr`, `owned_str`, a width), or
/// `None` for a builtin type with no C form. The bare and the qualified spellings both read it.
fn builtin_ctype(name: &str) -> Option<CType> {
    match name {
        "ptr" => Some(CType::Ptr),
        "owned_str" => Some(CType::OwnedStr),
        w => crate::native::cffi::width_ctype(w),
    }
}

impl Checker {
    /// Read-only type resolution (no error emission), for contexts that only hold `&self`. Returns
    /// `Ty::Unknown` for anything it can't resolve, which callers treat permissively.
    pub(super) fn resolve_ty_ro(&self, t: &Type) -> Ty {
        self.resolve_ty_ro_d(t, 0)
    }

    /// Depth-bounded read-only type resolution. `depth` guards against a recursive alias body
    /// (`type A = B; type B = A`): without `alias_resolving` (this is `&self`), a hard cap of 64
    /// expansions returns `Ty::Unknown` instead of overflowing the stack. 64 is far beyond any real
    /// alias chain.
    pub(super) fn resolve_ty_ro_d(&self, t: &Type, depth: usize) -> Ty {
        if depth > 64 {
            return Ty::Unknown;
        }
        match t {
            Type::Nil(_) => Ty::Nil,
            Type::Named { name: n, .. } => match n.as_str() {
                s if let Some(t) = self.builtin_type_named(s).and_then(BuiltinType::plain_ty) => t,
                _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                // A bound FFI width resolves to plain `int` (`float` for `float32`); the width is a
                // marshalling detail. So an exported alias body `type Len = int32` captures
                // `Ty::Int`, and `type Real = float32` captures `Ty::Float`.
                _ if let Some(w) = self
                    .bound_builtin(n)
                    .filter(|w| crate::native::ffi::is_width(w)) =>
                {
                    super::sig::ffi_width_ty(w)
                }
                // A bare alias name resolves to its (recursively-resolved) body.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ty_ro_d(&body, depth + 1)
                }
                _ if self.imported_alias_tys.contains_key(n) => self.imported_alias_tys[n].clone(),
                _ if self.struct_names.contains(n) => Ty::strukt(self.bare_key(n)),
                _ if self.enum_names.contains(n) => Ty::enum_ty(self.bare_key(n), Vec::new()),
                _ if self.protocol_shape(&self.user_protocol_key(n)).is_some() => {
                    Ty::Protocol(self.user_protocol_key(n), Vec::new())
                }
                _ => Ty::Unknown,
            },
            Type::Generic(n, args, ..) => match n.as_str() {
                s if let Some(b) = self.builtin_type_named(s)
                    && let Some(t) = b.applied_ty(
                        args.iter()
                            .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                            .collect(),
                    ) =>
                {
                    t
                }
                _ if self.struct_names.contains(n) => Ty::Struct(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if crate::ast::unprivate(n) != n.as_str() || self.enum_names.contains(n) => {
                    let key = if crate::ast::unprivate(n) != n.as_str() {
                        crate::ast::unprivate(n).to_string()
                    } else {
                        self.bare_key(n)
                    };
                    let resolved = args
                        .iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect();
                    let resolved = self.carrier_default_args(&key, resolved);
                    Ty::enum_ty(key, resolved)
                }
                // A parameterized protocol used as a value type (`Container[int]`). Mint the carried
                // args so the read-only resolver no longer silent-accepts it as `Unknown` (which would
                // erase the witness). Mirrors the mutable `resolve_type` protocol arm.
                _ if self.protocol_shape(&self.user_protocol_key(n)).is_some() => Ty::Protocol(
                    self.user_protocol_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ => Ty::Unknown,
            },
            Type::Func {
                params,
                ret,
                labels,
                variadic,
            } => super::resolve_type::fn_annotation_ty(
                params
                    .iter()
                    .map(|p| self.resolve_ty_ro_d(p, depth + 1))
                    .collect(),
                self.resolve_ty_ro_d(ret, depth + 1),
                labels,
                *variadic,
            ),
            Type::Tuple(ts) => Ty::Tuple(
                ts.iter()
                    .map(|t| self.resolve_ty_ro_d(t, depth + 1))
                    .collect(),
            ),
            Type::Qualified { module, name, args } => {
                let resolved_args: Vec<Ty> = args
                    .iter()
                    .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                    .collect();
                self.resolve_qualified_ro(module, name, &resolved_args)
            }
        }
    }

    /// Read-only resolution of a module-qualified type `module.name[args]` to a `Ty`. Looks the bound
    /// module up in `imported_modules`, finds the type in its `ModuleSig`, and returns the matching
    /// `Ty` (struct / enum / alias body). `Ty::Unknown` if anything is missing (callers permissive).
    pub(super) fn resolve_qualified_ro(&self, module: &str, name: &str, args: &[Ty]) -> Ty {
        let Some(mid) = self.imported_modules.get(module) else {
            return Ty::Unknown;
        };
        let Some(sig) = self.module_sigs.get(mid) else {
            return Ty::Unknown;
        };
        // A RESERVED native type (Shared/RwShared/Atomic/Executor/Socket/Listener) has a harvested
        // `sig.struct_defs` METHOD-table entry but is NOT nominal — skip it here so it resolves to the
        // reserved builtin `Ty` via the `sig.types` branch below (mirrors `resolve_type`'s guard).
        if sig.struct_defs.contains_key(name) && !sig.builtins.contains(name) {
            Ty::Struct(self.type_key(mid, name), args.to_vec())
        } else if sig.enum_defs.contains_key(name) {
            Ty::enum_ty(self.type_key(mid, name), args.to_vec())
        } else if let Some(asig) = sig.type_aliases.get(name) {
            asig.body.clone()
        } else if sig.protocol_defs.contains_key(name) {
            // Permissive mirror of `resolve_type`'s qualified protocol arm: no arity error and no
            // static-ctor gate — the mutable resolver owns both diagnostics.
            Ty::Protocol(self.type_key(mid, name), args.to_vec())
        } else if sig.builtins.contains(name) {
            // `resolve_type`'s qualified builtin branch on the READ-ONLY export path (so an
            // EXPORTED `type S = concurrency.Shared[int]` resolved via `resolve_ty_ro_d` carries the
            // right builtin `Ty`). Permissive: no errors.
            self.qualified_builtin_ty(name, args).unwrap_or(Ty::Unknown)
        } else {
            Ty::Unknown
        }
    }

    /// Resolve a surface extern `Type` to its WIDTH-BEARING [`CType`] in the CURRENT module's
    /// import/alias scope — the SINGLE resolver both backends consume (the FFI collision-fix root).
    /// Mirrors `resolve_ty_ro_d`'s alias / `from`-import / `Qualified` walk EXACTLY, but stops at the
    /// width-bearing leaf instead of collapsing every FFI integer to `Ty::Int`. Crucially:
    ///   * a LOCAL alias (`self.aliases`) recurses on its body — resolving each hop in THIS module's
    ///     scope (a colliding same-named alias in another module can never be reached);
    ///   * a `from`-imported alias reads `self.imported_alias_ctypes` (the alias's CType computed in
    ///     its DEFINING module's scope) — closing the named-import-hop hole;
    ///   * a `module.Alias` reads the TARGET module's `AliasSig.ctype` (likewise defining-scope).
    ///
    /// `depth > 64` returns `None` (cycle guard, matching `resolve_ty_ro_d`). `None` means "not
    /// C-marshallable here" — the marshallability gate (`assert_marshallable`) is the actual error,
    /// this is only the width carrier.
    pub(super) fn resolve_ctype(&self, t: &Type) -> Option<CType> {
        self.resolve_ctype_d(t, 0)
    }

    pub(super) fn resolve_ctype_d(&self, t: &Type, depth: usize) -> Option<CType> {
        if depth > 64 {
            return None; // cyclic alias — defended (the marshal gate rejects it cleanly).
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                // A module-owned C type this module bound (`ptr`, `owned_str`, a width), under any
                // alias. An import binds before a builtin of its name.
                _ if let Some(c) = self.bound_builtin(n) => builtin_ctype(c),
                "int" => Some(CType::Int),
                "float" => Some(CType::Float),
                "bool" => Some(CType::Bool),
                "str" => Some(CType::Str),
                // A LOCAL transparent alias: recurse on its body in THIS module's scope.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ctype_d(&body, depth + 1)
                }
                // A `from`-imported alias: its CType was computed in the DEFINING module's scope.
                _ if self.imported_alias_ctypes.contains_key(n) => {
                    self.imported_alias_ctypes[n].clone()
                }
                // A bare-visible struct (local or `from`-imported): a by-value flat-scalar struct.
                // SAME-MODULE path (current scope == defining scope): a cache hit (already populated)
                // OR a field-walk in THIS scope for a not-yet-populated forward-reference nested struct
                // — both correct here because this is the struct's own defining scope.
                _ if self.struct_names.contains(n) => {
                    let key = self.bare_key(n);
                    self.resolve_struct_ctype(&key)
                        .or_else(|| self.struct_ctype_from_asts(&key, depth))
                }
                _ => None,
            },
            // A sync scalar callback param (callbacks #4): `fn(scalars...) -> scalar` (or `-> nil`)
            // lowers to `CType::Callback`. Every param must lower to a C SCALAR (`is_scalar`) — a
            // non-scalar part (str/struct/nested callback) yields `None`, so the marshal gate rejects
            // it cleanly. (Param-only; a function-typed RETURN is rejected by `assert_marshallable`,
            // never lowered.)
            //
            // The return is `Option<Box<CType>>`, exactly like `Cffi::ret`: `None` means VOID. A
            // non-scalar return (e.g. `str`) is already rejected by `assert_marshallable` before this
            // runs, so a `None` reaching here in a program that checks clean can only be `nil` (or a
            // transparent alias to it) — never a silently-dropped scalar.
            // A variadic callback type (`fn(...int)`, a C `...`) has no C callback shape: `None`.
            Type::Func {
                variadic: crate::ast::Variadic::Pack | crate::ast::Variadic::C,
                ..
            } => None,
            Type::Func {
                params,
                ret,
                variadic: crate::ast::Variadic::No,
                ..
            } => {
                let mut cparams = Vec::with_capacity(params.len());
                for p in params {
                    let cp = self.resolve_ctype_d(p, depth + 1)?;
                    if !cp.is_scalar() {
                        return None;
                    }
                    cparams.push(cp);
                }
                let cret = match self.resolve_ctype_d(ret, depth + 1) {
                    Some(c) if c.is_scalar() => Some(Box::new(c)),
                    Some(_) => return None,
                    None => None,
                };
                Some(CType::Callback {
                    params: cparams,
                    ret: cret,
                })
            }
            // RETURN-ONLY nullable `char*` (`str?` / `owned_str?`): the inner type decides
            // borrowed (`str` → OptStr) vs owned (`owned_str` → OptOwnedStr).
            Type::Generic(n, args, ..)
                if crate::ast::unprivate(n) == "Option" && args.len() == 1 =>
            {
                match self.resolve_ctype_d(&args[0], depth + 1) {
                    Some(CType::Str) => Some(CType::OptStr),
                    Some(CType::OwnedStr) => Some(CType::OptOwnedStr),
                    _ => None,
                }
            }
            // A module-qualified type `mod.Name` — resolved in the TARGET module's scope, so a width
            // alias carries its DEFINING module's width and a struct its identity key (collision-proof
            // by construction: never the bare flat alias map).
            Type::Qualified { module, name, .. } => {
                let mid = self.imported_modules.get(module)?;
                let sig = self.module_sigs.get(mid)?;
                // A qualified FFI marshalling TYPE name (`ffi.int32` / `ffi.ptr`) in an extern
                // signature: map to its WIDTH-bearing CType, exactly like the bare `Type::Named`
                // arm above. The surface name is unchanged (the backends' `ctype_of` reads the
                // resulting CType, never re-derives from a module prefix), so it marshals identically
                // to the bare width. Gated on `sig.types` membership — only `std.ffi`'s sig carries
                // these reserved names — so this fires solely for genuine FFI types.
                if sig.builtins.contains(name) {
                    return builtin_ctype(name);
                }
                if sig.struct_defs.contains_key(name) {
                    // CROSS-MODULE: read the qualified struct's CType from the cache VERBATIM (computed
                    // in ITS defining module's scope, deps-first). NEVER field-walk here — the current
                    // scope is the IMPORTER's, where the struct's field aliases are invisible/colliding.
                    self.resolve_struct_ctype(&self.type_key(mid, name))
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    asig.ctype.clone()
                } else {
                    None
                }
            }
            Type::Generic(..) | Type::Tuple(_) | Type::Nil(_) => None,
        }
    }

    /// PURE CACHE READ of the by-value `CType::Struct` for the struct under IDENTITY `key`. The CType
    /// was pre-computed in the struct's OWN DEFINING module's scope (by `populate_struct_ctypes`, run
    /// after `hoist` in each module's `check_module` — deps-first AND before this module's own extern
    /// harvest, so a cross-module OR same-module struct is always cached before an extern needs it).
    /// This is the SINGLE-RESOLVER invariant that kills the FFI drift: `resolve_struct_ctype` NEVER
    /// re-resolves a struct's fields in the (wrong, importing) current scope — it only reads the cache,
    /// so a field typed via the defining module's local alias keeps its true width.
    pub(super) fn resolve_struct_ctype(&self, key: &str) -> Option<CType> {
        self.struct_ctypes.get(key).cloned().flatten()
    }

    /// Cache every struct DECLARED IN THIS MODULE under its identity key, its by-value `CType::Struct`
    /// computed HERE — in this (the DEFINING) module's import/alias scope (extending the
    /// `AliasSig::ctype` precedent to structs). Called once per module from `check_module` after
    /// `hoist` (so all of this module's aliases/`from`-imports are live) and BEFORE the check_stmt loop
    /// (so a same-module extern harvested in the loop reads the cache). Modules are checked deps-first,
    /// so a downstream importer's extern returning `mod.Struct` reads this cached, defining-scope CType
    /// verbatim. Gated to the extern-harvesting pass; a lone `check` never builds these.
    pub(super) fn populate_struct_ctypes(&mut self, stmts: &[Stmt], id: Option<&ModuleId>) {
        let Some(mid) = id else { return };
        let mid = mid.clone();
        for stmt in stmts {
            if let StmtKind::Struct { name, .. } = &stmt.kind {
                let key = self.type_key(&mid, name);
                if self.struct_ctypes.contains_key(&key) {
                    continue; // already cached (a cross-module forward-ref resolved it earlier).
                }
                let c = self.struct_ctype_from_asts(&key, 0);
                self.struct_ctypes.insert(key, c);
            }
        }
    }

    /// Build a by-value `CType::Struct` for the struct under IDENTITY `key` from its raw field ASTs,
    /// mapping each AST field type to its own width-bearing `CType` (so an `int32` field stays 4 bytes
    /// — the layout the C ABI expects) IN THE CURRENT SCOPE. `key` is the identity key (the tag a
    /// returned struct carries, so field lookup hits). `None` if a field isn't a scalar leaf (a
    /// non-marshallable struct — the marshal gate rejects it). The ONLY field-resolving path — only
    /// `populate_struct_ctypes` calls it, in the defining scope, so there is exactly one resolver.
    pub(super) fn struct_ctype_from_asts(&self, key: &str, depth: usize) -> Option<CType> {
        let fields = self.struct_field_asts.get(key)?;
        let mut cfields = Vec::with_capacity(fields.len());
        let mut field_names = Vec::with_capacity(fields.len());
        for (fname, fty) in fields {
            cfields.push(self.resolve_ctype_d(fty, depth + 1)?);
            field_names.push(fname.clone());
        }
        Some(CType::Struct {
            name: key.to_string(),
            field_names,
            fields: cfields,
        })
    }
}
