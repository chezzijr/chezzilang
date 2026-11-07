//! Checker: struct constructors, plain fn calls, own type params, method hovers, native handle
//! methods.
//! Split out of `src/checker/expr.rs`. `super::*` is the parent module.

use super::expr::NativeHandleMethod;
use super::*;

impl Checker {
    /// A bare struct constructor `S(args)` / `Alias(args)` the rule table answered
    /// `StructCtor(key)` for. Type arguments come from explicit call-site args (`S[int](…)`), an
    /// alias's pinned ones, else unifying the declared field types against the argument types.
    pub(super) fn infer_struct_ctor_call(
        &mut self,
        name: &str,
        key: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let key = key.to_string();
        let pinned = self.alias_struct_head(name).map(|(_, p)| p);
        let Some((tps, fields, defaulted)) = self.structs.get(&key).map(|i| {
            (
                i.type_params.clone(),
                i.fields.clone(),
                i.defaulted_fields.clone(),
            )
        }) else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let Some(targs) = self.written_head_args(
            name,
            self.type_param_count(&key),
            pinned,
            targs.to_vec(),
            span,
        ) else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let targs = targs.as_slice();
        let slots = self.structs.get(&key).and_then(|i| i.field_slots.clone());
        let Some(bound) = self.bind_call(slots.as_deref(), name, args, targs.len(), span) else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        let field_tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
        if tps.is_empty() {
            // Struct ctor float fields are coerced per-field by the `NewStruct` site.
            self.check_args(name, &field_tys, args, span);
            return Ty::strukt(key);
        }
        // Generic struct: type arguments come from explicit call-site args (`S[int](…)`)
        // when given, else are inferred by unifying the declared field types (which
        // contain the struct's `Ty::Param`s) against the argument types.
        let mut sub = self.seed_targs(name, &tps, targs, span);
        let (owned, seed) = self.slot_seed(&sub, hint, &Ty::Struct(key.clone(), param_shape(&tps)));
        let (arg_tys, passes) = self.infer_generic_arg_tys(args, &field_tys, &owned, &seed, &tps);
        self.check_ctor_arity(name, &tps, &fields, &defaulted, targs, args, span);
        for (decl, actual) in field_tys.iter().zip(&arg_tys) {
            self.unify_arg(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: a `let`/return/param annotation (`Heap[int]`) seeds
        // any type param the args left FREE, BEFORE the deadlock probe — so the annotation
        // breaks the `Heap([], fn(a, b): a < b)` deadlock (it pins `T`, which in turn pins
        // the comparator's closure params via the per-arg checking-mode re-infer below).
        seed_from_hint(hint, &Ty::Struct(key.clone(), param_shape(&tps)), &mut sub);
        self.widen_targs_from_hint(
            hint,
            &Ty::Struct(key.clone(), param_shape(&tps)),
            &tps,
            &field_tys,
            &arg_tys,
            !targs.is_empty(),
            &mut sub,
            span,
        );
        // Detect the un-inferable closure-param deadlock (e.g. `Heap([], fn(a,b): a<b)`)
        // BEFORE the per-arg check, so it reports the cause instead of leaking a
        // "cannot compare T and T" from inside the lambda. Binds the params to Unknown.
        self.report_uninferable_closure_params(name, &tps, &field_tys, args, &mut sub, span);
        self.enforce_bounds(&tps, &tps, &sub, span);
        // TICKET-238 -- the same loop-back a generic call runs: a closure argument's body solves a
        // type param on this statement (`M(1, fn(x): x * 2)` is `M[int, int]`).
        self.recover_return_only_params(
            name, &field_tys, &arg_tys, &passes, args, &field_tys, &tps, &mut sub, span,
        );
        let targs = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Struct(key, targs)
    }

    /// A call of the module-level fn `name` the rule table answered `Fn` for, through its slot.
    pub(super) fn infer_fn_call(
        &mut self,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let Some(sig) = self.functions.get(name).cloned() else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        // TICKET-186: keyword labels through a module slot bind only where
        // `labels_certain` says the slot holds this fn (K5).
        if let Some(named) = self
            .call_ctx
            .as_ref()
            .filter(|c| !c.named.is_empty())
            .map(|c| c.named.clone())
            && self.labels_certain(name).is_none()
        {
            self.consume_named();
            for a in args {
                self.infer_value(a);
            }
            for (_, v) in &named {
                self.infer_value(v);
            }
            self.error(span, super::globals::kw_ambiguous_msg(name));
            return sig.ret.clone();
        }
        let Some(bound) = self.bind_sig(&sig, name, args, 0, span) else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        // A generic function: infer its type parameters from the arguments, enforce
        // bounds, and substitute into the return type.
        if !sig.type_params.is_empty() {
            // M24 — the witness key span is the CALLEE TOKEN (`name_span`), never the
            // call node: a postfix chain's links all carry the primary expression's span, so
            // keying on it aliased two witness calls onto one entry.
            return self.infer_generic_call(
                name,
                &sig,
                args,
                targs,
                name_span,
                span,
                hint,
                WitnessCallee::Free,
            );
        }
        // Float params are coerced at the callee's prologue (compile_fn / extern).
        // Honor an optional trailing tail (`min_params < params.len()`, e.g. a native
        // `from`-imported fn with an optional arg); for plain sigs `min_params ==
        // params.len()`, so this is identical to the old exact-arity check.
        if sig.c_variadic {
            self.check_c_variadic_args(name, &sig.params, args, span);
        } else {
            self.check_args_range(name, &sig.params, sig.min_params, args, span);
        }
        sig.ret
    }

    /// Does the method `method` on receiver type `recv_ty` declare its OWN `[U]` type params? Only a
    /// user struct/enum method can; a builtin (`str`/`list`/…) member never does. Used to gate
    /// the member-level turbofish (`obj.method[A](x)`): a turbofish on anything else is an arity error.
    /// `None` when the receiver is a module with no member (and no type) `method`: a miss, which the
    /// module arm reports as one before any turbofish arity.
    pub(super) fn member_own_type_params(&self, recv_ty: &Ty, method: &str) -> Option<bool> {
        if let Ty::Module(mname) = recv_ty
            && let Some(sig) = self
                .imported_modules
                .get(mname)
                .and_then(|id| self.module_sigs.get(id))
            && sig.member(method).is_none()
            && !sig.types.contains(method)
        {
            return None;
        }
        Some(self.method_own_type_params(recv_ty, method))
    }

    fn method_own_type_params(&self, recv_ty: &Ty, method: &str) -> bool {
        match recv_ty {
            Ty::Struct(sname, _) => self
                .structs
                .get(sname)
                .and_then(|info| info.methods.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            Ty::Enum(ename, _) => self
                .enum_methods
                .get(ename)
                .and_then(|ms| ms.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            // A module-qualified fn (`geo.empty_list[int]()`): the module's own fns live in the
            // module sig, not in `structs`/`enums` — without this arm a generic module fn was told it
            // "declares no own type parameters" and its turbofish was an unreachable feature.
            Ty::Module(mname) => self
                .imported_modules
                .get(mname)
                .and_then(|id| self.module_sigs.get(id))
                .and_then(|s| s.certain_fn(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            // Reserved built-in receiver types: their harvested methods live in the re-seeded bare
            // `structs` tables (setup.rs phases 4c/5a), same as a user struct. Without these arms a
            // shipped generic method (`[1,2,3].map[int](...)`, `ex.submit[T](...)`) was told it
            // "declares no own type parameters" and its member turbofish was an arity error.
            Ty::List(_)
            | Ty::Map(_, _)
            | Ty::Set(_)
            | Ty::Shared(_)
            | Ty::RwShared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader => {
                let bare = match recv_ty {
                    Ty::List(_) => "List",
                    Ty::Map(_, _) => "Map",
                    Ty::Set(_) => "Set",
                    Ty::Shared(_) => "Shared",
                    Ty::RwShared(_) => "RwShared",
                    Ty::Atomic(_) => "Atomic",
                    Ty::AtomicInt => "AtomicInt",
                    Ty::Executor => "Executor",
                    Ty::Socket => "Socket",
                    Ty::Listener => "Listener",
                    Ty::Writer => "Writer",
                    Ty::Int
                    | Ty::Float
                    | Ty::Bool
                    | Ty::Str
                    | Ty::Bytes
                    | Ty::ByteArray
                    | Ty::Nil
                    | Ty::Func { .. }
                    | Ty::Tuple(_)
                    | Ty::Struct(..)
                    | Ty::Enum(..)
                    | Ty::Param(_)
                    | Ty::Result(..)
                    | Ty::Option(_)
                    | Ty::Channel(_)
                    | Ty::Reader
                    | Ty::Ptr
                    | Ty::Width(_)
                    | Ty::Protocol(..)
                    | Ty::Module(_)
                    | Ty::Unknown
                    | Ty::Var(_) => "Reader",
                };
                self.structs
                    .get(bare)
                    .and_then(|info| info.methods.get(method))
                    .is_some_and(|sig| !sig.type_params.is_empty())
            }
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
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Unknown
            | Ty::Var(_) => false,
        }
    }

    /// Editor hover (probe-gated no-op): record a builtin method's CALL signature at the method-name
    /// token. The builtin `*_method_sig` helpers carry NO receiver param (unlike user methods), so the
    /// sig is recorded as-is — `str.upper()`'s `[]→str` renders "fn() -> str". Pure side effect: only
    /// runs under the hover probe and emits no error / changes no checking. Mirrors the user-struct
    /// method-arm probe block (which strips the receiver first).
    pub(super) fn record_method_hover(&mut self, name_span: Span, sig: &FnSig) {
        if self.hover_probe.is_some() {
            let fty = crate::checker::fn_value_ty(sig);
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
    }

    /// Record a DECL-site method-name hover (the `dbl` in `fn dbl(self) -> int:`). Mirrors the
    /// CALL-site method hover ([`infer_method_call`]): the receiver `self` is stripped for an
    /// instance method (so `fn dbl(self) -> int` displays "fn() -> int"), but a STATIC method (no
    /// receiver) keeps all its params. Probe-gated no-op like [`record_method_hover`].
    pub(super) fn record_method_decl_hover(&mut self, name_span: Span, sig: &FnSig) {
        if self.hover_probe.is_some() {
            let fty = if !sig.is_static && !sig.params.is_empty() {
                crate::checker::fn_value_ty(&sig.receiverless())
            } else {
                crate::checker::fn_value_ty(sig)
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
    }

    /// Shared dispatch prefix for the reserved native-handle method arms: lookup the harvested sig
    /// (`native_handle_method`), record the editor hover, and — if the sig carries its OWN `[U]` type
    /// params — PREPEND the concrete receiver and route through the generic solver (mirrors the `List`
    /// arm). Extracting this makes it structurally impossible for a handle arm to forget the generic
    /// branch (the forgettable hazard); each arm keeps its residual special case inline (Atomic numeric
    /// gate, Executor `submit` capture-floor, RwShared `read` R-recovery) by matching on the result.
    /// `read(fn(T) -> R) -> R` on a `Shared` / `RwShared`: R is the closure argument's actual
    /// return type (`Unknown` on an arity error). `check_args` already inferred the closure and
    /// emitted any body errors; this is a RECOVERY-ONLY re-inference, so it rolls its own
    /// diagnostics back to avoid reporting those errors twice.
    pub(super) fn recover_read_ret(&mut self, args: &[Expr]) -> Ty {
        let mark = self.diag_mark();
        let recovered = args.first().map(|arg| self.infer_value(arg));
        self.diag_rollback(mark);
        match recovered {
            Some(Ty::Func { ret, .. }) => *ret,
            _ => Ty::Unknown,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_native_handle_method(
        &mut self,
        key: &str,
        method: &str,
        targs: &[Ty],
        name_span: Span,
        obj_ty: &Ty,
        type_args: &[Ty],
        args: &[Expr],
        span: Span,
        // Forwarded to the generic solver for the same reason as everywhere else on this path: a
        // harvested native method carrying its own `[U]` may have it only in the return type.
        hint: Option<&Ty>,
    ) -> NativeHandleMethod {
        let Some(sig) = self.native_handle_method(key, method, targs) else {
            return NativeHandleMethod::Miss;
        };
        self.record_method_hover(name_span, &sig);
        let Some(bound) = self.bind_sig(&sig, method, args, 0, span) else {
            return NativeHandleMethod::Generic(Ty::Unknown);
        };
        let args: &[Expr] = &bound;
        // A harvested method carrying its OWN `[U]` params needs the generic solver (the harvest
        // STRIPS `self`, so PREPEND the concrete receiver — mirrors the `List` arm).
        if !sig.type_params.is_empty() {
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            params.push(obj_ty.clone());
            params.extend(sig.params.iter().cloned());
            return NativeHandleMethod::Generic(self.infer_generic_method(
                method,
                &params,
                &[],
                &sig.ret,
                &sig.type_params,
                &[], // a native method never takes a witness (no user body to construct in)
                obj_ty,
                type_args,
                args,
                // The harvest strips `self` and the receiver is prepended just above, so the
                // declaration's own minimum shifts by one to match `params`.
                sig.min_params.saturating_add(1),
                name_span,
                span,
                hint,
            ));
        }
        self.check_args_range(method, &sig.params, sig.min_params, args, span);
        NativeHandleMethod::Concrete(Box::new(sig), args.to_vec())
    }

    /// The plain reserved-handle arm shared by `Socket`/`Listener`/`Writer`/`Reader`: no element type
    /// to substitute (`&[]`) and no per-arm residual — a Concrete sig (bound and checked) is just
    /// `sig.ret`. Routed through `resolve_native_handle_method` so a future bodied/generic method on
    /// any of these handles auto-infers instead of silently missing dispatch.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn infer_fixed_native_handle_method(
        &mut self,
        key: &str,
        method: &str,
        name_span: Span,
        obj_ty: &Ty,
        type_args: &[Ty],
        args: &[Expr],
        span: Span,
    ) -> Ty {
        match self.resolve_native_handle_method(
            key,
            method,
            &[],
            name_span,
            obj_ty,
            type_args,
            args,
            span,
            // No enclosing annotation reaches this fixed-arity native path.
            None,
        ) {
            NativeHandleMethod::Generic(t) => t,
            NativeHandleMethod::Concrete(sig, _) => sig.ret,
            NativeHandleMethod::Miss => {
                self.infer_all(args);
                let names = self.method_names(key);
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
        }
    }
}
