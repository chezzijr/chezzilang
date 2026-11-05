//! Checker: static, variant, alias and qualified-struct calls, scalar casts, call resolution
//! records.
//! Split out of `src/checker/expr.rs`. `super::*` is the parent module.

use super::setup::{HeadBinding, TypeHead};
use super::*;

impl Checker {
    /// Type-check a call through a type, `Type.method(args)`: the path value `Type.method` applied.
    /// `th` is the type head; `tname` its display name. Reads the sig from `type_member_fn`, so an
    /// instance method takes its receiver first (`P.get(p)`); rejects a refused member or an unknown
    /// method. The enclosing type's params (`targs`, from `Box[int].empty()`) AND the method's OWN
    /// `[U]` params (`mtargs`, from the combined `Box[int].make[U](x)`) compose into ONE by-name
    /// substitution map: enclosing seeded from `targs`, method from `mtargs`, then `hint` (a `let`/return
    /// annotation, `b: Box[int] = Box.empty()`) seeds any still-free ENCLOSING param, the rest inferred by
    /// unifying the declared param types against the argument types (like a generic free fn). A method-OWN
    /// `[U]` or ENCLOSING param left un-inferred (no turbofish, no arg binding, no hint) degrades to
    /// `Ty::Unknown`: a hole, which the binding check judges on the statement that stores the result
    /// (TICKET-238). Mirrors the instance-method arms minus the receiver.
    #[allow(clippy::too_many_arguments)] // enclosing key/name + method + args + enclosing & method targs + span + hint
    pub(super) fn infer_static_call(
        &mut self,
        th: &TypeHead,
        // How the TYPE was spelled at this call site — bare (`Holder`) or module-qualified
        // (`lib.Holder`, or an alias). Diagnostics only, and that includes the witness pin advice,
        // which must name a spelling that compiles.
        tname: &str,
        method: &str,
        args: &[Expr],
        targs: &[Ty],
        mtargs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        // The call through a type is the path's value applied: `type_member_fn` is the value
        // side's answer, so an instance method takes its receiver first (`P.get(p)`).
        // An overloaded name (several `convert`s): the argument's type picks the overload first,
        // and the call binds against that one's sig. The compiler reads the pick off the table.
        let pick = match self.head_overloads(th, method) {
            Some(set) => {
                let Some(k) = self.pick_overload(&set, tname, method, args, span) else {
                    return Ty::Unknown;
                };
                self.record_overload_call(method, &set[k].params[0], name_span, span);
                Some(k)
            }
            None => None,
        };
        let Some(pf) = self.type_member_fn_at(th, None, method, pick) else {
            self.infer_all(args);
            if !self.type_member_refusal(th, method, name_span) {
                self.error(
                    span,
                    format!("type '{tname}' has no static method '{method}'"),
                );
            }
            return Ty::Unknown;
        };
        let tps = pf.head_decl.clone();
        let mut sig = pf.sig;
        sig.type_params = sig.type_params.split_off(pf.head_params);
        // Editor hover (probe-gated no-op): record the static method's declared call signature at the
        // method-name token (`Foo.default()` → "fn() -> Foo"), mirroring the free-fn convention
        // (declared `FnSig` params/ret verbatim). Emits no error, changes no checking result.
        if self.hover_probe.is_some() {
            let fty = crate::checker::fn_value_ty(&sig);
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
        // A method-level turbofish (`Box.empty[int]()`) on a static method that declares NO own
        // `[U]` is an arity error: it takes no method type arguments.
        if !mtargs.is_empty() && sig.type_params.is_empty() {
            self.infer_all(args);
            self.error(
                span,
                format!(
                    "method '{method}' takes no type argument(s) (it declares no own type parameters)"
                ),
            );
            return Ty::Unknown;
        }
        let Some(bound) = self.bind_sig(&sig, method, args, 0, span) else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        // Non-generic static (no enclosing AND no method params): the simple substitution-free path.
        if tps.is_empty() && sig.type_params.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{tname}' takes no type arguments"));
            }
            self.check_args(method, &sig.params, args, span);
            return sig.ret;
        }
        // Generic: ONE by-name substitution map over BOTH the enclosing type's params and the
        // method's own `[U]` params. Seed each from its respective turbofish, then infer the rest by
        // unifying the declared param types (which may carry either set of `Ty::Param`s) against the
        // argument types — exactly like the struct ctor + a generic free fn.
        let mut sub = self.seed_targs(tname, &tps, targs, span);
        let msub = self.seed_targs(method, &sig.type_params, mtargs, span);
        sub.extend(msub);
        let all_tps: Vec<TyParam> = tps.iter().chain(sig.type_params.iter()).cloned().collect();
        let (owned, seed) = self.slot_seed(&sub, hint, &sig.ret);
        let (arg_tys, passes) =
            self.infer_generic_arg_tys(args, &sig.params, &owned, &seed, &all_tps);
        if arg_tys.len() != sig.params.len() {
            self.check_arity(method, sig.params.len(), args, span);
        }
        for (decl, actual) in sig.params.iter().zip(&arg_tys) {
            self.unify_arg(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        self.recover_iter_elems(&sig.type_params, &mut sub, span);
        self.recover_index_args(&sig.type_params, &mut sub, span);
        // Expected-type checking-mode: a `let`/return annotation seeds any ENCLOSING type param the
        // arguments left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing)
        // against the hint — so `b: Box[int] = Box.empty()` pins the return-only `T`. Runs AFTER
        // arg-unification + iter/index recovery and BEFORE the per-arg check + bounds enforcement, so
        // precedence stays turbofish > args > annotation (`seed_from_hint`'s `unify` binds only a param
        // still FREE) and bounds enforce against the hint-pinned concrete type (mirrors the free-fn
        // path, `infer_generic_call`).
        seed_from_hint(hint, &sig.ret, &mut sub);
        self.widen_targs_from_hint(
            hint,
            &sig.ret,
            &all_tps,
            &sig.params,
            &arg_tys,
            !targs.is_empty() || !mtargs.is_empty(),
            &mut sub,
            span,
        );
        for (i, (decl, (actual, arg))) in
            sig.params.iter().zip(arg_tys.iter().zip(args)).enumerate()
        {
            let expected = subst(decl, &sub);
            self.check_generic_arg(method, &expected, actual, arg, passes[i]);
        }
        self.enforce_bounds(&tps, &all_tps, &sub, span);
        self.enforce_bounds(&sig.type_params, &all_tps, &sub, span);
        // Conditional method: a STATIC method may carry a receiver-param `where` bound (naming the
        // enclosing type's own param) too — `fn_sig` is shared and records it regardless of `self`.
        // Enforce it against the enclosing param's inferred concrete type here (the sub map already
        // holds `{T -> concrete}`), mirroring the instance-method dispatch arms. Without this a
        // conditional factory `Box.of(Q(1))` (Q non-Comparable) would be silently accepted. No-op
        // when `where_bounds` is empty. (The non-generic fast path above cannot carry receiver
        // bounds: `where_bounds` non-empty ⟹ the enclosing type has params ⟹ `tps` non-empty.)
        self.enforce_bounds(&sig.where_bounds, &all_tps, &sub, span);
        // M24 Task 5 — half two of the contract for a STATIC method that declares its own witnessed
        // `[T]` (`Holder.build(c)`). Recorded BEFORE the degrade below so an un-inferable `T` reports
        // "is not determined here" (its own diagnostic) rather than looking bound to `Unknown`.
        // Only the METHOD's params can be witnessed, so the ENCLOSING type's `tps` are never here.
        if !sig.witness_params.is_empty() {
            let wparams = sig.witness_params.clone();
            // The pin goes AFTER the member name (`Holder.build[Counter]()`); the bare
            // `build[Counter](...)` the free spelling suggests is read as a free call.
            let recv = WitnessCallee::Dotted(tname.to_string());
            self.record_witness_call(method, &wparams, &sub, name_span, span, recv);
        }
        // A type param still un-inferred, the method's own or the enclosing type's, is a hole
        // (`Box.new()` is `Box[?]`): the statement that stores it is rejected (TICKET-238).
        for tp in all_tps.iter() {
            sub.entry(tp.name.clone()).or_insert(Ty::Unknown);
        }
        subst(&sig.ret, &sub)
    }

    /// Resolve a by-name call (builtin / constructor / variant / global fn). Returns `None` if
    /// `name` is none of those, so the caller can treat it as a value-call.
    /// Type-check an enum-variant constructor `Enum.Variant(args)` given its resolved `VariantInfo`.
    /// Handles both non-generic and generic enums (type args from explicit `[T]` or inferred from the
    /// payload). Shared by the qualified-call fast path and reachable only once the `(enum, variant)`
    /// pair is known.
    #[allow(clippy::too_many_arguments)] // variant shape + call args + span + expected-type hint
    pub(super) fn infer_variant_call(
        &mut self,
        v: &VariantInfo,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        // DEC-017 — `Ok()` with no argument is `Result[nil, E]`'s success value (a named interim,
        // TICKET-228 D2).
        if args.is_empty() && v.enum_name == "Result" && name == "Ok" {
            return Ty::result_e(Ty::Nil, Ty::Unknown);
        }
        let tps = self
            .enum_type_params
            .get(&v.enum_name)
            .cloned()
            .unwrap_or_default();
        // Editor hover (probe-gated no-op): record the variant's ctor signature at the variant-name
        // token (`Col.Val(3)` → "fn(int) -> Col"). The enum's declared `Ty::Param`s are preserved in
        // the return type, so a generic variant Displays "fn(T) -> Box[T]". Emits no error.
        if self.hover_probe.is_some() {
            let targs_disp: Vec<Ty> = tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect();
            let fty = Ty::Func {
                params: v.payload.clone(),
                ret: Box::new(Ty::enum_ty(v.enum_name.clone(), targs_disp)),
                labels: crate::checker::FnLabels::default(),
                variadic: crate::ast::Variadic::No,
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, None);
        }
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            self.check_args(name, &v.payload, args, span);
            return Ty::enum_ty(v.enum_name.clone(), Vec::new());
        }
        // Generic enum: type arguments come from explicit call-site args (`Tree.Node[int](…)`) when
        // given, else are inferred by unifying the variant's declared payload types (which contain
        // the enum's `Ty::Param`s) against the argument types, then check each argument against the
        // substituted payload.
        let mut sub = self.seed_targs(name, &tps, targs, span);
        let (owned, seed) = self.slot_seed(
            &sub,
            hint,
            &Ty::enum_ty(v.enum_name.clone(), param_shape(&tps)),
        );
        let (arg_tys, passes) = self.infer_generic_arg_tys(args, &v.payload, &owned, &seed, &tps);
        if arg_tys.len() != v.payload.len() {
            self.check_arity(name, v.payload.len(), args, span);
        }
        for (decl, actual) in v.payload.iter().zip(&arg_tys) {
            self.unify_arg(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: an annotation (`let`/return/param `Enum[int]`) seeds any type
        // param the args left FREE — unify the declared enum SHAPE (Param-bearing) against the hint
        // AFTER arg-unification, so turbofish/args win and the seed only breaks a genuine deadlock.
        seed_from_hint(
            hint,
            &Ty::enum_ty(v.enum_name.clone(), param_shape(&tps)),
            &mut sub,
        );
        self.widen_targs_from_hint(
            hint,
            &Ty::enum_ty(v.enum_name.clone(), param_shape(&tps)),
            &tps,
            &v.payload,
            &arg_tys,
            !targs.is_empty(),
            &mut sub,
            span,
        );
        for (i, (decl, (actual, arg))) in v.payload.iter().zip(arg_tys.iter().zip(args)).enumerate()
        {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg, passes[i]);
        }
        self.enforce_bounds(&tps, &tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::enum_ty(v.enum_name.clone(), targs_out)
    }

    /// `module.Alias(args)` through an exported alias whose `target` is a struct
    /// (TICKET-172): the target's constructor, with the alias body's pinned type arguments. An alias
    /// that pins its arguments takes no more, exactly like the bare alias constructor.
    pub(super) fn infer_alias_ctor_call(
        &mut self,
        target: &Ty,
        spelled: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        let Ty::Struct(key, head_targs) = target else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let Some(targs) = self.written_head_args(
            spelled,
            self.type_param_count(key),
            Some(head_targs.clone()),
            targs.to_vec(),
            span,
        ) else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let targs = targs.as_slice();
        let Some(info) = self.struct_shape(key).cloned() else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        self.infer_qualified_struct_call(&info, spelled, key, args, targs, span, expected)
    }

    /// Type-check a module-qualified struct constructor `module.Struct(args)` from the struct's
    /// resolved `StructInfo` (held in the defining module's `ModuleSig`), mirroring the bare struct
    /// path in `infer_named_call`. Returns a `Ty::Struct` keyed by the DECLARING module's runtime key
    /// (`key`, bare in the common case, `<dotted>::Name` on a genuine cross-module clash) so the value
    /// resolves its fields/methods against the right module's layout — and so it unifies with the
    /// declaring module's own signatures (which carry the same key).
    #[allow(clippy::too_many_arguments)] // struct shape + call args + span + expected-type hint
    pub(super) fn infer_qualified_struct_call(
        &mut self,
        info: &StructInfo,
        name: &str,
        key: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let tps = info.type_params.clone();
        let field_tys: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
        let Some(bound) =
            self.bind_call(info.field_slots.as_deref(), name, args, targs.len(), span)
        else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            // Struct ctor float fields are coerced per-field by the backend's `NewStruct` site.
            self.check_args(name, &field_tys, args, span);
            return Ty::strukt(key.to_string());
        }
        let mut sub = self.seed_targs(name, &tps, targs, span);
        let (owned, seed) =
            self.slot_seed(&sub, hint, &Ty::Struct(key.to_string(), param_shape(&tps)));
        let (arg_tys, passes) = self.infer_generic_arg_tys(args, &field_tys, &owned, &seed, &tps);
        self.check_ctor_arity(
            name,
            &tps,
            &info.fields,
            &info.defaulted_fields,
            targs,
            args,
            span,
        );
        for (decl, actual) in field_tys.iter().zip(&arg_tys) {
            self.unify_arg(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: a `let`/return/param annotation (`Heap[int]`) seeds any type
        // param the args left FREE, BEFORE the deadlock probe — so the annotation breaks the
        // `Heap([], fn(a, b): a < b)` deadlock (it pins `T`, which then pins the closure params).
        seed_from_hint(
            hint,
            &Ty::Struct(key.to_string(), param_shape(&tps)),
            &mut sub,
        );
        self.widen_targs_from_hint(
            hint,
            &Ty::Struct(key.to_string(), param_shape(&tps)),
            &tps,
            &field_tys,
            &arg_tys,
            !targs.is_empty(),
            &mut sub,
            span,
        );
        // Same un-inferable closure-param deadlock guard as the bare struct-ctor / free-fn paths
        // (`Heap([], fn(a, b): a < b)` via a module-qualified ctor): report the cause and bind the
        // params to Unknown BEFORE the per-arg closure body is checked, so it doesn't leak a
        // misleading "cannot compare T and T" from inside the lambda.
        self.report_uninferable_closure_params(name, &tps, &field_tys, args, &mut sub, span);
        for (i, (decl, (actual, arg))) in field_tys.iter().zip(arg_tys.iter().zip(args)).enumerate()
        {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg, passes[i]);
        }
        self.enforce_bounds(&tps, &tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Struct(key.to_string(), targs_out)
    }

    #[allow(clippy::too_many_arguments)] // call name/args/targs/spans + enum-qual + expected-type hint
    /// TICKET-180 — record what the name head `id` denotes. A compiler-synthesized node
    /// ([`crate::ast::NodeId::SYNTH`]) is never recorded, and neither is the generic-arg prepass (a
    /// closure body is walked there with its params unbound, DEC-025). Two different answers for one
    /// node are a hard `TableConflicts` error ([`crate::checker::record_call_table_entry`]).
    /// Whether this walk may write a per-node side table ([`Self::resolve_path`], the call
    /// plans of [`Self::bind_call`]): never for a synthesized node, the generic-arg prepass, or the
    /// return-inference walk, whose scope is not the final one.
    pub(super) fn records_node(&self, id: crate::ast::NodeId) -> bool {
        id.0 != crate::ast::NodeId::SYNTH.0 && !self.generic_arg_prepass && !self.resolving_returns
    }

    /// TICKET-184 — whether a callee resolved to `r` never returns: only the `panic` builtin. A fn
    /// call never diverges to the checker, however its body ends, and `std.os.exit` is an ordinary
    /// fn (TICKET-263, Go `os.Exit`). Never a name test: a user fn, method, local or parameter
    /// named `panic` returns normally.
    pub(super) fn resolution_diverges(&self, r: &Resolution) -> bool {
        match r {
            Resolution::Builtin(b) => *b == Builtin::Panic,
            Resolution::Fn { .. }
            | Resolution::ModuleMember { .. }
            | Resolution::Local
            | Resolution::Global { .. }
            | Resolution::StructCtor(_)
            | Resolution::Provider
            | Resolution::ParamMethodFn { .. }
            | Resolution::IndexCall
            | Resolution::Variant { .. }
            | Resolution::MethodFn { .. }
            | Resolution::NativeStatic(_)
            | Resolution::VariantFn { .. }
            | Resolution::WitnessStatic(_)
            | Resolution::Module(_)
            | Resolution::Member
            | Resolution::PatBinding
            | Resolution::PatStruct(_)
            | Resolution::Decode(_) => false,
        }
    }

    /// TICKET-184 — whether `e` is a call whose resolved callee never returns (the divergence oracle
    /// of `flow::stmt`). Reads what [`Self::resolve_path`] recorded for the callee.
    pub(super) fn call_diverges(&self, e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Call { callee, .. }
            if self.callee_diverges.get(&(self.graph_module_idx, callee.id.0)) == Some(&true))
    }

    /// The [`Resolution::Fn`] of a module-level fn bound as `name` in the current module.
    pub(super) fn fn_resolution(&self, name: &str) -> Resolution {
        let (module, name) = self
            .fn_homes
            .get(name)
            .cloned()
            .unwrap_or((self.graph_module_idx, name.to_string()));
        Resolution::Fn { module, name }
    }

    /// Record `Resolution::Variant` for a variant pattern head `Enum.V`.
    pub(super) fn record_variant(
        &mut self,
        id: crate::ast::NodeId,
        enum_key: &str,
        variant: &str,
        span: Span,
    ) {
        let r = Resolution::Variant {
            enum_key: enum_key.to_string(),
            variant: variant.to_string(),
        };
        self.record_pattern_head(id, r, span);
    }

    /// What a bare value head that a scope holds names: a scope >= 1 binding, a module-level fn
    /// slot, a whole-module import, or another module-scope value.
    pub(super) fn value_head_resolution(&self, name: &str) -> Resolution {
        match self.head_binding(name) {
            HeadBinding::Local => Resolution::Local,
            _ if self.functions.contains_key(name) => self.fn_resolution(name),
            HeadBinding::Module => match self
                .imported_modules
                .get(name)
                .and_then(|mid| self.module_idx_of.get(mid))
            {
                Some(&idx) => Resolution::Module(idx),
                None => Resolution::Global {
                    module: self.graph_module_idx,
                    name: name.to_string(),
                },
            },
            HeadBinding::TypeParam | HeadBinding::Global | HeadBinding::Unbound => {
                Resolution::Global {
                    module: self.graph_module_idx,
                    name: name.to_string(),
                }
            }
        }
    }

    /// What `obj.name` names when `obj` is not a type head: a member of a whole-module import when
    /// `obj` is that import's name, else a member of a value.
    pub(super) fn member_resolution(&self, obj: &Expr, name: &str) -> Resolution {
        if let ExprKind::Ident(m) = &obj.kind
            && !self.head_is_value(m)
            && let Some(mid) = self.imported_modules.get(m)
            && let Some(&module) = self.module_idx_of.get(mid)
        {
            return Resolution::ModuleMember {
                module,
                name: name.to_string(),
            };
        }
        Resolution::Member
    }
}
