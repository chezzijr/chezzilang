//! Checker: read-only type resolution and C types of structs.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::resolve_type::{GenericArm, NamedArm, QualifiedArm, alias_head_ty};
use super::*;

/// The C type of a module-owned builtin type, by its own name (`ptr`, `owned_str`, a width), or
/// `None` for a builtin type with no C form. The bare and the qualified spellings both read it.
fn builtin_ctype(b: BuiltinType) -> Option<CType> {
    match b {
        BuiltinType::Int => Some(CType::Int),
        BuiltinType::Float => Some(CType::Float),
        BuiltinType::Bool => Some(CType::Bool),
        BuiltinType::Str => Some(CType::Str),
        BuiltinType::Ptr => Some(CType::Ptr),
        BuiltinType::OwnedStr => Some(CType::OwnedStr),
        BuiltinType::Bytes
        | BuiltinType::ByteArray
        | BuiltinType::Iterator
        | BuiltinType::List
        | BuiltinType::Set
        | BuiltinType::Map
        | BuiltinType::Channel
        | BuiltinType::Range
        | BuiltinType::Shared
        | BuiltinType::RwShared
        | BuiltinType::Atomic
        | BuiltinType::AtomicInt
        | BuiltinType::Executor
        | BuiltinType::Socket
        | BuiltinType::Listener
        | BuiltinType::Writer
        | BuiltinType::Reader => None,
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
            Type::Named { name: n, .. } => match self.named_arm(n) {
                NamedArm::Scalar(_, t)
                | NamedArm::SelfTy(t)
                | NamedArm::Builtin(_, t)
                | NamedArm::ImportedAlias(t) => t,
                NamedArm::Param => Ty::Param(n.clone()),
                NamedArm::Alias(body) => self.resolve_ty_ro_d(&body, depth + 1),
                NamedArm::Width(w) => super::sig::ffi_width_ty(&w),
                NamedArm::Struct(key) => Ty::strukt(key),
                NamedArm::Enum(key) => Ty::enum_ty(key, Vec::new()),
                NamedArm::Protocol(key) => Ty::Protocol(key, Vec::new()),
                NamedArm::BareBox(_) | NamedArm::Unknown => Ty::Unknown,
            },
            Type::Generic(n, args, ..) => {
                let resolved: Vec<Ty> = args
                    .iter()
                    .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                    .collect();
                match self.generic_arm(n, args.len()) {
                    GenericArm::Builtin(b) => b.applied_ty(resolved).unwrap_or(Ty::Unknown),
                    GenericArm::Struct(key) => Ty::Struct(key, resolved),
                    GenericArm::Enum(key) => {
                        let resolved = self.carrier_default_args(&key, resolved);
                        Ty::enum_ty(key, resolved)
                    }
                    GenericArm::Protocol(key) => Ty::Protocol(key, resolved),
                    GenericArm::AliasHead(th) => {
                        let decl = self.type_param_count(&th.key);
                        super::fn_value::head_args(th.pinned.clone(), decl, resolved)
                            .map_or(Ty::Unknown, |targs| alias_head_ty(th, targs))
                    }
                    GenericArm::Unknown => Ty::Unknown,
                }
            }
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
        match self.qualified_arm(mid, name) {
            QualifiedArm::Struct { key, .. } => Ty::Struct(key, args.to_vec()),
            QualifiedArm::Enum { key, .. } => Ty::enum_ty(key, args.to_vec()),
            QualifiedArm::Alias { body, .. } => body,
            QualifiedArm::Protocol { key, .. } => Ty::Protocol(key, args.to_vec()),
            QualifiedArm::Builtin => self.qualified_builtin_ty(name, args).unwrap_or(Ty::Unknown),
            QualifiedArm::Missing => Ty::Unknown,
        }
    }

    /// Resolve a surface extern `Type` to its WIDTH-BEARING [`CType`] in the CURRENT module's
    /// import/alias scope — the SINGLE resolver both backends consume (the FFI collision-fix root).
    /// Reads the arm table `resolve_ty_ro_d` reads (`named_arm`, `qualified_arm`), but stops at the
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
            Type::Named { name: n, .. } => match self.named_arm(n) {
                NamedArm::Scalar(b, _) | NamedArm::Builtin(b, _) => builtin_ctype(b),
                NamedArm::Width(w) => crate::native::cffi::width_ctype(&w),
                // A LOCAL transparent alias: recurse on its body in THIS module's scope.
                NamedArm::Alias(body) => self.resolve_ctype_d(&body, depth + 1),
                // A `from`-imported alias: its CType was computed in the DEFINING module's scope.
                NamedArm::ImportedAlias(_) => self.imported_alias_ctypes.get(n).cloned().flatten(),
                // A bare-visible struct (local or `from`-imported): a by-value flat-scalar struct.
                NamedArm::Struct(key) => self
                    .resolve_struct_ctype(&key)
                    .or_else(|| self.struct_ctype_from_asts(&key, depth)),
                NamedArm::Param
                | NamedArm::SelfTy(_)
                | NamedArm::BareBox(_)
                | NamedArm::Enum(_)
                | NamedArm::Protocol(_)
                | NamedArm::Unknown => None,
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
                match self.qualified_arm(mid, name) {
                    QualifiedArm::Builtin => BuiltinType::from_name(name)
                        .map_or_else(|| crate::native::cffi::width_ctype(name), builtin_ctype),
                    // CROSS-MODULE: read the struct's CType from the cache VERBATIM (computed in ITS
                    // defining module's scope). NEVER field-walk here.
                    QualifiedArm::Struct { key, .. } => self.resolve_struct_ctype(&key),
                    QualifiedArm::Alias { ctype, .. } => ctype,
                    QualifiedArm::Enum { .. }
                    | QualifiedArm::Protocol { .. }
                    | QualifiedArm::Missing => None,
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
