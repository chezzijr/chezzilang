//! Checker: type resolution: arity and bounds, `resolve_type`, `resolve_type_at`.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::setup::TypeHead;
use super::setup::TypeHeadKind;
use super::sig::TypePos;
use super::sig::ffi_width_ty;
use super::sig::width_outside_c_msg;
use super::*;

impl Checker {
    /// Arity + protocol-bound check shared by the generic struct/enum arms of
    /// [`resolve_type`]. `tps` is the declared type-param list (`None` if the name isn't found, e.g.
    /// a non-generic type used with args). Errors are emitted, not returned. The protocol arm is NOT
    /// routed here — it has no bounds loop, a different arity message, and a static-method reject.
    pub(super) fn check_type_arity_and_bounds(
        &mut self,
        n: &str,
        tps: Option<Vec<TyParam>>,
        resolved: &[Ty],
        span: Span,
    ) {
        let Some(tps) = tps else { return };
        if tps.len() != resolved.len() {
            self.error(
                span,
                format!(
                    "type '{n}' expects {} type argument(s), got {}",
                    tps.len(),
                    resolved.len()
                ),
            );
        }
        // A bound's args name the type's own params; read them at this annotation's args.
        let map: HashMap<String, Ty> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .zip(resolved.iter().cloned())
            .collect();
        for (tp, arg) in tps.iter().zip(resolved) {
            for bound in &tp.bounds {
                let bargs: Vec<Ty> = bound.args.iter().map(|a| subst(a, &map)).collect();
                if let Err(msg) = self.satisfies_args(arg, &bound.name, &bargs) {
                    self.error(span, msg);
                }
            }
        }
    }

    /// The module a qualified type `module.name` is read from, or `None` after reporting why the
    /// binder names no module (ambiguous between two imports, or not imported). The one decider for
    /// a qualified type's binder: `resolve_type` and a `type` alias declaration both call it.
    pub(super) fn qualified_type_module(
        &mut self,
        module: &str,
        name: &str,
        span: Span,
    ) -> Option<ModuleId> {
        if let Some(mid) = self.imported_modules.get(module) {
            return Some(mid.clone());
        }
        let msg = self.ambiguous_bind_msg(module).unwrap_or_else(|| {
            format!("unknown module '{module}' (import it to use `{module}.{name}`)")
        });
        self.error(span, msg);
        None
    }

    /// Record the hover of an annotation head at `at`, with the doc its arm names ([`TypeDoc`]).
    /// Probe-gated so off-probe checks pay nothing; gated on `!generic_arg_prepass` so the
    /// generic-arg unification prepass cannot first-hit-wins latch an incomplete type.
    fn record_type_hover(&mut self, n: &str, at: Span, resolved: &Ty, doc: TypeDoc) {
        if self.hover_probe.is_none() || self.generic_arg_prepass {
            return;
        }
        let doc = match doc {
            TypeDoc::Builtin(b) => builtin_type_doc(b.name()),
            TypeDoc::Decl => self.name_docs.get(n).cloned(),
            TypeDoc::NoDoc => None,
        };
        self.hover_record_at(at, resolved, HoverKind::Type, doc);
    }

    /// TICKET-281 — the one answer to "which arm does the bare type name `n` take here". Both type
    /// resolvers read it: `resolve_type_at` adds diagnostics and hover, `resolve_ty_ro_d` adds none.
    pub(super) fn named_arm(&self, n: &str) -> NamedArm {
        // Only keywords are reserved: a type parameter in scope, then a type this module declares
        // or imports, shadow a builtin of the same name.
        let user_first = self.declares_type_here(n);
        let param = self.type_params.contains_key(n);
        if !user_first
            && !param
            && self.bound_builtin(n).is_none()
            && let Some(t) = Self::scalar_bound_ty(n)
            && let Some(b) = BuiltinType::from_name(n)
        {
            return NamedArm::Scalar(b, t);
        }
        if param {
            return NamedArm::Param;
        }
        if n == "Self"
            && let Some(t) = &self.current_self_ty
        {
            return NamedArm::SelfTy(t.clone());
        }
        if !user_first && let Some(b) = self.builtin_type_named(n) {
            if let Some(t) = b.plain_ty() {
                return NamedArm::Builtin(b, t);
            }
            if let BuiltinType::Shared | BuiltinType::RwShared | BuiltinType::Atomic = b {
                return NamedArm::BareBox(b);
            }
        }
        if let Some(body) = self.aliases.get(n) {
            return NamedArm::Alias(body.clone());
        }
        if let Some(t) = self.imported_alias_tys.get(n) {
            return NamedArm::ImportedAlias(t.clone());
        }
        if !user_first {
            if let Some(w) = self
                .bound_builtin(n)
                .filter(|w| crate::native::ffi::is_width(w))
            {
                return NamedArm::Width(w.to_string());
            }
            if crate::native::ffi::is_width(n)
                && self
                    .alias_resolving
                    .last()
                    .is_some_and(|a| self.ffi_alias_ok.contains(a))
            {
                return NamedArm::Width(n.to_string());
            }
        }
        if self.struct_names.contains(n) {
            return NamedArm::Struct(self.bare_key(n));
        }
        if self.enum_names.contains(n) {
            return NamedArm::Enum(self.bare_key(n));
        }
        if self.user_protocol_shape(n).is_some() {
            return NamedArm::Protocol(self.user_protocol_key(n).unwrap_or_else(|| n.to_string()));
        }
        NamedArm::Unknown
    }

    /// TICKET-281 — the one answer to "which arm does the generic head `n` written with `nargs`
    /// type arguments take here", read by both type resolvers.
    pub(super) fn generic_arm(&self, n: &str, nargs: usize) -> GenericArm {
        // A generic type this module declares shadows a builtin of the same name.
        if !self.declares_type_here(n)
            && let Some(b) = self.builtin_type_named(n)
            && b.applied_ty(vec![Ty::Unknown; nargs]).is_some()
        {
            return GenericArm::Builtin(b);
        }
        if self.struct_names.contains(n) {
            return GenericArm::Struct(self.bare_key(n));
        }
        // `T?` / `T!E` spell the carrier privately (`$Option`): it is the prelude's carrier.
        let bare = crate::ast::unprivate(n);
        if bare != n && self.enums.contains_key(bare) {
            return GenericArm::Enum(bare.to_string());
        }
        if self.enum_names.contains(n) {
            return GenericArm::Enum(self.bare_key(n));
        }
        if self.user_protocol_shape(n).is_some() {
            return GenericArm::Protocol(
                self.user_protocol_key(n).unwrap_or_else(|| n.to_string()),
            );
        }
        if let Some(th) = self.bare_type_head(n).filter(|th| th.pinned.is_some()) {
            return GenericArm::AliasHead(th);
        }
        GenericArm::Unknown
    }

    /// TICKET-281 — the one answer to "what does `name` in module `mid` name as a type", read by
    /// `resolve_type_at`, `resolve_qualified_ro` and `resolve_ctype_d`. A RESERVED native type
    /// (`Shared`, `Socket`, ...) has a harvested METHOD-table entry but is not nominal: `Builtin`.
    pub(super) fn qualified_arm(&self, mid: &ModuleId, name: &str) -> QualifiedArm {
        let Some(sig) = self.module_sigs.get(mid) else {
            return QualifiedArm::Missing;
        };
        let builtin = sig.builtins.contains(name);
        if let Some(info) = sig.struct_defs.get(name)
            && !builtin
        {
            QualifiedArm::Struct {
                key: self.type_key(mid, name),
                nparams: info.type_params.len(),
            }
        } else if let Some(edef) = sig.enum_defs.get(name) {
            QualifiedArm::Enum {
                key: self.type_key(mid, name),
                nparams: edef.type_params.len(),
            }
        } else if let Some(asig) = sig.type_aliases.get(name) {
            QualifiedArm::Alias {
                body: asig.body.clone(),
                ctype: asig.ctype.clone(),
            }
        } else if let Some(pdef) = sig.protocol_defs.get(name)
            && !builtin
        {
            QualifiedArm::Protocol {
                key: self.type_key(mid, name),
                nparams: pdef.info.type_params.len(),
            }
        } else if builtin {
            QualifiedArm::Builtin
        } else {
            QualifiedArm::Missing
        }
    }

    pub(super) fn resolve_type(&mut self, t: &Type, span: Span) -> Ty {
        self.resolve_type_at(t, span, TypePos::Value)
    }

    /// TICKET-262 — the one gate of owner rule 1: the width type `name` is its slot tag where `pos`
    /// is a C-boundary slot; anywhere else it is an error, and resolves to its plain scalar.
    fn width_at(&mut self, name: &str, span: Span, pos: TypePos) -> Ty {
        let ty = ffi_width_ty(name);
        if pos.width_ok() {
            return ty;
        }
        self.error(span, width_outside_c_msg(name));
        ty.scalar().clone()
    }

    /// [`Self::resolve_type`] for a position where `None` ("returns nothing") is legal: a declared
    /// return, a fn-type return, the success side of a carrier.
    pub(super) fn resolve_ret_type(&mut self, t: &Type, span: Span) -> Ty {
        self.resolve_type_at(t, span, TypePos::Void)
    }

    /// Whether this user module declares or imports a type named `n` (an alias, a struct or an
    /// enum). Such a name shadows a builtin type of the same name here: only keywords are
    /// reserved. A std module never shadows: its `native struct str` and the like are the builtin
    /// types' own method surfaces.
    pub(super) fn declares_type_here(&self, n: &str) -> bool {
        !self.current_module_is_stdlib
            && (self.aliases.contains_key(n)
                || self.imported_alias_tys.contains_key(n)
                || self.struct_names.contains(n)
                || self.enum_names.contains(n))
    }

    /// Resolve a spelled type at `pos`. `resolve_type` is the value-position default, so a new
    /// container or annotation site rejects `None` with no edit.
    pub(super) fn resolve_type_at(&mut self, t: &Type, span: Span, pos: TypePos) -> Ty {
        match t {
            // The one arm that resolves the type `None`: it is no value type, so every value
            // position rejects it here.
            Type::Nil(_) => {
                if !pos.void_ok() {
                    self.error(
                        span,
                        "'None' is not a value type: None means \"returns nothing\" and is \
                         legal only as a return type or as the success side of `None!E`",
                    );
                    Ty::Unknown
                } else {
                    Ty::Nil
                }
            }
            Type::Named {
                name: n,
                span: name_span,
            } => {
                // Only keywords are reserved: a type parameter in scope, then a type this module
                // declares or imports, shadow a builtin of the same name.
                let arm = self.named_arm(n);
                let doc = arm.doc();
                let resolved = match arm {
                    NamedArm::Scalar(_, t) | NamedArm::SelfTy(t) => t,
                    NamedArm::Param => Ty::Param(n.clone()),
                    NamedArm::Builtin(b, t) => self.builtin_gate(b, t, span),
                    // A bare generic box (`Shared`, `RwShared`, `Atomic`) written without its type
                    // argument.
                    NamedArm::BareBox(_) => {
                        self.error(
                            span,
                            format!("type '{n}' expects 1 type argument(s), got 0"),
                        );
                        Ty::Unknown
                    }
                    // The `alias_resolving` stack breaks cycles (`type A = B; type B = A`).
                    NamedArm::Alias(aliased) => {
                        if self.alias_resolving.iter().any(|a| a == n) {
                            self.error(span, format!("recursive type alias '{n}'"));
                            Ty::Unknown
                        } else {
                            self.alias_resolving.push(n.clone());
                            // An alias expands at the position of its USE.
                            let ty = self.resolve_type_at(&aliased, span, pos);
                            self.alias_resolving.pop();
                            ty
                        }
                    }
                    // The body was resolved read-only in its defining module (no gate), so a
                    // static-ctor protocol could hide in it: re-gate it out of value position here.
                    NamedArm::ImportedAlias(ty) => self.reject_static_protocol_value(ty, span),
                    NamedArm::Width(w) => self.width_at(&w, span, pos),
                    NamedArm::Struct(key) => {
                        // A generic struct written without type arguments is missing them.
                        let nparams = self.structs.get(&key).map_or(0, |i| i.type_params.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::strukt(key)
                    }
                    NamedArm::Enum(key) => {
                        // A generic enum written without type arguments is missing them.
                        let nparams = self.enum_type_params.get(&key).map_or(0, |tps| tps.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::enum_ty(key, Vec::new())
                    }
                    // A protocol with a STATIC method requirement is witnessable only by a static
                    // method, so it is BOUND-ONLY: rejected as a value type here.
                    NamedArm::Protocol(key) => {
                        if self.protocol_has_static_method(n) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(key, Vec::new())
                        }
                    }
                    NamedArm::Unknown => {
                        // A synthesized annotation's `name_span` is `Span::default()` (line 0);
                        // fall back to the enclosing-statement span there.
                        let at = if name_span.line != 0 {
                            *name_span
                        } else {
                            span
                        };
                        let msg = self.unknown_type_msg(n);
                        // A bound's own type arg (e.g. `T: Produces[Bogus]`) can be reported by more
                        // than one reporting resolution of the SAME `Type::Named` AST node (TICKET-202
                        // made `resolve_bounds` silent; `check_bounds` is the bound's reporter), so
                        // they share `at`. Skip a report that would be byte-for-byte identical to
                        // one already recorded.
                        let attributed = self.attribute(&msg);
                        if !self
                            .errors
                            .iter()
                            .any(|e| e.span == at && e.message == attributed)
                        {
                            let help = suggest::did_you_mean(n, &self.type_names());
                            self.error_help(at, msg, help);
                        }
                        Ty::Unknown
                    }
                };
                // Editor hover (LSP): record the resolved type at the name token's OWN span (NOT
                // the enclosing-annotation `span` param). Probe-gated so off-probe checks pay
                // nothing; gated on `!generic_arg_prepass` so the generic-arg unification prepass
                // can't first-hit-wins latch an incomplete type. Composite inner names
                // (`int` in `List[int]`) record via this same arm when resolve_type recurses.
                // A builtin/stdlib type name (`str`/`bytes`/bare `Shared`/…) carries its
                // `builtin_type_doc` usage+methods blurb (Tier C); a user type falls back to its
                // `name_docs` docstring — for an imported non-generic type used as `x: Foo`, the
                // import-line binding seeded `name_docs[Foo]` (its decl docstring or a kind+module
                // blurb). A current-module user type's own docstring also surfaces here (additive;
                // the Tier-A decl-name hover already covers the decl site). `builtin_type_doc` stays
                // FIRST so a builtin name is never shadowed by a same-named `name_docs` entry.
                self.record_type_hover(n, *name_span, &resolved, doc);
                resolved
            }
            Type::Func {
                params,
                ret,
                labels,
                variadic,
            } => {
                let (pp, rp) = pos.func_parts();
                let params = params
                    .iter()
                    .map(|p| self.resolve_type_at(p, span, pp))
                    .collect();
                // The annotation's optional labels ride onto the type (surface-only), so a value call
                // through e.g. a HOF param `f: fn(name: str) -> nil` can resolve `f(name="X")`.
                fn_annotation_ty(
                    params,
                    self.resolve_type_at(ret, span, rp),
                    labels,
                    *variadic,
                )
            }
            Type::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve_type(t, span)).collect()),
            Type::Generic(n, args, head_span) => {
                // TYPE-IDENTITY source for the generic container ctors: the `List`/`Map`/`Set` arms
                // below turn `List[int]` into `Ty::List(Int)` etc. — NOT expressible as a flat `FnSig`,
                // so it stays HERE. Their `CallBuiltin` dispatch is the `Builtin` id table
                // (`base/builtin.rs`); `builtin_container_sig` = flat display/placeholder.
                // `T?` / `T!E` spell the carrier privately (`$Option`): it is the prelude's carrier.
                let arm = self.generic_arm(n, args.len());
                let doc = arm.doc();
                let resolved = match arm {
                    GenericArm::Builtin(b) => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        match b {
                            // M3.2 — a channel holds its values by handle, so every element type
                            // is legal (Go `chan T`).
                            BuiltinType::Channel => {}
                            BuiltinType::Atomic => {
                                self.conformance_obligation(Obligation::AtomicPayload {
                                    elem: resolved[0].clone(),
                                    span,
                                });
                            }
                            BuiltinType::Map => {
                                self.conformance_obligation(Obligation::Hashable {
                                    ty: resolved[0].clone(),
                                    what: "Map key type",
                                    span,
                                });
                            }
                            BuiltinType::Set => {
                                self.conformance_obligation(Obligation::Hashable {
                                    ty: resolved[0].clone(),
                                    what: "Set element type",
                                    span,
                                });
                            }
                            BuiltinType::List
                            | BuiltinType::Iterator
                            | BuiltinType::Shared
                            | BuiltinType::RwShared
                            | BuiltinType::Int
                            | BuiltinType::Float
                            | BuiltinType::Bool
                            | BuiltinType::Str
                            | BuiltinType::Bytes
                            | BuiltinType::ByteArray
                            | BuiltinType::Range
                            | BuiltinType::Socket
                            | BuiltinType::Listener
                            | BuiltinType::Writer
                            | BuiltinType::Reader
                            | BuiltinType::Ptr
                            | BuiltinType::OwnedStr
                            | BuiltinType::AtomicInt
                            | BuiltinType::Executor => {}
                        }
                        b.applied_ty(resolved)
                            .expect("generic_arm checked the argument count")
                    }
                    GenericArm::Struct(key) => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        self.conformance_obligation(Obligation::Bound {
                            name: n.to_string(),
                            key: key.clone(),
                            is_struct: true,
                            args: resolved.clone(),
                            span,
                        });
                        Ty::Struct(key, resolved)
                    }
                    GenericArm::Enum(key) => {
                        // The success side of a carrier may be `None` (`None!E`; `None?` has
                        // its own reject below).
                        let carrier = key == "Option" || key == "Result";
                        let resolved: Vec<Ty> = args
                            .iter()
                            .enumerate()
                            .map(|(i, a)| {
                                if carrier && i == 0 {
                                    self.resolve_ret_type(a, span)
                                } else {
                                    self.resolve_type(a, span)
                                }
                            })
                            .collect();
                        let resolved = self.carrier_default_args(&key, resolved);
                        if key == "Option" && resolved.first() == Some(&Ty::Nil) {
                            self.error(
                                span,
                                "'None?' is not a type: None means \"returns nothing\", so it has \
                                 no optional form"
                                    .to_string(),
                            );
                            Ty::Unknown
                        } else {
                            self.conformance_obligation(Obligation::Bound {
                                name: n.to_string(),
                                key: key.clone(),
                                is_struct: false,
                                args: resolved.clone(),
                                span,
                            });
                            Ty::enum_ty(key, resolved)
                        }
                    }
                    GenericArm::Protocol(key) => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let nparams = self
                            .user_protocol_shape(n)
                            .map_or(0, |p| p.type_params.len());
                        if nparams != resolved.len() {
                            self.error(
                                span,
                                format!(
                                    "type '{n}' expects {nparams} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        if self.protocol_has_static_method(n) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(key, resolved)
                        }
                    }
                    GenericArm::AliasHead(th) => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let tps = self.type_params_of(&th.key);
                        let decl = tps.as_ref().map_or(0, Vec::len);
                        match self.written_head_args(n, decl, th.pinned.clone(), resolved, span) {
                            None => Ty::Unknown,
                            Some(targs) => {
                                self.check_type_arity_and_bounds(n, tps, &targs, span);
                                alias_head_ty(th, targs)
                            }
                        }
                    }
                    GenericArm::Unknown => {
                        self.error(
                            span,
                            format!("unknown generic type '{n}'{}", self.import_hint(n)),
                        );
                        Ty::Unknown
                    }
                };
                // Editor hover (LSP, Tier C): record the resolved type at the GENERIC HEAD-name
                // token's OWN span (`List`/`Heap` in `List[int]`/`Heap[int]`) — the gap the bare
                // `Type::Named` arm already covers for non-generic heads. A builtin/stdlib head
                // (`List`/`Map`/`Set`/`Channel`/…) carries its `builtin_type_doc` usage+methods
                // blurb; a user head (an imported generic struct/enum like `Heap`) falls back
                // to its `name_docs` docstring (the import-line binding seeds that entry). Probe-gated
                // so off-probe checks pay nothing; gated `!generic_arg_prepass` so the generic-arg
                // unification prepass can't first-hit-wins latch an incomplete type.
                self.record_type_hover(n, *head_span, &resolved, doc);
                resolved
            }
            // A module-qualified type `module.Type[args]` (a function is reached the same way, via its
            // bound module name). Resolve `module` in `imported_modules` → the target's `ModuleSig`,
            // confirm the type exists there, and return the matching `Ty`. Enforces arity for generic
            // struct/enum targets.
            Type::Qualified { module, name, args } => {
                let resolved: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, span)).collect();
                let Some(mid) = self.qualified_type_module(module, name, span) else {
                    return Ty::Unknown;
                };
                match self.qualified_arm(&mid, name) {
                    QualifiedArm::Struct { key, nparams } => {
                        if nparams != resolved.len() {
                            self.error(
                                span,
                                format!(
                                    "type '{module}.{name}' expects {nparams} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        Ty::Struct(key, resolved)
                    }
                    QualifiedArm::Enum { key, nparams } => {
                        if nparams != resolved.len() {
                            self.error(
                                span,
                                format!(
                                    "type '{module}.{name}' expects {nparams} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        Ty::enum_ty(key, resolved)
                    }
                    QualifiedArm::Alias { body, .. } => {
                        self.reject_static_protocol_value(body, span)
                    }
                    QualifiedArm::Protocol { key, nparams } => {
                        if !resolved.is_empty() && nparams != resolved.len() {
                            self.error(
                                span,
                                format!(
                                    "type '{module}.{name}' expects {nparams} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        if self.protocol_has_static_method(&key) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{module}.{name}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(key, resolved)
                        }
                    }
                    QualifiedArm::Builtin => {
                        // A module-owned builtin type reached by qualified path (`concurrency.Shared[int]`,
                        // `net.Socket`, `ffi.int32`, `ffi.ptr`): the same type the bare name bound by an
                        // import denotes.
                        if let Some(ty) = self.qualified_builtin_ty(name, &resolved) {
                            let expected =
                                BuiltinType::from_name(name).map_or(0, BuiltinType::arity);
                            if resolved.len() != expected {
                                self.error(
                                span,
                                format!(
                                    "type '{module}.{name}' expects {expected} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                            }
                            match BuiltinType::from_name(name) {
                                _ if crate::native::ffi::is_width(name) => {
                                    self.width_at(name, span, pos)
                                }
                                Some(BuiltinType::Atomic) => {
                                    if let Ty::Atomic(elem) = &ty {
                                        self.conformance_obligation(Obligation::AtomicPayload {
                                            elem: (**elem).clone(),
                                            span,
                                        });
                                    }
                                    ty
                                }
                                Some(b) => self.builtin_gate(b, ty, span),
                                None => ty,
                            }
                        } else {
                            self.error(span, format!("module '{module}' has no type '{name}'"));
                            Ty::Unknown
                        }
                    }
                    QualifiedArm::Missing => {
                        self.error(span, format!("module '{module}' has no type '{name}'"));
                        Ty::Unknown
                    }
                }
            }
        }
    }

    // ===== pass 2: check statements =====
}

/// The one builder of a function type from an ANNOTATION (`fn(int, ...int) -> int`), for both type
/// resolvers (`resolve_type_at` and the read-only `resolve_ty_ro_d`). A `Pack` annotation spells its
/// last param as the element type `T`; the type holds the slot type `List[T]`, as `fn_value_ty` does.
pub(super) fn fn_annotation_ty(
    mut params: Vec<Ty>,
    ret: Ty,
    labels: &[Option<String>],
    variadic: crate::ast::Variadic,
) -> Ty {
    if variadic == crate::ast::Variadic::Pack
        && let Some(last) = params.pop()
    {
        params.push(Ty::List(Box::new(last)));
    }
    Ty::Func {
        params,
        ret: Box::new(ret),
        labels: FnLabels::new(labels.to_vec()),
        variadic,
    }
}

/// TICKET-281 — the arm a bare type name takes ([`Checker::named_arm`]). Each resolver matches every
/// variant, so a new arm fails the build in both.
pub(super) enum NamedArm {
    Scalar(BuiltinType, Ty),
    Param,
    SelfTy(Ty),
    Builtin(BuiltinType, Ty),
    BareBox(BuiltinType),
    Alias(Type),
    ImportedAlias(Ty),
    Width(String),
    Struct(String),
    Enum(String),
    Protocol(String),
    Unknown,
}

/// TICKET-281 — the arm a generic head takes ([`Checker::generic_arm`]).
pub(super) enum GenericArm {
    Builtin(BuiltinType),
    Struct(String),
    Enum(String),
    Protocol(String),
    AliasHead(TypeHead),
    Unknown,
}

/// The type an alias head `th` applied to `targs` names. A protocol head names no value type.
pub(super) fn alias_head_ty(th: TypeHead, targs: Vec<Ty>) -> Ty {
    match th.kind {
        TypeHeadKind::Struct => Ty::Struct(th.key, targs),
        TypeHeadKind::Enum => Ty::enum_ty(th.key, targs),
        TypeHeadKind::Protocol => Ty::Unknown,
    }
}

/// TICKET-281 — the arm a module-qualified type takes ([`Checker::qualified_arm`]).
pub(super) enum QualifiedArm {
    Struct { key: String, nparams: usize },
    Enum { key: String, nparams: usize },
    Alias { body: Ty, ctype: Option<CType> },
    Protocol { key: String, nparams: usize },
    Builtin,
    Missing,
}

/// Where an annotation head's hover doc comes from. A builtin arm reads `builtin_type_doc`, a
/// declared type reads its `name_docs` docstring, and a type parameter reads none: it is an
/// unrelated entity even when it shadows a same-named top-level decl.
#[derive(Clone, Copy)]
pub(super) enum TypeDoc {
    Builtin(BuiltinType),
    Decl,
    NoDoc,
}

impl NamedArm {
    /// The doc source of this arm's hover.
    pub(super) fn doc(&self) -> TypeDoc {
        match self {
            NamedArm::Scalar(b, _) | NamedArm::Builtin(b, _) | NamedArm::BareBox(b) => {
                TypeDoc::Builtin(*b)
            }
            NamedArm::Param => TypeDoc::NoDoc,
            NamedArm::SelfTy(_)
            | NamedArm::Alias(_)
            | NamedArm::ImportedAlias(_)
            | NamedArm::Width(_)
            | NamedArm::Struct(_)
            | NamedArm::Enum(_)
            | NamedArm::Protocol(_)
            | NamedArm::Unknown => TypeDoc::Decl,
        }
    }
}

impl GenericArm {
    /// The doc source of this arm's hover.
    pub(super) fn doc(&self) -> TypeDoc {
        match self {
            GenericArm::Builtin(b) => TypeDoc::Builtin(*b),
            GenericArm::Struct(_)
            | GenericArm::Enum(_)
            | GenericArm::Protocol(_)
            | GenericArm::AliasHead(_)
            | GenericArm::Unknown => TypeDoc::Decl,
        }
    }
}
