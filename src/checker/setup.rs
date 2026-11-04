// checker::setup — split out of checker/mod.rs. `super::*` == the `checker` module.
// Checker construction; stdlib/struct/enum seeding; signature harvesting.

use super::*;

/// What a bare head names (`Checker::head_binding`), in precedence order (TICKET-180).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeadBinding {
    /// A binding in scope >= 1: a local, parameter, loop or pattern variable, or a capture.
    Local,
    /// A type parameter of an enclosing fn or type (DEC-108).
    TypeParam,
    /// A module-scope value: a top-level let, a top-level fn slot or an imported value.
    Global,
    /// A whole-module import binding (`Ty::Module` in scope 0).
    Module,
    /// No scope holds the name.
    Unbound,
}

/// Which kind of user type a head expression names (`Checker::type_head`, TICKET-204).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeHeadKind {
    Struct,
    Enum,
    Protocol,
}

/// A head expression that names a user type here (`Checker::type_head`, TICKET-204).
#[derive(Debug, Clone)]
pub(super) struct TypeHead {
    pub kind: TypeHeadKind,
    /// The canonical type key (`bare_key`, `type_key(mid, ..)`, `protocol_key`, or an alias
    /// body's target key).
    pub key: String,
    /// The name the head spells (`Bx` in `vlib.Bx`).
    pub name: String,
    /// `name` bare, `{module}.{name}` qualified.
    pub spelled: String,
    /// `None` for a declared type; `Some(<body type args>)` for a type alias head (DEC-172).
    pub pinned: Option<Vec<Ty>>,
    /// A reserved native handle (`net.Socket`): its methods dispatch natively and have no proto.
    pub native_handle: bool,
}

/// U10 (TICKET-259): the type a name read in value position names.
#[derive(Debug, Clone)]
pub(super) enum TypeRead {
    /// A struct, enum or protocol, a native handle, or a nominal alias of one.
    Head(TypeHead),
    /// An alias of any other body (`type C = int`), spelled as written.
    Alias(String),
    /// A payload variant, spelled as written (`E.V`, `V`): a constructor, never a value.
    Variant(String),
    /// A builtin type name (`BuiltinType`, a `std.ffi` type).
    Builtin(String),
    /// `Self` inside an inherent method.
    SelfType,
}

impl Checker {
    pub(super) fn new() -> Self {
        let mut c = Checker {
            errors: Vec::new(),
            warnings: Vec::new(),
            pending_key_reject: None,
            scopes: Vec::new(),
            const_decls: Vec::new(),
            hole_rejected: Vec::new(),
            loop_vars: Vec::new(),
            globals: HashMap::new(),
            functions: HashMap::new(),
            local_fn_names: std::collections::HashSet::new(),
            structs: HashMap::new(),
            protocols: HashMap::new(),
            type_params: HashMap::new(),
            enums: HashMap::new(),
            enum_type_params: HashMap::new(),
            enum_methods: HashMap::new(),
            variants: HashMap::new(),
            variant_owners: HashMap::new(),
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            aliases: HashMap::new(),
            alias_resolving: Vec::new(),
            ffi_alias_ok: std::collections::HashSet::new(),
            current_ret: Ty::Nil,
            current_self_ty: None,
            yield_ty: None,
            blocks: Vec::new(),
            generic_arg_prepass: false,
            tyvars: std::cell::RefCell::new(tyvar::TyVars::default()),
            expected_hint: None,
            hint_owner: None,
            handed_recv: None,
            hint_owned: false,
            arith_parent: false,
            const_scan_visits: 0,
            const_overflow_seen: std::collections::HashSet::new(),
            const_operand_faults: std::collections::HashMap::new(),
            inferring_ret: false,
            resolving_returns: false,
            in_generator: false,
            in_fn_body: false,
            let_binders: Vec::new(),
            coalesce_join: false,
            in_default_provider: false,
            decl_site_default: false,
            default_binders: Vec::new(),
            ret_declared: false,
            ret_owner: None,
            untyped_call: None,
            module_sigs: HashMap::new(),
            imported_modules: HashMap::new(),
            json_module: None,
            range_key: crate::resolver::RANGE_STRUCT.to_string(),
            import_paths: Vec::new(),
            ambiguous_binds: HashMap::new(),
            nameless_imports: std::collections::HashSet::new(),
            import_binds: HashMap::new(),
            imported_alias_tys: HashMap::new(),
            imported_alias_ctypes: HashMap::new(),
            extern_sigs: ExternTable::new(),
            extern_module_idx: None,
            call_plans: CallPlanTable::new(),
            call_ctx: None,
            current_provider: None,
            provider_edges: HashMap::new(),
            module_idx_of_file: HashMap::new(),
            graph_binds: std::rc::Rc::default(),
            harvest_keywords: false,
            witnesses: crate::checker::WitnessTable::default(),
            carriers: crate::checker::CarrierTable::new(),
            deferred_obligations: None,
            hook_table: crate::checker::HookTable::new(),
            hook_calls: crate::checker::HookCallTable::new(),
            field_widths: crate::checker::FieldWidthTable::new(),
            fresh_subscripts: Default::default(),
            sum_seeds: crate::checker::SumSeedTable::new(),
            ret_coerce: crate::checker::RetCoerceTable::new(),
            wraps: crate::checker::WrapTable::new(),
            table_conflicts: Vec::new(),
            resolutions: HashMap::new(),
            callee_diverges: HashMap::new(),
            fall_off: FallOffTable::new(),
            module_idx_of: HashMap::new(),
            fn_homes: HashMap::new(),
            next_opt_tmp: 0,
            witness_scope: Vec::new(),
            witness_member_names: std::collections::HashSet::new(),
            entry_fn: None,
            graph_module_idx: 0,
            kw_frag_ctx: Span::default(),
            kw_frag_ord: 0,
            in_extern_sig: false,
            struct_field_asts: HashMap::new(),
            struct_ctypes: HashMap::new(),
            types_by_name: HashMap::new(),
            imported_values: HashMap::new(),
            builtin_binds: HashMap::new(),
            current_module_label: None,
            loop_depth: 0,
            current_module_is_stdlib: false,
            net_socket_seed: None,
            net_listener_seed: None,
            io_writer_seed: None,
            io_reader_seed: None,
            concurrency_seeds: HashMap::new(),
            container_seeds: HashMap::new(),
            carrier_seeds: CarrierSeeds::default(),
            imported_variants: HashMap::new(),
            native_prelude_sigs: HashMap::new(),
            skipped_prelude_structs: Vec::new(),
            skip_fn_bodies: crate::resolver::SkippedFns::default(),
            type_keys: HashMap::new(),
            current_module_id: None,
            bare_types: HashMap::new(),
            hover_probe: None,
            hover_entry: None,
            hover_result: None,
            name_docs: HashMap::new(),
            kw_certain: HashMap::new(),
            kw_written: std::collections::HashSet::new(),
            kw_pending: Vec::new(),
            stmt_mark: super::StmtMark::default(),
            poison_reads: 0,
            ret_memo: HashMap::new(),
            memo_enabled: true,
            #[cfg(any(test, feature = "test-api"))]
            fn_body_checks: 0,
            #[cfg(any(test, feature = "test-api"))]
            index_value_checks: 0,
            #[cfg(any(test, feature = "test-api"))]
            bracket_receivers: 0,
            memo_verify: std::env::var_os("CHEZZI_MEMO_VERIFY").is_some(),
            memo_verifying: false,
        };
        c.seed_stdlib_structs();
        c
    }

    /// TICKET-262 — the one writer of [`FieldWidthTable`]: the C width of each field of struct `key`,
    /// read off the resolved field `Ty`. A struct with no width field gets no entry.
    pub(super) fn note_field_widths(&mut self, key: &str, fields: &[(String, Ty)]) {
        let widths: Vec<Option<CType>> = fields.iter().map(|(_, t)| t.width().cloned()).collect();
        if widths.iter().any(Option::is_some) {
            self.field_widths.insert(key.to_string(), widths);
        }
    }

    /// The runtime key for a bare-written type name in the CURRENT module (structs and enums alike):
    /// its `bare_types` entry when bare-visible (local / `from`-imported / std), else the name itself —
    /// which covers the reserved built-ins (`Result`/`Option`/`Ref`/…) and a not-bare-visible name
    /// (resolution then misses, as before).
    pub(super) fn bare_key(&self, name: &str) -> String {
        self.bare_types
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// Resolve a type alias `name` — LOCAL (`self.aliases`) or named-IMPORTED (`imported_alias_tys`)
    /// — read-only, to its nominal (struct/enum) target with the body's pinned type
    /// arguments. A local body resolves through `resolve_ty_ro`, so a chain, a named import or a
    /// `module.Type` body (TICKET-172) all resolve the way an annotation does. The compiler's
    /// `assign_type_keys` re-points the same spellings to the same canonical key, so everything
    /// accepted here lowers. A scalar/protocol/builtin target, or a cycle, is `None`: the checker
    /// already rejects a cycle as `recursive type alias` (`resolve_type`), which stays the only cycle
    /// diagnostic.
    pub(super) fn alias_body_ty(&self, name: &str) -> Option<Ty> {
        let Some(body) = self.aliases.get(name) else {
            return self
                .imported_alias_tys
                .get(name)
                .and_then(|t| self.nominal_alias_target(t));
        };
        // TICKET-187: the body resolves through the one type-position resolver (DEC-180).
        self.nominal_alias_target(&self.resolve_ty_ro(body))
    }

    /// `ty` when it names a struct/enum whose shape this module can reach (local table or
    /// the owning module's sig), else `None` — a scalar, protocol or container alias body is a type
    /// spelling, never a constructor head. A carrier (`T?`, `T!E`) alias body is a type spelling
    /// too: its variants have no name the user can write.
    fn nominal_alias_target(&self, ty: &Ty) -> Option<Ty> {
        let known = match ty {
            Ty::Struct(k, _) => self.struct_shape(k).is_some(),
            Ty::Enum(k, _) => self.enums.contains_key(k) || self.owning_enum_def(k).is_some(),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => false,
        };
        known.then(|| ty.clone())
    }

    /// The nominal target of an EXPORTED alias reached qualified (`mname.Alias`), through the
    /// module's `ModuleSig::type_aliases`. `None` for a local binding named `mname`, an unbound
    /// module, a missing member, or a non-nominal body (TICKET-172).
    pub(super) fn qualified_alias_ty(&self, mname: &str, name: &str) -> Option<Ty> {
        if self.head_is_value(mname) {
            return None;
        }
        let mid = self.imported_modules.get(mname)?;
        let asig = self.module_sigs.get(mid)?.type_aliases.get(name)?;
        self.nominal_alias_target(&asig.body)
    }

    /// Whether enum `key` reached this module as ITSELF — a named import under any bind name, or a
    /// whole-module import of its declaring module — and not only as the hydrated target of an
    /// alias (TICKET-172, DEC-066). Gates the pattern's scrutinee fallback for a bare enum name.
    pub(super) fn enum_key_imported(&self, key: &str) -> bool {
        self.bare_types.values().any(|k| k == key)
            || self.imported_modules.values().any(|mid| {
                self.module_sigs
                    .get(mid)
                    .is_some_and(|sig| sig.enum_defs.keys().any(|n| self.type_key(mid, n) == key))
            })
    }

    /// Register the SHAPE of an imported alias's nominal target under its canonical key
    /// (TICKET-172), so constructor/variant/static dispatch finds it in the local tables.
    /// Deliberately NOT the bare `*_names` sets or `bare_types`: importing `Position` must not
    /// license the name `Point`.
    pub(super) fn hydrate_alias_target(&mut self, ty: &Ty) {
        match ty {
            Ty::Struct(key, _) if !self.structs.contains_key(key) => {
                if let Some(info) = self.owning_struct_def(key).cloned() {
                    self.structs.insert(key.clone(), info);
                }
            }
            Ty::Enum(key, _) if !self.enums.contains_key(key) => {
                if let Some(edef) = self.owning_enum_def(key).cloned() {
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
            }
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => {}
        }
    }

    /// An alias whose body is an enum instantiation: `(identity key, pinned type args)`. `None` for
    /// an alias of a non-enum body (a scalar alias like `type M = int` keeps its existing "not a
    /// constructor" error) or a cycle.
    pub(super) fn alias_enum_head(&self, name: &str) -> Option<(String, Vec<Ty>)> {
        self.alias_body_ty(name)?
            .as_enum()
            .map(|(k, a)| (k.to_string(), a))
    }

    /// An alias whose body is a struct instantiation: `(identity key, pinned type args)`.
    pub(super) fn alias_struct_head(&self, name: &str) -> Option<(String, Vec<Ty>)> {
        match self.alias_body_ty(name)? {
            Ty::Struct(k, targs) => Some((k, targs)),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// Bind a from-imported struct name: registers the layout and makes the name bare-visible as a
    /// TYPE (`struct_names`/`bare_types`). A same-named declaration in this module is "already
    /// defined" through `GlobalBinding::clashes`.
    ///
    /// `implicit` is true for the import no one wrote (`Span::NOWHERE`): a module's own declaration
    /// of that name shadows it.
    pub(super) fn bind_imported_struct_name(
        &mut self,
        bind: &str,
        key: &str,
        info: &StructInfo,
        implicit: bool,
    ) {
        // A module's own declaration shadows the import no one wrote (only keywords are reserved):
        // `struct Range:` in a user module is that module's `Range`.
        if implicit && self.bare_types.get(bind).is_some_and(|own| own != key) {
            return;
        }
        self.structs.insert(key.to_string(), info.clone());
        self.struct_names.insert(bind.to_string());
        self.bare_types.insert(bind.to_string(), key.to_string());
    }

    /// The module-scoped runtime key for a type `name` declared in module `mid` (bare unless a genuine
    /// cross-module clash disambiguated it in [`check_graph`]). Mirrors the compiler's `type_key`.
    pub(super) fn type_key(&self, mid: &ModuleId, name: &str) -> String {
        self.type_keys
            .get(&(mid.clone(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// The key of a protocol name the USER wrote (a bound, an annotation, an embed), or `None` when
    /// the name is not visible in this module. A std protocol is visible when the module binds it
    /// (`import Add from std.ops`, `import Add as A from std.ops`, `ops.Add` after `import std.ops`),
    /// when it is global (`Any`, `Error`), or when the module is a std module. A module's own
    /// protocol shadows a std one of the same name (only keywords are reserved). Every other reader
    /// asks [`Checker::protocol_key`], which gives the std protocol with no import.
    pub(super) fn user_protocol_key(&self, name: &str) -> Option<String> {
        if let Some(key) = self.qualified_protocol_key(name) {
            return Some(key);
        }
        // `import Iterator as It from std.iter` binds the builtin type, not a protocol name: the
        // binding's member is the key, whatever the module spells it as.
        if let Some(bound) = self.bound_builtin(name)
            && is_std_protocol(bound)
        {
            return Some(bound.to_string());
        }
        if is_std_protocol(name) {
            if let Some(key) = self.bare_types.get(name) {
                return Some(key.clone());
            }
            return (is_global_protocol(name) || self.current_module_is_stdlib)
                .then(|| name.to_string());
        }
        Some(self.protocol_key(name))
    }

    /// The shape of the protocol the user-written `name` reaches in this module, if any.
    pub(super) fn user_protocol_shape(&self, name: &str) -> Option<&ProtocolInfo> {
        self.user_protocol_key(name)
            .and_then(|key| self.protocol_shape(&key))
    }

    /// The runtime key for a protocol name: unchanged (bare) for a std protocol (`is_std_protocol`),
    /// else [`Checker::bare_key`] (mirrors the struct/enum treatment, TICKET-027). Falls back to
    /// [`Checker::protocol_alias_key`] ONLY when `name` misses as a protocol directly — alias-first
    /// could send another module's bare protocol key to a same-named LOCAL alias. A module-qualified
    /// spelling `alias.Name` resolves first, through [`Checker::qualified_protocol_key`] (TICKET-173).
    pub(super) fn protocol_key(&self, name: &str) -> String {
        if let Some(key) = self.qualified_protocol_key(name) {
            return key;
        }
        // A builtin protocol's name is its key: the checker's own references (`==` reads `Eq`,
        // a map key needs `Hashable`) always mean the builtin. A name the USER wrote resolves
        // through `user_protocol_key`, where a module's own protocol shadows the builtin.
        let key = if is_std_protocol(name) {
            name.to_string()
        } else {
            self.bare_key(name)
        };
        if self.protocols.contains_key(&key) || self.owning_protocol_def(&key).is_some() {
            return key;
        }
        if let Some(alias) = self.protocol_alias_key(name) {
            return alias;
        }
        key
    }

    /// The protocol key a module-qualified bound spelling `alias.Name` names through a whole-module
    /// import (TICKET-173) — the `import m as alias` twin of [`Checker::protocol_alias_key`]'s
    /// imported arm. `Name` is a protocol in the module's `ModuleSig`, or a no-argument alias of one
    /// there. The module is everything before the LAST dot, so a full path `pkg.deep.P` reads the
    /// synthetic full-path bind `pkg.deep` (TICKET-175). `None` for a bare name, an unimported
    /// module, or any other member. A name containing `::` is skipped: it is already an identity key,
    /// and a module label may itself contain dots (`a.b::P`), so splitting it at `.` would read the
    /// wrong module.
    pub(super) fn qualified_protocol_key(&self, name: &str) -> Option<String> {
        if name.contains("::") {
            return None;
        }
        let (module, member) = name.rsplit_once('.')?;
        let mid = self.imported_modules.get(module)?;
        let sig = self.module_sigs.get(mid)?;
        if sig.protocol_defs.contains_key(member) {
            return Some(self.type_key(mid, member));
        }
        match sig.type_aliases.get(member).map(|a| &a.body) {
            Some(Ty::Protocol(key, args)) if args.is_empty() => Some(key.clone()),
            _ => None,
        }
    }

    /// The identity key of the protocol a type alias `name` ultimately names, TICKET-108 (W12-16c) —
    /// so `type N = Named` (local) or `import N from m` (where `m` declares `type N = Named`) makes
    /// `N` a valid bound (`fn f[T: N]`), interchangeably with `Named` itself. A LOCAL alias body
    /// resolves through `resolve_ty_ro` (so `type N = named.Named` works too, TICKET-187); otherwise
    /// `name` may be an imported protocol alias or a protocol. An alias that APPLIES TYPE ARGUMENTS (`type IntBag = Bag[int]`) is `None`: no bound-
    /// argument substitution exists to make that a sound bound by name (see `check_bounds`'s own
    /// diagnostic for that shape). `None` when the resolved head is not a protocol at all.
    pub(super) fn protocol_alias_key(&self, name: &str) -> Option<String> {
        // TICKET-187: a local alias body resolves through the one type-position resolver, so a
        // qualified body (`type N = named.Named`) is a bound like a bare one.
        if let Some(body) = self.aliases.get(name) {
            return match self.resolve_ty_ro(body) {
                Ty::Protocol(key, args) if args.is_empty() => Some(key),
                Ty::Int
                | Ty::Float
                | Ty::Bool
                | Ty::Str
                | Ty::Bytes
                | Ty::ByteArray
                | Ty::Nil
                | Ty::List(_)
                | Ty::Map(..)
                | Ty::Set(_)
                | Ty::Func { .. }
                | Ty::Tuple(_)
                | Ty::Struct(..)
                | Ty::Enum(..)
                | Ty::Param(_)
                | Ty::Result(..)
                | Ty::Option(_)
                | Ty::Channel(_)
                | Ty::Shared(_)
                | Ty::Atomic(_)
                | Ty::AtomicInt
                | Ty::RwShared(_)
                | Ty::Executor
                | Ty::Socket
                | Ty::Listener
                | Ty::Writer
                | Ty::Reader
                | Ty::Ptr
                | Ty::Width(_)
                | Ty::Protocol(..)
                | Ty::Module(_)
                | Ty::Unknown
                | Ty::Var(_) => None,
            };
        }
        let head = name.to_string();
        if let Some(Ty::Protocol(key, args)) = self.imported_alias_tys.get(&head) {
            return args.is_empty().then(|| key.clone());
        }
        let key = if is_std_protocol(&head) {
            head.clone()
        } else {
            self.bare_key(&head)
        };
        (self.protocols.contains_key(&key) || self.owning_protocol_def(&key).is_some())
            .then_some(key)
    }

    /// A protocol's shape looked up by identity key in the OWNING module's `ModuleSig` (miss-only
    /// fallback for [`Checker::protocol_shape`]). Mirrors [`Checker::owning_struct_def`].
    pub(super) fn owning_protocol_def(&self, key: &str) -> Option<&ProtocolInfo> {
        self.module_sigs.iter().find_map(|(mid, sig)| {
            sig.protocol_defs
                .iter()
                .find_map(|(name, info)| (self.type_key(mid, name) == key).then_some(&info.info))
        })
    }

    /// A protocol's shape by a source-written name or a key carried on a `Ty::Protocol`: keys the
    /// argument first (a no-op if it is already a key), then the local `self.protocols` table, else
    /// the owning module's `ModuleSig` (TICKET-027).
    pub(super) fn protocol_shape(&self, name_or_key: &str) -> Option<&ProtocolInfo> {
        let key = self.protocol_key(name_or_key);
        self.protocols
            .get(&key)
            .or_else(|| self.owning_protocol_def(&key))
    }

    // --- member-resolution fallback by the value's OWN module-scoped identity key ---
    //
    // User types are MODULE-SCOPED: their per-module shape tables (`self.structs` / `self.enums` +
    // `enum_methods`/`enum_type_params`) are populated ONLY
    // when the WHOLE module OR the type NAME is imported. A named FUNCTION import injects nothing, so a
    // factory result's value — correctly typed `Ty::Struct`/`Enum` carrying its owning
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
    // table hits first). Aligns the impl with docs/reference/spec/'s "reading their fields off a returned value
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

    /// A struct's method names, sorted. `methods` is a `HashMap`, so an unsorted list would make a
    /// near-miss suggestion on a distance tie depend on hash order — sort here, once, for every
    /// caller rather than at each call site.
    pub(super) fn method_names(&self, key: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .struct_shape(key)
            .map(|s| s.methods.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// An enum's method names, sorted (same `HashMap` ordering reason as `method_names`).
    pub(super) fn enum_method_names(&self, key: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .enum_methods_of(key)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// An enum's variant names, in declaration order. `self.enums` values are already a `Vec`, so no
    /// sort — declaration order is what the enum's own docs show.
    #[allow(dead_code)]
    pub(super) fn variant_names(&self, ekey: &str) -> Vec<String> {
        self.enums.get(ekey).cloned().unwrap_or_default()
    }

    /// A module's member names (functions + values), sorted — both tables backing this are
    /// `HashMap`s, so an unsorted list would make a near-miss suggestion depend on hash order.
    pub(super) fn module_member_names(&self, mname: &str) -> Vec<String> {
        let Some(sig) = self
            .imported_modules
            .get(mname)
            .and_then(|id| self.module_sigs.get(id))
        else {
            return Vec::new();
        };
        let mut names: Vec<String> = sig.members.keys().cloned().collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every name currently in scope (locals across the scope chain, plus top-level functions),
    /// sorted — `self.scopes`/`self.functions` are `HashMap`s, so an unsorted list would make a
    /// near-miss suggestion depend on hash order.
    pub(super) fn in_scope_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .scopes
            .iter()
            .enumerate()
            .flat_map(|(i, sc)| sc.keys().filter(move |n| self.scope_has(i, n)))
            .chain(self.functions.keys())
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every type name a bare annotation can resolve to: in-scope user types plus the reserved
    /// scalar/collection names, sorted — `bare_types`/`types_by_name`/`type_params` are `HashMap`s,
    /// so an unsorted list would make a near-miss suggestion depend on hash order.
    pub(super) fn type_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .bare_types
            .keys()
            .chain(self.types_by_name.keys())
            .chain(self.type_params.keys())
            .chain(self.builtin_binds.keys())
            .cloned()
            .chain(
                BuiltinType::ALL
                    .into_iter()
                    .filter(|b| b.owner().is_none())
                    .map(|b| b.name().to_string()),
            )
            .chain(
                graph_pass::std_protocol_decls()
                    .iter()
                    .filter(|p| is_global_protocol(&p.name))
                    .map(|p| p.name.clone()),
            )
            .chain([crate::lexer::NONE.to_string(), "tuple".to_string()])
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every name a module's `sig` exports that `import X from M` can bind: its functions, its
    /// top-level values, and its declared types — sorted, since `members` is a `HashMap` and an
    /// unsorted list would make a near-miss suggestion depend on hash order.
    pub(super) fn sig_member_names(sig: &ModuleSig) -> Vec<String> {
        let mut names: Vec<String> = sig
            .members
            .keys()
            .chain(sig.types.iter())
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// A struct's field names, in declaration order. `StructInfo.fields` is a `Vec`, so it is already
    /// deterministic and needs no sort.
    pub(super) fn field_names(&self, key: &str) -> Vec<String> {
        self.struct_shape(key)
            .map(|s| {
                s.fields
                    .iter()
                    .filter(|(n, _)| !self.hides_field(s, n))
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default()
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
    pub(super) fn enum_type_params_of(&self, key: &str) -> Option<&Vec<TyParam>> {
        self.enum_type_params
            .get(key)
            .or_else(|| self.owning_enum_def(key).map(|e| &e.type_params))
    }
}

/// The C type a value of static type `ty` passes as through a C `...`, after C's default argument
/// promotions (`CType::vararg_promoted`, the one table the runtime's `vararg_ctype_of` also ends in).
/// Every FFI width is a plain `int`/`float` to the program, so only the five C-scalar `Ty`s map;
/// `None` (a struct, closure, `str?`, list, …) cannot be a C vararg.
pub(super) fn c_vararg_ctype(ty: &Ty) -> Option<CType> {
    match ty {
        Ty::Int => Some(CType::Int),
        Ty::Float => Some(CType::Float),
        Ty::Bool => Some(CType::Bool),
        Ty::Str => Some(CType::Str),
        Ty::Ptr => Some(CType::Ptr),
        Ty::Bytes
        | Ty::ByteArray
        | Ty::Nil
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Result(..)
        | Ty::Option(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => None,
    }
    .and_then(|c| c.vararg_promoted())
}

/// Every variant import of a module, one entry per bound name: `(bound name — the alias wins, enum
/// name, variant, bound-name span)`. The checker's one reader of `Import::Variants`.
pub(super) fn variant_import_binds(stmts: &[Stmt]) -> Vec<(String, String, String, Span)> {
    let mut out = Vec::new();
    for s in stmts {
        if let StmtKind::Import(Import::Variants {
            enum_name,
            names,
            name_spans,
        }) = &s.kind
        {
            for (i, (member, alias)) in names.iter().enumerate() {
                out.push((
                    alias.clone().unwrap_or_else(|| member.clone()),
                    enum_name.clone(),
                    member.clone(),
                    name_spans.get(i).copied().unwrap_or(s.span),
                ));
            }
        }
    }
    out
}

/// The [`TypeHead`] of a type alias whose nominal body is `body`: the target's kind and canonical
/// key, with the body's type args pinned (DEC-172). No alias reaches a native handle.
pub(super) fn alias_type_head(name: &str, spelled: String, body: Ty) -> Option<TypeHead> {
    let (kind, key, args) = match body {
        Ty::Struct(k, a) => (TypeHeadKind::Struct, k, a),
        other @ Ty::Int
        | other @ Ty::Float
        | other @ Ty::Bool
        | other @ Ty::Str
        | other @ Ty::Bytes
        | other @ Ty::ByteArray
        | other @ Ty::Nil
        | other @ Ty::List(_)
        | other @ Ty::Map(..)
        | other @ Ty::Set(_)
        | other @ Ty::Func { .. }
        | other @ Ty::Tuple(_)
        | other @ Ty::Enum(..)
        | other @ Ty::Param(_)
        | other @ Ty::Result(..)
        | other @ Ty::Option(_)
        | other @ Ty::Channel(_)
        | other @ Ty::Shared(_)
        | other @ Ty::Atomic(_)
        | other @ Ty::AtomicInt
        | other @ Ty::RwShared(_)
        | other @ Ty::Executor
        | other @ Ty::Socket
        | other @ Ty::Listener
        | other @ Ty::Writer
        | other @ Ty::Reader
        | other @ Ty::Ptr
        | other @ Ty::Width(_)
        | other @ Ty::Protocol(..)
        | other @ Ty::Module(_)
        | other @ Ty::Unknown
        | other @ Ty::Var(_) => {
            let (k, a) = other.as_enum()?;
            (TypeHeadKind::Enum, k.to_string(), a)
        }
    };
    Some(TypeHead {
        kind,
        key,
        name: name.to_string(),
        spelled,
        pinned: Some(args),
        native_handle: false,
    })
}

/// TICKET-238 -- where a binding is written, for the fix the hole error names.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum BindSite {
    /// An untyped `name := value`.
    Let,
    /// A lambda param, a loop variable, a pattern binding, a destructuring target.
    Other,
}

/// The one text for a binding whose type still has a hole when its statement ends. `F` is the
/// type with every hole shown as `int`, so both fixes are spelled out.
pub(super) fn open_binding_message(name: &str, t: &Ty, site: BindSite) -> String {
    fn filled(t: &Ty) -> Ty {
        let v = |ts: &[Ty]| ts.iter().map(filled).collect::<Vec<_>>();
        let b = |x: &Ty| Box::new(filled(x));
        match t {
            Ty::Unknown => Ty::Int,
            Ty::List(x) => Ty::List(b(x)),
            Ty::Option(x) => Ty::Option(b(x)),
            Ty::Set(x) => Ty::Set(b(x)),
            Ty::Channel(x) => Ty::Channel(b(x)),
            Ty::Shared(x) => Ty::Shared(b(x)),
            Ty::RwShared(x) => Ty::RwShared(b(x)),
            Ty::Atomic(x) => Ty::Atomic(b(x)),
            Ty::Map(k, x) => Ty::Map(b(k), b(x)),
            Ty::Result(k, x) => Ty::Result(b(k), b(x)),
            Ty::Struct(n, a) => Ty::Struct(n.clone(), v(a)),
            Ty::Enum(n, a) => Ty::Enum(n.clone(), v(a)),
            Ty::Protocol(n, a) => Ty::Protocol(n.clone(), v(a)),
            Ty::Tuple(ts) => Ty::Tuple(v(ts)),
            Ty::Func {
                params,
                ret,
                labels,
                variadic,
            } => Ty::Func {
                params: v(params),
                ret: b(ret),
                labels: labels.clone(),
                variadic: *variadic,
            },
            other @ Ty::Int
            | other @ Ty::Float
            | other @ Ty::Bool
            | other @ Ty::Str
            | other @ Ty::Bytes
            | other @ Ty::ByteArray
            | other @ Ty::Nil
            | other @ Ty::Param(_)
            | other @ Ty::AtomicInt
            | other @ Ty::Executor
            | other @ Ty::Socket
            | other @ Ty::Listener
            | other @ Ty::Writer
            | other @ Ty::Reader
            | other @ Ty::Ptr
            | other @ Ty::Width(_)
            | other @ Ty::Module(_)
            | other @ Ty::Var(_) => other.clone(),
        }
    }
    let f = filled(t);
    let head = match t {
        Ty::List(_)
        | Ty::Set(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::RwShared(_)
        | Ty::Atomic(_) => {
            format!("cannot infer the element type of `{name}`")
        }
        Ty::Map(..) => format!("cannot infer the key and value types of `{name}`"),
        Ty::Option(_) => format!("cannot infer the type of `{name}` from `None`"),
        Ty::Result(..) => format!("cannot infer the success type of `{name}`"),
        Ty::Struct(..) | Ty::Enum(..) => format!("cannot infer the type arguments of `{name}`"),
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Bytes
        | Ty::ByteArray
        | Ty::Nil
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Param(_)
        | Ty::AtomicInt
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => format!("cannot infer the type of `{name}`"),
    };
    let tail = match (site, t) {
        (BindSite::Other, _) => {
            format!("give it a type, e.g. `{name}: {f}`, or type the value it comes from")
        }
        (BindSite::Let, Ty::List(_) | Ty::Set(_) | Ty::Map(..)) => {
            format!("write `{name}: {f} = ...` or `{name} := {f}()`")
        }
        (BindSite::Let, Ty::Struct(..) | Ty::Enum(..)) => format!(
            "write `{name}: {f} = ...` or put them on the constructor, e.g. `{f}(...)`, `{f}.new()` or `{f}.Variant`"
        ),
        (BindSite::Let, _) => format!("write `{name}: {f} = ...`"),
    };
    format!("{head}: {tail}")
}
