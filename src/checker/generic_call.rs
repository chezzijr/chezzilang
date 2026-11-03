//! Checker: generic and witness calls: `infer_generic_call`, `infer_generic_method`, generic fn-
//! value args.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::expr::ArgPass;
use super::proto::unbound_params;
use super::*;
use std::collections::HashMap;

impl Checker {
    /// M24 — `T.method(args)` where `T` is an in-scope generic type PARAMETER: the static-witness
    /// call. The instance twin is `infer_method_call`'s `Ty::Param` arm; this one differs in that
    /// there is no receiver (every `msig.params` slot is a real argument) and `Self` maps to
    /// `Ty::Param(T)` rather than to a receiver type.
    ///
    /// Accepted only when BOTH hold, because both are what the compiler can actually lower:
    /// * one of `T`'s bounds declares `method` as a **static** requirement, and
    /// * `T`'s hidden `$w:T` witness binding is reachable here ([`Checker::witness_scope`]) — the
    ///   declaring body, or (Task 4) any nested body inside it, which captures it.
    ///
    /// Anything else keeps the pre-M24 "generics are erased" diagnostic.
    pub(super) fn infer_witness_static_call(
        &mut self,
        tname: &str,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let in_scope = self.witness_scope.iter().any(|w| w == tname);
        // A type param declared by the ENCLOSING TYPE (`struct Bx[T]`) can never carry a witness,
        // WHATEVER its bounds: the witness lives in the calling frame, and a `Bx` value has no
        // frame — so this verdict is checked BEFORE the bound lookup. It used to sit after it, so an
        // UNBOUNDED enclosing param was told to "bound it by a protocol", which is advice that
        // produces a second rejection. A method may not reuse its host's type-param name
        // (`method_type_param_shadowing_struct_param_rejected`), so a hit here is always the host's.
        // The `in_scope` guard keeps a legitimately witnessed member param out of this arm.
        if !in_scope && let Some(host) = self.enclosing_type_declaring(tname) {
            self.infer_all(args);
            self.error(
                span,
                format!(
                    "`{tname}.{method}(...)`: '{tname}' is a type parameter of the enclosing type '{host}', which carries no hidden type witness — the concrete type is erased once a '{host}' value exists, so only a value could hold it, and no bound on '{host}' can change that. Declare the type parameter on the MEMBER instead (`fn {method}_of[{tname}: <bound>](self, ...)`, whose witness rides on the call), or pass a factory function (a `fn(...) -> {tname}` parameter/field)"
                ),
            );
            return Ty::Unknown;
        }
        let Some((_, msig, map)) = self.bound_method(tname, method, &|s| s.is_static) else {
            self.infer_all(args);
            // Name a protocol that ALREADY declares this static method rather than inventing a
            // signature for it — the invented `fn {method}(...) -> Self` was simply wrong for a
            // requirement returning anything else. rustc does the same ("the following trait defines
            // an item `tag`, perhaps you need to restrict type parameter `Item` with it").
            // `p` is the qualified IDENTITY key (`<module-key>::Name`, TICKET-027); render the BARE
            // display name in this advice, matching every other protocol-name diagnostic.
            let mut hosts: Vec<String> = self
                .protocols
                .keys()
                .filter(|p| {
                    self.protocol_method_sig(p, method)
                        .is_some_and(|s| s.is_static)
                })
                .map(|p| crate::base::words::bare_display(p))
                .collect();
            hosts.sort();
            let advice = match hosts.first() {
                Some(p) => format!("bound '{tname}' by '{p}', which declares a static '{method}'"),
                None => format!(
                    "no protocol in scope declares a static '{method}' — declare one and bound '{tname}' by it"
                ),
            };
            self.error(
                span,
                format!(
                    "cannot call a static method through the generic type parameter '{tname}' (`{tname}.{method}`): no bound on '{tname}' declares a static method '{method}', so generics are erased here and there is no concrete type to dispatch to — {advice}, call the concrete type's static method directly (e.g. `SomeType.{method}(...)`), or pass a factory function (a `fn(...) -> {tname}` parameter)"
                ),
            );
            return Ty::Unknown;
        };
        if !in_scope {
            self.infer_all(args);
            // Reached when the `enclosing_type_declaring` arm above cannot see the host — notably a
            // nested `fn` inside a method of `struct Bx[T]`, where `current_self_ty` is reset. So
            // this message must ALSO be true for a type-declared param: it states the rule, and
            // never claims the current body carries a witness.
            self.error(
                span,
                format!(
                    "`{tname}.{method}(...)`: no hidden type witness for '{tname}' is reachable here. One exists only where '{tname}' is declared by a FUNCTION or a MEMBER (`fn f[{tname}: <bound>](...)`) — in that body and in any closure, `spawn:`/`defer:` block or nested `fn` inside it. A type parameter of an enclosing TYPE (`struct Bx[{tname}]`) never has one: the concrete type is erased once a value exists. Declare '{tname}' on the function or member, or pass a factory function (a `fn(...) -> {tname}` parameter)"
                ),
            );
            return Ty::Unknown;
        }
        // A STATIC requirement has NO receiver slot, so every declared param is a real argument.
        let expected: Vec<Ty> = msig.params.iter().map(|t| subst(t, &map)).collect();
        let slots = super::expr::callable_slots(&msig.labels, expected.len(), expected.len(), None);
        let Some(bound) = self.bind_call(Some(&slots), method, args, 0, span) else {
            return Ty::Unknown;
        };
        self.check_args(method, &expected, &bound, span);
        subst(&msig.ret, &map)
    }

    /// M24 — record (or reject) the witness arguments a call to `name` needs. Runs AFTER every
    /// type-param recovery pass in [`Self::infer_generic_call`], because a param recovered only by
    /// the loop-back would otherwise look un-determined here.
    ///
    /// Task 3 — the callee's module does NOT matter here. The entry is keyed by the CALLING module
    /// ([`crate::checker::witness_key`]), which is the module the compiler is emitting when it looks
    /// it up; the callee's own requirement rides on its [`FnSig::witness_params`], which crosses the
    /// boundary inside its `ModuleSig`. So a `from`-imported callee (`reset(c)`) and a qualified one
    /// (`lib.reset(c)`) record exactly like a local one. M24-5 — a `defer`/`spawn` STATEMENT TARGET
    /// records here too: `compile_defer`/`compile_spawn` thread the witness at their own emit sites,
    /// widening `Op::DeferCall`/`DeferMethod`/`SpawnCall`/`SpawnRecv`'s `argc` exactly as a plain
    /// call widens `Op::Call`'s.
    ///
    /// `key_span` is the [`crate::checker::witness_key_span`] of this call site (the member-name
    /// token for a `Field` callee, the call node otherwise) and is a TABLE KEY only; every
    /// diagnostic anchors on `span`, the call node — the two must never be conflated (`49bd9f80`).
    pub(super) fn record_witness_call(
        &mut self,
        name: &str,
        wparams: &[String],
        sub: &HashMap<String, Ty>,
        key_span: Span,
        span: Span,
        recv: WitnessCallee,
    ) {
        let mut srcs = Vec::with_capacity(wparams.len());
        for w in wparams {
            // Presence in `sub` is NOT enough: `enforce_bounds` silently SKIPS a param missing from
            // the map, and a param bound to a partly-`Unknown` type has no runtime identity either.
            let concrete = sub.get(w).filter(|t| ty_fully_concrete(t));
            match concrete {
                // The `Ty::Struct`/`Ty::Enum` name IS the runtime identity key (`<module-key>::Name`,
                // bare for a std/native type) — the checker and the compiler derive it from the same
                // `resolver::module_keys`, so this is exactly what `do_static_call` resolves.
                Some(Ty::Struct(key, _) | Ty::Enum(key, _)) => {
                    srcs.push(WitnessSrc::Concrete(key.clone()));
                }
                Some(other) => {
                    let other = other.clone();
                    self.error(
                        span,
                        format!(
                            "type parameter '{w}' of '{name}' is bound to {other}, which cannot host a \
                             static method — only a struct or an enum can"
                        ),
                    );
                    return;
                }
                // Bound to the CALLER's own still-abstract type param — FORWARDING (slice 2). The
                // caller's `$w:p` binding becomes the argument, but ONLY when it is reachable here:
                // `witness_scope` is empty in a body that neither declares `p` nor is nested inside
                // one that does. Forwarding from such a body would push a name the callee reads as
                // an identity key (or nothing at all), so it stays an error.
                None if matches!(sub.get(w), Some(Ty::Param(_))) => {
                    let Some(Ty::Param(p)) = sub.get(w) else {
                        unreachable!("guarded by the arm's own pattern")
                    };
                    let p = p.clone();
                    if !self.witness_scope.contains(&p)
                        && self.enclosing_type_declaring(&p).is_none()
                        && self.type_params.contains_key(&p)
                    {
                        let pin = match &recv {
                            WitnessCallee::Free => format!("`{name}[{p}](...)`"),
                            WitnessCallee::Dotted(prefix) => format!("`{prefix}.{name}[{p}](...)`"),
                            WitnessCallee::Member => format!("`<receiver>.{name}[{p}](...)`"),
                        };
                        self.error(
                            span,
                            format!(
                                "type parameter '{w}' of '{name}' is bound to {p}, which is still \
                                 abstract here. '{p}' IS declared by the enclosing function or \
                                 member, so a hidden type witness for it exists in this frame — but \
                                 this call site does not carry it: the forwarding walk reads '{p}' \
                                 only from an explicit type argument, or from an argument naming a \
                                 parameter of this function annotated with '{p}'. Write the type \
                                 argument ({pin}), which always forwards the enclosing witness"
                            ),
                        );
                        return;
                    }
                    if !self.witness_scope.contains(&p) {
                        self.error(
                            span,
                            format!(
                                "type parameter '{w}' of '{name}' is bound to {p}, which is still \
                                 abstract here, and no hidden type witness for '{p}' is reachable \
                                 at this call site. One exists only where '{p}' is declared by a \
                                 FUNCTION or a MEMBER — in that body and in any closure, \
                                 `spawn:`/`defer:` block or nested `fn` inside it. A type \
                                 parameter of an enclosing TYPE (`struct Bx[{p}]`) never has one: \
                                 the concrete type is erased once a value exists. Declare '{p}' on \
                                 the member instead (`fn m[{p}: <bound>](self, ...)`), or take a \
                                 factory parameter (`fn(...) -> {p}`) and call that"
                            ),
                        );
                        return;
                    }
                    srcs.push(WitnessSrc::Forward(p));
                }
                None => {
                    // The suggested spelling has to be one that PARSES. A member's type arguments
                    // go on the METHOD (`h.make[Counter]()`); `make[Counter](...)` is read as a free
                    // call and answers "'make' takes no type arguments", and an annotated result
                    // does not reach a method's own `[T]` either — so neither is offered there.
                    let pin = match &recv {
                        WitnessCallee::Free => format!(
                            "pin it with a type argument (`{name}[SomeType](...)`) or an annotated result"
                        ),
                        WitnessCallee::Dotted(prefix) => format!(
                            "pin it with a type argument (`{prefix}.{name}[SomeType](...)`) or an annotated result"
                        ),
                        WitnessCallee::Member => format!(
                            "pin it with a type argument ON THE METHOD (`<receiver>.{name}[SomeType](...)`)"
                        ),
                    };
                    self.error(
                        span,
                        format!(
                            "type parameter '{w}' of '{name}' is not determined here, so its static \
                             protocol method has no concrete type to dispatch to — {pin}"
                        ),
                    );
                    return;
                }
            }
        }
        if self.harvest_keywords {
            crate::checker::record_call_table_entry(
                &mut self.witnesses.calls,
                &mut self.table_conflicts,
                crate::checker::witness_key(
                    self.graph_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    key_span,
                ),
                srcs,
                "static-witness",
                span,
            );
        }
    }

    /// PROBE for a result-only type parameter that another parameter's protocol bound needs, and
    /// bind each candidate to `Unknown` so the caller can ask whether the bound is satisfiable
    /// *given a hole there*. Returns the candidates in declaration order; the caller decides
    /// whether they are the real cause (see the call site in `infer_generic_call`).
    ///
    /// `recover_protocol_args` has already pinned every param recoverable from a witnessing
    /// method, so reaching here means the bound genuinely supplied nothing. Expected-result and
    /// explicit-type-argument inference have also run. Params mentioned by an argument slot are
    /// excluded because the closure-return loop-back later in the call may still recover them.
    ///
    /// The `Unknown` binding is load-bearing, not cleanup: `method_matches` compares through
    /// `compatible`, where `Unknown` is a wildcard, so it turns the follow-up `enforce_bounds`
    /// into exactly the question "would this conform if the param were inferable?".
    fn probe_uninferable_dependent_result_params(
        &mut self,
        sig: &FnSig,
        sub: &mut HashMap<String, Ty>,
    ) -> Vec<String> {
        let wanted: std::collections::HashSet<String> =
            sig.type_params.iter().map(|tp| tp.name.clone()).collect();
        let mut in_result = Vec::new();
        ty_collect_params(&sig.ret, Some(&wanted), &mut in_result);
        let mut in_params = Vec::new();
        for param in &sig.params {
            ty_collect_params(param, Some(&wanted), &mut in_params);
        }

        let candidates: std::collections::HashSet<String> = in_result
            .into_iter()
            .filter(|n| !in_params.contains(n) && !sub.contains_key(n))
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }

        let mut needed_by_bound = Vec::new();
        for tp in &sig.type_params {
            if !sub.get(&tp.name).is_some_and(|ty| !ty.is_unknown()) {
                continue;
            }
            for bound in &tp.bounds {
                for arg in &bound.args {
                    ty_collect_params(arg, Some(&candidates), &mut needed_by_bound);
                }
            }
        }

        // Preserve declaration order and probe each independently if multiple result params feed
        // parameterized bounds.
        let mut probed = Vec::new();
        for tp in &sig.type_params {
            if needed_by_bound.contains(&tp.name) {
                sub.insert(tp.name.clone(), Ty::Unknown);
                probed.push(tp.name.clone());
            }
        }
        probed
    }

    /// Type-check a call to a generic function: infer each type parameter from the arguments,
    /// enforce the declared bounds, and substitute into the return type. Shared by the bare callee
    /// (local or `from`-imported) and the module-qualified one (`m.f(...)`) — M24's witness record is
    /// keyed by the CALLING module either way, so the spelling makes no difference here (Task 3).
    /// `key_span` is this call site's [`crate::checker::witness_key_span`]: the call node's own span
    /// for the bare spelling, the member-name token for the module-qualified one (`lib.reset(c)`).
    /// `recv` is how the callee is SPELLED — the two spellings differ only in the pin the
    /// "not determined here" diagnostic may suggest ([`WitnessCallee`]).
    #[allow(clippy::too_many_arguments)] // the callee's sig pieces + call args + both spans + hint
    pub(super) fn infer_generic_call(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &[Expr],
        targs: &[Ty],
        key_span: Span,
        span: Span,
        hint: Option<&Ty>,
        recv: WitnessCallee,
    ) -> Ty {
        // `sig.min_params` is `ast::min_callable_params`, which returns `params.len()` whenever the
        // sig carries witness params, so a witness-taking callee keeps its full arity.
        if !(sig.min_params..=sig.params.len()).contains(&args.len()) {
            self.check_arity(name, sig.params.len(), args, span);
        }
        // Explicit call-site type arguments (`max[int](…)`) seed the substitution; remaining (or
        // all, when none given) parameters are inferred from positional arguments. `unify` only
        // binds a parameter that isn't already in the map, so explicit args take precedence and a
        // conflicting argument is caught by the per-argument check below.
        let mut subst_map: HashMap<String, Ty> =
            self.seed_targs(name, &sig.type_params, targs, span);
        let (owned, seed) = self.slot_seed(&subst_map, hint, &sig.ret);
        let (mut arg_tys, passes) =
            self.infer_generic_arg_tys(args, &sig.params, &owned, &seed, &sig.type_params);
        // Bare generic-fn args this pass could not pin YET — re-pinned once everything else has had
        // its turn (see the second pass below).
        let mut deferred_fn_args: Vec<usize> = Vec::new();
        // Clamp to the shorter of the two, exactly as the `zip` this loop replaced did: `arg_tys.len()
        // == args.len()` can be < `sig.params.len()` when the call is short an argument (the arity
        // error is already reported above and does not early-return), so nothing may index past it.
        for i in 0..sig.params.len().min(arg_tys.len()) {
            let decl = &sig.params[i];
            // The GENERIC-callee ordering: a bare same-module generic fn is prepass-typed rigid
            // (`fn(T) -> T`, its OWN free params) because the prepass has no expected hint. Re-pin its
            // `[T]` from the slot with everything bound SO FAR; when that is not enough yet, the rigid
            // type must NOT unify either — it carries no information about this call, and `unify` is
            // first-binding-wins, so binding the callee's `[U]` to that leaked `T` is permanent and a
            // LATER argument that really determines `U` could never correct it (`applyg(ident, 5)`).
            // Same shape as `infer_generic_method`'s deferral, one level up. `try_pin_…` only fires on
            // a bare-ident generic fn that pins FULLY concrete; otherwise nothing changes here.
            if self.generic_fn_value_sig(&args[i]).is_some() {
                let want = subst(decl, &subst_map);
                let free = unbound_params(&sig.type_params, &subst_map);
                if let Some(refined) =
                    self.try_pin_generic_fn_value_arg(&args[i], &want, &free, span)
                {
                    self.solve(&refined, &arg_tys[i]);
                    arg_tys[i] = refined;
                } else {
                    deferred_fn_args.push(i);
                    continue;
                }
            }
            let actual = &arg_tys[i];
            // Bug D (free-fn analog): for an unannotated CLOSURE arg, unify against a RETURN-MASKED
            // copy so only its PARAMETER positions can bind a function type param in pass 1. Its
            // prepass return may be a leaked `Ty::Param` (an unannotated body that is a nested free
            // generic call, `fn(x): ident(x)`); letting it bind here would prematurely pin the fn's
            // return-position `[U]` to that leaked param, and the loop-back in
            // `recover_return_only_params` (which only fills params still FREE) could not correct it.
            // Non-closure args unify unchanged. Mirrors `infer_generic_method`.
            if passes[i].masks_ret() {
                unify(decl, &mask_closure_ret(actual), &mut subst_map);
            } else {
                self.unify_arg(decl, &actual.clone(), &mut subst_map);
            }
        }
        // Bug 1 recovery (free-fn path only): after the masked pass-1 above has let every VALUE and
        // closure-PARAMETER arg pin its params, bind the HOF's return-only `[U]` from a bare closure
        // whose prepass return is ALREADY CONCRETE (`fn(): 5` → `int`, no leaked `Ty::Param`). This runs
        // BEFORE `report_uninferable_closure_params` so a `[U]` recoverable from such a closure RETURN is
        // not mis-reported as an un-inferable deadlock when a SIBLING closure uses the same `[U]` in
        // PARAMETER position (`pair(fn(): 5, fn(x): x + 1)` — adversarial-review bug 1). `unify` only
        // binds a param still FREE, so a sibling VALUE arg that already pinned `[U]` STILL WINS (no
        // closure-vs-value binding race, e.g. `apply(fn(x): str(x), 5, 99)` keeps `B` pinned to the
        // `sink: B` = 99 → `int` and the mismatching closure is rejected). A LEAKED-param prepass return
        // (`fn(x): ident(x)`) stays masked, deferred to the loop-back in `recover_return_only_params`.
        for (i, (decl, actual)) in sig.params.iter().zip(&arg_tys).enumerate() {
            // Gate on FULLY CONCRETE (no `Ty::Param` AND no `Ty::Unknown`, nested too), not merely
            // "contains no param": a param-dependent body prepass-types to an Unknown-CORED container
            // (`fn(x): [x]` → `List[Unknown]`), which has no `Ty::Param` — so a bare no-param gate would
            // `unify(U, List[Unknown])`, binding `U = List[Unknown]` (unify only skips a TOP-LEVEL
            // Unknown, not a nested one) and laundering `List[str]` onto it. A fully-concrete prepass
            // return (`fn(): 5` → `int`) still pre-binds here (needed so the `pair(fn(): 5, fn(x): x+1)`
            // ordering resolves); an Unknown/param-cored one defers to the loop-back's refined
            // checking-mode re-inference, which recovers the concrete type.
            if passes.get(i).is_some_and(|p| p.masks_ret())
                && matches!(actual, Ty::Func { ret, .. } if ty_fully_concrete(ret))
            {
                unify(decl, actual, &mut subst_map);
            }
        }
        // Recover element types from parameterized `Iterator[T]` bounds (bind `T` to the iterand's
        // element), then enforce every declared bound against its inferred binding.
        self.recover_iter_elems(&sig.type_params, &mut subst_map, span);
        self.recover_index_args(&sig.type_params, &mut subst_map, span);
        // …and the same recovery for a USER parameterized bound (`T: Produces[R]` pins `R` from the
        // bound type's own `produce`), so it no longer needs an annotation the way `Iterator[T]`
        // never did.
        self.recover_protocol_args(&sig.type_params, &mut subst_map);
        // Expected-type checking-mode: a `let`/return/param annotation seeds any type param the args
        // left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing) against the
        // hint — so `xs: List[int] = empty()` pins a return-only `T`, and the deadlock probe below
        // sees it bound. After arg-unification ⇒ turbofish/args win.
        seed_from_hint(hint, &sig.ret, &mut subst_map);
        // W12-9 (TICKET-106): let the expected type widen a type arg the ARGUMENTS already bound.
        self.widen_targs_from_hint(
            hint,
            &sig.ret,
            &sig.type_params,
            &sig.params,
            &arg_tys,
            !targs.is_empty(),
            &mut subst_map,
            span,
        );
        // Second pass for the deferred bare generic-fn args, placed HERE — after every sibling value
        // argument, the closure-return recovery, the `Iterator`/index recovery AND the annotation
        // hint have bound what they bind, and before `enforce_bounds` so a bound on a param this pass
        // fills is still checked. That is the same "last moment that can still pin" the method path
        // uses; it just has more sources to wait for (the method path has no hint to seed). A pin
        // replaces the rigid prepass type and unifies the CONCRETE result; one that still cannot pin
        // unifies its rigid type after all, so the existing assignability diagnostic still fires — by
        // now every real binding has already won, so the leak can no longer displace one.
        for i in std::mem::take(&mut deferred_fn_args) {
            let want = subst(&sig.params[i], &subst_map);
            let free = unbound_params(&sig.type_params, &subst_map);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, &free, span) {
                self.solve(&refined, &arg_tys[i]);
                arg_tys[i] = refined;
            } else {
                self.sentinel_slot_arg(&sig.params[i], &arg_tys[i]);
            }
            unify(&sig.params[i], &arg_tys[i].clone(), &mut subst_map);
        }
        // Same un-inferable closure-param deadlock guard as the struct-ctor path: report the cause
        // (and bind the params to Unknown) before the per-arg closure body is checked.
        self.report_uninferable_closure_params(
            name,
            &sig.type_params,
            &sig.params,
            args,
            &mut subst_map,
            span,
        );
        // An un-inferred type param and a genuinely non-conforming method BOTH surface as a bound
        // failure, and only one of them is the user's problem — so decide which by measurement, not
        // by the shape of the signature. Bind the candidates to a hole, then ask `enforce_bounds`:
        // if the bound conforms with the hole, the missing inference IS the cause; if it still
        // fails, the method could not conform for ANY instantiation and its own message is the true
        // one. Measured, the structural test alone is too wide — a wrong ARITY or a wrong PARAM
        // type leaves the param un-inferred too, and there "add a result annotation" is advice that
        // does not work (`docs/contributing/gaps.md` W8-43's neighbour table).
        let probed = self.probe_uninferable_dependent_result_params(sig, &mut subst_map);
        let before = self.errors.len();
        self.enforce_bounds(&sig.type_params, &sig.type_params, &subst_map, span);
        if self.errors.len() == before {
            for pname in probed {
                self.error(
                    span,
                    format!(
                        "cannot infer type parameter {pname} for '{name}'; add a result annotation or explicit type arguments"
                    ),
                );
            }
        }
        // Each argument must match its parameter's substituted type (catches a type param used in
        // two positions with conflicting types, e.g. `max(1, "x")`), AND recover a return-only type
        // param from an inferable closure/fn body (Bug D's loop-back, now shared with the method
        // path): the free-fn path formerly discarded the refined closure type, leaking `Ty::Param`
        // into the return so a downstream `+1`/`.upper()` was spuriously rejected.
        self.recover_return_only_params(
            name,
            &sig.params,
            &arg_tys,
            &passes,
            args,
            &sig.params,
            &sig.type_params,
            &mut subst_map,
            span,
        );
        // M24 — half two of the static-witness contract, recorded LAST: `recover_return_only_params`
        // above can still bind a param that `enforce_bounds` never saw, so anything earlier would
        // read a param as un-determined that the call actually pins.
        if !sig.witness_params.is_empty() {
            let wparams = sig.witness_params.clone();
            self.record_witness_call(name, &wparams, &subst_map, key_span, span, recv);
        }
        self.report_uninferable_result_params(
            name,
            &sig.params,
            &sig.ret,
            &sig.type_params,
            &mut subst_map,
            span,
        );
        subst(&sig.ret, &subst_map)
    }

    /// A type parameter that appears in the RETURN type and is still unbound after every inference
    /// source has run is un-inferable at this call site: nothing downstream can pin it, so the value
    /// carries a rigid `Ty::Param` incompatible with every concrete type. Rust refuses the same
    /// shape up front — `fn make<U>() -> U; let z = make();` is `E0282: type annotations needed`,
    /// and `let xs = empty();` on `fn empty<T>() -> Vec<T>` is `E0282 … for Vec<_>`.
    ///
    /// Reported at the CONSTRUCTION site rather than left to leak. The leak was never unsound —
    /// every typed USE of the value already errored (`z + 1` → *cannot apply + to U and int*,
    /// `xs.push(1)` → a message that already said "bind it at the construction site") — but a value
    /// never used in a typed position slipped through entirely, and the blame landed downstream of
    /// the call that actually needed the annotation.
    ///
    /// **Where this must stay silent, derived by running each context, not from its shape.** A
    /// DECL-SITE default copy (`fn f(x: T = mkl())`, `struct S: f: T = mkl()`) is checked once at the
    /// declaration purely to catch a wrong-typed default; the expression's real home is the
    /// synthesized provider or the splice at each call, where the enclosing generic IS bound. Firing
    /// there rejected `fn tot[U](self, xs: List[Self] = mkl())` — a declaration correct at every real
    /// call site. Same context W7-51 had to neutralize for `?`, for the same reason.
    fn report_uninferable_result_params(
        &mut self,
        name: &str,
        params: &[Ty],
        ret: &Ty,
        tps: &[TyParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        if self.decl_site_default {
            return;
        }
        let wanted: std::collections::HashSet<String> =
            tps.iter().map(|tp| tp.name.clone()).collect();
        let mut in_ret = Vec::new();
        ty_collect_params(ret, Some(&wanted), &mut in_ret);
        // RETURN-ONLY: a param that also appears in a PARAMETER slot is a different rule with a
        // different, already-tuned diagnostic. `fn tag[U](xs: List[U]) -> List[U]` called `tag([])`
        // leaves `U` unbound (an empty literal binds nothing), but each later `x.push(v)` already
        // reports it AND already names the fix — "the collection's element type is the un-inferred
        // type parameter U; bind it at the construction site with a turbofish or annotation". Firing
        // here too would add a third error saying the same thing. This rule owns only the case with
        // no parameter to blame at all.
        let mut in_params = Vec::new();
        for p in params {
            ty_collect_params(p, Some(&wanted), &mut in_params);
        }
        // Declaration order, so a multi-param signature reads left-to-right.
        let unbound: Vec<&TyParam> = tps
            .iter()
            .filter(|tp| {
                in_ret.contains(&tp.name)
                    && !in_params.contains(&tp.name)
                    && !sub.contains_key(&tp.name)
            })
            .collect();
        if unbound.is_empty() {
            return;
        }

        for tp in unbound {
            self.error(
                span,
                format!(
                    "cannot infer type parameter {} for '{name}'; add a result annotation or explicit type arguments",
                    tp.name
                ),
            );
        }
    }

    /// Infer a generic *method*'s own type parameters from the call arguments. `params`/`ret` are the
    /// method signature already substituted with the receiver struct's type arguments, so only the
    /// method's own `[U]` params remain free; `params[0]` is the receiver (bound from `obj`, not an
    /// explicit arg). `targs` are the EXPLICIT member-level turbofish (`obj.method[A, B](...)`): they
    /// seed the `[U]` params first; the rest are inferred positionally. Mirrors `infer_generic_call`.
    ///
    /// M24 Task 5 — `wparams` are the method's [`FnSig::witness_params`] (empty for every native
    /// method and for a user method that never constructs through a bound), and `key_span` is this
    /// call site's [`crate::checker::witness_key_span`] — the method-name token, so two links of one
    /// postfix chain cannot collide on the shared call span.
    #[allow(clippy::too_many_arguments)] // the method's resolved signature pieces + receiver + targs + call
    pub(super) fn infer_generic_method(
        &mut self,
        method: &str,
        params: &[Ty],
        // The PRE-substitution declared param list, receiver included — TICKET-094 defect C's
        // license must be keyed on this, never on `params` above (already substituted with the
        // receiver's own type args), or a slot written `T` on `P[float]` would widen (DEC-054).
        // Empty declines every license, which reproduces this method's behaviour before this ticket.
        declared: &[Ty],
        ret: &Ty,
        mtps: &[TyParam],
        wparams: &[String],
        recv_ty: &Ty,
        targs: &[Ty],
        args: &[Expr],
        // The declaration's `FnSig::min_params` — how few arguments it accepts INCLUDING the
        // receiver slot. `usize::MAX` for a caller with no signature to hand over (a synthesized
        // builtin sig), which collapses to the exact-arity check below.
        min_params: usize,
        key_span: Span,
        span: Span,
        // The enclosing `let`/annotation's expected type, same role as on the free-fn path: it pins
        // a type param that appears ONLY in the return. Threaded here so `v: int? = w.take(xs)`
        // solves `R` — before this the method path had no hint at all, so a result annotation could
        // not pin a return-only param and only a turbofish worked.
        hint: Option<&Ty>,
    ) -> Ty {
        // The first parameter is the receiver (bound from `obj`). A method with NO params has no
        // receiver slot — reject, mirroring the non-generic path.
        let Some((receiver, expected)) = params.split_first() else {
            self.error(
                span,
                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
            );
            self.infer_all(args);
            return Ty::Unknown;
        };
        // Trailing defaulted parameters are filled by the CALLEE, so the accepted count is a range;
        // `min_params` counts the receiver slot, which `expected` has already dropped.
        let min_args = min_params.saturating_sub(1).min(expected.len());
        if !(min_args..=expected.len()).contains(&args.len()) {
            let want = if min_args == expected.len() {
                format!("{}", expected.len())
            } else {
                format!("{min_args}-{}", expected.len())
            };
            self.error(
                span,
                format!("'{method}' expects {want} argument(s), got {}", args.len()),
            );
        }
        let dec_args = declared.split_first().map_or(&[][..], |(_, d)| d);
        // Explicit member-level turbofish seeds the `[U]` params (arity-checked); `unify` only binds
        // a param not already in the map, so an explicit targ wins and a conflicting arg is caught by
        // the per-argument check below.
        let mut mmap: HashMap<String, Ty> = self.seed_targs(method, mtps, targs, span);
        // This path has no return hint to pre-seed from: the slots come from the type args only.
        let (owned, seed) = self.slot_seed(&mmap, None, &Ty::Nil);
        let (mut arg_tys, passes) = self.infer_generic_arg_tys(args, dec_args, &owned, &seed, mtps);
        // Every other param in the instantiated signature is the CALLER's (a receiver arg `Box[B]`
        // inside `fn go[A, B]`), rigid here: pin it to itself so neither an argument nor the hint
        // can bind it (`r: (A, A) = b.pair(a)` must not turn the caller's `B` into `A`).
        let mut rigid = Vec::new();
        params
            .iter()
            .chain([ret])
            .for_each(|t| ty_collect_params(t, None, &mut rigid));
        for n in rigid {
            if !mtps.iter().any(|tp| tp.name == n) {
                mmap.entry(n.clone()).or_insert(Ty::Param(n));
            }
        }
        // A method type param may appear in the receiver position (`fn f[U](u: U)`); bind it from the
        // actual receiver type so it isn't left unresolved.
        unify(receiver, recv_ty, &mut mmap);
        // Clamp to the shorter of the two lengths — `arg_tys.len() == args.len()` can be < `expected.len()`
        // when the method is called with too few arguments. The arity error is already reported above
        // (it does not early-return), so this loop must not index `args[i]`/`arg_tys[i]` out of bounds;
        // the base `expected.iter().zip(&arg_tys)` clamped implicitly, this preserves that.
        // Bare generic-fn args whose slot could not pin them YET (see the `continue` below) — re-pinned
        // in the second pass after the loop, once the VALUE args have bound what they bind.
        let mut deferred_fn_args: Vec<usize> = Vec::new();
        for i in 0..expected.len().min(arg_tys.len()) {
            let decl = &expected[i];
            // Scope-A-through-a-method-slot: a bare same-module GENERIC fn passed as a non-closure arg
            // is prepass-typed rigid (`fn(T) -> str`) because `infer_generic_arg_tys` has no expected
            // hint. Re-pin its OWN `[T]` from the slot with everything bound SO FAR (`fn(int) -> U` for
            // `.map`; `fn(int, int) -> int` for `.fold`'s arg1 once `init` bound `U`), replacing the
            // rigid prepass type with the concrete one so pass-1 unify + the loop-back compose. This is
            // interleaved (uses the LIVE `mmap`) so `.fold`'s accumulator ordering holds. The helper only
            // fires on a bare-ident generic fn that pins FULLY concrete; otherwise `arg_tys[i]` is
            // unchanged and behavior is byte-identical.
            let want = subst(decl, &mmap);
            let free = unbound_params(mtps, &mmap);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, &free, span) {
                self.solve(&refined, &arg_tys[i]);
                arg_tys[i] = refined;
            } else if self.generic_fn_value_sig(&args[i]).is_some() {
                // …and when the slot can NOT pin it yet, the rigid prepass type (`fn(T) -> T`, the
                // CALLEE's own free params) must not unify either: it carries no information about
                // this call, and `unify` is first-binding-wins, so binding the method's `[U]` to that
                // leaked `T` is permanent — a LATER argument that really determines `U` can never
                // correct it (`b.app(id, 5)` on `fn app[U](self, f: fn(U) -> U, x: U) -> U` reported
                // "argument to 'app' has type int, expected T"). Defer it past the value args, the
                // mirror of `.fold`'s accumulator ordering for a slot the other way round. Same shape
                // as Bug D's `mask_closure_ret` for an unannotated closure.
                deferred_fn_args.push(i);
                continue;
            }
            let actual = &arg_tys[i];
            // Bug D: for a CLOSURE arg, unify against a RETURN-MASKED copy so only its PARAMETER
            // positions can bind a method type param in pass 1. Its prepass return may be a leaked
            // `Ty::Param` (an unannotated body that is a nested free generic call, `fn(x): ident(x)`);
            // letting it bind here would prematurely pin the method's return-position `[U]` to that
            // leaked param, and the loop-back below (which only fills params still FREE) could not
            // correct it. Masking defers `U` to the loop-back's checking-mode re-inference, which
            // recovers it as the CONCRETE return type. Non-closure args unify unchanged.
            if passes.get(i).is_some_and(|p| p.masks_ret()) {
                unify(decl, &mask_closure_ret(actual), &mut mmap);
            } else {
                self.unify_arg(decl, &actual.clone(), &mut mmap);
            }
        }
        // Second pass for the deferred bare generic-fn args: every value argument has now bound what
        // it binds, so their slots are as substituted as they will get. A pin here replaces the rigid
        // prepass type and unifies the CONCRETE result; one that still cannot pin unifies its rigid
        // type after all, so the existing assignability diagnostic still fires — by now every real
        // binding has already won, so the leak can no longer displace one.
        for i in std::mem::take(&mut deferred_fn_args) {
            let want = subst(&expected[i], &mmap);
            let free = unbound_params(mtps, &mmap);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, &free, span) {
                self.solve(&refined, &arg_tys[i]);
                arg_tys[i] = refined;
            } else {
                self.sentinel_slot_arg(&expected[i], &arg_tys[i]);
            }
            unify(&expected[i], &arg_tys[i].clone(), &mut mmap);
        }
        // Recover element types from `Iterator[T]` bounds, then enforce every declared bound.
        self.recover_iter_elems(mtps, &mut mmap, span);
        self.recover_index_args(mtps, &mut mmap, span);
        // Same user-parameterized-bound recovery as the free-fn path. It matters MORE here: this
        // path has no `seed_from_hint`, so before the recovery a `[R, T: Produces[R]]` METHOD could
        // not be pinned by a result annotation either — only turbofish worked.
        self.recover_protocol_args(mtps, &mut mmap);
        // Expected-type checking-mode, after the recoveries so precedence stays
        // turbofish > arguments > recovery > annotation (`seed_from_hint` only fills a param still
        // FREE). The free-fn path has done this all along; the method path had no hint plumbed to
        // it, which is why `v: int? = w.take(xs)` used to report a false conformance error PLUS
        // `cannot assign R to variable of type Option[int]`.
        seed_from_hint(hint, ret, &mut mmap);
        // W12-9 (TICKET-106): let the expected type widen a type arg the ARGUMENTS already bound.
        self.widen_targs_from_hint(
            hint,
            ret,
            mtps,
            expected,
            &arg_tys,
            !targs.is_empty(),
            &mut mmap,
            span,
        );
        // …and the same probe-gated inference diagnostic as the free-fn path, so a recovery miss
        // names the un-inferable param instead of blaming the witnessing method's signature. The
        // gate is the `enforce_bounds` error-count delta: bind the candidates to a hole, and report
        // only if the bound conforms GIVEN that hole (a method that could not conform for ANY
        // instantiation keeps its own true message instead).
        // The probe reads only `params` / `ret` / `type_params`; the rest is inert padding.
        let msig = FnSig {
            params: expected.to_vec(),
            labels: Vec::new(),
            ret: ret.clone(),
            type_params: mtps.to_vec(),
            where_bounds: Vec::new(),
            min_params: expected.len(),
            is_static: false,
            doc: None,
            witness_params: Vec::new(),
            variadic: None,
            slots: None,
            native: None,
            c_variadic: false,
        };
        let probed = self.probe_uninferable_dependent_result_params(&msig, &mut mmap);
        let before = self.errors.len();
        self.enforce_bounds(mtps, mtps, &mmap, span);
        if self.errors.len() == before {
            for pname in probed {
                self.error(
                    span,
                    format!(
                        "cannot infer type parameter {pname} for '{method}'; add a result annotation or explicit type arguments"
                    ),
                );
            }
        }
        // …and the general return-only case, same rule and same wording as the free-fn path: a param
        // in the return that nothing bound is un-inferable here, bound or not.
        self.report_uninferable_result_params(method, expected, ret, mtps, &mut mmap, span);
        // The receiver must still match its declared type AFTER substitution. Without this, a method
        // type param in receiver position (`fn m[U](self: U)`) turbofished to a contradicting type
        // (`b.m[str]()` on a `Box[int]`) is unchecked — `unify` silently drops the conflict once `[str]`
        // is seeded — and a wrong static type escapes onto the value (a soundness hole).
        let want_recv = subst(receiver, &mmap);
        if !self.assignable(&want_recv, recv_ty) {
            let [recv_s, want_s] = Ty::render_distinct([recv_ty, &want_recv]);
            self.error(
                span,
                format!("receiver of '{method}' has type {recv_s}, expected {want_s}"),
            );
        }
        // Bug D closure-return recovery (shared with the free-fn path via `recover_return_only_params`):
        // re-infer each closure arg in checking-mode, reject a wrong CONCRETE return, loop-back-unify to
        // fill a still-free return-only `[U]`, re-enforce newly-recovered bounds, and degrade a still-
        // unbound param-position param to `Unknown`. `expected` = arg slots (sans receiver); `params` =
        // the full list incl receiver for the param-position degrade.
        self.recover_return_only_params(
            method, expected, &arg_tys, &passes, args, params, mtps, &mut mmap, span,
        );
        // M24 Task 5 — half two of the static-witness contract for a MEMBER-declared type param,
        // recorded LAST for the same reason the free-fn path does it last (`recover_return_only_params`
        // can still bind a param nothing else saw). The caller passes an `instantiate_method` result,
        // so `mmap`'s non-identity keys are the METHOD's own (possibly renamed) params, and `wparams`
        // carries the same names — exactly the set that can be witnessed.
        if !wparams.is_empty() {
            self.record_witness_call(
                method,
                wparams,
                &mmap,
                key_span,
                span,
                WitnessCallee::Member,
            );
        }
        subst(ret, &mmap)
    }

    /// Scope A, delivered through a generic METHOD's concrete parameter slot. `arg` is a call argument
    /// to a builtin/generic container method (`.map`/`.fold`/…); `want` is that argument's declared slot
    /// type substituted with everything the method has pinned SO FAR (for `.map` this is `fn(int) -> U`,
    /// for `.fold`'s arg1 `fn(int, int) -> int`). When `arg` is a BARE reference to a same-module generic
    /// fn (`[1,2,3].map(conv)`), its `[T]` params are pinned from `want`'s concrete positions and the
    /// FULLY-substituted concrete fn type returned — the direct analog of `infer_ident`'s Scope A (a HOF
    /// PARAMETER-slot hint), which fires for a user HOF but not for these own-`[U]`-carrying builtin
    /// methods (their arg goes through `infer_generic_arg_tys` with no `expected_hint`, so it stays
    /// rigid). Returns `None` (leaving the rigid prepass type untouched) unless EVERY arg-fn param binds
    /// AND the result is fully concrete — so a still-free `want` slot (`.fold`'s arg1 before `init` binds
    /// `U`) or a genuinely un-inferable return-only arg-fn param defers to the existing path unchanged,
    /// preserving the Category-1 leak guard and every clean reject. A FRESH substitution map per call
    /// means two distinct pins never launder.
    fn try_pin_generic_fn_value_arg(
        &mut self,
        arg: &Expr,
        want: &Ty,
        call_free: &[String],
        span: Span,
    ) -> Option<Ty> {
        let (_, sig, _) = self.generic_fn_value_sig(arg)?;
        let declared = fn_value_ty(&sig);
        // Accept ONLY the fully-pinned verdict. A slot position that is still a free method param
        // (`.fold` arg1 before `init` binds `U`) or a return-only arg-fn param never pinned leaves the
        // shared derivation at `Undetermined`/`Skip` → bail, arg type unchanged. Reporting an
        // `Undetermined` here would be an EAGER check and would refuse `[1,2,3].fold(0, pick)`; the
        // verdict is re-asked once, at the end of the call, by
        // [`Checker::close_tyvar_frame`].
        let FnValuePin::Pinned(m, refined) =
            pin_generic_fn_value(&sig.type_params, &declared, want, &|n| {
                self.rigid_param(n, call_free)
            })
        else {
            return None;
        };
        // Enforce the arg fn's declared bounds against the bindings, exactly as Scope A does.
        self.enforce_bounds(&sig.type_params, &sig.type_params, &m, span);
        // A method value's receiver `where` bound (`Bx.total`); empty for a fn.
        self.enforce_bounds(&sig.where_bounds, &sig.type_params, &m, span);
        Some(refined)
    }

    /// Bug D's closure-return recovery, shared by the generic-METHOD (`infer_generic_method`) and
    /// generic FREE-FN/module-qualified-fn (`infer_generic_call`) HOF paths. After pass-1 unification
    /// has seeded `map` with everything the argument SHAPES pin, this:
    ///  1. re-infers each closure arg in checking-mode against its substituted slot type (binding its
    ///     unannotated params, re-reporting its body errors) and captures the REFINED type;
    ///  2. SOUNDNESS: when the closure's expected return is ALREADY concrete (a return-only `[U]` pinned
    ///     by a sibling value arg or an explicit slot), rejects a genuinely wrong body against it — the
    ///     free-fn analog of `fold`-init laundering; a mask alone would launder a mismatching return;
    ///  3. LOOP-BACK: feeds the refined types into a SECOND `unify`, filling a return-only param still
    ///     free after pass 1 (recovered from an inferable closure/fn body) so it no longer leaks a
    ///     `Ty::Param` into the return;
    ///  4. re-enforces bounds on params NEWLY bound by the loop-back only (each enforced exactly once);
    ///  5. degrades a still-unbound PARAMETER-position param to `Unknown` (`[].map(fn(x): x*2)` is
    ///     `List[?]`, `tag([])` is `List[?]`) on every path, so no call leaks the callee's own
    ///     `Ty::Param` and the hole check judges the result (TICKET-238). A RETURN-ONLY param
    ///     stays a leaked `Ty::Param` here; the caller reports it.
    ///
    /// `arg_decls` are the per-argument declared slot types (method: params sans receiver; free-fn: all
    /// params) — parallel to `arg_tys`/`args`. `all_params` is the full param list used only for the
    /// param-position degrade scan (method: params INCL receiver; free-fn: all params).
    ///
    #[allow(clippy::too_many_arguments)] // arg decls + arity + span
    pub(super) fn recover_return_only_params(
        &mut self,
        name: &str,
        arg_decls: &[Ty],
        arg_tys: &[Ty],
        passes: &[ArgPass],
        args: &[Expr],
        all_params: &[Ty],
        tps: &[TyParam],
        map: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        // Snapshot the params bound after pass 1, so the loop-back below only re-enforces bounds on
        // params NEWLY bound from a refined arg (pass-1 bounds are enforced by the caller).
        let bound_after_pass1: std::collections::HashSet<String> = map.keys().cloned().collect();
        for (decl, ((actual, arg), pass)) in
            arg_decls.iter().zip(arg_tys.iter().zip(args).zip(passes))
        {
            let want = subst(decl, map);
            // For a closure whose UNANNOTATED body is a nested free generic call, the prepass return
            // leaks the callee's own `Ty::Param` (`fn(?) -> T`) — not the lenient `Unknown` a direct
            // body yields — so an unmasked fallback check would spuriously mismatch `want`'s return
            // whether that return is a still-free `[U]` OR already concrete. Mask an unannotated
            // closure's fallback return: `check_generic_arg`'s internal check stays on params + arity,
            // and the REAL return contract is enforced below against the REFINED type. Non-closure and
            // annotated-closure args use their prepass type unchanged.
            let is_bare_closure = pass.masks_ret();
            let fallback = if is_bare_closure {
                mask_closure_ret(actual)
            } else {
                actual.clone()
            };
            let refined = self.check_generic_arg(name, &want, &fallback, arg, *pass);
            // SOUNDNESS: when the closure's expected return is ALREADY concrete (a return-only `[U]`
            // pinned by a sibling value arg or an explicit slot), enforce it explicitly here against the
            // REFINED return — rejecting a genuinely wrong body while ACCEPTING a nested-generic-call
            // body that merely leaked a rigid `Ty::Param` in the prepass. When the expected return is a
            // still-free `[U]` it is non-concrete here and deferred to the loop-back (no contract yet).
            if is_bare_closure
                && let (Ty::Func { ret: want_ret, .. }, Ty::Func { ret: got_ret, .. }) =
                    (&want, &refined)
                && ty_fully_concrete(want_ret)
                && !self.assignable(want_ret, got_ret)
            {
                let [got_s, want_s] = Ty::render_distinct([got_ret, want_ret]);
                self.error(
                    arg.span,
                    format!("closure argument to '{name}' returns {got_s}, expected {want_s}"),
                );
            }
            // LOOP-BACK (INTERLEAVED per-arg): a closure re-inferred WITH its expected param types has a
            // concrete return (`fn(int) -> int`); feed it straight back into a SECOND `unify` NOW, before
            // the NEXT arg's `want` is substituted. `unify` only binds a param NOT already in the map and
            // IGNORES an `Unknown` actual, so pass-1-resolved params are a strict no-op — it ONLY fills a
            // return-position param still free after pass 1 (recovered from the closure body). Interleaving
            // (vs a separate post-loop pass) is load-bearing for SOUNDNESS: once one closure binds a
            // return-only `[U]`, a SIBLING closure binding the SAME `[U]` to a CONFLICTING type sees `want`
            // now CONCRETE and is REJECTED by the soundness check above, instead of being silently dropped
            // by only-bind-unbound `unify` (adversarial-review bug 2: `two(fn(x): x*2, fn(x): str(x))`).
            self.unify_arg(decl, &refined, map);
        }
        // Enforce bounds for params NEWLY bound by the loop-back only (pass-1-bound params were already
        // enforced by the caller — each enforced exactly once avoids a double-report). So a
        // `[U: Add]`/`[U: Comparable]` recovered from a closure body still has its bound checked.
        let newly_bound: Vec<TyParam> = tps
            .iter()
            .filter(|tp| !bound_after_pass1.contains(&tp.name) && map.contains_key(&tp.name))
            .cloned()
            .collect();
        self.enforce_bounds(&newly_bound, tps, map, span);
        // Degrade a STILL-unbound type param to `Unknown` when it appears in a PARAMETER position:
        // it was recoverable from an argument whose type was itself open (`[].map(fn(x): x*2)`).
        // A param ONLY in the RETURN position (`fn make[U]() -> U`) stays a leaked `Ty::Param`.
        let wanted: std::collections::HashSet<String> =
            tps.iter().map(|tp| tp.name.clone()).collect();
        let mut in_param_pos: Vec<String> = Vec::new();
        for p in all_params {
            ty_collect_params(p, Some(&wanted), &mut in_param_pos);
        }
        for tp in tps {
            if in_param_pos.contains(&tp.name) {
                map.entry(tp.name.clone()).or_insert(Ty::Unknown);
            }
        }
    }
}
