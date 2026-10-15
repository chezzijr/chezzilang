//! Checker: loop variables, captures, task writes, places and subscripts, self writers, closure
//! captures.
//! Split out of `src/checker/setup.rs`. `super::*` is the parent module.

use super::setup::chain_path;
use super::*;

impl Checker {
    /// Mark `name` (already declared in the current scope) as an immutable `for`-loop variable.
    pub(super) fn mark_loop_var(&mut self, name: &str) {
        if let Some(set) = self.loop_vars.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name`'s nearest binding a `for`-loop variable? Resolves to the binding's defining scope
    /// so an inner `:=` shadow (a fresh local) is correctly reported as not-a-loop-var.
    pub(super) fn is_loop_var(&self, name: &str) -> bool {
        self.owning_scope(name)
            .is_some_and(|i| self.loop_vars[i].contains(name))
    }
    /// Is `name` a binding **captured** by an enclosing `spawn:` task — i.e. defined in a local
    /// scope below the innermost task's floor? Such bindings are read-only inside the task body
    /// (the airlock: a task gets its own copy, so reassigning the capture can't leak out). A
    /// task-local binding (declared inside the task) and a global/function are not captures.
    pub(super) fn is_captured(&self, name: &str) -> bool {
        let Some(&floor) = self.capture_floors.last() else {
            return false;
        };
        self.owning_scope(name).is_some_and(|i| i < floor)
    }
    /// Like [`is_captured`], but excludes module-level (scope 0) bindings — imports and top-level
    /// declarations are globals resolvable identically in every task (like free functions), not
    /// per-task value captures. Used by the *read* sendability gate so reading an imported module or
    /// a top-level closure inside a `spawn:` block isn't flagged; the *reassign* gate keeps the
    /// broader [`is_captured`] (writing a copy of any capture, global or not, can't leak out).
    pub(super) fn is_local_capture(&self, name: &str) -> bool {
        let Some(&floor) = self.capture_floors.last() else {
            return false;
        };
        self.owning_scope(name).is_some_and(|i| i > 0 && i < floor)
    }

    /// W8-3 — record (or clear) the airlock-staleness taint for the binding an assignment TARGET
    /// writes through. The root of an lvalue chain is the binding that crosses the airlock, so
    /// `xs[i] = v`, `p.f = v` and `a.b[0].c = v` all resolve to `xs`/`p`/`a`; a `Tuple` target is not
    /// handled here because `check_assign` recurses into this same path once per element.
    ///
    /// Called at the TOP of `check_assign`, before the per-arm code infers the target's receiver: a
    /// parent-side `xs[0] = v` must untaint *first*, so the receiver read it then performs cannot
    /// report a write the parent has just superseded.
    pub(super) fn note_assign_root(&mut self, target: &Expr) {
        let Some((name, _, _)) = chain_path(target) else {
            return;
        };
        let name = name.clone();
        if self.assign_is_checked_write(target) {
            self.note_task_write(&name, target.span);
        }
    }
    pub(super) fn enter_own_frame(&mut self) -> bool {
        std::mem::replace(&mut self.in_spawn_block, false)
    }

    pub(super) fn exit_own_frame(&mut self, was_spawn: bool) {
        self.in_spawn_block = was_spawn;
    }
    /// W8-3 — a WRITE to `name`. Inside a `spawn:` body a write to a captured local is lost at the
    /// join (measured: reassign / `push` / `xs[i]=v` / `p.f=v` / `m[k]=v` all read back the pre-spawn
    /// value), so taint it with the write's span; in the parent the same write SUPERSEDES any pending
    /// taint, so drop it. The first write inside the task wins the citation.
    ///
    /// A `defer:` block in the PARENT is not a task write: it runs in the parent frame and shares the
    /// enclosing cell, so its write IS visible there (measured: `defer: xs.push(1)` beside a later
    /// `defer: print(xs.len())` prints 1), and `in_spawn_block` is false. A `defer:` block nested
    /// INSIDE a `spawn:` body is on the far side of the airlock like any other task statement — it
    /// does reach here with `in_spawn_block` true, and taints correctly (measured: `spawn: defer:
    /// xs.push(1)` leaves `xs.len() == 0` after the join, and the read warns).
    pub(super) fn note_task_write(&mut self, name: &str, span: Span) {
        if let Some(scope) = self.owning_scope(name) {
            for (floor, writes) in &mut self.closure_write_frames {
                if scope < *floor {
                    writes.insert(name.to_string());
                }
            }
        }
        if self.in_spawn_block && self.is_captured(name) {
            self.error(span, format!("'{name}' {}", crate::vm::COPY_WRITE_TAIL));
        }
    }

    /// TICKET-189 — the source of each declaration slot of call `call_id`, read from the one binder's
    /// plan ([`Self::bind_call`], `call_plans`), so keyword arguments, default fills and a variadic
    /// pack land in their compiled slot. With no plan the call is positional. A value callee's
    /// callee-filled trailing slots are absent from the plan: they are built inside the task.
    pub(super) fn bound_slots<'a>(
        &self,
        call_id: crate::ast::NodeId,
        args: &'a [Expr],
        named: &'a [(String, Expr)],
    ) -> Vec<fn_writes::SlotSrc<'a>> {
        let Some(plan) = self.call_plans.get(&(self.graph_module_idx, call_id.0)) else {
            return args.iter().map(fn_writes::SlotSrc::Arg).collect();
        };
        let combined = |i: usize| {
            args.get(i)
                .or_else(|| named.get(i - args.len()).map(|(_, v)| v))
        };
        plan.iter()
            .filter_map(|fill| match fill {
                crate::checker::ArgFill::Arg(i) => combined(*i).map(fn_writes::SlotSrc::Arg),
                // A pack with an index the call does not hold has no positional shape.
                crate::checker::ArgFill::Pack(idx) => Some(
                    idx.iter()
                        .map(|i| combined(*i))
                        .collect::<Option<Vec<_>>>()
                        .map_or(fn_writes::SlotSrc::Default { inline: false }, |elems| {
                            fn_writes::SlotSrc::Pack(elems)
                        }),
                ),
                crate::checker::ArgFill::Inline { .. } => {
                    Some(fn_writes::SlotSrc::Default { inline: true })
                }
                crate::checker::ArgFill::Provider(_) => {
                    Some(fn_writes::SlotSrc::Default { inline: false })
                }
            })
            .collect()
    }

    /// D4 layer A: report a certain write the statically named callee makes to a parent binding.
    /// Parameter `i` reads `slots[i]`; a slot that is not a caller-written expression, or whose
    /// expression is not a name chain (`fn_writes::chain`), reaches no parent binding.
    pub(super) fn report_named_call_writes(
        &mut self,
        callee: &Expr,
        slots: &[fn_writes::SlotSrc],
        force_task: bool,
    ) {
        if !force_task && !self.in_spawn_block {
            return;
        }
        let Some((callee_name, summary)) = self.named_fn_summary(callee) else {
            return;
        };
        let mut reported = std::collections::HashSet::new();
        for effect in summary.writes {
            let (name, mut path, copied) = match &effect.root {
                fn_writes::WriteRoot::Param(index) => {
                    let Some(fn_writes::SlotSrc::Arg(arg)) = slots.get(*index) else {
                        continue;
                    };
                    let Some((name, path)) = fn_writes::chain(arg) else {
                        continue;
                    };
                    let copied = force_task || self.is_captured(&name);
                    (name, path, copied)
                }
                fn_writes::WriteRoot::Capture(name) => (
                    name.clone(),
                    Vec::new(),
                    force_task || self.is_captured(name),
                ),
                fn_writes::WriteRoot::Global(name) => (name.clone(), Vec::new(), true),
            };
            if !copied || reported.contains(&name) {
                continue;
            }
            path.extend(effect.path.iter().cloned());
            let root_ty = self.lookup(&name).or_else(|| effect.global_ty.clone());
            let Some(ty) = root_ty else {
                continue;
            };
            let prefix_len = if matches!(effect.kind, fn_writes::WriteKind::Store) {
                path.len().saturating_sub(1)
            } else {
                path.len()
            };
            let Some(ty) = self.place_ty(&ty, &path[..prefix_len]) else {
                continue;
            };
            let proven = match &effect.kind {
                fn_writes::WriteKind::Method(method) => self.call_writes_receiver(&ty, method),
                fn_writes::WriteKind::Store => match path.last() {
                    None => !matches!(effect.root, fn_writes::WriteRoot::Param(_)),
                    Some(ChainLink::Field(field)) => self
                        .checked_link_ty(&ty, &ChainLink::Field(field.clone()))
                        .is_some(),
                    Some(ChainLink::Index(_) | ChainLink::Element | ChainLink::Slice) => {
                        matches!(ty, Ty::List(_) | Ty::Map(..) | Ty::ByteArray)
                            || matches!(&ty, Ty::Struct(key, _) if self.method_writes_self(key, "set_index"))
                    }
                },
            };
            if !proven {
                continue;
            }
            reported.insert(name.clone());
            let help = format!(
                "call to '{callee_name}' writes '{name}' through '{}'",
                effect.operation
            );
            let old_errors = self.errors.len();
            self.note_task_write(&name, callee.span);
            if self.errors.len() > old_errors {
                if let Some(error) = self.errors.last_mut() {
                    error.help = Some(help);
                }
            } else {
                self.error_help(
                    callee.span,
                    format!("'{name}' {}", crate::vm::COPY_WRITE_TAIL),
                    Some(help),
                );
            }
        }
    }
    /// TICKET-165 — a mutator (`push`, `add`, …) called on a PROJECTED receiver (`xs[0].push(v)`,
    /// `s.xs.push(v)`, `m[k].push(v)`) is a write through the root binding at the projected path, not
    /// through a bare `Ident`. `chain_path` resolves `obj` down to its root and constant path; a
    /// receiver that does not bottom out on a name (or is a bare `Ident`, already handled at the
    /// `infer_method_call` call site) is a no-op here.
    pub(super) fn note_projected_task_write(&mut self, obj: &Expr) {
        if let Some((name, _, path)) = chain_path(obj)
            && !path.is_empty()
            && self.checked_chain_ty(obj).is_some()
        {
            let name = name.clone();
            self.note_task_write(&name, obj.span);
        }
    }

    fn checked_link_ty(&self, recv: &Ty, link: &ChainLink) -> Option<Ty> {
        match (recv, &self.resolve_link(link)) {
            (Ty::Struct(key, targs), ChainLink::Field(field)) => {
                let info = self.struct_shape(key)?;
                (info.origin == StructOrigin::User).then_some(())?;
                let ty = info
                    .fields
                    .iter()
                    .find(|(name, _)| name == field)?
                    .1
                    .clone();
                Some(subst(&ty, &struct_param_map(info, targs)))
            }
            (slice, ChainLink::Slice) if fn_writes::builtin_slice(slice) => Some(recv.clone()),
            (Ty::List(elem), ChainLink::Element) => Some((**elem).clone()),
            (Ty::Map(_, value), ChainLink::Element) => Some((**value).clone()),
            (Ty::ByteArray, ChainLink::Element) => Some(Ty::Int),
            _ => None,
        }
    }

    /// The one decider of "is this subscript a place or a new container": a syntactic `Index` link
    /// resolves through [`Self::is_fresh_subscript`], which only the module that typed the subscript
    /// may ask. Every other link is already resolved.
    pub(super) fn resolve_link(&self, link: &ChainLink) -> ChainLink {
        match link {
            ChainLink::Index(span) if self.is_fresh_subscript(*span) => ChainLink::Slice,
            ChainLink::Index(_) => ChainLink::Element,
            ChainLink::Field(_) | ChainLink::Element | ChainLink::Slice => link.clone(),
        }
    }

    fn checked_chain_ty(&self, expr: &Expr) -> Option<Ty> {
        let (root, links) = fn_writes::chain(expr)?;
        self.place_ty(&self.lookup(&root)?, &links)
    }

    pub(super) fn subscript_key(&self, index_span: Span) -> crate::checker::CarrierKey {
        crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            index_span,
        )
    }

    pub(super) fn is_fresh_subscript(&self, index_span: Span) -> bool {
        self.fresh_subscripts
            .contains(&self.subscript_key(index_span))
    }

    fn place_ty(&self, root: &Ty, links: &[ChainLink]) -> Option<Ty> {
        let mut ty = root.clone();
        for link in links {
            ty = self.checked_link_ty(&ty, link)?;
        }
        if links
            .last()
            .is_some_and(|link| self.resolve_link(link) == ChainLink::Slice)
        {
            return None;
        }
        Some(ty)
    }

    /// The name `stmt` declares when it is a `struct` declaration.
    fn struct_decl_name(stmt: &Stmt) -> Option<&str> {
        match &stmt.kind {
            StmtKind::Struct { name, .. } => Some(name),
            StmtKind::Fn(_)
            | StmtKind::Let { .. }
            | StmtKind::Assign { .. }
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
        }
    }

    /// TICKET-254 — does the write summary of this declaration hold a subscript link? The summaries
    /// run before any body is typed, so they call every subscript a place; only a typed body can
    /// tell a builtin slice from an index. A declaration with such a link is typed once ahead of
    /// the pass ([`Self::pretype_place_subscripts`]).
    fn holds_subscript_write(stmt: &Stmt) -> bool {
        match &stmt.kind {
            StmtKind::Fn(decl) => fn_writes::has_subscript_write(decl),
            StmtKind::Struct { methods, .. } => methods.iter().any(|m| {
                fn_writes::has_subscript_write(m)
                    || super::self_writes::self_ops(&m.body)
                        .iter()
                        .any(|op| match op {
                            super::self_writes::SelfOp::Store(links)
                            | super::self_writes::SelfOp::Call(links, _) => {
                                links.iter().any(|l| matches!(l, ChainLink::Index(_)))
                            }
                        })
            }),
            StmtKind::Let { .. }
            | StmtKind::Assign { .. }
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
            | StmtKind::Expr(_) => false,
        }
    }

    /// TICKET-254 — the second phase of the write summaries. Type each top-level declaration whose
    /// summary holds a subscript link, with every diagnostic rolled back; that records which
    /// subscripts are builtin slices in `fresh_subscripts`. Then run both summaries again, now
    /// resolving a subscript link to `Element` or `Slice` ([`Self::resolve_link`]).
    pub(super) fn pretype_place_subscripts(&mut self, stmts: &[Stmt]) {
        let mut any = false;
        for s in stmts.iter().filter(|s| Self::holds_subscript_write(s)) {
            any = true;
            let mark = self.diag_mark();
            let saved = std::mem::replace(&mut self.resolving_returns, true);
            self.check_stmt(s);
            self.resolving_returns = saved;
            self.diag_rollback(mark);
        }
        if any && !self.fresh_subscripts.is_empty() {
            for name in stmts.iter().filter_map(Self::struct_decl_name) {
                let key = self.bare_key(name);
                if let Some(info) = self.structs.get_mut(&key) {
                    info.self_writers.clear();
                }
            }
            self.infer_self_writers(stmts);
            self.infer_fn_writers(stmts);
        }
    }

    fn assign_is_checked_write(&self, target: &Expr) -> bool {
        match &target.kind {
            ExprKind::Ident(_) => true,
            ExprKind::Field { obj, name, .. } => self.checked_chain_ty(obj).is_some_and(|t| {
                self.checked_link_ty(&t, &ChainLink::Field(name.clone()))
                    .is_some()
            }),
            ExprKind::Index { obj, .. } => match self.checked_chain_ty(obj) {
                Some(Ty::List(_) | Ty::Map(..) | Ty::ByteArray) => true,
                Some(Ty::Struct(key, _)) => self.method_writes_self(&key, "set_index"),
                _ => false,
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
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => false,
        }
    }

    fn method_writes_self(&self, key: &str, method: &str) -> bool {
        self.struct_shape(key)
            .is_some_and(|info| info.self_writers.contains(method))
    }

    pub(super) fn call_writes_receiver(&self, recv: &Ty, method: &str) -> bool {
        mutates_receiver(recv, method)
            || matches!(recv, Ty::Struct(key, _) if self.method_writes_self(key, method))
    }

    pub(super) fn value_writes(&self, expr: &Expr) -> Vec<String> {
        match &expr.kind {
            ExprKind::Ident(name) => self
                .owning_scope(name)
                .and_then(|scope| self.written_captures.get(scope))
                .and_then(|table| table.get(name))
                .cloned()
                .unwrap_or_default(),
            ExprKind::Closure { body, .. } => self
                .closure_literal_writes
                .get(&(self.graph_module_idx, body.span))
                .cloned()
                .unwrap_or_default(),
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
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => Vec::new(),
        }
    }

    pub(super) fn infer_self_writers(&mut self, stmts: &[Stmt]) {
        type MethodKey = (String, String);
        let mut summaries: HashMap<MethodKey, (bool, Vec<MethodKey>)> = HashMap::new();

        for stmt in stmts {
            let StmtKind::Struct {
                name,
                type_params,
                methods,
                ..
            } = &stmt.kind
            else {
                continue;
            };
            let key = self.bare_key(name);
            if !self.structs.contains_key(&key) {
                continue;
            }
            let self_ty = Ty::Struct(
                key.clone(),
                type_params
                    .iter()
                    .map(|param| Ty::Param(param.name.clone()))
                    .collect(),
            );
            for method in methods {
                if !method.takes_self() {
                    continue;
                }
                let mut direct = false;
                let mut edges = Vec::new();
                for op in super::self_writes::self_ops(&method.body) {
                    match op {
                        super::self_writes::SelfOp::Store(links) => {
                            let Some((last, prefix)) = links.split_last() else {
                                continue;
                            };
                            let Some(recv) = self.place_ty(&self_ty, prefix) else {
                                continue;
                            };
                            match (last, &recv) {
                                (
                                    ChainLink::Index(_) | ChainLink::Element | ChainLink::Slice,
                                    Ty::Struct(target, _),
                                ) => {
                                    edges.push((target.clone(), "set_index".to_string()));
                                }
                                (
                                    ChainLink::Index(_) | ChainLink::Element | ChainLink::Slice,
                                    Ty::List(_) | Ty::Map(..) | Ty::ByteArray,
                                ) => {
                                    direct = true;
                                }
                                (ChainLink::Field(_), _) => {
                                    direct |= self.checked_link_ty(&recv, last).is_some();
                                }
                                _ => {}
                            }
                        }
                        super::self_writes::SelfOp::Call(links, called) => {
                            let Some(recv) = self.place_ty(&self_ty, &links) else {
                                continue;
                            };
                            if mutates_receiver(&recv, &called) {
                                direct = true;
                            }
                            if let Ty::Struct(target, _) = recv {
                                edges.push((target, called));
                            }
                        }
                    }
                }
                summaries.insert((key.clone(), method.name.clone()), (direct, edges));
            }
        }

        let mut writers: HashSet<MethodKey> = summaries
            .iter()
            .filter(|(_, (direct, _))| *direct)
            .map(|(key, _)| key.clone())
            .collect();
        loop {
            let mut changed = false;
            for (method, (_, edges)) in &summaries {
                if writers.contains(method) {
                    continue;
                }
                let writes = edges.iter().any(|edge| {
                    if summaries.contains_key(edge) {
                        writers.contains(edge)
                    } else {
                        self.struct_shape(&edge.0)
                            .is_some_and(|info| info.self_writers.contains(&edge.1))
                    }
                });
                if writes {
                    writers.insert(method.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        for ((key, method), _) in summaries {
            if writers.contains(&(key.clone(), method.clone()))
                && let Some(info) = self.structs.get_mut(&key)
            {
                info.self_writers.insert(method);
            }
        }
    }
    /// Does `name` resolve to the MODULE scope (index 0) — i.e. is it a module-level binding rather
    /// than a local/param shadow? Used by the from-imported-global rebind gate, so a fn-local `:=`
    /// shadow of an imported name stays assignable.
    pub(super) fn resolves_at_module_scope(&self, name: &str) -> bool {
        self.owning_scope(name) == Some(0)
    }

    // ===== B3.3 (Task 2a): capture-sendability gate at spawn callee/arg sites =====

    /// The non-sendable LOCAL captures among the free-variable set `free`, resolved against the
    /// CURRENT scope stack. A name resolving at scope index **0** (a module global — a read-only
    /// namespace resolvable in every task, like a free fn) is EXCLUDED: this is the invariant that
    /// keeps a MODULE-GLOBAL `ref` out of the gate. A name that resolves in no scope (a free fn /
    /// import / builtin) is likewise excluded. Only a name at scope index `> 0` (an enclosing local —
    /// the closure's own param scope is already popped by the time this runs, mirroring
    /// [`is_local_capture`]) whose type is `!sendable` is a capture. Sorted by name for deterministic
    /// diagnostics (HashSet iteration order is nondeterministic).
    pub(super) fn local_captures_of(
        &self,
        free: &std::collections::HashSet<String>,
    ) -> Vec<Capture> {
        let mut caps = Vec::new();
        for name in free {
            let Some(i) = self.owning_scope(name) else {
                continue;
            };
            let ty = self.scopes[i][name.as_str()].clone();
            // Scope 0 = module globals: read-only across tasks, NOT a per-task capture (PITFALL —
            // a module-global `ref` must never be gated).
            if i == 0 || self.sendable(&ty) {
                continue;
            }
            caps.push(Capture {
                name: name.clone(),
                ty,
            });
        }
        caps.sort_by(|a, b| a.name.cmp(&b.name));
        caps
    }

    /// Record (keyed by the bound `name`) the non-sendable local captures of a just-declared
    /// closure/nested-fn whose free-variable set is `free`. No-op when there are none.
    pub(super) fn record_closure_captures(
        &mut self,
        name: &str,
        free: &std::collections::HashSet<String>,
    ) {
        let caps = self.local_captures_of(free);
        if !caps.is_empty()
            && let Some(tbl) = self.capture_table.last_mut()
        {
            tbl.insert(name.to_string(), caps);
        }
    }

    /// The recorded non-sendable captures of a named closure/nested-fn binding (innermost scope wins,
    /// mirroring `lookup`). Empty when `name` is not a recorded capturing value (a sendable closure, a
    /// free fn, a builtin, …).
    pub(super) fn lookup_captures(&self, name: &str) -> Vec<Capture> {
        for tbl in self.capture_table.iter().rev() {
            if let Some(caps) = tbl.get(name) {
                return caps.clone();
            }
        }
        Vec::new()
    }

    /// The non-sendable local captures of a closure VALUE appearing at a spawn callee/arg site: an
    /// `Ident` resolves through the recorded side-table; an INLINE `Closure` is analyzed on the spot
    /// (its free-var set minus its params). Any other expression carries no capturing closure value.
    pub(super) fn spawn_value_captures(&self, ex: &crate::ast::Expr) -> Vec<Capture> {
        match &ex.kind {
            ExprKind::Ident(name) => self.lookup_captures(name),
            ExprKind::Closure { params, body, .. } => {
                let bound: std::collections::HashSet<String> =
                    params.iter().map(|p| p.name.clone()).collect();
                let free = crate::compiler::free_names_of_expr(body, &bound);
                self.local_captures_of(&free)
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
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => Vec::new(),
        }
    }

    /// Emit the block-form diagnostic (verbatim, so `spawn:` block ≡ callee/arg) at `span` for each
    /// non-sendable capture, rendering the captured binding's type.
    pub(super) fn emit_capture_errors(&mut self, caps: &[Capture], span: Span) {
        for c in caps {
            let disp = c.ty.to_string();
            self.error(
                span,
                format!(
                    "cannot use non-sendable captured binding '{name}' of type {disp} inside a \
                     spawned task (captures cross the airlock — communicate via a Channel or Shared)",
                    name = c.name
                ),
            );
        }
    }

    // ===== pass 1: hoist declarations =====
}
