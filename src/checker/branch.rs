//! Checker: `wait`, `match` and `if`/`else` checking and inference, branch unification.
//! Split out of `src/checker/pattern.rs`. `super::*` is the parent module.

use super::pattern::BindMode;
use super::*;

impl Checker {
    /// `wait:` — Chezzi's `select` (§6d). Each arm's channel expr must be a `Channel[T]`; the arm's
    /// target binds (`:=`)/assigns (`=`)/discards (`_`) the element `T`. `wait` is a runtime race, not
    /// a type match, so it is **not** exhaustive — no coverage analysis, ≥1 arm is the only structural
    /// rule (parser-enforced). Each arm body is its own lexical sub-scope (like a `match` arm). An
    /// arm's `if` guard must be `bool`; it is checked before the arm's target binds.
    pub(super) fn check_wait(&mut self, arms: &[WaitArm], else_block: Option<&Block>) {
        for arm in arms {
            self.push_scope();
            // Checked before the target binds, so the guard cannot see the arm's binding.
            if let Some(g) = &arm.guard {
                self.expect_bool(g, "wait guard");
            }
            match &arm.kind {
                WaitArmKind::Recv { target, chan } => {
                    let elem = match self.infer(chan) {
                        Ty::Channel(e) => *e,
                        Ty::Unknown => Ty::Unknown,
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
                        | other @ Ty::Struct(..)
                        | other @ Ty::Enum(..)
                        | other @ Ty::Param(_)
                        | other @ Ty::Result(..)
                        | other @ Ty::Option(_)
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
                        | other @ Ty::Var(_) => {
                            self.error(
                                chan.span,
                                format!("a wait arm must recv from a Channel, found {other}"),
                            );
                            Ty::Unknown
                        }
                    };
                    let mut leaves = Vec::new();
                    self.bind_subpattern(
                        target,
                        &elem,
                        chan.span,
                        &mut BindMode::Target(&mut leaves),
                    );
                    self.declare_target_leaves(&leaves, chan.span);
                }
                WaitArmKind::Send { call } => self.check_wait_send(call),
            }
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        if let Some(b) = else_block {
            self.check_block(b);
        }
    }

    /// A send-`wait:` arm must be exactly `chan.send(value)` with `chan: Channel[T]` and `value: T`.
    /// Anything else (`try_send`, a non-`send` call, a bare non-call expr) is rejected with the list
    /// of legal arm forms — the parser is lenient, so this is the sole gate on send-arm shape. When
    /// the shape IS `chan.send(value)`, inferring the call reuses the ordinary channel-`send` checks
    /// (element-type match + sendability) so a send arm and a plain `ch.send(v)` type-check identically.
    fn check_wait_send(&mut self, call: &Expr) {
        // Decompose the required shape `<recv>.send(<1 positional arg>)`, no named args.
        if let ExprKind::Call {
            callee,
            args,
            named,
            ..
        } = &call.kind
            && let ExprKind::Field { obj, name, .. } = &callee.kind
            && name == "send"
            && args.len() == 1
            && named.is_empty()
        {
            // The receiver MUST be a `Channel[T]`. A user type that merely HAS a `send` method
            // would type-check clean, but the compiler lowers a send-arm as a raw channel op and
            // `op_wait_poll` calls `channel_core` on the handle — a non-channel receiver hits an
            // `unreachable!` VM panic (the checker-superset-of-compiler soundness class). Gate it
            // here, mirroring the recv-arm's `Ty::Channel(e)` guard, before the ordinary call infer.
            // Infer the receiver for its TYPE only — snapshot + truncate its errors, because the
            // `self.infer(call)` below re-infers the same `obj` sub-expression and would re-report
            // them, doubling a diagnostic (e.g. an undefined receiver → two "undefined variable"s).
            // Mirrors the RwShared `read` recovery-only re-inference idiom.
            let mark = self.diag_mark();
            let recv_ty = self.infer(obj);
            self.diag_rollback(mark);
            match recv_ty {
                Ty::Channel(_) | Ty::Unknown => {}
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
                | other @ Ty::Struct(..)
                | other @ Ty::Enum(..)
                | other @ Ty::Param(_)
                | other @ Ty::Result(..)
                | other @ Ty::Option(_)
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
                | other @ Ty::Var(_) => {
                    self.error(
                        obj.span,
                        format!("a wait send arm must send to a Channel, found {other}"),
                    );
                    return;
                }
            }
            // Receiver is a channel — infer the whole call to surface element-type/arg errors (and the
            // receiver's own errors, reported exactly once here) so a send arm and a plain
            // `ch.send(v)` type-check identically.
            self.infer(call);
            return;
        }
        // Not `chan.send(value)` — surface any nested-expr errors, then list the legal arm forms.
        self.infer(call);
        self.error(
            call.span,
            "a wait arm must be a recv (`x := ch.recv()`), a send (`ch.send(v)`), a timer, \
             or `else`"
                .to_string(),
        );
    }

    pub(super) fn check_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchArm]) {
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut exh = self.exh_new(&kind);
        let mut arm_pattern_error = false;
        for arm in arms {
            self.warn_unreachable_arm(
                has_wildcard,
                arm.body.first().map_or(scrutinee.span, |s| s.span),
            );
            let pat_mark = self.errors.len();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                arm.span,
                &mut covered,
                arm.guard.is_some(),
            );
            arm_pattern_error |= self.errors.len() > pat_mark;
            // The guard is type-checked with the arm's bindings in scope. A guarded arm is never
            // irrefutable — its guard may fail at runtime — so it can't make the match exhaustive.
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            has_wildcard |= Self::bool_domain_closed(&kind, &covered);
            has_wildcard |= self.exh_add(&mut exh, &arm.pattern, arm.guard.is_some());
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        let help = self.exh_help(&exh);
        if !arm_pattern_error {
            self.check_exhaustive(&kind, &covered, has_wildcard, help, scrutinee.span);
        }
    }

    /// Infer an expression-position `match`: bind each arm, infer its value, and unify the arm
    /// types into one result. Exhaustiveness is still enforced.
    pub(super) fn infer_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[crate::ast::MatchExprArm],
        owned: bool,
    ) -> Ty {
        // Capture + clear the expected-type hint before the scrutinee/guards (the hint is for the
        // arm BODIES, the tail values). It is re-installed before each arm body below — every arm
        // is equally the value, and `infer_call` drains the single take()-once slot, so without
        // re-installing per arm only the first-inferred arm would get the hint (branch-order bug).
        let hint = self.expected_hint.take();
        // Set by `infer_null_coalesce` for the `match` it lowers `??` to: a mismatch names `??`.
        let coalesce = std::mem::take(&mut self.coalesce_join);
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let beside = hint.is_none() && arms.iter().any(|a| Self::is_none_lit(&a.body));
        let mut sib: Option<Ty> = None;
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut exh = self.exh_new(&kind);
        let mut arm_pattern_error = false;
        let mut arm_tys: Vec<(Span, Ty)> = Vec::new();
        for arm in arms {
            self.warn_unreachable_arm(has_wildcard, arm.body.span);
            let pat_mark = self.errors.len();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                arm.span,
                &mut covered,
                arm.guard.is_some(),
            );
            arm_pattern_error |= self.errors.len() > pat_mark;
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            has_wildcard |= Self::bool_domain_closed(&kind, &covered);
            has_wildcard |= self.exh_add(&mut exh, &arm.pattern, arm.guard.is_some());
            match &hint {
                Some(h) if owned || h.carrier_parts().is_some() => {
                    self.install_hint(&arm.body, h.clone(), owned)
                }
                Some(_) => self.expected_hint = None,
                None => match self.branch_slot(beside, &mut sib, &arm.body) {
                    Some(s) => self.install_hint(&arm.body, s, true),
                    None => self.expected_hint = None,
                },
            }
            let t = self.infer(&arm.body);
            self.pop_scope();
            arm_tys.push((arm.body.span, t));
        }
        self.expected_hint = None;
        self.hint_owner = None;
        let owned = owned || hint.is_none();
        let hint = hint.or(sib);
        let mut result = None;
        for (sp, t) in arm_tys {
            result = Some(self.unify_branch(result, t, sp, hint.as_ref(), owned, coalesce));
        }
        let help = self.exh_help(&exh);
        if !arm_pattern_error {
            self.check_exhaustive(&kind, &covered, has_wildcard, help, scrutinee.span);
        }
        result.unwrap_or(Ty::Unknown)
    }

    /// Infer an expression-position `if c: a else: b`: condition is bool, the two branches unify.
    pub(super) fn infer_if_else(
        &mut self,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
        owned: bool,
    ) -> Ty {
        self.infer_if_else_chain(cond, then, els, owned)
    }

    /// Chain-aware body of `infer_if_else`: an `elif` desugars to a nested `IfElse` in `els`, and the
    /// nested `els` sub-chain is inferred by a DIRECT recursive call here (not generic `infer`).
    fn infer_if_else_chain(&mut self, cond: &Expr, then: &Expr, els: &Expr, owned: bool) -> Ty {
        // Capture + clear the expected-type hint before the condition: the hint is for the branch
        // VALUES (tail position), not the bool condition. Re-install it for EACH branch — both are
        // equally the tail value, and `infer_call` drains the single slot via `take()`, so without
        // re-installing it the second-inferred branch would lose the hint and a generic ctor there
        // would deadlock (acceptance would depend on branch order).
        // A seed (an unowned hint) wraps nothing, so it never hides the `None` sibling slot.
        let hint = self.expected_hint.take();
        let (hint, owned) = match hint {
            h if h.is_none() || !owned => {
                let mut leaves = vec![then];
                let mut tail = els;
                while let ExprKind::IfElse {
                    then: t2, els: e2, ..
                } = &tail.kind
                {
                    leaves.push(t2);
                    tail = e2;
                }
                leaves.push(tail);
                match self.none_sibling_slot(&leaves) {
                    Some(s) => (Some(s), true),
                    None => (h, owned),
                }
            }
            h => (h, owned),
        };
        self.expect_bool(cond, "if condition");
        // Each branch value owns the hint when this if owns it (TICKET-227): it wraps on its own.
        // A seed that is not a carrier tells a branch nothing it may use: an `!e` branch takes no
        // success type from `bool` or `Any`, so it stays open for the frame close to judge.
        let branch_hint = hint
            .clone()
            .filter(|h| owned || h.carrier_parts().is_some());
        match &branch_hint {
            Some(h) => self.install_hint(then, h.clone(), owned),
            None => self.expected_hint = None,
        }
        let t_then = self.infer(then);
        match &branch_hint {
            Some(h) => self.install_hint(els, h.clone(), owned),
            None => self.expected_hint = None,
        }
        // A nested-`IfElse` `els` is the `elif` tail — recurse DIRECTLY (it owns the hint exactly
        // when this chain does); any other `els` is the final leaf, inferred normally.
        let t_els = if let ExprKind::IfElse {
            cond: c2,
            then: t2,
            els: e2,
        } = &els.kind
        {
            // The `elif` node itself never wraps: its own branches did.
            self.hint_owner = None;
            self.infer_if_else_chain(c2, t2, e2, owned)
        } else {
            self.infer(els)
        };
        self.expected_hint = None;
        self.hint_owner = None;
        let acc = self.unify_branch(None, t_then, then.span, hint.as_ref(), owned, false);
        self.unify_branch(Some(acc), t_els, els.span, hint.as_ref(), owned, false)
    }

    /// Fold one branch's type into a match/if expression's running result type. The first concrete
    /// branch sets the type; a later incompatible branch is a real error (and yields `Unknown` to
    /// suppress cascades). `Unknown` branches never override a concrete result. `hint` is the
    /// statically known expected type at this position (an annotated binding, a call argument, a
    /// declared return; `owned`) or a seed (an operand of `==`; not `owned`) — it is read ONLY on a
    /// `compatible` mismatch, and ONLY through `assignable`,
    /// never folded into the accumulator (so an existing diagnostic like `Sq and int` stays that,
    /// not `Sh and int`). An int branch beside a float branch is an error (D3: no int→float widening).
    pub(super) fn unify_branch(
        &mut self,
        acc: Option<Ty>,
        t: Ty,
        span: Span,
        hint: Option<&Ty>,
        owned: bool,
        coalesce: bool,
    ) -> Ty {
        match acc {
            None => t,
            Some(prev) => {
                // The join runs first: a `T!E` sibling types a bare `!e`. An `!e` beside an arm it
                // cannot join (`if c: 1 else: !"bad"`) keeps the `!e` message: its type has no
                // variable, so no later compare can fill the `!e` and leave the `1` unwrapped. A
                // seed never joins two arms that differ by a carrier layer (`int` and `int?`):
                // nothing wraps the plain arm, as with no hint.
                if let Some(m) = self.join_fill(&prev, &t) {
                    m
                } else if let Some(s) = super::tyvar::sealed_open_err(&prev)
                    .or_else(|| super::tyvar::sealed_open_err(&t))
                {
                    s
                } else if let Some(h) = hint
                    && ty_fully_concrete(h)
                    && (owned || !self.differ_by_carrier(&prev, &t))
                    && self.assignable(h, &prev)
                    && self.assignable(h, &t)
                {
                    h.clone()
                } else {
                    let what = if coalesce { "'??' sides" } else { "branches" };
                    self.error(
                        span,
                        format!(
                            "{what} have incompatible types: {prev} and {t}{}{}",
                            float_fix_note_join(&prev, &t),
                            crate::checker::carrier_join_note(&prev, &t)
                        ),
                    );
                    Ty::Unknown
                }
            }
        }
    }

    /// Whether one arm fits the other only through a wrap (`int` beside `int?` or `int!str`): the
    /// one test of "needs a wrap", [`Self::wrap_mode`], asked both ways. An unpinned `?x` arm is
    /// read as its default, `T?`.
    fn differ_by_carrier(&self, a: &Ty, b: &Ty) -> bool {
        let view = |t: &Ty| {
            if let Ty::Var(v) = t
                && let Some(p) = self.tyvars.borrow().present_payload(*v)
            {
                Ty::Option(Box::new(p))
            } else {
                t.clone()
            }
        };
        let (a, b) = (view(a), view(b));
        self.wrap_mode(&a, &b).is_some() || self.wrap_mode(&b, &a).is_some()
    }

    // ===== expression inference =====
}
