//! Checker: return-type inference and module-global typing (`infer_returns`, `seed_module_globals`,
//! `type_globals_pass`, `let_value_ty`, `fill_ret`).
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::pattern::BindMode;
use super::setup::HeadBinding;
use super::*;

impl Checker {
    /// Pass-1.5: type the module globals and every INLINE EXPRESSION body that omitted `-> T`, in
    /// dependency order, and overwrite the pending `Unknown` `fn_sig` left for such a body. A block
    /// body is not walked here: `fn_sig` already wrote `None` for it. Runs after `hoist`, so all
    /// type names, variants and function sigs are visible.
    ///
    /// The loop only ORDERS the walks (a callee below its caller, a global below its reader,
    /// TICKET-183): each pass re-walks the bodies still pending, and a type a clean walk wrote is
    /// visible to the next pass. A body is typed only from a walk in which every call had a type
    /// (`untyped_call`), so nothing is joined and nothing a clean walk wrote is revised. A recursive
    /// or mutually recursive inline fn therefore never gets a type; the finalize pass reports it
    /// and asks for `->`.
    ///
    /// `seed_module_globals` writes four facts into scope 0 (type, `const`, keyword certainty
    /// `kw_certain`, closure writes `written_captures`) and `type_globals_pass` refines the type
    /// alongside each return pass. A global left `Unknown` because its callee returns it is
    /// reported by `report_untyped_globals` as a named initialization cycle.
    pub(super) fn infer_returns(&mut self, stmts: &[Stmt]) {
        // The main walk records every Resolution; this pre-pass records none (TICKET-180).
        // `inferring_ret` cannot gate it: `check_fn_body` clears that for a nested fn.
        let saved_resolving = std::mem::replace(&mut self.resolving_returns, true);
        self.seed_module_globals(stmts);
        // Bound: each productive pass resolves at least one more `Unknown`→concrete (a return or a
        // global); `+1` lets the final pass confirm no change (the fixpoint). A non-productive pass
        // breaks the loop early.
        let seeded = self.globals.values().filter(|g| g.seeded).count();
        let cap = self.count_uninferred(stmts) + seeded + 1;
        for _ in 0..cap {
            let a = self.infer_returns_pass(stmts, false);
            let b = self.type_globals_pass(stmts);
            if !a && !b {
                break;
            }
        }
        // FINALIZE: one last pass that reports every inline body still without a type (a call cycle,
        // an empty `[]`), kept separate so the passes above stay silent while a callee is pending.
        self.infer_returns_pass(stmts, true);
        // Global types read the finalized returns.
        self.type_globals_pass(stmts);
        // Closure writes: a body above `h := fn(): ...` calling `h()` must see which captures `h`
        // writes. The walk above recorded them per closure literal (`closure_literal_writes` is not
        // in `DiagMark`, so the rollback kept them).
        for (s, firsts) in self.seeded_first_lets(stmts) {
            if let StmtKind::Let { target, value, .. } = &s.kind
                && let Some(name) = target.target_ident()
                && firsts.len() == 1
                && let ExprKind::Closure { body, .. } = &value.kind
                && let Some(writes) = self
                    .closure_literal_writes
                    .get(&(self.graph_module_idx, body.span))
                && !writes.is_empty()
            {
                let mut writes = writes.clone();
                writes.sort();
                self.written_captures[0].insert(name.to_string(), writes);
            }
        }
        self.report_untyped_globals(stmts);
        self.resolving_returns = saved_resolving;
    }

    /// TICKET-183 — each top-level `let` that is the FIRST let of at least one seeded global, with
    /// the index of each such name in the let's `names`, in source order.
    fn seeded_first_lets<'a>(&self, stmts: &'a [Stmt]) -> Vec<(&'a Stmt, Vec<usize>)> {
        let mut out = Vec::new();
        for s in stmts {
            let StmtKind::Let { target, .. } = &s.kind else {
                continue;
            };
            let names = target.binder_names();
            let firsts: Vec<usize> = (0..names.len())
                .filter(|&i| {
                    self.globals
                        .get(&names[i])
                        .is_some_and(|g| g.seeded && g.first_let() == Some(s.span))
                })
                .collect();
            if !firsts.is_empty() {
                out.push((s, firsts));
            }
        }
        out
    }

    /// TICKET-183 — seed scope 0 with every let-only module global before any body is walked, so a
    /// body sees a global declared below it. Writes two of the per-binding facts the `Let` arm
    /// records (type from the annotation or `Unknown`, keyword certainty); `const` is the record's
    /// (`GlobalBinding::is_const`, TICKET-186). The fixpoint refines the type and `infer_returns`
    /// adds the closure writes. Never calls `declare` (it would
    /// untaint the import, const and alias tables and mark `kw_written`). A name that a hoisted
    /// binding with a runtime slot owns (import, fn, extern, native) is not seeded: its slot holds
    /// the hoisted value until the let runs (W7-42). A let named like a type, a builtin or a builtin
    /// ctor IS seeded: those have no slot, so in every body the name is the global, in both source
    /// orders (TICKET-180).
    pub(super) fn seed_module_globals(&mut self, stmts: &[Stmt]) {
        let mut hoisted: HashSet<&str> = HashSet::new();
        for s in stmts {
            match &s.kind {
                StmtKind::Native(d) => {
                    hoisted.insert(&d.name);
                }
                StmtKind::Extern { fns, .. } => hoisted.extend(fns.iter().map(|f| f.name.as_str())),
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Struct { .. }
                | StmtKind::Protocol { .. }
                | StmtKind::Enum { .. }
                | StmtKind::TypeAlias { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(_)
                | StmtKind::Yield(_)
                | StmtKind::Defer(_)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(_)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::Import(_)
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
            }
        }
        for s in stmts {
            let StmtKind::Let {
                target, ty, value, ..
            } = &s.kind
            else {
                continue;
            };
            for n in &target.binder_names() {
                if hoisted.contains(n.as_str())
                    || self.scopes[0].contains_key(n)
                    || self.functions.contains_key(n)
                {
                    continue;
                }
                let mark = self.diag_mark();
                let t = match ty {
                    Some(t) if target.is_one_slot() => self.resolve_type(t, s.span),
                    _ => Ty::Unknown,
                };
                self.diag_rollback(mark);
                // After the rollback: `DiagMark` restores `kw_certain`.
                let g = self.globals.entry(n.clone()).or_default();
                g.seeded = true;
                g.reached = false;
                self.scopes[0].insert(n.clone(), t);
                if let Some(deps) = self.let_holds_one_known_fn(target, ty, value) {
                    self.kw_certain.insert((0, n.clone()), deps);
                }
            }
        }
    }

    /// The type of each name a top-level let binds, in `binders` order. A single name takes
    /// its annotation or the value's type. A target list binds through the one binder, and each
    /// name keeps the type the binder collected (`Unknown` only where the shape is not proved).
    fn global_leaf_tys(
        &mut self,
        target: &Pattern,
        annotated: Option<Ty>,
        val_ty: Ty,
        span: Span,
    ) -> Vec<Ty> {
        if target.target_ident().is_some() {
            return vec![annotated.unwrap_or(val_ty)];
        }
        let mut leaves = Vec::new();
        self.bind_subpattern(target, &val_ty, span, &mut BindMode::Target(&mut leaves));
        leaves.into_iter().map(|(_, _, t)| t).collect()
    }

    /// TICKET-183 — one speculative walk over the first let of every seeded global, in source
    /// order, writing each value's type into scope 0. Top-level lexical visibility is replayed (a
    /// let's value sees the globals above it; a closure body sees them all). Returns `true` iff any
    /// global's type changed (drives the joint fixpoint in `infer_returns`).
    pub(super) fn type_globals_pass(&mut self, stmts: &[Stmt]) -> bool {
        let mut changed = false;
        let mark = self.diag_mark();
        let saved_flag = std::mem::replace(&mut self.inferring_ret, true);
        self.ret_memo.clear();
        let saved_reached = self.save_reached();
        let firsts: std::collections::HashMap<Span, Vec<usize>> = self
            .seeded_first_lets(stmts)
            .into_iter()
            .map(|(s, f)| (s.span, f))
            .collect();
        for s in stmts {
            let StmtKind::Let {
                target, ty, value, ..
            } = &s.kind
            else {
                continue;
            };
            let names = target.binder_names();
            if let Some(idxs) = firsts.get(&s.span) {
                let (annotated, val_ty) = self.let_value_ty(target, ty, value, s.span);
                let leaf_tys = self.global_leaf_tys(target, annotated, val_ty, s.span);
                for &i in idxs {
                    // TICKET-225: a seeded global type outlives this walk.
                    let t = self.settle(&leaf_tys[i]);
                    if self.scopes[0].get(&names[i]) != Some(&t) {
                        self.scopes[0].insert(names[i].clone(), t);
                        changed = true;
                    }
                }
            }
            for n in &names {
                self.reach_global(n);
            }
        }
        self.restore_reached(saved_reached);
        self.ret_memo.clear();
        self.inferring_ret = saved_flag;
        self.diag_rollback(mark);
        changed
    }

    /// TICKET-183 — report each seeded, single-name, un-annotated global whose type is still
    /// `Unknown` after the joint fixpoint when it is a named initialization cycle: `x := f()` where
    /// `f`'s return is computed from `x`. Any other `Unknown` global is not reported here: the walk
    /// reports it (`xs := []` is the hole error on its own line, TICKET-238), a body above it is declined in
    /// `infer_ident`, and a body below it that returns it fails return inference. A let whose value
    /// itself errors is skipped: the walk reports that error.
    fn report_untyped_globals(&mut self, stmts: &[Stmt]) {
        let saved_reached = self.save_reached();
        let saved_flag = std::mem::replace(&mut self.inferring_ret, true);
        let firsts: HashSet<Span> = self
            .seeded_first_lets(stmts)
            .into_iter()
            .map(|(s, _)| s.span)
            .collect();
        let mut reports: Vec<(Span, String, Span)> = Vec::new();
        for s in stmts {
            let StmtKind::Let {
                target, ty, value, ..
            } = &s.kind
            else {
                continue;
            };
            if firsts.contains(&s.span)
                && let Some(x) = target.target_ident()
                && ty.is_none()
                && self.scopes[0].get(x) == Some(&Ty::Unknown)
            {
                let mark = self.diag_mark();
                self.ret_memo.clear();
                self.let_value_ty(target, ty, value, s.span);
                let errored = self.errors.len() > mark.errors;
                self.diag_rollback(mark);
                if !errored
                    && let Some((msg, f_span)) = self.initialization_cycle_message(stmts, x, value)
                {
                    reports.push((s.span, msg, f_span));
                    if let Some(g) = self.globals.get_mut(x) {
                        g.cycle = true;
                    }
                }
            }
            for n in &target.binder_names() {
                self.reach_global(n);
            }
        }
        self.ret_memo.clear();
        self.inferring_ret = saved_flag;
        self.restore_reached(saved_reached);
        for (span, msg, f_span) in reports {
            // The cycle is the cause; `f`'s residual-`Unknown` return error (emitted earlier by
            // `finalize_ret` at `f`'s name) is its consequence. Report the cause once.
            self.errors.retain(|e| {
                !(e.span == f_span && e.message.contains("cannot infer return type of '"))
            });
            self.error(span, msg);
        }
    }

    /// The cycle message for an `Unknown`-typed seeded global `x` (see `report_untyped_globals`),
    /// with the span of the callee's name (whose derived return-type error the report replaces),
    /// or `None` when it is not a cycle: probe whether `x := f()`'s callee return depends on `x` by
    /// typing `x` as a fresh parameter and re-inferring `f`.
    fn initialization_cycle_message(
        &mut self,
        stmts: &[Stmt],
        x: &str,
        value: &Expr,
    ) -> Option<(String, Span)> {
        if let ExprKind::Call { callee, .. } = &value.kind
            && let ExprKind::Ident(f) = &callee.kind
            && self.local_fn_names.contains(f)
            && let Some(sig) = self.functions.get(f).cloned()
            && sig.ret == Ty::Unknown
            && let Some(decl) = stmts.iter().find_map(|s| match &s.kind {
                StmtKind::Fn(d) if &d.name == f => Some(d),
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Struct { .. }
                | StmtKind::Protocol { .. }
                | StmtKind::Enum { .. }
                | StmtKind::TypeAlias { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(_)
                | StmtKind::Yield(_)
                | StmtKind::Defer(_)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(_)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::Import(_)
                | StmtKind::Extern { .. }
                | StmtKind::Native(_)
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => None,
            })
        {
            let probe = format!("${x}");
            self.scopes[0].insert(x.to_string(), Ty::Param(probe.clone()));
            let mark = self.diag_mark();
            let r = self.infer_inline_fn_ret(decl, None, &sig, false);
            self.diag_rollback(mark);
            self.scopes[0].insert(x.to_string(), Ty::Unknown);
            let dependent = subst(&r, &HashMap::from([(probe, Ty::Nil)])) != r;
            if dependent {
                return Some((
                    format!(
                        "initialization cycle: the type of '{x}' comes from '{f}()', and '{f}' returns a value computed from '{x}' -- annotate '{x}' (`{x}: T = {f}()`) or give '{f}' a return type"
                    ),
                    decl.name_span,
                ));
            }
        }
        None
    }

    /// The `Let` arm's value typing, shared with the TICKET-183 globals pass: resolve the annotation
    /// (once — `resolve_type` reports as a side effect) and infer the value against it. Returns
    /// `(annotated, val_ty)`.
    pub(super) fn let_value_ty(
        &mut self,
        target: &Pattern,
        ty: &Option<Type>,
        value: &Expr,
        span: Span,
    ) -> (Option<Ty>, Ty) {
        // `resolve_type` REPORTS as a side effect of resolving (`unknown type 'X'`, the
        // Map-key/Set-element Hashable ban at `sig.rs:1753-1767`), so the annotation must be
        // resolved exactly once per statement or every diagnostic it emits doubles. A
        // destructuring `let` cannot carry an annotation (`a, b: T = ...` is the parse error
        // `expected '=' after a multi-target assignment list`), so a single name binds
        // whenever `ty` is `Some`. `hover_record_at` is first-write-wins, so the second call
        // this replaces never contributed a hover record.
        let annotated: Option<Ty> = match ty {
            Some(t) if target.is_one_slot() => Some(self.resolve_type(t, span)),
            _ => None,
        };
        // A closure bound to a `fn`-typed annotation is inferred in checking-mode (source #1):
        // resolve the annotation first so its unannotated params bind to the slot's param
        // types. Only the single-name, `fn`-typed case (destructuring never binds one).
        // Otherwise ordinary bottom-up inference.
        let let_mark = self.tyvar_mark();
        let val_ty = match &annotated {
            // Expected-type checking-mode for a NON-closure value bound to an annotation:
            // thread the annotation as a hint into the value's inference so a generic
            // ctor / generic fn-call pre-seeds its type params from it — `a: Heap[int] =
            // Heap([], fn(x, y): x < y)` pins `T=int`, which then pins the comparator's
            // params. `infer_call` clears the hint, but pair the set with an immediate clear
            // so a non-call value never leaks it into the next statement.
            // TICKET-227: the value owns the annotation as its slot (it may wrap).
            Some(expected) => self.infer_arg(value, Some(expected)),
            // A discarded `else` guard (`save(p) else e: ...`, `_ := save(p) else e: ...`) may
            // guard a `None!E` call: its success value is nothing, and nothing binds it.
            None if matches!(value.kind, ExprKind::ElseGuard { .. })
                && matches!(target, Pattern::Wildcard) =>
            {
                self.infer(value)
            }
            None => self.infer_value(value),
        };
        let val_ty = self.close_operand(let_mark, val_ty);
        (annotated, val_ty)
    }

    /// TICKET-187 — the one "this call head is the builtin" test: a first-class builtin name that
    /// no binding in scope shadows (a local `print := fn(...)` is the local, not the builtin).
    pub(super) fn names_builtin_fn(&self, name: &str) -> bool {
        matches!(self.head_binding(name), HeadBinding::Unbound) && is_firstclass_builtin_fn(name)
    }

    /// TICKET-139 (W14-2) — an unannotated single-name `:=` of a closure literal or of a top-level
    /// user fn (no local shadow) holds exactly ONE known function, so its labels are certain. Shared
    /// by the `Let` arm and `seed_module_globals` (TICKET-183). TICKET-197 adds two arms: a local
    /// `Ident` source is certain when `labels_certain` says so (a nested fn or a certain alias), and
    /// a turbofish `Index` over a generic fn value is certain. The result is the dependency keys
    /// `kw_certain` records: a write to any of them voids the binding.
    pub(super) fn let_holds_one_known_fn(
        &self,
        target: &Pattern,
        ty: &Option<Type>,
        value: &Expr,
    ) -> Option<Vec<(usize, String)>> {
        if target.target_ident().is_none() || ty.is_some() {
            return None;
        }
        match &value.kind {
            ExprKind::Closure { .. } => Some(Vec::new()),
            ExprKind::Ident(n) if self.lookup(n).is_none() => {
                (self.functions.contains_key(n) || self.names_builtin_fn(n)).then(Vec::new)
            }
            ExprKind::Ident(n) => self.labels_certain(n),
            // TICKET-187/204: `g := m.f`, a type-applied fn value (`pair[str, int]`) and a type
            // path value (`Bx[int].make`) hold one known function exactly as `g := f` does.
            ExprKind::Index { .. } | ExprKind::Field { .. } => crate::ast::type_application(value)
                .map_or_else(
                    || self.path_fn(value).is_some(),
                    |app| self.path_fn(app.head).is_some(),
                )
                .then(Vec::new),
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
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        }
    }

    /// Count the un-annotated free fns + struct/enum methods that `infer_returns` infers — the
    /// fixpoint iteration bound. (Annotated decls are skipped by the pass, so they cannot extend the
    /// chain.)
    pub(super) fn count_uninferred(&self, stmts: &[Stmt]) -> usize {
        let mut n = 0;
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() && decl.inline_expr_body => n += 1,
                StmtKind::Struct { methods, .. } | StmtKind::Enum { methods, .. } => {
                    n += methods
                        .iter()
                        .filter(|m| m.ret.is_none() && m.inline_expr_body)
                        .count();
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Protocol { .. }
                | StmtKind::TypeAlias { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(_)
                | StmtKind::Yield(_)
                | StmtKind::Defer(_)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(_)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::Import(_)
                | StmtKind::Extern { .. }
                | StmtKind::Native(_)
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
            }
        }
        n
    }

    /// One inference pass over every un-annotated fn/method. Re-infers each from the body (idempotent
    /// per the truncate-errors model in `infer_inline_fn_ret`) and writes the result back into the stored
    /// `FnSig.ret` immediately, so a callee resolved earlier in THIS pass is already visible to a
    /// caller later in the pass. Returns `true` iff any stored ret changed (drives the fixpoint).
    pub(super) fn infer_returns_pass(&mut self, stmts: &[Stmt], finalize: bool) -> bool {
        let mut changed = false;
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() && decl.inline_expr_body => {
                    let Some(sig) = self.functions.get(&decl.name).cloned() else {
                        continue;
                    };
                    let ret = self.infer_inline_fn_ret(decl, None, &sig, finalize);
                    if let Some(sig) = self.functions.get_mut(&decl.name)
                        && sig.ret != ret
                    {
                        sig.ret = ret;
                        changed = true;
                    }
                }
                StmtKind::Struct {
                    name,
                    type_params,
                    methods,
                    ..
                } => {
                    let self_ty = self.struct_self_ty(name);
                    // The layout is stored under the runtime key (bare unless disambiguated); a bare
                    // `name` lookup misses in the multi-module path, so the inferred ret would be written
                    // to a non-existent slot and never reach call sites / protocol satisfaction.
                    let key = self.bare_key(name);
                    let saved = self.enter_type_params(type_params);
                    for m in methods {
                        if m.ret.is_some() || !m.inline_expr_body {
                            continue;
                        }
                        let Some(sig) = self
                            .structs
                            .get(&key)
                            .and_then(|s| s.methods.get(&m.name))
                            .cloned()
                        else {
                            continue;
                        };
                        let ret =
                            self.infer_inline_fn_ret(m, Some(self_ty.clone()), &sig, finalize);
                        if let Some(ms) = self
                            .structs
                            .get_mut(&key)
                            .and_then(|s| s.methods.get_mut(&m.name))
                            && ms.ret != ret
                        {
                            ms.ret = ret;
                            changed = true;
                        }
                    }
                    self.exit_type_params(saved);
                }
                StmtKind::Enum {
                    name,
                    type_params,
                    methods,
                    ..
                } => {
                    // Mirror the struct arm for enum methods (same un-annotated-return inference): read
                    // and write the inferred ret into `enum_methods` under the enum's runtime key.
                    let self_ty = self.enum_self_ty(name);
                    let key = self.bare_key(name);
                    let saved = self.enter_type_params(type_params);
                    for m in methods {
                        if m.ret.is_some() || !m.inline_expr_body {
                            continue;
                        }
                        let Some(sig) = self
                            .enum_methods
                            .get(&key)
                            .and_then(|ms| ms.get(&m.name))
                            .cloned()
                        else {
                            continue;
                        };
                        let ret =
                            self.infer_inline_fn_ret(m, Some(self_ty.clone()), &sig, finalize);
                        if let Some(ms) = self
                            .enum_methods
                            .get_mut(&key)
                            .and_then(|ms| ms.get_mut(&m.name))
                            && ms.ret != ret
                        {
                            ms.ret = ret;
                            changed = true;
                        }
                    }
                    self.exit_type_params(saved);
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Protocol { .. }
                | StmtKind::TypeAlias { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(_)
                | StmtKind::Yield(_)
                | StmtKind::Defer(_)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(_)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::Import(_)
                | StmtKind::Extern { .. }
                | StmtKind::Native(_)
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
            }
        }
        changed
    }

    /// TICKET-157 (W12-12) — the speculative return inference a nested un-annotated `fn` runs, once
    /// per decl span per outermost fn walk (`Checker::ret_memo`). Without it each enclosing
    /// speculative walk repeats this inference AND the nested `check_fn_body`, which doubles per
    /// nesting level. A hit replays the type plus the diagnostics `infer_inline_fn_ret` emits AFTER its own
    /// rollback; everything it did before that was erased by the rollback (`DiagMark`).
    /// `check_fn_body` is deliberately not memoized — see `ret_memo`.
    pub(super) fn infer_nested_fn_ret(&mut self, decl: &FnDecl, sig: &FnSig) -> Ty {
        let hit = if self.memo_enabled {
            self.ret_memo.get(&decl.name_span).cloned()
        } else {
            None
        };
        if let Some(stored) = hit {
            if self.memo_verify && !self.memo_verifying {
                self.verify_memo_hit(decl, sig, &stored);
            }
            let (ty, errs, warns) = stored;
            self.errors.extend(errs);
            self.warnings.extend(warns);
            return ty;
        }
        let (e0, w0) = (self.errors.len(), self.warnings.len());
        let ty = self.infer_inline_fn_ret(decl, None, sig, true);
        if self.memo_enabled {
            self.ret_memo.insert(
                decl.name_span,
                (
                    ty.clone(),
                    self.errors[e0..].to_vec(),
                    self.warnings[w0..].to_vec(),
                ),
            );
        }
        ty
    }

    /// `CHEZZI_MEMO_VERIFY` probe: recompute a memo hit, roll the recompute back, and print a
    /// `MEMO MISMATCH` line when the stored type or diagnostics differ. Prints rather than panics so
    /// one run sweeps a whole corpus.
    fn verify_memo_hit(
        &mut self,
        decl: &FnDecl,
        sig: &FnSig,
        stored: &(Ty, Vec<CheckError>, Vec<CheckError>),
    ) {
        self.memo_verifying = true;
        let vmark = self.diag_mark();
        let (e0, w0) = (self.errors.len(), self.warnings.len());
        let ty = self.infer_inline_fn_ret(decl, None, sig, true);
        let recomputed = (ty, self.errors[e0..].to_vec(), self.warnings[w0..].to_vec());
        self.diag_rollback(vmark);
        self.memo_verifying = false;
        if format!("{stored:?}") != format!("{recomputed:?}") {
            eprintln!(
                "MEMO MISMATCH at {:?}: stored {stored:?} vs recomputed {recomputed:?}",
                decl.name_span
            );
        }
    }

    /// Type one INLINE EXPRESSION body (`fn f(): <expr>`): its return type is the type of that one
    /// expression (`inline_body_ty`, the owner a lambda reads too). The walk is speculative: its
    /// diagnostics are rolled back and the main walk re-reports them. A walk in which a call had no
    /// type yet gives NO type, so a recursive or mutually recursive inline fn never gets one and
    /// asks for `->` on the finalize pass. Nothing is joined: a block body is never walked here
    /// (`fn_sig` already wrote `None` for it).
    pub(super) fn infer_inline_fn_ret(
        &mut self,
        decl: &FnDecl,
        self_ty: Option<Ty>,
        sig: &FnSig,
        finalize: bool,
    ) -> Ty {
        // An OUTERMOST fn walk starts here: drop the nested-fn return memo. `infer_returns` re-infers
        // the same decls over a fixpoint (`infer_returns_pass`), each pass against different stored
        // sigs, so an entry from an earlier pass would be stale (span-keyed aliasing, DEC-029/094).
        if !self.in_fn_body {
            self.ret_memo.clear();
        }
        let mark = self.diag_mark();
        let saved_tps = self.enter_type_params(&decl.type_params);
        // `Self` in this body/inline-expr resolves to the enclosing type (`None` for a free fn, which
        // correctly resets an enclosing method's binding when a nested fn is inference-checked).
        let saved_self = std::mem::replace(&mut self.current_self_ty, self_ty.clone());
        // TICKET-029 — inside a module-level fn's own body that shadows a same-named struct ctor,
        // the bare struct name is the RAW field constructor (else `fn Path(...): return Path(...)`
        // is infinite recursion). Every other body inherits the flag, like `current_self_ty`.
        let saved_raw = if self_ty.is_none()
            && self.local_fn_names.contains(&decl.name)
            && self.struct_names.contains(&decl.name)
        {
            self.raw_ctor_owner.replace(self.bare_key(&decl.name))
        } else {
            self.raw_ctor_owner.clone()
        };
        let saved_ret = std::mem::replace(&mut self.current_ret, Ty::Unknown);
        let saved_ret_decl = std::mem::replace(&mut self.ret_declared, false);
        // In a fn body during return inference (mirrors `check_fn_body`): a `?` here targets this
        // body, not module top-level. Saved/restored beside `current_ret`.
        let saved_in_fn = std::mem::replace(&mut self.in_fn_body, true);
        let saved_in_dflt = std::mem::replace(
            &mut self.in_default_provider,
            decl.name
                .starts_with(crate::base::provider_name::PROVIDER_PREFIX),
        );
        // …and a fn DECLARED inside a `spawn:` block is not itself the task (W7-48), so it also does
        // not inherit the enclosing frame's W8-3 airlock taint (`enter_own_frame` moves the pair).
        let saved_frame = self.enter_own_frame();
        let saved_flag = std::mem::replace(&mut self.inferring_ret, true);
        // An inline expression body is never a generator: a stray `yield` in it is diagnosed.
        let saved_ig = std::mem::replace(&mut self.in_generator, false);
        let saved_gf = self.gen_frame.take();
        // M24 — same rule as `check_fn_body` (a module-level free fn or a member, never a nested fn).
        // Without it an UNANNOTATED `fn reset[T: Default](old: T): return T.default()` would infer
        // its return as `Unknown` here (the pass-1 error is truncated), and that residual Unknown is
        // a type-check bypass. Read the ONE stored answer (the hoist's fixpoint) off the caller's
        // signature, not a fresh derivation: forwarding makes the set transitive, so a recomputation
        // from a partly-updated state could disagree with the arity `check_fn_body` and the compiler
        // use.
        // Task 4 — a nested fn declares none and INHERITS the enclosing scope (its `MakeClosure`
        // carries the `$w:T` capture entries), so leave `witness_scope` alone in that arm.
        let saved_witness_scope = if saved_in_fn {
            self.witness_scope.clone()
        } else {
            std::mem::replace(&mut self.witness_scope, sig.witness_params.clone())
        };
        self.push_scope();
        for (i, param) in decl.params.iter().enumerate() {
            let ty = sig.params.get(i).cloned().unwrap_or(Ty::Unknown);
            self.declare(&param.name, ty);
        }
        // Only an inline expression body reaches this walk; anything else returns nothing.
        let (raw, untyped) = if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            let saved = self.untyped_call.replace(None);
            let body_mark = self.tyvar_mark();
            let raw = self.inline_body_ty(e, None);
            let raw = self.close_carriers(body_mark, raw);
            let untyped = std::mem::replace(&mut self.untyped_call, saved).flatten();
            (raw, untyped)
        } else {
            (Ty::Nil, None)
        };
        self.pop_scope();
        self.in_generator = saved_ig;
        self.gen_frame = saved_gf;
        self.inferring_ret = saved_flag;
        self.current_ret = saved_ret;
        self.ret_declared = saved_ret_decl;
        self.in_fn_body = saved_in_fn;
        self.in_default_provider = saved_in_dflt;
        self.exit_own_frame(saved_frame);
        self.current_self_ty = saved_self;
        self.raw_ctor_owner = saved_raw;
        self.witness_scope = saved_witness_scope;
        self.exit_type_params(saved_tps);
        // Did the body inference itself emit an error (undefined name, bad call, …)? If so the real
        // diagnostic surfaces in pass 2, so a residual `Unknown`/conflict here is a CASCADE, not a
        // genuine un-inferable return — suppress the finalize error to avoid piling on.
        let body_had_err = self.errors.len() > mark.errors;
        // Discard inference-time diagnostics; pass 2 re-reports them for real. BOTH channels: a
        // warning raised inside this body would otherwise be emitted here AND again in pass 2.
        self.diag_rollback(mark);
        // (i) A call in the body had no type yet: this walk gives no type. On the finalize pass that
        // is a cycle (or a callee that itself has no type), and the fn asks for `->`.
        if untyped.is_some() {
            if finalize && !body_had_err {
                self.error(
                    decl.name_span,
                    format!(
                        "cannot infer return type of '{}': a call in its body has no known type yet (return types that depend on each other are not inferred); add a -> annotation",
                        decl.name
                    ),
                );
            }
            return Ty::Unknown;
        }
        // (ii) An unpinned `!e`, read BEFORE `settle` erases the open slot: the `!e` node reports in
        // the main walk, so this site reports nothing.
        if super::tyvar::open_err(&raw) {
            return Ty::Unknown;
        }
        // TICKET-225: an inferred return outlives this walk, so a type variable the walk created
        // must not reach it; a still-unbound one settles to `Unknown`.
        let settled = self.settle(&raw);
        if !finalize {
            return settled;
        }
        self.finalize_ret(&settled, &decl.name, decl.name_span, body_had_err)
    }

    /// FINALIZE an inline body's type: REJECT any residual `Unknown` (top-level, a `T!E` slot,
    /// a `T?` slot, a List/Set/Map element/key/value, or a Struct/Enum
    /// type-arg) with `cannot infer return type of '<name>'`. A `Ty::Param`
    /// (generic fns / the proto.rs HOF loop-back) is LEFT UNTOUCHED — not this pass's concern. When
    /// `suppress` is set (the body already emitted a real error) the residual-`Unknown` diagnostic is
    /// skipped to avoid a cascade, but the E-default fill still applies.
    pub(super) fn finalize_ret(&mut self, t: &Ty, name: &str, span: Span, suppress: bool) -> Ty {
        let mut bad = false;
        let filled = self.fill_ret(t, &mut bad);
        if bad && !suppress {
            self.error(
                span,
                format!("cannot infer return type of '{name}'; add a -> annotation"),
            );
        }
        filled
    }

    /// Recursive helper for [`Self::finalize_ret`]: rebuild `t`, flagging (`*bad = true`) every
    /// residual `Unknown`. `Ty::Param` and all leaf types pass through unchanged.
    fn fill_ret(&self, t: &Ty, bad: &mut bool) -> Ty {
        match t {
            Ty::Unknown | Ty::Var(_) => {
                *bad = true;
                Ty::Unknown
            }
            // No slot is defaulted: a `T!E` type is written or forwarded whole, never invented.
            Ty::Result(a, b) => {
                let na = self.fill_ret(a, bad);
                let nb = self.fill_ret(b, bad);
                Ty::Result(Box::new(na), Box::new(nb))
            }
            Ty::Option(x) => Ty::Option(Box::new(self.fill_ret(x, bad))),
            Ty::List(x) => Ty::List(Box::new(self.fill_ret(x, bad))),
            Ty::Set(x) => Ty::Set(Box::new(self.fill_ret(x, bad))),
            Ty::Map(k, v) => Ty::Map(
                Box::new(self.fill_ret(k, bad)),
                Box::new(self.fill_ret(v, bad)),
            ),
            Ty::Struct(n, a) => {
                Ty::Struct(n.clone(), a.iter().map(|x| self.fill_ret(x, bad)).collect())
            }
            Ty::Enum(n, a) => {
                Ty::Enum(n.clone(), a.iter().map(|x| self.fill_ret(x, bad)).collect())
            }
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|x| self.fill_ret(x, bad)).collect()),
            // A parameterized protocol existential (`Container[int]`) carries inner `Ty` args, so a
            // residual `Unknown` nested there must be flagged too — recurse into every arg (matches
            // the Struct/Enum/concurrency-box recursion; a bare `Error` has no args, a no-op).
            Ty::Protocol(n, a) => {
                Ty::Protocol(n.clone(), a.iter().map(|x| self.fill_ret(x, bad)).collect())
            }
            // Concurrency boxes and function types ALSO carry inner `Ty`, so a residual `Unknown`
            // nested here must be flagged too — otherwise `import std.concurrency; fn f(): return
            // Shared([])` launders a `Shared[List[Unknown]]` past the rejector (List[int] vs
            // List[str] both then assignable off `.get()`). Recurse into every child.
            Ty::Channel(x) => Ty::Channel(Box::new(self.fill_ret(x, bad))),
            Ty::Shared(x) => Ty::Shared(Box::new(self.fill_ret(x, bad))),
            Ty::Atomic(x) => Ty::Atomic(Box::new(self.fill_ret(x, bad))),
            Ty::RwShared(x) => Ty::RwShared(Box::new(self.fill_ret(x, bad))),
            Ty::Func {
                params,
                ret,
                labels,
                variadic,
            } => Ty::Func {
                params: params.iter().map(|x| self.fill_ret(x, bad)).collect(),
                ret: Box::new(self.fill_ret(ret, bad)),
                labels: labels.clone(),
                variadic: *variadic,
            },
            // Leaf types carry no inner `Ty` (nothing to fill or flag). `Ty::Param` is intentionally
            // passed through UNTOUCHED — generic fns / the proto.rs HOF loop-back own it, an `Unknown`
            // it later resolves is not this pass's concern. Enumerated exhaustively (NO catch-all) so
            // any FUTURE `Ty` variant carrying an inner type fails to compile here instead of silently
            // re-opening the residual-`Unknown` leak.
            Ty::Int
            | Ty::Width(_)
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Param(_)
            | Ty::AtomicInt
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Module(_) => t.clone(),
        }
    }
}
