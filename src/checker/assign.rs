//! Checker: assignment and destructuring targets (`check_assign`, `declare_target_leaves`).
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::pattern::BindMode;
use super::setup::BindSite;
use super::*;

impl Checker {
    /// Check a destructuring let `a, (b, c), … := value`. The one binder (`bind_subpattern`, target
    /// mode) matches the target against the value's type and collects each name with its element
    /// type; a leaf under a shape the type does not prove (an `Unknown` value, a reported mismatch)
    /// carries `Unknown`, so no cascade follows. `declare_target_leaves` then binds them.
    pub(super) fn check_destructure(
        &mut self,
        target: &Pattern,
        val_ty: &Ty,
        span: Span,
        stmt_span: Span,
    ) {
        let mut leaves = Vec::new();
        self.bind_subpattern(target, val_ty, span, &mut BindMode::Target(&mut leaves));
        self.declare_target_leaves(&leaves, stmt_span);
    }

    /// The one declare policy for the leaves of a binding target (`:=`, `for`, comprehension, wait
    /// arm), each with the type `bind_subpattern` collected for it.
    pub(super) fn declare_target_leaves(&mut self, leaves: &[(String, Span, Ty)], stmt_span: Span) {
        // Each name is bound once per target (`reject_repeated_binders`, parser). Every check runs
        // BEFORE any `declare`, so each name is judged against the binding as it stood before this
        // statement. TICKET-142 (W14-32): a `_` element is the blank identifier, never declared.
        // TICKET-186: a name whose first let this is, and whose element refines its seed, is not
        // re-declared (`refine_first_let`).
        let refines: Vec<bool> = leaves
            .iter()
            .map(|(name, _, ty)| self.first_let_merge(name, ty, stmt_span).is_some())
            .collect();
        for (i, (name, name_span, _)) in leaves.iter().enumerate() {
            if !refines[i] {
                self.reject_redeclare(name, *name_span);
            }
        }
        for (i, (name, name_span, ty)) in leaves.iter().enumerate() {
            // EDITOR HOVER: each destructure target (`a`/`b` in `a, b := (1,2)`) is a NAME,
            // not an `Expr` the probe visits; record its tuple-element type at its own span
            // (no-op unless a probe is armed → zero overhead on normal checks).
            self.hover_record_at(*name_span, ty, HoverKind::Local, None);
            if refines[i] && self.refine_first_let(name, ty.clone(), stmt_span, BindSite::Other) {
                continue;
            }
            self.declare(name, ty.clone());
        }
    }

    pub(super) fn check_assign(
        &mut self,
        target: &Expr,
        op: AssignOp,
        val_ty: Ty,
        value: Option<&Expr>,
        span: Span,
    ) {
        // M3.1 — an index/field-assign (`m[k]=v`, `s.field=x`) on a module global inside a task is
        // legal: the task writes the run's one copy (Go).
        //
        // W8-3 — a write to a captured LOCAL is invisible to the parent, and nothing said so. Taint
        // the root binding here (inside a task) / untaint it (in the parent) for every lvalue shape
        // at once: this `match` is the sole lvalue dispatch, its `Tuple` arm recurses back into
        // `check_assign` per element, and the ident/index/field arms all write THROUGH the same
        // root binding. Done before the arms run so a parent-side `xs[0] = v` untaints ahead of its
        // own receiver read. A blank `_` element of a tuple target (`a, _ = t`) discards its
        // element, exactly as a whole `_ = e` statement does; it is never a variable to resolve or
        // store.
        if op == AssignOp::Eq && target.is_blank() {
            return;
        }
        let val_ty = self.pinning_value_ty(&val_ty);
        self.note_assign_root(target);
        match &target.kind {
            ExprKind::Ident(name) => {
                let Some(var_ty) = self.lookup(name) else {
                    self.error(
                        span,
                        format!("cannot assign to undeclared variable '{name}'"),
                    );
                    return;
                };
                if op == AssignOp::Eq
                    && let Some(scope) = self.owning_scope(name)
                    && let Some(table) = self.written_captures.get_mut(scope)
                {
                    table.remove(name);
                }
                // TICKET-139 (W14-2) — a write voids the certainty of a keyword call through `name`.
                if let Some(s) = self.owning_scope(name) {
                    self.kw_written.insert((s, name.clone()));
                }
                if self.is_loop_var(name) {
                    self.error(
                        target.span,
                        format!("cannot assign to loop variable '{name}' (loop variables are rebound each iteration)"),
                    );
                    return;
                }
                // Uniform by-reference capture: a `spawn:` task gets its OWN per-task copy of a
                // captured LOCAL (the airlock deep-copies its cell), so reassigning it is allowed —
                // the write mutates the isolated copy and is NOT visible to the parent (design §4 F1,
                // the one deliberate divergence from Go). A captured MODULE GLOBAL is handled the
                // same way — see the Task 1 note below.
                // A `from`-imported module global is a SNAPSHOT copy of the module's value, so
                // rebinding the bare name would write a LOCAL alias that is silently lost (the module
                // global is unchanged, and every other module keeps its own snapshot). Reject it,
                // consistent with the qualified form (`st.COUNT = 5` → "cannot assign to field"). Gated
                // on the name resolving at MODULE scope (index 0), so a fn-local `COUNT := 1` shadow
                // stays assignable. Mutating THROUGH the binding (`LST.push(7)`, `m[k] = v`) is a
                // different arm and keeps working — a container IS the same heap object.
                if self.resolves_at_module_scope(name)
                    && let Some(m) = self.imported_values.get(name).cloned()
                {
                    let msg = if self.globals.get(name).is_some_and(|g| g.is_const()) {
                        format!("cannot reassign '{name}' — it is declared const in module '{m}'")
                    } else {
                        format!(
                            "cannot assign to '{name}' imported from module '{m}' (a from-imported global is a snapshot copy — call a mutator fn in that module, or use a Shared/Ref)"
                        )
                    };
                    self.error(target.span, msg);
                    return;
                }
                // A `const` binding is immutable: reject any reassignment of the NAME (covers `=` and
                // every compound `+=`/`-=`/…, which all route through here). Mutating THROUGH it
                // (`xs.push(v)`, `xs[i] = v`) is a different arm and stays allowed — const is shallow.
                // A from-imported name never reaches here: the imported-global guard above owns its message, const or not.
                if self.is_const_decl(name) {
                    self.error(
                        target.span,
                        format!("cannot reassign const binding '{name}'"),
                    );
                    return;
                }
                // M3.1 — reassigning a MODULE GLOBAL inside a task (`g = g + 1`) is legal: every
                // task of the run shares the global (Go), so the write lands for the parent and
                // siblings. A `defer:` block runs in the SAME task (no airlock), so it shares the
                // enclosing binding's cell — reassigning a captured local mutates the shared cell
                // (A2/A3/E1). Editor hover (decl-site): a reassignment's LHS `i` is an `Ident`
                // lvalue the probe does NOT visit via `infer` (it's the assignment TARGET, not an
                // evaluated expr), so record its type at the target span here. Probe-gated no-op
                // off the probe; mirrors the let-binding/for-binding `Local` hover. Simple-Ident
                // lvalue only (Index/Field targets are handled in their own arms below, where the
                // receiver IS inferred).
                self.hover_record_at(target.span, &var_ty, HoverKind::Local, None);
                self.check_assign_value(&var_ty, op, &val_ty, value, target.span);
            }
            // `xs[i] = v` — only lists are mutable by index. Strings are immutable; other types
            // aren't indexable. (`infer_index` would green-light a str index — handle it here.)
            ExprKind::Index {
                obj,
                index: Some(index),
                ..
            } => {
                match self.infer(obj) {
                    Ty::Map(k, v) => {
                        let idx_ty = self.infer_arg(index, Some(&k));
                        if !self.join_ty(&k, &idx_ty) && !self.assignable(&k, &idx_ty) {
                            let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                            self.error(index.span, format!("map key must be {k_s}, found {idx_s}"));
                        }
                        // Direct insertion-site Hashable / float-key ban: reject a non-Hashable key
                        // expr even when the map's key type is still `Unknown` (an empty `{}`), so
                        // `m:={}; m[1.5]=..` faults here (mirrors the literal `{1.5:..}` ban) rather
                        // than slipping past check.
                        if !idx_ty.is_unknown()
                            && let Some(why) = self.key_ty_reject(&idx_ty)
                        {
                            self.error(index.span, format!("map key type {why}"));
                        }
                        self.check_assign_value(&v, op, &val_ty, value, target.span);
                    }
                    Ty::List(elem) => {
                        self.expect_assign_index(index);
                        self.check_assign_value(&elem, op, &val_ty, value, target.span);
                    }
                    // `ba[i] = x` — the MUTABLE sibling of bytes. Int index, int value (0–255
                    // validated at runtime). Bytes has NO arm here (immutable); bytearray adds one.
                    Ty::ByteArray => {
                        self.expect_assign_index(index);
                        self.check_assign_value(&Ty::Int, op, &val_ty, value, target.span);
                    }
                    Ty::Str => {
                        self.expect_assign_index(index);
                        self.error(
                            target.span,
                            "cannot assign to an index of str (strings are immutable)",
                        );
                    }
                    Ty::Unknown => {
                        self.expect_int(index, "index");
                    }
                    // A bounded `[C: IndexSet[K, V]]` type parameter is index-assignable in the body.
                    Ty::Param(name) => {
                        if let Some((k, v)) = self.param_indexset_kv(&name) {
                            let idx_ty = self.infer(index);
                            if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                                let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                                self.error(
                                    index.span,
                                    format!("index must be {k_s}, found {idx_s}"),
                                );
                            }
                            self.check_assign_value(&v, op, &val_ty, value, target.span);
                        } else {
                            self.error(target.span, format!("cannot index-assign into {name}"));
                        }
                    }
                    other @ Ty::Int
                    | other @ Ty::Float
                    | other @ Ty::Bool
                    | other @ Ty::Bytes
                    | other @ Ty::Nil
                    | other @ Ty::Set(_)
                    | other @ Ty::Func { .. }
                    | other @ Ty::Tuple(_)
                    | other @ Ty::Struct(..)
                    | other @ Ty::Enum(..)
                    | other @ Ty::Result(..)
                    | other @ Ty::Option(_)
                    | other @ Ty::Channel(_)
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
                        // A struct satisfying `IndexSet` (has `index` + `set_index`) is mutable by index.
                        if let Some((set_k, set_v)) = self.index_set_kv(&other) {
                            // `x OP= v` is `x = x OP v` (owner rule U15), and the compiler lowers it
                            // that way (Dup2 -> GetIndex -> op -> SetIndex). A compound reads through
                            // `index`, so `x OP v` is typed from `index`'s RETURN; the result is then
                            // stored through `set_index`, so it meets `set_index`'s `val` exactly as
                            // `=` does. A plain `=` never reads.
                            let read = if op == AssignOp::Eq {
                                None
                            } else {
                                self.index_kv(&other)
                            };
                            let (read_k, read_v) = read.unwrap_or((set_k.clone(), set_v.clone()));
                            let idx_ty = self.infer(index);
                            let mut keys = vec![set_k.clone()];
                            if read_k != set_k {
                                keys.push(read_k);
                            }
                            for k in &keys {
                                if !idx_ty.is_unknown() && !self.assignable(k, &idx_ty) {
                                    let [k_s, idx_s] = Ty::render_distinct([k, &idx_ty]);
                                    self.error(
                                        index.span,
                                        format!("index must be {k_s}, found {idx_s}"),
                                    );
                                }
                            }
                            self.check_assign_read_write(
                                &read_v,
                                &set_v,
                                op,
                                &val_ty,
                                value,
                                target.span,
                            );
                        } else {
                            self.expect_int(index, "index");
                            let note = self.hook_name_note(&other, "IndexSet", "set_index");
                            self.error(
                                target.span,
                                format!("cannot index-assign into {other}{note}"),
                            );
                        }
                    }
                }
            }
            // `p.x = v` — only data fields of a struct are assignable (not methods, not module
            // members). `infer_field` would accept those, so check the field kind here.
            ExprKind::Field {
                obj,
                name,
                name_span,
                ..
            } => {
                let obj_ty = self.infer(obj);
                match &obj_ty {
                    Ty::Struct(sname, targs) => {
                        let field_ty = self.struct_field_ty(sname, targs, name);
                        match field_ty {
                            Some(ty) => {
                                self.check_assign_value(&ty, op, &val_ty, value, target.span);
                            }
                            None => {
                                let names = self.field_names(sname);
                                self.error_help(
                                    *name_span,
                                    format!(
                                        "cannot assign to '{name}': type {obj_ty} has no field '{name}'"
                                    ),
                                    suggest::did_you_mean(name, &names),
                                )
                            }
                        }
                    }
                    Ty::Unknown => {}
                    // A module member (`math.pi = x`) is never assignable. If it is a `const`, say so
                    // (a wrong "call a mutator fn" cure would otherwise be implied for an immutable).
                    Ty::Module(mname)
                        if self
                            .imported_modules
                            .get(mname)
                            .and_then(|id| self.module_sigs.get(id))
                            .is_some_and(|sig| sig.member(name).is_some_and(|m| m.is_const)) =>
                    {
                        self.error(
                            target.span,
                            format!(
                                "cannot reassign '{name}' — it is declared const in module '{mname}'"
                            ),
                        );
                    }
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
                    | other @ Ty::Enum(..)
                    | other @ Ty::Param(_)
                    | other @ Ty::Result(..)
                    | other @ Ty::Option(_)
                    | other @ Ty::Channel(_)
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
                    | other @ Ty::Var(_) => self.error(
                        target.span,
                        format!("cannot assign to field '{name}' of {other}"),
                    ),
                }
            }
            // `a, b = b, a` (and index/field forms) — multi-target tuple assignment. The parser
            // guarantees `op == Eq` here. The value must be a tuple of equal arity; each target is
            // then checked against its positional element type (recursing into the ident/index/field
            // arms above — so vars, list elements, and struct fields all work, identically).
            ExprKind::Tuple(targets) => {
                let Ty::Tuple(elems) = &val_ty else {
                    if !val_ty.is_unknown() {
                        self.error(
                            span,
                            format!("cannot assign {val_ty} to {} targets", targets.len()),
                        );
                    }
                    return;
                };
                if elems.len() != targets.len() {
                    self.error(
                        span,
                        format!(
                            "assignment has {} target(s) but the value has {} element(s)",
                            targets.len(),
                            elems.len()
                        ),
                    );
                    return;
                }
                let elems = elems.clone();
                for (t, ety) in targets.iter().zip(elems) {
                    self.check_assign(t, AssignOp::Eq, ety, None, span);
                }
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
            // The parser admits only places (`ExprKind::is_place`); reaching here is a front-end bug.
            | ExprKind::ElseGuard { .. } => self.error(
                target.span,
                "internal: a non-place assignment target passed the parser (`ExprKind::is_place`)",
            ),
        }
    }

    pub(super) fn check_assign_value(
        &mut self,
        target_ty: &Ty,
        op: AssignOp,
        val_ty: &Ty,
        value: Option<&Expr>,
        span: Span,
    ) {
        self.check_assign_read_write(target_ty, target_ty, op, val_ty, value, span);
    }

    /// Check `target OP= v`, or `target = v`. `read_ty` is the type a compound reads the target as;
    /// `slot_ty` is the type the result is stored into. They differ only for an `index`/`set_index`
    /// hook pair.
    pub(super) fn check_assign_read_write(
        &mut self,
        read_ty: &Ty,
        slot_ty: &Ty,
        op: AssignOp,
        val_ty: &Ty,
        value: Option<&Expr>,
        span: Span,
    ) {
        // A constant meets the target's width (Go: `300 (untyped int constant) overflows int8`).
        // A compound operand meets no slot: it is an operand of `x OP v`, and arithmetic on a width
        // value yields plain `int` (DEC-218).
        if op == AssignOp::Eq
            && let Some(v) = value
        {
            self.const_meets_slot(v, Some(slot_ty));
        }
        let target_ty = slot_ty.scalar();
        // `x OP= v` is `x = x OP v` (owner rule U15): type `x OP v` with the binary-operator rules,
        // then judge the result as `=` would. No compound-only rule exists; the no-widening rule
        // (`int += float`) is `=`'s (`x = 1.5` into an `int` slot).
        let val_ty = match op.to_binop() {
            None => val_ty.clone(),
            Some(bin) => {
                self.binary_result(bin, read_ty.scalar().clone(), val_ty.clone(), span, span)
            }
        };
        if !self.assignable(target_ty, &val_ty) {
            let note = self.protocol_note(target_ty, &val_ty);
            let [val_s, target_s] = Ty::render_distinct([&val_ty, target_ty]);
            self.error(
                span,
                format!(
                    "cannot assign {val_s} to {target_s}{}{note}{}",
                    crate::checker::ty::fn_arity_note(target_ty, &val_ty),
                    slot_note(target_ty, &val_ty)
                ),
            );
        }
    }
}
