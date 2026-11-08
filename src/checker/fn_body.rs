//! Checker: return and yield checks, generator restrictions, default scope, fn bodies, the
//! discarded-carrier warning.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::*;

impl Checker {
    pub(super) fn check_return(&mut self, value: Option<&Expr>, span: Span) {
        // A module-level `return` is an error, as in CPython (`'return' outside function`). The
        // compiler still lowers it, so `run_file` tests (which skip the checker) reach the
        // runtime check. A block around the `return` names itself instead, and its error is the
        // only one: the enclosing fn's return type does not apply to a `return` it rejects.
        let rejected = self.reject_in_block(boundary::Construct::Return, span);
        if rejected || !self.in_fn_body {
            if let Some(e) = value {
                self.infer(e);
            }
            if !rejected {
                self.error(span, "'return' outside a function");
            }
            return;
        }
        // Inside a generator, a `return` may only be bare (stop the iterator early). A returned
        // value is meaningless — the generator's result type is the stream, not a single value.
        if self.yield_ty.is_some() {
            if let Some(e) = value {
                let _ = self.infer(e);
                self.error(
                    e.span,
                    "a generator cannot `return` a value; use a bare `return` to stop early",
                );
            }
            return;
        }
        let ret = self.current_ret.clone();
        match value {
            Some(e) => {
                // A return keeps plain `infer` (NOT `infer_value`): returning `nil` just makes a void fn — it
                // is not "using nil as a value", so it must not get `infer_value`'s nil-rejection on
                // top of check_return's own `function returns nothing` diagnostic.
                let ty = {
                    // Expected-type checking-mode: thread the declared return type as a hint so a
                    // returned generic ctor / generic fn-call pre-seeds its type params from it —
                    // `fn mk() -> Heap[int]: return Heap([], fn(x, y): x < y)` pins `T=int`. `unify`
                    // no-ops on a `Nil` (void) ret, so setting it unconditionally is safe; pair with
                    // an immediate clear so a non-call return value never leaks the hint.
                    // TICKET-227: a DECLARED return is a typed slot, so the value owns it and may
                    // wrap (`Some(v)`/`Ok(v)`); an inferred return is a seed only. A default
                    // provider's return is a slot like any other (its decl-site half wraps too).
                    self.install_hint(e, ret.clone(), self.ret_declared);
                    let t = self.infer(e);
                    self.expected_hint = None;
                    self.hint_owner = None;
                    t
                };
                if ret == Ty::Nil && !self.ret_declared && self.ret_owner.is_some() {
                    if super::tyvar::open_err(&ty) {
                        self.error(e.span, super::tyvar::ERR_VALUE_NEEDS_TYPE.to_string());
                    } else {
                        let name = self.ret_owner.clone().unwrap_or_default();
                        let fix = if ty_fully_concrete(&ty) && ty != Ty::Nil {
                            format!("add '-> {ty}' to '{name}'")
                        } else {
                            format!("add a '->' return type to '{name}'")
                        };
                        self.error(
                            e.span,
                            format!(
                                "'{name}' declares no return type, so it returns nothing; {fix}"
                            ),
                        );
                    }
                } else if ret == Ty::Nil {
                    self.error(e.span, "function returns nothing, cannot return a value");
                } else if !self.assignable(&ret, &ty) {
                    let note = self.protocol_note(&ret, &ty);
                    let [ret_s, ty_s] = Ty::render_distinct([&ret, &ty]);
                    self.error(
                        e.span,
                        format!(
                            "expected return type {ret_s}, found {ty_s}{note}{}",
                            slot_note(&ret, &ty)
                        ),
                    );
                }
            }
            None => {
                // W8-21 — a bare `return` at a `Result[nil, E]` sink coerces to DEC-017's zero-arg
                // `Ok()`. See `ret_coerce_bare`. A bare `return` has no value node, so this is
                // the one span-keyed wrap left.
                let mode = if self.ret_declared {
                    self.ret_coerce_bare(&ret)
                } else {
                    None
                };
                self.record_ret_coerce(span, mode);
                if mode.is_none() && ret != Ty::Nil && !ret.is_unknown() {
                    self.error(span, format!("expected a return value of type {ret}"));
                }
            }
        }
    }

    /// A generator body rejects the statements `boundary::RULES` names for it, in every block the
    /// body holds (`ast::stmt_blocks`), but not in nested `fn` definitions (those have their own
    /// generator status). A rejected statement's own blocks are not walked: one error per statement.
    pub(super) fn check_generator_restrictions(&mut self, body: &[Stmt]) {
        for s in body {
            if let Some(c) = boundary::Construct::of_stmt(&s.kind)
                && let Some(boundary::Rule::Rejected(reason)) = boundary::Block::Generator.rule(c)
            {
                self.error(
                    s.span,
                    boundary::message(boundary::Block::Generator, c, reason),
                );
                continue;
            }
            let mut blocks = Vec::new();
            crate::ast::stmt_blocks(s, &mut blocks);
            for b in blocks {
                self.check_generator_restrictions(b);
            }
        }
    }

    /// TICKET-184 — the control-flow summary of `body` ([`flow::block`]), with divergence read from
    /// each call's resolved callee. Read it only after `body` has been walked in this pass.
    pub(super) fn block_flow(&self, body: &[Stmt]) -> flow::Flow {
        flow::block(body, &|e| self.call_diverges(e))
    }

    /// `yield <expr>` — legal only inside a generator function (one whose return type is
    /// `Iterator[T]`); the operand must be assignable to the element type `T`.
    pub(super) fn check_yield(&mut self, e: &Expr, span: Span) {
        let slot = self.yield_ty.clone();
        let ty = self.infer_in_slot(e, slot);
        // `in_generator` (not `yield_ty.is_some()`) is the in-bounds signal.
        if !self.in_generator {
            self.error(span, "`yield` can only appear inside a generator function");
            return;
        }
        // Pass 2: validate each yield against the pinned element type `T`. An `int` yielded under an
        // inferred/annotated `float` `T` is rejected (D3: no int→float slot widening).
        if let Some(elem) = self.yield_ty.clone()
            && !self.assignable(&elem, &ty)
        {
            let note = self.protocol_note(&elem, &ty);
            let [elem_s, ty_s] = Ty::render_distinct([&elem, &ty]);
            self.error(
                e.span,
                format!(
                    "expected yield type {elem_s}, found {ty_s}{note}{}",
                    slot_note(&elem, &ty)
                ),
            );
        }
    }

    /// TICKET-201 — the one default-legality rule. A default runs in module scope (its provider
    /// fn, or the prologue with the frame's locals hidden), so a free name of `def` must not:
    /// (a) resolve to an enclosing fn's local (TICKET-142: the compiler would panic or silently
    /// read the global), or (b) name a struct-body binder in `default_binders` declared strictly
    /// before `owner`, the name of the field or method that owns the default. CPython evaluates
    /// both where the default is written, so each is declined, never silently rebound. A binder
    /// inside the default (lambda param, comprehension variable) is not free and never matches.
    pub(super) fn check_default_scope(&mut self, def: &Expr, owner: Span) {
        let mut free: Vec<String> =
            crate::compiler::free_names_of_expr(def, &std::collections::HashSet::new())
                .into_iter()
                .collect();
        free.sort();
        for n in &free {
            if self.owning_scope(n).is_some_and(|i| i > 0) {
                self.error(
                    def.span,
                    format!(
                        "a nested fn's default cannot read the enclosing fn's locals: '{n}' is local here, and a default is evaluated in module scope (pass it as an argument, or read a module-level binding)"
                    ),
                );
                return;
            }
            // `Span` derives no `Ord`: compare `(line, col)` within one file.
            let body = self.default_binders.iter().find(|(b, sp, _)| {
                b == n && sp.file == owner.file && (sp.line, sp.col) < (owner.line, owner.col)
            });
            if let Some((_, _, ty_name)) = body {
                let msg = format!(
                    "default value cannot read '{n}': '{n}' is bound in the body of '{ty_name}' where the default is written (an earlier field default or method), but a default runs in module scope; pass it explicitly or read a module-level binding"
                );
                self.error(def.span, msg);
                return;
            }
        }
    }

    pub(super) fn check_fn_body(&mut self, decl: &FnDecl, self_ty: Option<Ty>, sig: FnSig) {
        #[cfg(any(test, feature = "test-api"))]
        {
            self.fn_body_checks += 1;
        }
        let provider = decl
            .name
            .starts_with(crate::base::provider_name::PROVIDER_PREFIX)
            .then(|| (decl.name.clone(), decl.name_span));
        if let Some((n, sp)) = &provider {
            self.provider_edges
                .entry(n.clone())
                .or_insert_with(|| (Vec::new(), *sp));
        }
        let saved_provider = std::mem::replace(&mut self.current_provider, provider);
        // TICKET-225: one fn body is one type-variable frame.
        let frame = self.tyvar_mark();
        let span = decl
            .body
            .first()
            .filter(|_| decl.inline_expr_body)
            .map_or(decl.name_span, |s| s.span);
        self.with_stmt_mark(span, |c| c.check_fn_body_inner(decl, self_ty, sig));
        self.close_tyvar_frame(frame);
        self.current_provider = saved_provider;
    }

    /// Whether a discarded value of `t` warns: a `Result`/`Option`, or a std struct that
    /// `MUST_USE_STD_STRUCTS` names (a `Task` holds a job's fault or cancel that only `get()`
    /// reads). A user struct of the same name has `StructOrigin::User` and never matches.
    pub(super) fn is_must_use(&self, t: &Ty) -> bool {
        match t {
            Ty::Result(..) | Ty::Option(_) => true,
            Ty::Struct(key, _) => {
                MUST_USE_STD_STRUCTS.contains(&key.as_str())
                    && self
                        .struct_shape(key)
                        .is_some_and(|s| s.origin == StructOrigin::Builtin)
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
            | Ty::Enum(..)
            | Ty::Param(_)
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
        }
    }

    /// An expression whose value is discarded (an expression statement). The one place that
    /// decides it: the carrier and lost-write warnings fire here, and a one-line `if`/`else`
    /// statement checks each branch here instead of joining the branch types.
    pub(super) fn check_discarded(&mut self, e: &Expr) {
        // TICKET-242 — `if c: a() else: b()` as a statement is the if-expression with its value
        // discarded, so the branches need not join. Each branch is a discarded value of its own
        // (an `elif` chain is a nested `IfElse` in `els`).
        if let ExprKind::IfElse { cond, then, els } = &e.kind {
            self.expect_bool(cond, "if condition");
            self.check_discarded(then);
            self.check_discarded(els);
            return;
        }
        // W8-2 — warn on a dropped `Result`/`Option`, in every proto including module top
        // level. Rust owns both types and marks them `#[must_use]`, with `let _ = …` as the
        // escape; this is that warning, non-fatal so the exit code is unchanged.
        //
        // TICKET-038 reversed the old top-level exemption: the runtime no longer checks a
        // top-level drop at all (the top-level pop opcode's check is deleted), so this
        // warning is now the ONLY check on it, type-dependent and firing in every spelling —
        // strictly wider than the old value-dependent abort, which caught only a bare drop
        // and was already escaped by both `r := g()` and `_ := g()`.
        //
        // Also NOT reached by: `defer f.close()` (its own arm above — Go's canonical
        // unchecked idiom), a `fn f(): g()` inline-expr body (an implicit return, inferred
        // off `check_fn_body`), and a value-block's trailing expression (inferred directly
        // by the value-`match`/`if` tails; a `recover:` block ends in an ordinary statement). `?`/`??`/`?.` already yield
        // the UNWRAPPED payload, so they are not carriers here.
        let t = self.infer(e);
        // TICKET-254 — a bare mutating call on a temporary copy (`xs.copy().push(1)`) drops the
        // write. Returns BEFORE the carrier match — `xs.copy().pop()` also returns a carrier, and
        // the lost-write message is the more actionable one, so one statement never carries both.
        if let Some((obj, method)) = fn_writes::method_call_parts(e)
            && self.temporary_write(obj)
        {
            let recv = self.probe_ty(obj);
            if mutates_receiver(&recv, method) {
                self.warn(
                    e.span,
                    format!(
                        "'{method}' changes a temporary copy; the result is dropped: bind the value to a name first, or call '{method}' on the source"
                    ),
                );
                return;
            }
        }
        // `Unknown` lands here too: an already-reported expression must not cascade.
        if !self.is_must_use(&t) {
            return;
        }
        // Name the callee when there is one, so the warning points at the culprit rather
        // than at a line. The hint has to be code the user can actually TYPE, so it spells
        // the call back only when the call is genuinely reproducible from the callee name
        // alone — a plain NULLARY `g()`. With arguments, `format!("{name}()")` dropped them
        // and emitted a hint that does not compile: `takes(1, "a")` suggested `r := takes()`,
        // which is `'takes' expects 2 argument(s), got 0`. Reconstructing an argument list
        // (or a METHOD call's receiver) from the AST would be guesswork, so both elide to
        // `…` — the subject already names the callee, which is what points at the culprit.
        let (subject, fix) = match &e.kind {
            ExprKind::Call { callee, args, .. } => match &callee.kind {
                ExprKind::Ident(name) => (
                    format!("the `{t}` value returned by '{name}'"),
                    if args.is_empty() {
                        format!("{name}()")
                    } else {
                        "…".to_string()
                    },
                ),
                ExprKind::Field { name, .. } => {
                    (format!("the `{t}` value returned by '{name}'"), "…".into())
                }
                ExprKind::Int(_)
                | ExprKind::Float(_)
                | ExprKind::Str(_)
                | ExprKind::Interp(_)
                | ExprKind::Bytes(_)
                | ExprKind::RawStr(_)
                | ExprKind::Bool(_)
                | ExprKind::NoneLit
                | ExprKind::Pass
                | ExprKind::List(..)
                | ExprKind::Tuple(_)
                | ExprKind::Map(_)
                | ExprKind::Set(_)
                | ExprKind::Comprehension { .. }
                | ExprKind::Unary { .. }
                | ExprKind::Binary { .. }
                | ExprKind::Compare { .. }
                | ExprKind::Range { .. }
                | ExprKind::Call { .. }
                | ExprKind::Index { .. }
                | ExprKind::Try(_)
                | ExprKind::OptChain { .. }
                | ExprKind::NullCoalesce { .. }
                | ExprKind::Closure { .. }
                | ExprKind::Match { .. }
                | ExprKind::IfElse { .. }
                | ExprKind::Recover(..)
                | ExprKind::ElseGuard { .. } => (format!("the `{t}` value here"), "…".to_string()),
            },
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => (format!("the `{t}` value here"), "…".to_string()),
        };
        self.warn(
            e.span,
            format!(
                "{subject} is discarded — bind it (`r := {fix}`), or discard it explicitly \
                 (`_ := {fix}`)"
            ),
        );
    }

    fn check_fn_body_inner(&mut self, decl: &FnDecl, self_ty: Option<Ty>, sig: FnSig) {
        // Enter the sig's type params (NOT `decl.type_params`): `fn_sig` folds any `where T: Bound`
        // clause into them, so the body sees a `where`-bounded param as satisfying its bound (e.g. a
        // `where T: Comparable` param may use `<` in the body). Same names as `decl.type_params`
        // (the merge only adds bounds), so the reserved/shadow checks in `fn_sig` still cover them.
        let saved_tps = self.enter_ty_params(&sig.type_params);
        // CONDITIONAL METHOD: a receiver `where T: Bound` (`T` the ENCLOSING type's param, carried on
        // `sig.where_bounds`) constrains the receiver at call sites — but the method's BODY may also
        // use the bounded operation (e.g. `<` needs `Comparable`), exactly as a free fn's `where`
        // does (which merges into `type_params`, so the body sees the bound). The enclosing param is
        // already in scope here (entered by the type's decl hoist with its BARE bounds); merge the
        // receiver bounds onto it for the duration of this body so `self.val < other` type-checks.
        // `exit_type_params(saved_tps)` restores the bare enclosing param afterward. No-op for a free
        // fn or a plain method (`where_bounds` empty). Dedup so a bound already declared on the
        // enclosing param is not doubled (harmless for bound-checking, but keeps the set clean).
        for tp in &sig.where_bounds {
            if let Some(bounds) = self.type_params.get_mut(&tp.name) {
                for b in &tp.bounds {
                    if !bounds.contains(b) {
                        bounds.push(b.clone());
                    }
                }
            }
        }
        let saved_ret = std::mem::replace(&mut self.current_ret, sig.ret.clone());
        // W8-21 — is this sink DECLARED? Gates the success-coercion sinks: an un-annotated fn's
        // `sig.ret` is INFERRED from the body, so gating on it would be circular.
        let saved_ret_decl = std::mem::replace(&mut self.ret_declared, decl.ret.is_some());
        let saved_ret_owner = self.ret_owner.replace(decl.name.clone());
        // Inside a fn body now: a `?` on a `Nil`-returning body must be REJECTED (would swallow the
        // Err/None), unlike module top-level where `Nil` accepts either. Saved/restored beside
        // `current_ret`.
        // The other outermost fn walk (see `infer_inline_fn_ret`): `infer_returns`' finalize pass settles
        // stored rets (the `Result` E-slot among them) before this pass-2 check walk runs, so an
        // entry computed under the earlier sigs must not be served here.
        if !self.in_fn_body {
            self.ret_memo.clear();
        }
        let saved_in_fn = std::mem::replace(&mut self.in_fn_body, true);
        // W7-51 — is this a synthesized default-argument provider? Read off the name (the `$` prefix
        // is unspellable in source), and saved/restored beside `current_ret` so a closure or nested
        // fn INSIDE a provider clears it and keeps its own `?` diagnostics.
        let saved_in_dflt = std::mem::replace(
            &mut self.in_default_provider,
            decl.name
                .starts_with(crate::base::provider_name::PROVIDER_PREFIX),
        );
        // `Self` in this method body resolves to the enclosing type (`None` for a free fn / nested fn,
        // which resets an enclosing method's binding). Restored below beside `current_ret`.
        let saved_self = std::mem::replace(&mut self.current_self_ty, self_ty.clone());
        // A generator (`is_generator`, i.e. its body contains `yield`) declares `-> Iterator[T]`;
        // `T` is the per-yield element type. The `_` arm fires for a wrong annotation (`-> int`)
        // and for a generator with no `->`: the element type is written, never inferred.
        let new_yield_ty = if decl.is_generator {
            match &sig.ret {
                Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                    Some(args[0].clone())
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
                | Ty::Var(_) => {
                    let span = decl.body.first().map(|s| s.span);
                    if let Some(span) = span {
                        self.error(
                            span,
                            "a generator function (one that uses `yield`) must declare a return type of `Iterator[T]`",
                        );
                    }
                    None
                }
            }
        } else {
            None
        };
        if decl.is_generator {
            self.check_generator_restrictions(&decl.body);
        }
        let saved_yield = std::mem::replace(&mut self.yield_ty, new_yield_ty);
        // `in_generator` (not `yield_ty.is_some()`) is the in-bounds signal for a `yield` — kept in
        // sync with `yield_ty` here so pass-2 `check_yield` validates a generator's yields (and a
        // stray `yield` in a non-generator body is diagnosed as out-of-bounds).
        let saved_ig = std::mem::replace(&mut self.in_generator, decl.is_generator);
        // A nested function checked inside a speculative walk of an outer inline body is checked
        // for real: its `return`s are diagnosed.
        let saved_inferring = std::mem::replace(&mut self.inferring_ret, false);
        // A function body opens a fresh loop context: a loop enclosing this fn's *definition* must
        // not make a `break`/`continue` in the body legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        // A nested fn is its own frame: a `?` or `return` in it targets it, whatever block holds
        // its definition.
        let saved_frame = self.enter_own_frame();
        // M24 — the witness params whose `$w:T` binding this body can reach, and the name the
        // contract's fn-half keys them under. A MODULE-LEVEL FREE fn keys on its own name; a MEMBER
        // (Task 5 — a method or static method declaring its own `[T]`) keys on `<type key>.<method>`,
        // which cannot collide with a fn name (no fn name contains a `.`). `saved_in_fn` (the
        // pre-replace `in_fn_body`) being true means this is a NESTED fn, which the compiler lowers
        // as a closure that DECLARES no witness param — it INHERITS the enclosing scope through the
        // capture entries instead (Task 4), so its scope is left untouched below. A body whose
        // receiver is neither struct nor enum declares none and inherits none (nothing
        // there declares a proto the witness could ride on).
        // A `from` overload keys on its internal name (`from$int`), so two overloads never
        // share an entry.
        let witness_fn_name = match &self_ty {
            _ if saved_in_fn => None,
            None => Some(decl.name.clone()),
            Some(Ty::Struct(k, _) | Ty::Enum(k, _)) => {
                let m = self
                    .witnesses
                    .overload_names
                    .get(&(self.graph_module_idx, decl.name_span))
                    .unwrap_or(&decl.name);
                Some(format!("{k}.{m}"))
            }
            Some(_) => None,
        };
        let wparams = match &witness_fn_name {
            Some(_) => sig.witness_params.clone(),
            None => Vec::new(),
        };
        // Half one of the contract: which fns need hidden trailing witness params — read off the
        // signature, which computed it once at the hoist ([`Checker::witness_params_of`]), so this
        // arity agrees with every other consumer by construction. Nested fns are never recorded, so a
        // nested fn sharing a top-level fn's name cannot inherit its arity.
        if self.harvest_keywords
            && !wparams.is_empty()
            && let Some(fname) = witness_fn_name
        {
            self.witnesses.fns.insert(
                (self.graph_module_idx, fname),
                wparams.iter().map(WitnessParam::tag).collect(),
            );
        }
        // A NESTED fn inherits (Task 4 — its `MakeClosure` carries the `$w:T` entries); every other
        // body gets exactly the witness params it declares. Restoring the clone is a no-op for the
        // inherit arm, which is what "inherits" means.
        let saved_witness_scope = if saved_in_fn {
            self.witness_scope.clone()
        } else {
            std::mem::replace(&mut self.witness_scope, wparams)
        };
        // TICKET-201: default legality, judged in the scope the default runs in. A nested fn sees
        // the enclosing fn's locals (declined) but no struct-body binder.
        let saved_binders = saved_in_fn.then(|| std::mem::take(&mut self.default_binders));
        for param in &decl.params {
            if let Some(def) = &param.default {
                self.check_default_scope(def, decl.name_span);
            }
        }
        if let Some(b) = saved_binders {
            self.default_binders = b;
        }
        self.push_scope();
        // Editor hover: record the function's OWN signature at its decl-site name token (no-op
        // off-probe; behavior-neutral — `name_span` is runtime-inert). Covers free fns AND methods,
        // both routed through here. For a method, `record_method_decl_hover` (called from the
        // struct/enum arm BEFORE this) already latched the receiver-stripped sig first
        // (first-hit-wins in `hover_record_at`), so this is a harmless no-op there; for a free fn
        // there is no prior record, so this produces the previously-missing fn-name hover.
        if self.hover_probe.is_some() {
            let fty = crate::checker::fn_value_ty(&sig);
            self.hover_record_at(decl.name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
        let param_ty = |i: usize, _: &Param| sig.params.get(i).cloned().unwrap_or(Ty::Unknown);
        // TICKET-201: every decl-site default is typed before any parameter of this fn is
        // declared: a default runs in module scope, so a parameter's name there reads the module
        // binding (CPython), and an unbound one is `unknown name`.
        for (i, param) in decl.params.iter().enumerate() {
            let ty = param_ty(i, param);
            // A constant-literal default must itself be assignable to the parameter's type — checked
            // here (where type params are in scope) so a wrong-typed default is caught at the
            // declaration even when every call overrides it.
            if let Some(def) = &param.default {
                // W7-51 — this DECL-SITE copy is not in this function's body: a default runs in its
                // own provider, in its defining module. Validating a `?` in it against the enclosing
                // `current_ret` therefore named a return type that does not describe where the
                // default runs, and duplicated the tailored provider-body diagnostic at the same
                // span (measured: `fn f(x: int = getr()?.len()) -> int` emitted BOTH the stale
                // `'?' used in a function that returns int` and the tailored message, while the same
                // default under a `-> int!str` enclosing fn emitted only the tailored one — the
                // wording depended on the caller's shape). `Nil` + `!in_fn_body` is the one
                // `current_ret` pairing `infer_try` accepts silently for both carriers, so the
                // provider body stays the single place a default's `?` is judged.
                //
                // …but ONLY when there IS a provider body. `desugar::dflt_for` keeps the historical
                // inline clone for an un-annotated parameter, and for one whose type or expression
                // mentions an enclosing type parameter or `Self`; for those, silencing the `?` here
                // silences it everywhere. Measured: `fn f[T](x: T = mk[T]()?) -> T` was `1 type
                // error` on `b1307258` and `ok: no type errors` on `dfdc7a1b`, the error reappearing
                // only if someone CALLED `f`. So ask whether the provider exists — under the one
                // name `desugar` would have given it — instead of assuming it does.
                // Rebuilding that name here from the host's identity key once missed for every
                // METHOD default (`<module-key>::Name` vs desugar's bare `Name`, measured on
                // `0104d57b`: two diagnostics at one span).
                // So the name is read off the sig's slots, which the one owner builder named
                // (`provider_name::method_owner`, an overload's ordinal included).
                let provider = sig.slots.iter().flatten().find_map(|s| {
                    if s.name.as_deref() != Some(param.name.as_str()) {
                        return None;
                    }
                    if let Some(crate::desugar::Dflt::Provider { name }) = &s.default {
                        return Some(name);
                    }
                    None
                });
                let judged_by_provider = provider.is_some_and(|n| self.functions.contains_key(n));
                let saved_ret = self.current_ret.clone();
                let saved_in_fn = self.in_fn_body;
                if judged_by_provider {
                    self.current_ret = Ty::Nil;
                    self.in_fn_body = false;
                }
                // **A generic provider's own binders are universally quantified here.** The
                // decl-site copy runs before any call, so `fn mkl[Z]() -> List[G[Z]]` used as a
                // default for `List[G[T]]` comes back still spelling `Z`. Seeding the declared type
                // as the hint is what resolves them: `seed_from_hint` unifies the provider's return
                // against the slot, binding `Z := T` — and, because that binding then EXISTS,
                // `enforce_bounds` inside the call can finally check it.
                //
                // Without the seed the comparison matched binders by NAME: `mkl[T]` against `G[T]`
                // was accepted while the alpha-renamed `mkl[Z]` gave *default value for parameter
                // 'xs': expected List[G[T]], found List[G[Z]]* — one declaration, two verdicts,
                // decided by a letter — and the provider's own `where` bound was never enforced at
                // all, so `fn mkl[Z: Show]() -> List[G[Z]]` defaulting a `List[G[T]]` was accepted
                // with nothing showing `T: Show` (both spellings, measured), while the SAME call
                // written out was correctly rejected.
                //
                // **The gate names the ONE shape that must be EXCLUDED, never the shapes that work.**
                // A first cut gated on `matches!(def.kind, ExprKind::Call { .. })` — the provider
                // shape — which is a SYNTACTIC test where the rule is about the TYPE: any call inside
                // anything skipped the seed and fell back to the spelling-matched comparison, so
                // `= mkl() + mkl()` still gave `ok` for `mkl[T]` and the mismatch for `mkl[Z]`, and
                // its `where` bound went unenforced while the bare `= mkl()` twin was correctly
                // rejected — same declaration, opposite verdicts, decided by a wrapper.
                //
                // A BARE GENERIC FN VALUE is seeded when its slot is concrete up to in-scope type
                // params (`f: fn(U) -> U = ident` inside `fn g[U]`): the caller's `U` pins like a
                // concrete type (`rigid_param`), whatever the callee's own param is spelled. Only a
                // slot with an `Unknown` leaf stays unseeded; there the read reports *'ident' is
                // generic and … is not determined here* in every spelling.
                let seed = ty_concrete_but(&ty, &|n| self.rigid_param(n, &[]))
                    || self.generic_fn_value_sig(def).is_none();
                let hint = seed.then(|| ty.clone());
                let saved_dsd = std::mem::replace(&mut self.decl_site_default, true);
                let actual = self.infer_arg(def, hint.as_ref());
                let actual = self.resolve_default_binders(&ty, actual);
                self.decl_site_default = saved_dsd;
                self.current_ret = saved_ret;
                self.in_fn_body = saved_in_fn;
                if !matches!(ty, Ty::Unknown) && !self.assignable(&ty, &actual) {
                    let note = self.protocol_note(&ty, &actual);
                    let [ty_s, actual_s] = Ty::render_distinct([&ty, &actual]);
                    self.error(
                        def.span,
                        format!(
                            "default value for parameter '{}': expected {ty_s}, found {actual_s}{note}{}",
                            param.name,
                            slot_note(&ty, &actual)
                        ),
                    );
                }
            }
        }
        for (i, param) in decl.params.iter().enumerate() {
            let ty = param_ty(i, param);
            // Editor hover: record the param's declared type at its DECL-site name span (no-op
            // off-probe; covers free fns AND methods, both routed through check_fn_body).
            self.hover_record_at(param.name_span, &ty, HoverKind::Param, None);
            // A declared fn's param type is written in its signature. A hole in it is an
            // annotation the signature already rejected (`x: List[None]`), so it is not judged
            // again (TICKET-238). `infer_inline_fn_ret` declares the same params through the
            // checked `declare`: that walk is rolled back, so its check reports nothing.
            self.declare_unchecked(&param.name, ty);
        }
        // An inline-expr body (`fn a() -> T: <expr>`) implicitly returns its single expression,
        // exactly as a `return <expr>` would. We infer that expr ONCE here and validate it against
        // the declared return type with the same diagnostics `check_return` uses — so we must NOT
        // also run the statement-position `check_stmt` on it (that would infer it a second time and
        // double every error inside the expression). Any other body is checked statement-by-
        // statement as usual.
        if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            let ret = sig.ret.clone();
            // TICKET-227: a declared inline body is a typed slot; its value may wrap.
            let ty = self.infer_in_slot(e, decl.ret.is_some().then(|| ret.clone()));
            if ret == Ty::Nil {
                // A NON-nil expr against `-> nil` is a void fn that actually returns a value —
                // reject it, mirroring the multiline `return <expr>` path. A nil-typed inline expr
                // (e.g. a bare void call) implicitly returns nil and stays legal.
                if ty != Ty::Nil && !ty.is_unknown() {
                    self.error(e.span, "function returns nothing, cannot return a value");
                }
            } else {
                // An inline-expr body implicitly returns its single expression; a wrapped value
                // already has the slot's type.
                if !self.assignable(&ret, &ty) {
                    let note = self.protocol_note(&ret, &ty);
                    let [ret_s, ty_s] = Ty::render_distinct([&ret, &ty]);
                    self.error(
                        e.span,
                        format!(
                            "expected return type {ret_s}, found {ty_s}{note}{}",
                            slot_note(&ret, &ty)
                        ),
                    );
                }
            }
        } else {
            for stmt in &decl.body {
                self.check_stmt(stmt);
            }
        }
        // Option B: a function with a *declared* non-void return type must return a value on every
        // control-flow path. The gate is the user's *annotation* (`decl.ret.is_some()`), NOT the
        // resolved `sig.ret`: an UN-annotated fn that returns a value on some path (the common
        // early-return / `find` idiom) infers a non-nil `sig.ret`, but with no `-> T` it stays
        // legal — gating on `sig.ret` alone would wrongly reject it. A bare `fn a(): 10` (no
        // annotation) is exempt; generators (`-> Iterator[T]`, value-produced via `yield`) too. If
        // the body can fall off the end, that silently yields nil at runtime — turn it into a loud
        // static error.
        let must_return = !decl.is_generator
            && !decl.inline_expr_body
            && decl.ret.is_some()
            && sig.ret != Ty::Nil;
        let falls_through = self.block_flow(&decl.body).falls_through;
        // TICKET-227: a `None!E` fn's end is a success, `Ok(nil)`, like a bare `return`.
        let ok_nil =
            matches!(&sig.ret, Ty::Result(t, _) if **t == Ty::Nil) && ty_fully_concrete(&sig.ret);
        // TICKET-184: a proved-unreachable end is recorded, so the compiler traps there instead of
        // returning a silent `nil`. Main pass only; a decl with a default span has no key.
        let fall_off = if !falls_through {
            Some(FallOff::Trap)
        } else if ok_nil {
            Some(FallOff::OkNil)
        } else {
            None
        };
        if must_return
            && let Some(f) = fall_off
            && !self.generic_arg_prepass
            && !self.resolving_returns
            && decl.name_span != Span::default()
        {
            self.fall_off
                .insert((self.graph_module_idx, decl.name_span), f);
        }
        if must_return
            && falls_through
            && !ok_nil
            && let Some(span) = decl.body.first().map(|s| s.span)
        {
            let ret = &sig.ret;
            // TICKET-263: a call the checker does not know never returns (`os.exit`, a `die()`
            // helper) leaves the fn falling off; the hint names the fix.
            let hint = match decl.body.last().and_then(|s| s.kind.as_expr()) {
                Some(Expr {
                    kind: ExprKind::Call { .. },
                    ..
                }) => "; if a call above never returns, write `panic(...)` after it",
                _ => "",
            };
            self.error(
                span,
                format!(
                    "function '{}' has return type {ret} but can fall off the end without returning a value; add an explicit `return`, or use a closure `fn() -> {ret}: <expr>` which implicitly returns its expression body{hint}",
                    decl.name
                ),
            );
        }
        self.pop_scope();
        self.current_ret = saved_ret;
        self.ret_owner = saved_ret_owner;
        self.ret_declared = saved_ret_decl;
        self.in_fn_body = saved_in_fn;
        self.in_default_provider = saved_in_dflt;
        self.current_self_ty = saved_self;
        self.yield_ty = saved_yield;
        self.in_generator = saved_ig;
        self.inferring_ret = saved_inferring;
        self.loop_depth = saved_loop_depth;
        self.exit_own_frame(saved_frame);
        self.witness_scope = saved_witness_scope;
        self.exit_type_params(saved_tps);
    }
}
