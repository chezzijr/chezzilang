//! Checker: closure-parameter scans and closures.
//! Split out of `src/checker/pattern.rs`. `super::*` is the parent module.

use super::*;

impl Checker {
    /// A free closure's param type, inferred from how its body USES the param (sources #2/#3 — only
    /// when there is no expected/slot type). **Shallow + precise** (closes the bare-param structural
    /// trap without over-pinning): source #2 — a `match` whose scrutinee is the BARE param identifier
    /// (`match x:`, not `match x.f:` / `match g(x):`), pinned from its first concrete arm; source #3 —
    /// a member access on the bare param (`x.f` / `x.m()`) whose name is declared by exactly one struct.
    /// Source #2 wins. Does NOT descend into nested closures (an inner param shadowing the name is
    /// unrelated). Read-only; returns `None` when nothing pins the param.
    pub(super) fn scan_free_closure_param(&self, name: &str, body: &Expr) -> Option<Ty> {
        let mut match_pin = None;
        let mut member_pin = None;
        self.scan_expr_for_pin(name, body, &mut match_pin, &mut member_pin);
        match_pin.or(member_pin)
    }

    /// Walk `e` (skipping nested closures) accumulating the source-#2 (`match_pin`) and source-#3
    /// (`member_pin`) candidates for a free closure's param `name`. Stops descending once a source-#2
    /// pin is found (highest priority). See [`Checker::scan_free_closure_param`].
    pub(super) fn scan_expr_for_pin(
        &self,
        name: &str,
        e: &Expr,
        match_pin: &mut Option<Ty>,
        member_pin: &mut Option<Ty>,
    ) {
        if match_pin.is_some() {
            return;
        }
        match &e.kind {
            // Source #2: a match whose scrutinee is the BARE param — pin from the first concrete arm.
            ExprKind::Match { scrutinee, arms } => {
                if let ExprKind::Ident(s) = &scrutinee.kind
                    && s == name
                {
                    for arm in arms {
                        if let Some(t) = self.pin_ty_of_pattern(&arm.pattern) {
                            *match_pin = Some(t);
                            return;
                        }
                    }
                }
                self.scan_expr_for_pin(name, scrutinee, match_pin, member_pin);
                for arm in arms {
                    // Scope-awareness: an arm whose pattern BINDS `name` (a tuple/variant sub-position
                    // or a bare catch-all of the same spelling) shadows the closure param inside the
                    // guard + body — a `match <name>:` there reads that binding, not the param, so it
                    // must NOT pin. Skip the shadowed arm's guard/body.
                    if arm.pattern.binders().iter().any(|(n, _)| *n == name) {
                        continue;
                    }
                    if let Some(g) = &arm.guard {
                        self.scan_expr_for_pin(name, g, match_pin, member_pin);
                    }
                    self.scan_expr_for_pin(name, &arm.body, match_pin, member_pin);
                }
            }
            // Source #3: a member access (field or method receiver) on the bare param.
            ExprKind::Field {
                obj, name: member, ..
            } => {
                if member_pin.is_none()
                    && let ExprKind::Ident(r) = &obj.kind
                    && r == name
                    && let Some(t) = self.unique_member_owner(member)
                {
                    *member_pin = Some(t);
                }
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
            }
            // A nested closure is its own scope — never descend (it may shadow `name`).
            ExprKind::Closure { .. } => {}
            // Every other expression: recurse into its child expressions.
            ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
                for c in es {
                    self.scan_expr_for_pin(name, c, match_pin, member_pin);
                }
            }
            ExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    self.scan_expr_for_pin(name, k, match_pin, member_pin);
                    self.scan_expr_for_pin(name, v, match_pin, member_pin);
                }
            }
            ExprKind::Comprehension {
                key, elem, clauses, ..
            } => {
                // Scope-awareness: a clause's `vars` shadow `name` for every LATER clause's
                // iter/guards and for the key/elem. Scan each clause's iter (evaluated before this
                // clause binds), then stop once a clause binds `name` — its own guards and everything
                // downstream read the shadowing binding, not the param.
                let mut shadowed = false;
                for c in clauses {
                    if !shadowed {
                        self.scan_expr_for_pin(name, &c.iter, match_pin, member_pin);
                    }
                    if c.target.binders().iter().any(|(v, _)| *v == name) {
                        shadowed = true;
                    }
                    if !shadowed {
                        for g in &c.guards {
                            self.scan_expr_for_pin(name, g, match_pin, member_pin);
                        }
                    }
                }
                if !shadowed {
                    if let Some(k) = key {
                        self.scan_expr_for_pin(name, k, match_pin, member_pin);
                    }
                    self.scan_expr_for_pin(name, elem, match_pin, member_pin);
                }
            }
            ExprKind::Unary { expr, .. } => {
                self.scan_expr_for_pin(name, expr, match_pin, member_pin)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.scan_expr_for_pin(name, lhs, match_pin, member_pin);
                self.scan_expr_for_pin(name, rhs, match_pin, member_pin);
            }
            ExprKind::Compare { operands, .. } => {
                for o in operands {
                    self.scan_expr_for_pin(name, o, match_pin, member_pin);
                }
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.scan_expr_for_pin(name, start, match_pin, member_pin);
                }
                if let Some(end) = end {
                    self.scan_expr_for_pin(name, end, match_pin, member_pin);
                }
            }
            ExprKind::Call {
                callee,
                args,
                bracket,
                ..
            } => {
                self.scan_expr_for_pin(name, callee, match_pin, member_pin);
                for a in args.iter().chain(bracket.as_deref()) {
                    self.scan_expr_for_pin(name, a, match_pin, member_pin);
                }
            }
            ExprKind::Index { obj, index, .. } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                if let Some(index) = index {
                    self.scan_expr_for_pin(name, index, match_pin, member_pin);
                }
            }
            ExprKind::Try(inner) => self.scan_expr_for_pin(name, inner, match_pin, member_pin),
            ExprKind::IfElse { cond, then, els } => {
                self.scan_expr_for_pin(name, cond, match_pin, member_pin);
                self.scan_expr_for_pin(name, then, match_pin, member_pin);
                self.scan_expr_for_pin(name, els, match_pin, member_pin);
            }
            // The fragments are ordinary children: scan them like any other expression.
            ExprKind::Interp(chunks) => {
                for chunk in chunks {
                    if let crate::ast::Chunk::Expr(frag, _, fields) = chunk {
                        self.scan_expr_for_pin(name, frag, match_pin, member_pin);
                        for f in fields {
                            self.scan_expr_for_pin(name, f, match_pin, member_pin);
                        }
                    }
                }
            }
            // `?.`/`??` carriers are lowered before checking. `recover:` carries a statement block
            // (which can introduce its own bindings); it is NOT scanned — a param pinnable only from
            // inside a `recover:` body stays un-inferable and requires an annotation (sound: this is
            // the conservative v1 fallback, never a mis-pin). Leaves (`Ident`/literals/`RawStr`)
            // have no child to scan.
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => {}
        }
    }

    /// The scrutinee type a top-level match arm pattern implies (source #2 classification). Mirrors
    /// [`Checker::reconstruct_unknown_kind`]'s arm classification: a qualified/unique enum variant or
    /// builtin `Ok`/`Err`/`Some`/`None` → that enum/Result/Option (type args `Unknown`); a tuple → an
    /// all-`Unknown` tuple of that arity; a literal/range → its scalar; a binding/wildcard/ambiguous →
    /// `None` (no pin).
    pub(super) fn pin_ty_of_pattern(&self, p: &Pattern) -> Option<Ty> {
        match p {
            Pattern::Or(alts) => alts.first().and_then(|a| self.pin_ty_of_pattern(a)),
            // `?v` fits a `T?` and a `T!E` alike: no pin.
            Pattern::Carrier { .. } => None,
            Pattern::Tuple(subs) => Some(Ty::Tuple(vec![Ty::Unknown; subs.len()])),
            Pattern::Literal(lit) => Some(lit_pattern_ty(lit)),
            Pattern::Range { .. } => Some(Ty::Int),
            Pattern::Variant {
                name,
                enum_name,
                module_name,
                ..
            } => {
                // A module-qualified variant can't be resolved through the bare-name table — no pin.
                if module_name.is_some() {
                    return None;
                }
                if let Some(en) = enum_name {
                    let key = self.bare_key(en);
                    return self
                        .enums
                        .contains_key(&key)
                        .then(|| self.enum_ty_unknown_args(&key));
                }
                match name.as_str() {
                    other if self.imported_variants.contains_key(other) => {
                        let key = self.imported_variants[other].head.key.clone();
                        Some(self.enum_ty_unknown_args(&key))
                    }
                    other => {
                        // A bare variant uniquely owned by one enum pins it; an ambiguous one, or a
                        // bare binding name (not a known variant), does not.
                        let owners = self.variant_owners.get(other)?;
                        if owners.len() != 1 {
                            return None;
                        }
                        let key = self.bare_key(&owners[0]);
                        self.enums
                            .contains_key(&key)
                            .then(|| self.enum_ty_unknown_args(&key))
                    }
                }
            }
            Pattern::Ident(..) | Pattern::Wildcard => None,
        }
    }

    /// A user enum's `Ty::Enum` with its type arguments filled as `Unknown` (the scrutinee shape an
    /// arm pattern pins — element types are unknown, the enum identity is what matters for call-site
    /// checking).
    pub(super) fn enum_ty_unknown_args(&self, key: &str) -> Ty {
        let n = self
            .enum_type_params
            .get(key)
            .map(|tps| tps.len())
            .unwrap_or(0);
        Ty::enum_ty(key.to_string(), vec![Ty::Unknown; n])
    }

    /// If exactly one struct declares a field OR method `member`, return that struct's type (type args
    /// `Unknown`); else `None` (source #3 only fires for a UNIQUELY-owned member — a name shared by
    /// >1 type, or none, never pins). Read-only.
    pub(super) fn unique_member_owner(&self, member: &str) -> Option<Ty> {
        // A member shared by any PARAMETERIZED collection (`len`/`map`/`get`/`push`/… on
        // `list`/`map`/`set`) is never a unique pin: it is shared across types AND would only pin a
        // weak `list[Unknown]`-style type (design §3 — "methods/fields shared by >1 type … never
        // pin"). Bail before collecting owners so such members fall through to the annotation rule.
        // Phase 5a-containers — the `List`/`Map`/`Set` method tables are harvested from
        // `std/prelude.chz` and re-seeded into `self.structs` by `seed_stdlib_structs`; check them for
        // membership (the retired `list_method_sig`/`map_method_sig`/`set_method_sig` arms' replacement).
        // As of phase 6 the `List` table ALSO contains the closure-driven HOFs (`map`/`filter`/`fold`/
        // `sort_by`/`sort_by_key`, formerly the bespoke `infer_list_hof` arm), so those names now bail
        // here too — correct, since they ARE shared by the parameterized `List` (a name shared across
        // types never uniquely pins), matching the design's collection-method bail.
        if ["List", "Map", "Set"].iter().any(|ty| {
            self.structs
                .get(*ty)
                .is_some_and(|info| info.methods.contains_key(member))
        }) {
            return None;
        }
        // Collect every PINNABLE owner of `member`: user structs (by field or method) plus the
        // concrete scalar builtins `str`/`bytes` (the design's `x.upper()` → `str` case). The pin
        // fires only when exactly one type owns it.
        let mut owners: Vec<Ty> = Vec::new();
        for (key, info) in &self.structs {
            // Source #3 pins only from a USER type. The `Builtin`-origin native structs
            // (Match/Response/ProcResult/…) are seeded into `self.structs` unconditionally at init
            // regardless of imports, so scanning them would mis-pin a param to an unimported,
            // unreferenced builtin (their fields like `end`/`code`/`status`). Skip them.
            if info.origin == StructOrigin::Builtin {
                continue;
            }
            let has =
                info.fields.iter().any(|(n, _)| n == member) || info.methods.contains_key(member);
            if has {
                let n = info.type_params.len();
                owners.push(Ty::Struct(key.clone(), vec![Ty::Unknown; n]));
            }
        }
        // `str`/`bytes` method sets are now the file-backed `native struct` tables seeded into
        // `self.structs` (the retired `str_method_sig`/`bytes_method_sig` replacement); the loop above
        // skips them (Builtin origin), so check them explicitly here to preserve the `x.upper()` → `str`
        // pin case.
        if self
            .structs
            .get("str")
            .is_some_and(|info| info.methods.contains_key(member))
        {
            owners.push(Ty::Str);
        }
        if self
            .structs
            .get("bytes")
            .is_some_and(|info| info.methods.contains_key(member))
        {
            owners.push(Ty::Bytes);
        }
        if owners.len() == 1 {
            owners.pop()
        } else {
            None
        }
    }

    /// The type of an inline expression body, for a named fn (`infer_inline_fn_ret`) and for a
    /// lambda (`infer_closure`). A sole diverging call (`panic(...)`, `os.exit(...)`) never returns
    /// a value, so it is `None` and not an un-inferable `Unknown`. This is the one place that
    /// decides it.
    pub(super) fn inline_body_ty(&mut self, body: &Expr, slot: Option<Ty>) -> Ty {
        let t = self.infer_in_slot(body, slot);
        if t.is_unknown() && self.call_diverges(body) {
            Ty::Nil
        } else {
            t
        }
    }

    pub(super) fn infer_closure(
        &mut self,
        params: &[Param],
        ret: Option<&Type>,
        body: &Expr,
        expected: Option<&Ty>,
    ) -> Ty {
        self.receiver_or_report(params, None, body.span);
        self.closure_write_frames
            .push((self.scopes.len(), HashSet::new()));
        // Source #1 — the *expected* type of the slot the closure literal sits in. When it is a
        // `fn(..)` whose arity matches, an UNANNOTATED param binds to the expected param type
        // (checking-mode), and a non-`Unknown` expected return becomes the body's return context.
        // On an arity mismatch the params stay `Unknown` here and the call site's `assignable` check
        // reports the mismatch (single diagnostic).
        let (exp_params, exp_ret): (Option<Vec<Ty>>, Option<Ty>) = match expected {
            Some(Ty::Func {
                params: p,
                ret: r,
                variadic: crate::ast::Variadic::No,
                ..
            }) if p.len() == params.len() => (Some(p.clone()), Some((**r).clone())),
            _ => (None, None),
        };
        // A `fn`-typed slot whose arity does NOT match: keep unannotated params silently `Unknown`
        // here and let the call site's `assignable` check report the single arity diagnostic — do
        // NOT route them through the free-closure scan (which would emit a spurious, misdirecting
        // "cannot infer type of parameter"). A variadic expected type (`Pack` or `C`) is the same
        // case: a closure literal is never variadic, so the call site's `assignable` check rejects it.
        let expected_arity_mismatch = matches!(
            expected,
            Some(Ty::Func { params: p, variadic, .. })
                if p.len() != params.len() || *variadic != crate::ast::Variadic::No
        );
        // A closure body opens a fresh loop context (same rule as `check_fn_body`): a loop around
        // the closure's definition must not make a `break`/`continue` inside it legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
        let saved_in_defer = std::mem::replace(&mut self.in_defer_block, false);
        // `?` inside the body targets THIS closure's return, not the enclosing function's. With no
        // annotation there is no Result/Option context, so `?` is rejected (`Unknown` → `infer_try`
        // errors). Mirrors `check_fn_body`'s `current_ret` handling. An expected (slot) return type
        // supplies that context when the closure is unannotated.
        let declared_ret = ret
            .map(|t| self.resolve_ret_type(t, body.span))
            .or_else(|| exp_ret.clone().filter(|r| !r.is_unknown()))
            .unwrap_or(Ty::Unknown);
        let saved_ret = std::mem::replace(&mut self.current_ret, declared_ret);
        // A closure body is a fn body: a `?` on a `Nil`-returning closure is rejected here (already the
        // pre-existing behavior via `current_ret == Unknown` for an unannotated closure; this keeps the
        // signal exact for an explicitly `-> nil` closure too). Saved/restored beside `current_ret`.
        let saved_in_fn = std::mem::replace(&mut self.in_fn_body, true);
        // …and a closure inside a default-argument provider has its OWN caller (W7-51).
        let saved_in_dflt = std::mem::replace(&mut self.in_default_provider, false);
        // A closure DECLARED inside a `spawn:` block is not itself the task — it has a caller, so a
        // `?` in its body targets the closure's own return (W7-48). Saved/restored beside
        // `current_ret`. W8-3 — the airlock taint is per-frame for the same reason, and
        // `enter_own_frame` moves the pair so neither can be reset without the other (this site was
        // the one that cleared `in_spawn_block` alone: the closure body then reported the enclosing
        // task's pending write AND ate the entry, so the parent's real stale read went silent).
        let saved_frame = self.enter_own_frame();
        // A closure inside a generator is NOT itself a generator: clear the yield context so a stray
        // `yield` in the closure is diagnosed as "outside a generator", not bound to the enclosing
        // one. (Closure bodies are single expressions today, so this is a latent-invariant guard.)
        let saved_yield = self.yield_ty.take();
        // Same for the in-bounds signal: a `yield` inside the closure must be out-of-bounds.
        // (Defensive; closures are single-expression so a closure `yield` is unparseable today.)
        let saved_ig = std::mem::replace(&mut self.in_generator, false);
        let saved_gf = self.gen_frame.take();
        // M24 Task 4: the witness scope CARRIES INTO a closure body. `$w:T` is never a free variable
        // (it is unspellable), so `compile_closure` appends it to the capture entries explicitly —
        // and, since M24-2, only where the body can REACH it, which is a strict superset of what
        // this scope licenses (`compiler::nested_body_needs_witness`). The witness crosses BY VALUE,
        // so a closure that outlives its defining frame still constructs the right type.
        // Mark BEFORE param binding so the free-closure finalize (below) is suppressed if EITHER an
        // un-inferable PARAM (`cannot infer type of parameter`) or the body emits a real error — a
        // residual `Unknown` return is then a cascade, not a genuine un-inferable return.
        // NOT a speculative-rollback site (hence the raw length, not `diag_mark`): the closure body is
        // checked exactly ONCE here, so its diagnostics — errors and any future warning alike — stay.
        // This mark is only read, to answer "did the body error?".
        let closure_mark = self.errors.len();
        self.push_scope();
        let param_tys: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // An annotated param keeps its type; an unannotated param takes the expected (slot)
                // param type (source #1), else is inferred from the body, else `Unknown`.
                let ty = match &p.ty {
                    Some(t) => self.resolve_type(t, body.span),
                    None => {
                        // An unannotated param: prefer the expected (slot) param type (source #1);
                        // else infer it from the body — source #2 (a match whose scrutinee is the
                        // bare param) / source #3 (a uniquely-owned member access).
                        //
                        // An `Unknown` expected param type is NOT a pin: it arises when a generic
                        // slot's type param was unified ONLY from this closure (`store(fn(a): …)` →
                        // `T = fn(Unknown) -> Unknown`), so binding the param to it silently would
                        // leave the call site unchecked → check-passes-then-traps. Filter it out and
                        // fall through to the body scan / annotation requirement (soundness).
                        if let Some(t) = exp_params
                            .as_ref()
                            .and_then(|ps| ps.get(i))
                            .filter(|t| !t.is_unknown())
                            .cloned()
                        {
                            t
                        } else if expected_arity_mismatch {
                            // Arity mismatch against a `fn`-typed slot — stay `Unknown`; the call
                            // site reports the mismatch (single diagnostic).
                            Ty::Unknown
                        } else if self.generic_arg_prepass {
                            // Generic unification prepass: keep the param `Unknown` so the other
                            // args / substituted slot type drive unification; `check_generic_arg`
                            // re-infers it in checking-mode afterwards. Running the free scan here
                            // would corrupt unification (see `generic_arg_prepass` doc).
                            Ty::Unknown
                        } else if let Some(t) = self.scan_free_closure_param(&p.name, body) {
                            t
                        } else {
                            // Genuinely unresolved: no expected/slot type and nothing in the body
                            // pins it. Require an annotation rather than degrade the param to a
                            // runtime `Unknown` value (the one place `Unknown` could reach a value).
                            // Bind `Unknown` after erroring so the body still checks (no cascade).
                            self.error(
                                p.name_span,
                                format!(
                                    "cannot infer type of parameter '{}'; add a type annotation",
                                    p.name
                                ),
                            );
                            Ty::Unknown
                        }
                    }
                };
                // Editor hover: record the closure param's type at its DECL-site name span (no-op
                // off-probe; first-hit-wins, so a body-use span records separately). SKIP during the
                // generic-arg unification prepass: there an unannotated param is forced `Unknown`
                // (see the `generic_arg_prepass` arm above), and first-hit-wins would latch that `?`
                // over the real type the later per-arg check (run with the substituted slot type and
                // `generic_arg_prepass=false`) infers — so `xs.map(fn(a): a + 1)` would hover `?`.
                if !self.generic_arg_prepass {
                    self.hover_record_at(p.name_span, &ty, HoverKind::Param, None);
                }
                self.declare(&p.name, ty.clone());
                ty
            })
            .collect();
        // TICKET-227: the body's slot is the declared return, else a concrete return of the
        // `fn`-typed slot this closure lands in.
        let slot = if ret.is_some() {
            Some(self.current_ret.clone())
        } else {
            match expected {
                Some(Ty::Func { ret: er, .. }) if ty_fully_concrete(er) => Some((**er).clone()),
                _ => None,
            }
        };
        let body_mark = self.tyvar_mark();
        let body_ty = self.inline_body_ty(body, slot);
        self.last_closure_writes = self
            .closure_write_frames
            .pop()
            .map(|(_, writes)| writes)
            .unwrap_or_default();
        if !self.last_closure_writes.is_empty() {
            let mut writes: Vec<String> = self.last_closure_writes.iter().cloned().collect();
            writes.sort();
            self.closure_literal_writes
                .insert((self.graph_module_idx, body.span), writes);
        }
        let closure_had_err = self.errors.len() > closure_mark;
        self.pop_scope();
        self.loop_depth = saved_loop_depth;
        self.recover_depth = saved_recover;
        self.in_defer_block = saved_in_defer;
        self.current_ret = saved_ret;
        self.in_fn_body = saved_in_fn;
        self.in_default_provider = saved_in_dflt;
        self.exit_own_frame(saved_frame);
        self.yield_ty = saved_yield;
        self.in_generator = saved_ig;
        self.gen_frame = saved_gf;
        let ret_ty = match ret {
            Some(t) => {
                let declared = self.resolve_ret_type(t, body.span);
                // The body owned the declared return as its slot, so a wrapped body already has
                // the declared type (TICKET-227).
                if !self.assignable(&declared, &body_ty) {
                    self.error(
                        body.span,
                        format!(
                            "closure body has type {body_ty}, but its return type is {declared}{}",
                            slot_note(&declared, &body_ty)
                        ),
                    );
                }
                declared
            }
            // An un-annotated closure's body IS its inferred return — apply the SAME finalize as a
            // free fn/method (Result E-slot default + reject a residual un-inferable `Unknown`), but
            // ONLY for a GENUINELY FREE closure literal. Gated on `expected.is_none()` so a closure
            // sitting in a `fn`-typed slot (source #1) is untouched, and `!generic_arg_prepass` so the
            // proto.rs generic/HOF loop-back contexts (where an `Unknown`/`Param` return is legit and
            // resolved later) are excluded. `!body_had_err` avoids piling onto a real body error.
            None => {
                if expected.is_none() && !self.generic_arg_prepass && !closure_had_err {
                    // TICKET-238 -- a free closure's return binds its body: close the body's
                    // variables first, so `fn(): ?5` is `fn() -> int?`.
                    let body_ty = self.close_operand(body_mark, body_ty);
                    self.finalize_ret(
                        &body_ty,
                        "<closure>",
                        body.span,
                        super::tyvar::open_err(&body_ty),
                    )
                } else {
                    body_ty
                }
            }
        };
        // A closure/lambda value carries its param names as labels, so a keyword call through a
        // closure value (`cb := fn(name: str): …; cb(name="X")`) resolves.
        let labels: Vec<Option<String>> = params.iter().map(|p| Some(p.name.clone())).collect();
        Ty::Func {
            params: param_tys,
            ret: Box::new(ret_ty),
            labels: FnLabels::new(labels),
            variadic: crate::ast::Variadic::No,
        }
    }

    // ===== calls =====
}
