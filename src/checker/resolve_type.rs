//! Checker: type resolution: arity and bounds, `resolve_type`, `resolve_type_at`.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

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
                let user_first = self.declares_type_here(n);
                let resolved = match n.as_str() {
                    s if !user_first
                        && !self.type_params.contains_key(s)
                        && self.bound_builtin(s).is_none()
                        && let Some(t) = Self::scalar_bound_ty(s) =>
                    {
                        t
                    }
                    // A generic type parameter (`T`) or `Self`, in scope while checking a generic fn
                    // signature/body or a protocol method. Resolved BEFORE every reserved/module name
                    // below (ptr / owned_str / Executor / Shared|RwShared|Atomic / Socket / Listener)
                    // and before user structs/aliases, so an in-scope type param uniformly shadows them
                    // (e.g. `fn id[Executor](x: Executor)`), instead of being hijacked by the builtin
                    // arm or mis-erroring with a bogus import hint. Placed below the scalar primitives
                    // (int/float/bool/str/…) so those keep resolving to their scalar even when used as a
                    // type-param name (`fn id[int](x: int)` → x is `int`), preserving existing behavior.
                    _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                    // `Self` inside a struct/enum method's signature or body → the concrete
                    // ENCLOSING type (`fn dup(self) -> Self` in `struct P` ⇒ `Ty::Struct("P", …)`).
                    // Placed AFTER the type-param arm above so a PROTOCOL method's `Self` (which is in
                    // `type_params` as `Ty::Param("Self")`, with `current_self_ty` left `None`) keeps
                    // its existential param binding — this concrete arm fires only for inherent
                    // methods, where `current_self_ty` is set. Resolving to the concrete type (not a
                    // `Ty::Param`) makes `-> Self` enforce the real enclosing type. Outside any method
                    // `current_self_ty` is `None`, so `Self` falls through to `unknown type 'Self'`.
                    "Self" if self.current_self_ty.is_some() => {
                        self.current_self_ty.clone().unwrap()
                    }
                    // U10: a bare builtin type name resolves through `BuiltinType`: a global one, or
                    // a module-owned one this module bound (`builtin_type_named`). A type parameter or
                    // `Self` above shadows these.
                    s if !user_first
                        && let Some(b) = self.builtin_type_named(s)
                        && let Some(t) = b.plain_ty() =>
                    {
                        self.builtin_gate(b, t, span)
                    }
                    // A bare generic box (`Shared`, `RwShared`, `Atomic`) written without its type
                    // argument.
                    s if !user_first
                        && let Some(
                            BuiltinType::Shared | BuiltinType::RwShared | BuiltinType::Atomic,
                        ) = self.builtin_type_named(s) =>
                    {
                        self.error(
                            span,
                            format!("type '{n}' expects 1 type argument(s), got 0"),
                        );
                        Ty::Unknown
                    }
                    // A transparent type alias resolves to its underlying type (recursively). The
                    // `alias_resolving` stack breaks cycles (`type A = B; type B = A`).
                    _ if self.aliases.contains_key(n) => {
                        if self.alias_resolving.iter().any(|a| a == n) {
                            self.error(span, format!("recursive type alias '{n}'"));
                            Ty::Unknown
                        } else {
                            let aliased = self.aliases[n].clone();
                            self.alias_resolving.push(n.clone());
                            // An alias expands at the position of its USE.
                            let ty = self.resolve_type_at(&aliased, span, pos);
                            self.alias_resolving.pop();
                            ty
                        }
                    }
                    // A `from`-imported type alias resolves to its pre-resolved body (computed in the
                    // defining module's scope). A licensed FFI-width alias was already re-seeded into
                    // `ffi_alias_ok`, but since the body is already a concrete `Ty` no width re-check is
                    // needed here.
                    _ if self.imported_alias_tys.contains_key(n) => {
                        // The body was resolved read-only in its defining module (no gate), so a
                        // static-ctor protocol could hide in it — re-gate it out of value position here.
                        let ty = self.imported_alias_tys[n].clone();
                        self.reject_static_protocol_value(ty, span)
                    }
                    // A C width (`int8`..`uint64`, `float32`) this module bound: `Ty::Width`
                    // (`ffi_width_ty`), a slot tag whose value is a plain `int`/`float`; the backends
                    // recover the C width from the checker's `resolve_ctype`.
                    _ if !user_first
                        && let Some(w) = self
                            .bound_builtin(n)
                            .filter(|w| crate::native::ffi::is_width(w))
                            .map(str::to_string) =>
                    {
                        self.width_at(&w, span, pos)
                    }
                    // A width reached by expanding an alias body whose defining module bound it
                    // (`ffi_alias_ok`): the alias stays valid wherever it is used.
                    _ if !user_first
                        && crate::native::ffi::is_width(n)
                        && self
                            .alias_resolving
                            .last()
                            .is_some_and(|a| self.ffi_alias_ok.contains(a)) =>
                    {
                        self.width_at(n, span, pos)
                    }
                    _ if self.struct_names.contains(n) => {
                        // The layout is keyed by the runtime key (bare unless disambiguated); the written
                        // name's bare-visibility is the `struct_names` gate above. Carry the key on the Ty.
                        let key = self.bare_key(n);
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
                    _ if self.enum_names.contains(n) => {
                        let key = self.bare_key(n);
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
                    // A protocol name used as a value type (existential), e.g. `Error`. BUT a protocol
                    // with a STATIC method requirement (`Convert`-style static ctor) is witnessable only
                    // by a static method — a VALUE can't invoke it — so it is BOUND-ONLY, rejected here.
                    _ if self.protocol_shape(&self.user_protocol_key(n)).is_some() => {
                        if self.protocol_has_static_method(n) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(self.user_protocol_key(n), Vec::new())
                        }
                    }
                    _ => {
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
                if self.hover_probe.is_some() && !self.generic_arg_prepass {
                    // Suppress the `name_docs` fallback when the name resolved to a type PARAMETER
                    // (a generic `[T]` in scope) that merely SHADOWS a same-named top-level decl —
                    // the param is an unrelated entity and must not borrow that decl's docstring
                    // (mirrors the value-ident hover's scope guard). `builtin_type_doc` is unaffected
                    // (it never names a param). A real generic head is never a `Ty::Param`, so this
                    // is a no-op at the head site.
                    let doc = builtin_type_doc(n).or_else(|| {
                        if matches!(resolved, Ty::Param(_)) {
                            None
                        } else {
                            self.name_docs.get(n).cloned()
                        }
                    });
                    self.hover_record_at(*name_span, &resolved, HoverKind::Type, doc);
                }
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
                let carrier_key = (crate::ast::unprivate(n) != n)
                    .then(|| crate::ast::unprivate(n).to_string())
                    .filter(|k| self.enums.contains_key(k));
                // A generic type this module declares shadows a builtin of the same name.
                let builtin = if self.declares_type_here(n) {
                    None
                } else {
                    self.builtin_type_named(n)
                };
                let resolved = match (builtin, args.as_slice()) {
                    (Some(BuiltinType::List), [inner]) => Ty::list(self.resolve_type(inner, span)),
                    // `Iterator[T]` as a *value* type — the result of calling a generator function.
                    // Represented as `Ty::Struct("Iterator", [T])`, an existential iterator whose element
                    // type `iter_elem` recovers (so `for`-loops and `[S: Iterator[T]]` bounds accept it —
                    // a cursor is exactly what the `Iterator` bound requires).
                    // Experimental: only generators produce these; ordinary code still uses adapter
                    // structs / built-in collections.
                    (Some(BuiltinType::Iterator), [elem]) => {
                        Ty::Struct("Iterator".to_string(), vec![self.resolve_type(elem, span)])
                    }
                    (Some(BuiltinType::Channel), [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if !self.sendable(&elem) {
                            let hint = self.sendable_error_hint(&elem);
                            self.error(
                                span,
                                format!("Channel element type must be sendable (able to cross a task boundary), found {elem}{hint}"),
                            );
                        }
                        Ty::channel(elem)
                    }
                    // `Shared[T]` (C3): the cross-task mutable box. Unlike a `Channel`, its element type
                    // isn't gated on sendability — the value lives in one owner and is copied in/out
                    // through `get`/`set`; the *handle* is what crosses (always sendable). The same for
                    // the `RwShared`/`Atomic` siblings below.
                    (Some(BuiltinType::Shared), [inner]) => {
                        Ty::shared(self.resolve_type(inner, span))
                    }
                    (Some(BuiltinType::RwShared), [inner]) => {
                        Ty::rwshared(self.resolve_type(inner, span))
                    }
                    (Some(BuiltinType::Atomic), [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        self.conformance_obligation(Obligation::AtomicPayload {
                            elem: elem.clone(),
                            span,
                        });
                        Ty::atomic(elem)
                    }
                    (Some(BuiltinType::Map), [k, v]) => {
                        let key = self.resolve_type(k, span);
                        let value = self.resolve_type(v, span);
                        self.conformance_obligation(Obligation::Hashable {
                            ty: key.clone(),
                            what: "Map key type",
                            span,
                        });
                        Ty::map(key, value)
                    }
                    (Some(BuiltinType::Set), [t]) => {
                        let elem = self.resolve_type(t, span);
                        self.conformance_obligation(Obligation::Hashable {
                            ty: elem.clone(),
                            what: "Set element type",
                            span,
                        });
                        Ty::set(elem)
                    }
                    // A user-defined generic struct instantiated with type arguments: `Pair[int, str]`.
                    _ if self.struct_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        // Clone the param list out so the borrow on `self.structs` is dropped before
                        // the `satisfies`/`error` calls below.
                        self.conformance_obligation(Obligation::Bound {
                            name: n.to_string(),
                            key: key.clone(),
                            is_struct: true,
                            args: resolved.clone(),
                            span,
                        });
                        Ty::Struct(key, resolved)
                    }
                    // A user-defined generic enum instantiated with type arguments: `Tree[int]`.
                    _ if carrier_key.is_some() || self.enum_names.contains(n) => {
                        let key = carrier_key.clone().unwrap_or_else(|| self.bare_key(n));
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
                    // A parameterized protocol used as a value type (`Container[int]`): resolve the
                    // args, arity-check against the protocol's own type params, and carry them on the
                    // existential. Conformance is witnessed at every store/pass boundary (via
                    // `assignable` → `satisfies_args`); the args are then erased at runtime. Mirrors
                    // the struct/enum parameterized arms above.
                    _ if self.protocol_shape(&self.user_protocol_key(n)).is_some() => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let nparams = self
                            .protocol_shape(&self.user_protocol_key(n))
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
                        // A static-method (static-ctor) protocol like `Convert[int]` is BOUND-ONLY —
                        // no value can witness a static requirement, so it is rejected in value position.
                        if self.protocol_has_static_method(n) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(self.user_protocol_key(n), resolved)
                        }
                    }
                    _ if self.bare_type_head(n).is_some_and(|th| th.pinned.is_some()) => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let th = self.bare_type_head(n).expect("matched by the guard");
                        let tps = self.type_params_of(&th.key);
                        let decl = tps.as_ref().map_or(0, Vec::len);
                        // A pinning or non-generic alias rejects its written args; an unpinned
                        // generic alias (`type BB = Box`) takes them, as `BB[int](9)` does.
                        match self.written_head_args(n, decl, th.pinned, resolved, span) {
                            None => Ty::Unknown,
                            Some(targs) => {
                                self.check_type_arity_and_bounds(n, tps, &targs, span);
                                match th.kind {
                                    TypeHeadKind::Struct => Ty::Struct(th.key, targs),
                                    TypeHeadKind::Enum => Ty::enum_ty(th.key, targs),
                                    TypeHeadKind::Protocol => Ty::Unknown,
                                }
                            }
                        }
                    }
                    _ => {
                        self.error(span, format!("unknown generic type '{n}'"));
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
                if self.hover_probe.is_some() && !self.generic_arg_prepass {
                    // Suppress the `name_docs` fallback when the name resolved to a type PARAMETER
                    // (a generic `[T]` in scope) that merely SHADOWS a same-named top-level decl —
                    // the param is an unrelated entity and must not borrow that decl's docstring
                    // (mirrors the value-ident hover's scope guard). `builtin_type_doc` is unaffected
                    // (it never names a param). A real generic head is never a `Ty::Param`, so this
                    // is a no-op at the head site.
                    let doc = builtin_type_doc(n).or_else(|| {
                        if matches!(resolved, Ty::Param(_)) {
                            None
                        } else {
                            self.name_docs.get(n).cloned()
                        }
                    });
                    self.hover_record_at(*head_span, &resolved, HoverKind::Type, doc);
                }
                resolved
            }
            // A module-qualified type `module.Type[args]` (mirrors how a function is reached via its
            // bound module name). Resolve `module` in `imported_modules` → the target's `ModuleSig`,
            // confirm the type exists there, and return the matching `Ty`. Enforces arity for generic
            // struct/enum targets.
            Type::Qualified { module, name, args } => {
                let resolved: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, span)).collect();
                let Some(mid) = self.qualified_type_module(module, name, span) else {
                    return Ty::Unknown;
                };
                let Some(sig) = self.module_sigs.get(&mid).cloned() else {
                    self.error(span, format!("module '{module}' has no type '{name}'"));
                    return Ty::Unknown;
                };
                // A RESERVED native type (std.concurrency's `Shared`/`RwShared`/`Atomic`/`Executor`,
                // std.net's `Socket`/`Listener`) now ALSO has a `sig.struct_defs` entry — for its
                // harvested METHOD table — but it is NOT a nominal struct: it must resolve to the opaque
                // reserved `Ty::Shared`/etc via the `sig.types` → `qualified_builtin_ty` branch below,
                // NOT `Ty::Struct(...)`. Skip the struct_defs arm for these so `concurrency.Shared[int]`
                // (a qualified annotation / `type` alias body) keeps its reserved `Ty`
                // (matching the bare-after-import path); otherwise it would mint a divergent
                // `Ty::Struct("Shared", …)` that fails to unify with a `Shared(v)` ctor's `Ty::shared`.
                if let Some(info) = sig.struct_defs.get(name)
                    && !sig.builtins.contains(name)
                {
                    if info.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                info.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::Struct(self.type_key(&mid, name), resolved)
                } else if let Some(edef) = sig.enum_defs.get(name) {
                    if edef.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                edef.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::enum_ty(self.type_key(&mid, name), resolved)
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    // Pre-resolved in the exporting module by the read-only resolver (no gate) — re-gate
                    // a static-ctor protocol out of value position (`import a; c: a.Foo`).
                    let body = asig.body.clone();
                    self.reject_static_protocol_value(body, span)
                } else if let Some(pdef) = sig.protocol_defs.get(name) {
                    // Mirrors the bare generic-protocol arm: arity is checked only when args are
                    // WRITTEN, so a bare `shapes.Container` stays legal as an unbound existential —
                    // exactly like the bare `Container` spelling — while `shapes.Container[int, str]`
                    // is still caught.
                    if !resolved.is_empty() && pdef.info.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                pdef.info.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    if self.protocol_has_static_method(&self.type_key(&mid, name)) {
                        self.error(
                            span,
                            format!(
                                "protocol '{module}.{name}' has a static method and can only be used as a bound, not a value type"
                            ),
                        );
                        Ty::Unknown
                    } else {
                        Ty::Protocol(self.type_key(&mid, name), resolved)
                    }
                } else if sig.builtins.contains(name) {
                    // A module-owned builtin type reached by qualified path (`concurrency.Shared[int]`,
                    // `net.Socket`, `ffi.int32`, `ffi.ptr`): the same type the bare name bound by an
                    // import denotes.
                    if let Some(ty) = self.qualified_builtin_ty(name, &resolved) {
                        let expected = Self::qualified_builtin_arity(name);
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
                } else {
                    self.error(span, format!("module '{module}' has no type '{name}'"));
                    Ty::Unknown
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
