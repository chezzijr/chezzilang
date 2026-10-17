//! Checker: argument checking, arity, `expect_*`, type-param scopes, bounds.
//! Split out of `src/checker/expr.rs`. `super::*` is the parent module.

use super::expr::ArgPass;
use super::*;

impl Checker {
    /// TICKET-124 (W13-13): infer the single argument, reaching an expected type that is
    /// concrete up to in-scope type params (`ty_concrete_but` + `rigid_param`) into it, so `Some([A()])` under an
    /// `Option[List[Named]]` hint infers the list literal's elements as `Named`, not `A`.
    /// TICKET-227: here `h` is the argument's slot type (a `Shared[T]`/`RwShared[T]`/`Atomic[T]`
    /// turbofish), so a plain value wraps into a carrier `T`. See [`Self::one_arg_seeded`].
    pub(super) fn one_arg_hinted(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
        h: Option<&Ty>,
    ) -> Ty {
        self.check_arity(name, 1, args, span);
        let Some(a) = args.first() else {
            return Ty::Unknown;
        };
        match h {
            Some(t) if ty_concrete_but(t, &|n| self.rigid_param(n, &[])) => {
                self.infer_arg(a, Some(t))
            }
            _ => self.infer_value(a),
        }
    }

    pub(super) fn infer_all(&mut self, args: &[Expr]) {
        for a in args {
            self.infer_value(a);
        }
    }

    /// Check argument count and each argument's type against a known parameter list. STRICT — an int
    /// never widens into a `float` slot (D3, TICKET-138).
    pub(super) fn check_args(&mut self, name: &str, params: &[Ty], args: &[Expr], span: Span) {
        self.check_args_range(name, params, params.len(), args, span);
    }

    /// D6c — `check_args` generalized to an optional trailing tail: the arg count must fall in
    /// `min_params..=params.len()`, and each supplied arg must match its positional param. Used for the
    /// net socket ops whose `timeout_ms` is optional. `min_params == params.len()` reproduces the
    /// exact-arity behavior of [`Checker::check_args`].
    pub(super) fn check_args_range(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_decl(name, params, min_params, args, span, false);
    }

    /// The arg check for a call to a C variadic `extern` fn (`fn printf(fmt: str, ...)`): the fixed
    /// params check as usual, and each surplus arg must be a C vararg (`c_vararg_ctype`).
    pub(super) fn check_c_variadic_args(
        &mut self,
        name: &str,
        params: &[Ty],
        args: &[Expr],
        span: Span,
    ) {
        if args.len() < params.len() {
            self.error(
                span,
                format!(
                    "'{name}' expects at least {} argument(s), got {}",
                    params.len(),
                    args.len()
                ),
            );
            self.infer_all(args);
            return;
        }
        let (fixed, extra) = args.split_at(params.len());
        self.check_args_range(name, params, params.len(), fixed, span);
        for (k, arg) in extra.iter().enumerate() {
            let ty = self.infer_arg(arg, None);
            if !matches!(ty, Ty::Unknown) && super::setup::c_vararg_ctype(&ty).is_none() {
                self.error(
                    arg.span,
                    format!(
                        "argument {} to extern fn '{name}' has type '{ty}', which cannot be passed \
                         to a C variadic parameter (C varargs take int, float, bool, str or ptr)",
                        params.len() + k + 1
                    ),
                );
            }
        }
    }

    /// TICKET-225 / TICKET-227: the iterable of a type-applied conversion (`List[int8]([300])`,
    /// `List[int?]([5])`). The turbofish element is the expected type of each element of a list
    /// literal argument: a constant meets its C width, a plain value wraps into a carrier element
    /// (`Map[K, V]` elements are `(K, V)` tuples, split per part). Anything else infers bottom-up,
    /// and the conversion's own element check reports a mismatch.
    pub(super) fn infer_conv_arg(&mut self, arg: &Expr, elem: Option<Ty>) -> Ty {
        match elem {
            Some(e) if matches!(arg.kind, ExprKind::List(..)) && ty_fully_concrete(&e) => {
                self.infer_arg(arg, Some(&Ty::list(e)))
            }
            _ => self.infer_value(arg),
        }
    }

    /// Infer a single call argument in *checking mode*: the argument owns `expected` as its slot
    /// (a closure literal binds its unannotated params from it, a generic call pre-seeds its type
    /// params, a plain `T` wraps into a `T?`/`T!E`); with no expected type it is the ordinary
    /// bottom-up [`Checker::infer_value`].
    pub(super) fn infer_arg(&mut self, arg: &Expr, expected: Option<&Ty>) -> Ty {
        // Thread the declared parameter type as an expected-type hint so a generic
        // ctor / generic fn-call passed directly as a call argument pre-seeds its type params —
        // `take(Heap([], fn(x, y): x < y))` with `fn take(h: Heap[int])` pins `T=int`. `infer_call`
        // consumes the hint; pair set+clear so a non-call arg never leaks it into a sibling arg.
        // TICKET-227: the arg owns its slot, so a plain `T` wraps into a `T?`/`T!E` parameter.
        if let Some(e) = expected {
            return self.infer_value_in(arg, e);
        }
        self.infer_value(arg)
    }

    /// [`Self::infer_arg`] with `hint` as a SEED only: it guides inference (a generic ctor pins its
    /// type params from it) but never wraps the arg. For a hint that is not the arg's own slot type
    /// (the `Some`/`Ok`/`Err` payload, the `seed` half of [`Checker::slot_seed`]).
    pub(super) fn infer_arg_seeded(&mut self, arg: &Expr, hint: &Ty) -> Ty {
        self.install_hint(arg, hint.clone(), false);
        let t = self.infer_value(arg);
        self.expected_hint = None;
        t
    }

    /// First-pass bottom-up inference of a generic ctor/call's args (the pass that drives unification).
    /// A closure arg is inferred WITHOUT an expected type here (its params would be `Unknown`); its body
    /// errors — and the phase-5 "cannot infer parameter" error — are SUPPRESSED, because the per-arg
    /// check ([`Checker::check_generic_arg`]) re-infers each closure against its SUBSTITUTED expected
    /// type and reports cleanly there (mirrors the `RwShared.read` recovery-reinfer idiom). Every
    /// generic ctor/variant/fn/method path uses this pair so closure params are pinned by the field/
    /// param type, not left `Unknown`.
    ///
    /// TICKET-239: `owned` and `seed` are [`Checker::slot_seed`]'s answer, substituted FIRST. An
    /// argument whose slot is concrete under `owned` owns that slot (the same `infer_arg` call a
    /// non-generic path makes, so it wraps and meets its width); an argument whose slot is the
    /// bare parameter `seed` guesses takes the guess as a seed; any other closure is prepassed.
    /// `seed` is never substituted into a compound slot. The returned [`ArgPass`] per argument
    /// is the one record of which happened.
    pub(super) fn infer_generic_arg_tys(
        &mut self,
        args: &[Expr],
        declared: &[Ty],
        owned: &HashMap<String, Ty>,
        seed: &HashMap<String, Ty>,
        tps: &[TyParam],
    ) -> (Vec<Ty>, Vec<ArgPass>) {
        // A callee's own still-unbound param never passes as a caller's same-named param.
        let free: Vec<String> = tps
            .iter()
            .filter(|tp| !owned.contains_key(&tp.name))
            .map(|tp| tp.name.clone())
            .collect();
        args.iter()
            .enumerate()
            .map(|(i, a)| {
                let slot = declared.get(i).map(|d| subst(d, owned));
                if let Some(s) = &slot
                    && ty_concrete_but(s, &|n| self.rigid_param(n, &free))
                {
                    return (self.infer_arg(a, Some(s)), ArgPass::Checked);
                }
                // A seed is a guess at `T`, so it reaches an argument only where the slot IS that
                // `T`, and as a seed: `5` stays `int`, `?5` and `None` read their carrier from it.
                // Substituted into a compound slot (`List[T]`, `fn(T) -> T`) it would be an owned
                // slot for the elements, and `r: int? = find([1, 2], 2)` would solve `T = int?`.
                let seeded = match &slot {
                    Some(Ty::Param(n)) => seed.get(n),
                    _ => None,
                };
                if let ExprKind::Closure { ret, .. } = &a.kind
                    && seeded.is_none()
                {
                    let mark = self.diag_mark();
                    // Keep the closure's unannotated params `Unknown` in the unification prepass —
                    // the free-body scan (sources #2/#3) must not pin them here (see the field doc).
                    let saved = std::mem::replace(&mut self.generic_arg_prepass, true);
                    let t = self.infer_value(a);
                    self.generic_arg_prepass = saved;
                    self.diag_rollback(mark);
                    // An annotated return is a written type, never a prepass guess.
                    let ret_inferred = ret.is_none();
                    return (t, ArgPass::Prepass { ret_inferred });
                }
                if let Some(s) = seeded {
                    return (self.infer_arg_seeded(a, s), ArgPass::Checked);
                }
                (self.infer_value(a), ArgPass::Checked)
            })
            .unzip()
    }

    /// Per-argument check for a generic ctor/call/method. `expected` is the arg's SUBSTITUTED declared
    /// type; `fallback` is its first-pass inferred type. A closure arg is re-inferred in checking-mode
    /// against `expected` (binding its unannotated params + reporting body errors here, since
    /// [`Checker::infer_generic_arg_tys`] suppressed them); other args keep `fallback`. Then assert
    /// assignability with the uniform "argument to '<name>'" diagnostic.
    /// Returns the arg's REFINED actual type: for a closure, its type re-inferred in checking-mode
    /// against `expected` (so `fn(x): x*2` against `fn(int) -> U` becomes the concrete `fn(int) -> int`
    /// — its body-return type is now known); for a non-closure, `fallback` unchanged. The generic-METHOD
    /// path ([`Checker::infer_generic_method`]) feeds these refined types back into a SECOND `unify`
    /// pass (the loop-back), so a return-position type param bound ONLY from a closure body (e.g. `map`'s
    /// `U`) resolves concretely instead of leaking a `Ty::Param`. The free-fn/ctor path
    /// ([`Checker::infer_generic_call`]) ignores the return (its behavior is unchanged).
    pub(super) fn check_generic_arg(
        &mut self,
        name: &str,
        expected: &Ty,
        fallback: &Ty,
        arg: &Expr,
        pass: ArgPass,
    ) -> Ty {
        let refined = if pass.deferred() {
            // Re-infer the closure in checking-mode against the substituted expected type: this binds
            // its unannotated params and re-reports its body errors (which `infer_generic_arg_tys`
            // suppressed). The assignability check below still uses the first-pass `fallback`
            // (Unknown-bearing) type — its params/return are leniently assignable, so a type param
            // bound ONLY from this closure's body (e.g. `Mapped`'s `U`, recovered from the closure's
            // return) doesn't spuriously fail against an unbound `Ty::Param`. The param binding + body
            // re-check is the real enforcement; the `fallback` check still catches an arity or
            // annotated-return mismatch. The re-inferred type (`fn(int) -> int`) is RETURNED for the
            // caller's loop-back unify.
            self.infer_arg(arg, Some(expected))
        } else {
            fallback.clone()
        };
        if !self.assignable(expected, fallback) {
            let [fallback_s, expected_s] = Ty::render_distinct([fallback, expected]);
            self.error(
                arg.span,
                format!(
                    "argument to '{name}' has type {fallback_s}, expected {expected_s}{}",
                    float_fix_note_join(fallback, expected)
                ),
            );
        }
        refined
    }

    /// Trial-check the given closure args (by index into `args`/`decl_tys`) against their declared
    /// slot type substituted with `sub`, reporting whether ANY error was produced. Errors emitted
    /// during the probe are rolled back (`truncate`) so the trial never leaks diagnostics — it only
    /// answers "would these bodies error under this substitution?". Used by
    /// [`Checker::report_uninferable_closure_params`] to discriminate a genuine type-param deadlock
    /// from a harmless body. (Hover side effects are first-hit-wins and idempotent, so the eventual
    /// real per-arg check records the same result.)
    pub(super) fn trial_check_closure_args(
        &mut self,
        idxs: &[usize],
        decl_tys: &[Ty],
        args: &[Expr],
        sub: &HashMap<String, Ty>,
        prepass: bool,
    ) -> bool {
        // `prepass` routes an unannotated closure param whose substituted slot type is `Unknown`
        // through the silent-`Unknown` binding (see `infer_closure`) instead of the
        // free-scan/annotation-required path — so the "params bound to `Unknown`" trial actually
        // checks the body with `Unknown` params (no spurious "cannot infer parameter").
        let saved = std::mem::replace(&mut self.generic_arg_prepass, prepass);
        let mark = self.diag_mark();
        for &i in idxs {
            if let (Some(decl), Some(arg)) = (decl_tys.get(i), args.get(i)) {
                let expected = subst(decl, sub);
                self.infer_arg(arg, Some(&expected));
            }
        }
        let errored = self.errors.len() > mark.errors;
        self.diag_rollback(mark);
        self.generic_arg_prepass = saved;
        errored
    }

    /// Detect (and clearly report) the un-inferable type-parameter DEADLOCK that arises when a
    /// generic ctor/fn is given an **unannotated closure** for a closure-typed slot AND no other
    /// argument pins the type parameter that slot mentions (e.g. `Heap([], fn(a, b): a < b)` — the
    /// empty `[]` gives no element type and the bare comparator params give none either). Without
    /// this, the leftover `Ty::Param(T)` flows into the closure body and surfaces as a misleading
    /// "cannot compare T and T" inside the user's lambda.
    ///
    /// Fires ONLY on the genuine deadlock: a still-unbound param (`!sub.contains_key`) that is
    /// mentioned by an unannotated closure's PARAMETER slot AND whose body actually *constrains* it.
    /// The mention is a NECESSARY condition (forms that pin the param — turbofish, a concrete
    /// element, annotated closure params — leave `sub` populated, so the guard never reaches here);
    /// the body PROBE ([`Checker::trial_check_closure_args`], two trials) is the SUFFICIENT one: it
    /// fires only when leaving the param as the unbound `Ty::Param` errors the body but binding it to
    /// `Unknown` does not. That keeps a harmless body that never uses the param (`fn(x): print(x)`,
    /// `fn(x): 42`) type-checking — those ran on `main` and must not be newly rejected — and leaves an
    /// unrelated body error (errors under BOTH trials) for the normal per-arg check to report.
    /// On firing it binds the offending params to `Unknown` in `sub` so the downstream substituted
    /// closure check sees `Unknown` params (no cascade) — the resulting type is identical to the
    /// existing `unwrap_or(Ty::Unknown)` fallback, so this is behavior-preserving apart from the
    /// message. Returns `true` if it fired.
    pub(super) fn report_uninferable_closure_params(
        &mut self,
        name: &str,
        tps: &[TyParam],
        decl_tys: &[Ty],
        args: &[Expr],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) -> bool {
        let unbound: std::collections::HashSet<String> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .filter(|n| !sub.contains_key(n))
            .collect();
        if unbound.is_empty() {
            return false;
        }
        // Only an unbound param appearing in a closure's PARAMETER position is a deadlock: such a
        // param can't be recovered from the closure body (the params would have to be typed first).
        // A param that appears ONLY in the closure's RETURN slot (e.g. `Mapped`'s `f: fn(T) -> U`)
        // is still inferable from the body, so it must NOT trigger — scan declared param slots only,
        // aligned to the UNANNOTATED closure parameters.
        let mut mentioned: Vec<String> = Vec::new();
        let mut candidate_idxs: Vec<usize> = Vec::new();
        for (ai, (decl, arg)) in decl_tys.iter().zip(args).enumerate() {
            if let ExprKind::Closure {
                params: cparams, ..
            } = &arg.kind
                && let Ty::Func {
                    params: dparams,
                    variadic: crate::ast::Variadic::No,
                    ..
                } = decl
            {
                let before = mentioned.len();
                for (i, cp) in cparams.iter().enumerate() {
                    if cp.ty.is_none()
                        && let Some(dp) = dparams.get(i)
                    {
                        ty_collect_params(dp, Some(&unbound), &mut mentioned);
                    }
                }
                // This closure arg has an unannotated param slot mentioning an unbound param — it is
                // a candidate whose BODY we must probe before deciding the deadlock actually bites.
                if mentioned.len() > before {
                    candidate_idxs.push(ai);
                }
            }
        }
        if mentioned.is_empty() {
            return false;
        }
        // Preserve declaration order of the type params in the message.
        let names: Vec<String> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .filter(|n| mentioned.contains(n))
            .collect();
        // PROBE the candidate closure bodies before firing. A still-unbound param in an unannotated
        // closure slot is a *potential* deadlock, but only a GENUINE one when the body actually
        // constrains that param (e.g. `a < b` — ordering on an unconstrained `Ty::Param` is
        // rejected). A harmless body (`print(x)`, a constant) imposes no constraint, type-checks on
        // every engine, and ran on `main`; firing there would reject previously-valid code. The
        // discriminator: check the candidate bodies twice — once leaving the params as the unbound
        // `Ty::Param` (the current `sub`), once with them bound to `Unknown`. Fire ONLY when the
        // unbound form errors AND the `Unknown` form is clean. That isolates a true "needs `T`"
        // body from a harmless one (clean under BOTH) and from an unrelated body error (errors under
        // BOTH — left for the normal per-arg check to report as itself).
        let errored_unbound =
            self.trial_check_closure_args(&candidate_idxs, decl_tys, args, sub, false);
        if !errored_unbound {
            return false;
        }
        let mut sub_unknown = sub.clone();
        for n in &names {
            sub_unknown.insert(n.clone(), Ty::Unknown);
        }
        let errored_unknown =
            self.trial_check_closure_args(&candidate_idxs, decl_tys, args, &sub_unknown, true);
        if errored_unknown {
            // The body errors even with the params known (Unknown) — the failure is unrelated to
            // `T`. Don't mask it; let the normal per-arg check surface the real diagnostic.
            return false;
        }
        let list = names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let turbo = names.join(", ");
        let word = if names.len() == 1 {
            "type parameter"
        } else {
            "type parameters"
        };
        self.error(
            span,
            format!(
                "cannot infer {word} {list} of `{name}`; annotate `{name}[{turbo}](…)` or the closure parameters"
            ),
        );
        for n in names {
            sub.insert(n, Ty::Unknown);
        }
        true
    }

    /// [`Checker::check_args_range`] for a List/Set COLLECTION mutator receiver — the only path that
    /// may show the element-pin annotation hint on a `push`/`add`/`insert` mismatch (handle methods
    /// like `Atomic.add` route through `check_args_range` and never see it).
    pub(super) fn check_args_range_coll(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_decl(name, params, min_params, args, span, true);
    }

    /// [`Checker::check_args_range`] for a SUBSTITUTED parameter list (a method of a generic type,
    /// whose `T`s are already replaced by the receiver's type args).
    pub(super) fn check_args_subst(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_decl(
            name,
            params,
            min_params.min(params.len()),
            args,
            span,
            false,
        );
    }

    fn check_args_range_decl(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
        is_collection: bool,
    ) {
        if !(min_params..=params.len()).contains(&args.len()) {
            let want = if min_params == params.len() {
                format!("{}", params.len())
            } else {
                format!("{min_params}–{}", params.len())
            };
            self.error(
                span,
                format!("'{name}' expects {want} argument(s), got {}", args.len()),
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let at = self.infer_arg(arg, params.get(i));
            if let Some(pt) = params.get(i)
                && !self.assignable(pt, &at)
            {
                let [expected, actual] = Ty::render_distinct([pt, &at]);
                // Annotation hint for a collection mutator. The element type was fixed on the
                // statement that created the collection (`xs := [a]` is `List[<type of a>]`); an
                // element of a different (e.g. protocol-sibling) type is a real mismatch — point
                // the user at the explicit annotation that makes a mixed/protocol collection legal.
                // The hint is only valid for a List/Set collection receiver. The
                // method name `add` also names `Atomic.add` (a handle), whose float mismatch must NOT
                // show the collection hint — gate on the receiver actually being a collection.
                let pnote = self.protocol_note(pt, &at);
                // An int at a `float` slot is not a mixed collection, so `slot_note` names
                // the fix instead.
                let coll_mismatch_is_float_widen = matches!((pt, &at), (Ty::Float, Ty::Int));
                let hint = if !pnote.is_empty() {
                    pnote
                } else if is_collection
                    && i == 0
                    && matches!(name, "push" | "add" | "insert")
                    && !coll_mismatch_is_float_widen
                {
                    // Only an UN-BOUND/leaked type param (not in scope here) means "un-inferred": a
                    // return-only `T` from `empty[T]()` called with nothing to bind it from. A
                    // `Ty::Param` that IS in scope (`self.type_params`) is a legitimately-bound
                    // generic param — keep the annotation hint for it (and for every concrete
                    // element type).
                    if matches!(pt, Ty::Param(p) if !self.type_params.contains_key(p)) {
                        // The expected element type is an un-inferred type parameter (e.g. a
                        // return-only `T` from `empty[T]()` with nothing to bind it from).
                        // `List[<protocol>] = []` would not help — the fix is to bind the
                        // parameter at the construction site.
                        format!(
                            " (the collection's element type is the un-inferred type parameter {expected}; bind it at the construction site with a turbofish or annotation, e.g. `empty[int]()` or `xs: List[int] = ...`)"
                        )
                    } else {
                        // TICKET-124 (W13-15): the slot is DECLARED (`l: List[float] = [1.5]`) or
                        // inferred on its own statement (`l := [1.5]`); the text covers both.
                        format!(
                            " (the collection's element type is {expected}, fixed where the binding is declared; annotate the binding, e.g. `List[<protocol>] = []`, for a mixed/protocol collection)"
                        )
                    }
                } else {
                    slot_note(pt, &at).to_string()
                };
                self.error(
                    arg.span,
                    format!(
                        "argument {} of '{name}': expected {expected}, found {actual}{hint}",
                        i + 1
                    ),
                );
            }
        }
    }

    pub(super) fn check_arity(&mut self, name: &str, n: usize, args: &[Expr], span: Span) {
        if args.len() != n {
            self.error(
                span,
                format!("'{name}' expects {n} argument(s), got {}", args.len()),
            );
        }
    }

    /// A generic struct ctor's arity check (W8-47). `desugar` already declined to splice a
    /// `GenericProvider`-defaulted field when the call carries no turbofish (or a mismatched one),
    /// so `args` is genuinely short here. An UNBOUNDED owner type parameter with every omitted
    /// trailing field defaulted is a binder-inference problem, not a missing-argument one — report
    /// that instead of the plain arity message, naming the FIRST omitted field and the first type
    /// parameter its type mentions. Anything else falls through to `check_arity`.
    #[allow(clippy::too_many_arguments)] // ctor shape (tps/fields/defaulted) + call args + span
    pub(super) fn check_ctor_arity(
        &mut self,
        name: &str,
        tps: &[TyParam],
        fields: &[(String, Ty)],
        defaulted: &[String],
        targs: &[Ty],
        args: &[Expr],
        span: Span,
    ) {
        if args.len() == fields.len() {
            return;
        }
        if targs.is_empty()
            && args.len() < fields.len()
            && !tps.is_empty()
            && tps.iter().all(|tp| tp.bounds.is_empty())
        {
            let wanted: std::collections::HashSet<String> =
                tps.iter().map(|tp| tp.name.clone()).collect();
            let all_defaulted = fields[args.len()..]
                .iter()
                .all(|(fname, _)| defaulted.iter().any(|d| d == fname));
            if all_defaulted {
                for (fname, fty) in &fields[args.len()..] {
                    let mut got = Vec::new();
                    ty_collect_params(fty, Some(&wanted), &mut got);
                    if let Some(tp) = got.first() {
                        self.error(
                            span,
                            format!(
                                "cannot infer type parameter {tp} for '{name}'; the default for field '{fname}' can only be filled with explicit type arguments, e.g. {name}[int](...)"
                            ),
                        );
                        return;
                    }
                }
            }
        }
        self.check_arity(name, fields.len(), args, span);
    }

    pub(super) fn expect_bool(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer_value(e);
        if t != Ty::Bool && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be bool, found {t}"));
        }
    }

    pub(super) fn expect_int(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer_value(e);
        self.expect_int_ty(e, &t, ctx);
    }

    /// `expect_int` for an expression the caller already inferred to `t`.
    pub(super) fn expect_int_ty(&mut self, e: &Expr, t: &Ty, ctx: &str) {
        if *t != Ty::Int && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be int, found {t}"));
        }
    }

    /// The index of an index ASSIGNMENT `xs[i] = v` into a list, `bytearray` or `str`: an `int`. A
    /// `Range` index is a slice (TICKET-253), and a slice is not a place.
    pub(super) fn expect_assign_index(&mut self, e: &Expr) {
        let t = self.infer_value(e);
        if t == self.range_ty() {
            self.error(e.span, "cannot assign to a slice");
        } else if t != Ty::Int && !t.is_unknown() {
            self.error(e.span, format!("index must be int, found {t}"));
        }
    }

    // ===== generics & protocols =====

    /// The `Self` type for a struct's own methods: `Struct(name, [Param(p) for each type param])`,
    /// so inside `struct Stack[T]` the receiver is `Stack[T]` and `self.items` is `list[T]`.
    pub(super) fn struct_self_ty(&self, name: &str) -> Ty {
        // Key by the struct's runtime key (bare unless a cross-module clash disambiguated it), exactly
        // like `enum_self_ty`. In the multi-module (`build_graph`) path the layout is
        // stored under `<module-key>::Name`, so a bare-`name` lookup here would miss — leaving `self`'s
        // type wrong during both return inference and pass-2 body checking.
        let key = self.bare_key(name);
        let args = self
            .structs
            .get(&key)
            .map(|i| {
                i.type_params
                    .iter()
                    .map(|tp| Ty::Param(tp.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ty::Struct(key, args)
    }

    /// The `Ty::Enum` of an enum's own `self`: keyed by its runtime key, parameterized by its own
    /// generic type params as `Ty::Param`s (so `fn get(self) -> T` inside `enum Box[T]` resolves).
    pub(super) fn enum_self_ty(&self, name: &str) -> Ty {
        let key = self.bare_key(name);
        let args = self
            .enum_type_params
            .get(&key)
            .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
            .unwrap_or_default();
        Ty::enum_ty(key, args)
    }

    /// Install `tps` as the in-scope generic type parameters, returning the previous map to restore.
    /// The bounds are resolved here, in the declaring scope, by the silent `resolve_bounds`
    /// (TICKET-202).
    pub(super) fn enter_type_params(&mut self, tps: &[TypeParam]) -> HashMap<String, Vec<TyBound>> {
        let resolved = self.resolve_bounds(tps, Span::default());
        let saved = self.type_params.clone();
        for (tp, rtp) in tps.iter().zip(resolved) {
            // Editor hover (decl-site): record the bound generic param `T` at its DECLARATION token
            // (`fn id[T]`, `struct Box[T]`, a method `[U]`). This is the single funnel for entering
            // type params, so every generic decl form is covered. The hover renders the bare param
            // name (`T`); a bound suffix (`T: Comparable`) is not representable through the `Ty`-only
            // hover channel. GUARD on the probe so the `Ty::Param(..clone())` argument is not built on
            // every generic check (enter_type_params is hot — runs for every generic fn/struct/method).
            if self.hover_probe.is_some() {
                self.hover_record_at(
                    tp.name_span,
                    &Ty::Param(tp.name.clone()),
                    HoverKind::Struct,
                    None,
                );
            }
            self.type_params.insert(tp.name.clone(), rtp.bounds);
        }
        saved
    }

    /// [`Self::enter_type_params`] at a DECLARATION (a fn signature, a struct, an enum): it also
    /// reports each bad bound, once, since the declaration is visited once. A body re-entry uses
    /// `enter_type_params`.
    pub(super) fn declare_type_params(
        &mut self,
        tps: &[TypeParam],
        span: Span,
    ) -> HashMap<String, Vec<TyBound>> {
        let saved = self.enter_type_params(tps);
        self.check_param_bounds(tps, span);
        saved
    }

    /// Report the bad bounds of each declared type parameter (see [`Self::check_bounds`]).
    pub(super) fn check_param_bounds(&mut self, tps: &[TypeParam], span: Span) {
        for tp in tps {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
    }

    /// The message for a protocol name that does not resolve, at `site` (`in bound on 'T'`,
    /// `embedded in 'P'`). A std protocol the module did not bind reads like any other unimported
    /// name, and the import hint names its module.
    pub(super) fn unknown_protocol_msg(&self, name: &str, site: &str) -> String {
        let hint = self.import_hint(name);
        let what = if is_std_protocol(name) {
            "name"
        } else {
            "protocol"
        };
        format!("unknown {what} '{name}' {site}{hint}")
    }

    /// [`Self::enter_type_params`] for an already-resolved list (a stored signature's).
    pub(super) fn enter_ty_params(&mut self, tps: &[TyParam]) -> HashMap<String, Vec<TyBound>> {
        let saved = self.type_params.clone();
        for tp in tps {
            if self.hover_probe.is_some() {
                self.hover_record_at(
                    tp.name_span,
                    &Ty::Param(tp.name.clone()),
                    HoverKind::Struct,
                    None,
                );
            }
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        saved
    }

    pub(super) fn exit_type_params(&mut self, saved: HashMap<String, Vec<TyBound>>) {
        self.type_params = saved;
    }

    /// Validate the bounds declared on a type parameter: each names a known protocol, and the number
    /// of type args matches the protocol's arity (a parameterized `protocol Container[T]` requires
    /// one; a bare protocol requires none). `Iterator` additionally may appear at most once (its
    /// element recovery can't disambiguate two).
    pub(super) fn check_bounds(&mut self, bounds: &[Bound], param: &str, span: Span) {
        let mut seen_iterator = false;
        for b in bounds {
            let Some(arity) = self
                .user_protocol_shape(&b.name)
                .map(|p| p.type_params.len())
            else {
                // A module-qualified bound (TICKET-173) that misses: an unimported module reuses the
                // qualified annotation's text (`src/checker/sig.rs`, `Type::Qualified`). The module
                // is everything before the LAST dot, so a full path `pkg.deep.P` names `pkg.deep`.
                if !b.name.contains("::")
                    && let Some((module, member)) = b.name.rsplit_once('.')
                    && !self.imported_modules.contains_key(module)
                {
                    let msg = self.ambiguous_bind_msg(module).unwrap_or_else(|| {
                        format!("unknown module '{module}' (import it to use `{module}.{member}`)")
                    });
                    self.error(span, msg);
                    continue;
                }
                // A protocol alias that APPLIES TYPE ARGUMENTS (`type IntBag = Bag[int]`) cannot name
                // a bound: there is no substitution seam to carry `int` into `T`'s bound. Diagnose
                // this shape by name rather than falling through to the generic "unknown protocol".
                if let Some(Type::Generic(head, ..)) = self.aliases.get(&b.name).cloned()
                    && self.user_protocol_shape(&head).is_some()
                {
                    self.error(
                        span,
                        format!(
                            "type alias '{}' applies type arguments, so it cannot name a bound on '{param}' -- write the protocol and its arguments in the bound",
                            b.name
                        ),
                    );
                    continue;
                }
                // A `where T: <scalar>` equality bound (int/float/bool/str/…) — not a protocol, but a
                // valid constraint pinning `T` to exactly that scalar type. It takes no type args.
                if Self::scalar_bound_ty(&b.name).is_some() {
                    if !b.args.is_empty() {
                        self.error(span, format!("type '{}' takes no type arguments", b.name));
                    }
                    continue;
                }
                // A `where T: List/Map/Set` constructor-kind bound (no element binder — takes no type
                // args here; the element/key/value types are free). Mirrors the scalar accept above.
                if Self::container_bound_matches(&b.name, &Ty::Unknown).is_some() {
                    if !b.args.is_empty() {
                        self.error(span, format!("type '{}' takes no type arguments", b.name));
                    }
                    continue;
                }
                let msg = self.unknown_protocol_msg(&b.name, &format!("in bound on '{param}'"));
                self.error(span, msg);
                continue;
            };
            if b.args.len() != arity {
                let msg = if arity == 0 {
                    format!("protocol '{}' takes no type arguments", b.name)
                } else {
                    format!(
                        "protocol '{}' takes {arity} type argument(s), found {}",
                        b.name,
                        b.args.len()
                    )
                };
                self.error(span, msg);
            }
            if self.user_protocol_key(&b.name).as_deref() == Some("Iterator") {
                if seen_iterator {
                    self.error(span, format!("'{param}' has more than one Iterator bound"));
                }
                seen_iterator = true;
            }
            // Resolve the bound's type args (with the surrounding params in scope) so an unknown type
            // inside a bound — e.g. `Container[Bogus]` — is reported rather than silently accepted.
            for a in &b.args {
                let _ = self.resolve_type(a, span);
            }
        }
    }
}
