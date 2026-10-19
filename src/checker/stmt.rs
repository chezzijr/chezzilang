//! Checker: pass-2 statement checking (`check_stmt_inner`), fn shape, lifecycle hooks,
//! redeclaration.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::setup::BindSite;
use super::sig::TempWrite;
use super::sig::TypePos;
use super::sig::method_binders;
use super::*;

impl Checker {
    /// One statement is one type-variable frame (TICKET-238): nothing it stored stays pending.
    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
        self.with_stmt_mark(stmt.span, |c| {
            let frame = c.tyvar_mark();
            c.check_stmt_inner(stmt);
            c.close_tyvar_frame(frame);
        });
    }
    fn check_stmt_inner(&mut self, stmt: &Stmt) {
        self.pending_key_reject = None;
        let span = stmt.span;
        match &stmt.kind {
            StmtKind::Let {
                target,
                ty,
                value,
                is_const,
                ..
            } => {
                let is_const = *is_const;
                let (annotated, val_ty) = self.let_value_ty(target, ty, value, span);
                let names = target.binder_names();
                // TICKET-183: from a seeded module global's first let on, top-level statements see
                // it. Its seed in scope 0 stays: the first let refines it (`refine_first_let`,
                // TICKET-186), because the bodies above were typed against it.
                if self.scopes.len() == 1 {
                    for n in &names {
                        self.reach_global(n);
                    }
                }
                if target.target_ident().is_none() && !matches!(target, Pattern::Wildcard) {
                    // TICKET-190: a destructured name is never a private frame slot.
                    if let Some(acc) = &mut self.gen_frame {
                        for n in &names {
                            acc.fresh.insert(n.clone(), Fresh::Marked);
                        }
                    }
                    // destructuring let `a, (b, c) := expr` — `expr` must be a tuple of matching shape.
                    self.check_destructure(target, &val_ty, value.span, span);
                    return;
                }
                let mut declared = match annotated {
                    Some(expected) => {
                        if !self.assignable(&expected, &val_ty) {
                            let note = self.protocol_note(&expected, &val_ty);
                            let [val_s, expected_s] = Ty::render_distinct([&val_ty, &expected]);
                            self.error(
                                value.span,
                                format!(
                                    "cannot assign {val_s} to variable of type {expected_s}{}{note}{}",
                                    crate::checker::ty::fn_arity_note(&expected, &val_ty),
                                    slot_note(&expected, &val_ty)
                                ),
                            );
                        }
                        expected
                    }
                    None => val_ty,
                };
                // TICKET-142 (W14-32): `_ := e` is Go's blank identifier: `e` is evaluated and
                // discarded, `_` is never declared (so no type to freeze, no module global slot, and it
                // may repeat with any types). The annotation check above still ran.
                if matches!(target, Pattern::Wildcard) {
                    return;
                }
                let name = &names[0];
                // TICKET-190: a frame slot is private only if every binding of it is fresh.
                if self.gen_frame.is_some() {
                    let fresh = self.fresh_shape(value, 0, true);
                    if let Some(acc) = &mut self.gen_frame {
                        acc.bind(name, fresh);
                        acc.tys
                            .entry(name.clone())
                            .or_default()
                            .push(declared.clone());
                    }
                }
                // TICKET-186: the first let of a seeded module global merges into the seed the
                // bodies above it read; it never wipes it. `declared` is the merged type.
                let refined = self.refine_first_let(name, declared.clone(), span, BindSite::Let);
                if refined {
                    declared = self.scopes[0][name].clone();
                }
                // EDITOR HOVER: the let-binding target (`x` in `x := …`) is a NAME, not an `Expr` the
                // probe visits during `infer`; record it here. The statement span starts at the first
                // binding name, so it is that token's position (single-name let — the common case).
                if self.hover_probe.is_some() {
                    // doc surfaces at the binding site too, but ONLY for a top-level `let` (the module
                    // scope is then the lone open scope). A function-local/block let that shadows a
                    // documented global's name must NOT borrow that global's doc (`name_docs` is keyed
                    // by bare name); such a local has no doc of its own.
                    let doc = if self.scopes.len() == 1 {
                        self.name_docs.get(name).cloned()
                    } else {
                        None
                    };
                    self.hover_record_at(span, &declared, HoverKind::Local, doc);
                }
                if !refined {
                    self.reject_redeclare(name, span);
                }
                // Computed BEFORE `declare` so `h := h` cannot see itself.
                let one_known_fn = self.let_holds_one_known_fn(target, ty, value);
                if !refined {
                    self.declare_at(name, declared, BindSite::Let);
                }
                if let Some(deps) = one_known_fn
                    && let Some(s) = self.owning_scope(name)
                {
                    self.kw_certain.insert((s, name.to_string()), deps);
                }
                if is_const {
                    self.declare_const(name);
                }
                // B3.3 (Task 2a): a closure bound to a name records its non-sendable LOCAL captures
                // keyed by the binding, so a later `spawn <name>()` (or `spawn f(<name>)`) rejects a
                // captured `ref` at compile time. Uses the SAME free-var over-approximation the runtime
                // uses to build the closure's captures. Module globals (scope 0) are excluded at record
                // time (see `local_captures_of`), so a module-global `ref` is never gated.
                if let ExprKind::Closure { params, body, .. } = &value.kind {
                    let bound: std::collections::HashSet<String> =
                        params.iter().map(|p| p.name.clone()).collect();
                    let free = crate::compiler::free_names_of_expr(body, &bound);
                    self.record_closure_captures(name, &free);
                    let mut writes = self
                        .closure_literal_writes
                        .get(&(self.graph_module_idx, body.span))
                        .cloned()
                        .unwrap_or_else(|| {
                            std::mem::take(&mut self.last_closure_writes)
                                .into_iter()
                                .collect()
                        });
                    writes.sort();
                    let binding_scope = self.owning_scope(name);
                    if let Some(table) =
                        binding_scope.and_then(|scope| self.written_captures.get_mut(scope))
                    {
                        if writes.is_empty() {
                            table.remove(name);
                        } else {
                            table.insert(name.clone(), writes);
                        }
                    }
                }
            }
            StmtKind::Assign { target, op, value } => {
                // TICKET-142 (W14-32): `_ = e` is the blank identifier — evaluate and discard, at
                // every scope. A compound `_ += 1` keeps the ordinary path (and its error).
                if *op == AssignOp::Eq && target.is_blank() {
                    self.infer_value(value);
                    return;
                }
                let val_ty = if matches!(
                    target.kind,
                    ExprKind::Ident(_)
                        | ExprKind::Index { .. }
                        | ExprKind::Field { .. }
                        | ExprKind::Tuple(_)
                ) {
                    // TICKET-124 (W13-14): the hint that seeds a FRESH literal/call value used to
                    // exist only at DECLARATION (an annotated `let`, a call argument, a return) —
                    // reassignment, index-assign and field-assign have a statically known target
                    // type too. Probe it speculatively (mark/rollback) so an lvalue read never
                    // double-diagnoses.
                    // TICKET-227: the target type is the value's slot, for every value kind, so a
                    // plain value wraps into a carrier target; a tuple literal splits per element
                    // (`x, y = 5, 0`). A compound operand is seeded with the slot, never wrapped (DEC-107).
                    let mark = self.diag_mark();
                    let target_ty = self.assign_slot_ty(target);
                    self.diag_rollback(mark);
                    if ty_concrete_but(&target_ty, &|n| self.rigid_param(n, &[])) {
                        if *op == AssignOp::Eq {
                            self.infer_arg(value, Some(&target_ty))
                        } else {
                            // TICKET-258 (U15): `x OP= v` is `x = x OP v`: the operand is
                            // inferred by the call `infer_binary` makes for its right operand.
                            let folded = self.scan_const_overflow(value);
                            // TICKET-268: `x /= 0` and `x <<= 64` always fault for an int target.
                            if let crate::ast::consteval::Fold::Value(
                                crate::ast::consteval::Const::Int(b),
                            ) = folded
                                && let Some(f) = op
                                    .to_binop()
                                    .and_then(|bop| crate::ast::consteval::operand_fault(bop, b))
                                && matches!(target_ty.scalar(), Ty::Int)
                            {
                                self.report_const_fault(f, span);
                            }
                            self.infer_binary_rhs(value, Some(target_ty))
                        }
                    } else {
                        self.infer_value(value)
                    }
                } else {
                    self.infer_value(value)
                };
                // TICKET-190: a plain rebind keeps a frame slot private only with a fresh value; a
                // compound one only on a type whose operator cannot return a parent's value.
                if self.gen_frame.is_some()
                    && let ExprKind::Ident(n) = &target.kind
                {
                    let fresh = if *op == AssignOp::Eq {
                        self.fresh_shape(value, 0, true)
                    } else if self
                        .lookup(n)
                        .is_some_and(|t| self.use_keeps_root(&super::fn_writes::RootUse::Op, &t))
                    {
                        Fresh::root()
                    } else {
                        Fresh::Marked
                    };
                    if let Some(acc) = &mut self.gen_frame {
                        acc.bind(n, fresh);
                    }
                }
                self.check_assign(target, *op, val_ty, Some(value), span);
                // TICKET-089 / TICKET-254 — `b.get().v = 9`, `xs.copy()[0] = 9`, `g[0..2][0] = v`
                // store into a value no name holds, so the store is dropped.
                if let Some((obj, is_index)) = fn_writes::place_parent(target)
                    && self.store_is_native(obj, is_index)
                    && let Some(temp) = self.temporary_write(obj)
                {
                    let text = match temp {
                        TempWrite::BoxRead { name, reader, fix } => format!(
                            "this write lands on the copy returned by '{name}.{reader}()', so it is discarded — write through '{name}.{fix}(…)' instead"
                        ),
                        TempWrite::Copy => "this write changes a temporary copy; the result is dropped: bind the value to a name first, or write to the source".to_string(),
                    };
                    self.warn(target.span, text);
                }
            }
            StmtKind::Fn(decl) => {
                if decl.is_test {
                    self.validate_test_fn_shape(decl, None);
                }
                // A NESTED `fn` (declared inside any block / fn body — `scopes.len() > 1`; the module
                // top level is exactly ONE scope, so a top-level fn sees `len == 1`) is a FIRST-CLASS
                // LOCAL function: lexical nearest-scope, body-checked, recursive (letrec). A TOP-LEVEL
                // fn keeps the hoisted-global path below (checked against `self.functions`).
                if self.scopes.len() > 1 {
                    // v1 limit: nested GENERIC fns are unsupported (monomorphic only). Clean reject
                    // BEFORE any generic machinery runs (`fn_sig` would emit reserved-type-param
                    // diagnostics and enter the params). Mutual recursion between siblings is likewise
                    // out of scope — it surfaces as a plain forward-reference `unknown name` because a
                    // sibling is only declared AFTER its own body is checked (single-cell letrec).
                    if !decl.type_params.is_empty() {
                        self.error(
                            decl.name_span,
                            "nested generic functions are not supported".to_string(),
                        );
                        return;
                    }
                    let mut sig = self.fn_sig(decl, decl.name_span, None);
                    self.bind_nested_fn_writes(decl);
                    // TICKET-139 (W14-2) — a nested fn's own name is certain to hold that one fn (a
                    // keyword call through it is legal). Its two declares below are a same-scope
                    // re-declaration, which `declare` marks as a write; undo that mark unless the
                    // name was already written or declared here before this fn.
                    let kw_key = (self.scopes.len() - 1, decl.name.clone());
                    let kw_was_written = self.kw_written.contains(&kw_key)
                        || self
                            .scopes
                            .last()
                            .is_some_and(|s| s.contains_key(decl.name.as_str()));
                    // No `-> T` on an INLINE body (`fn_sig` leaves only that one `Unknown`): type its
                    // one expression. Declare a PROVISIONAL `Ty::Func` first so a self-recursive call
                    // resolves as an arity-checked value-call; its untyped result then makes the
                    // walk decline, and the fn asks for `->`.
                    if decl.ret.is_none() && matches!(sig.ret, Ty::Unknown) {
                        // `sig.ret` is `Unknown` here, so this is the provisional `-> ?` type.
                        self.declare_unchecked(&decl.name, fn_value_ty(&sig));
                        self.kw_certain.insert(kw_key.clone(), Vec::new());
                        let inferred = self.infer_nested_fn_ret(decl, &sig);
                        sig.ret = inferred;
                    }
                    // Nearest-scope binding: the name resolves to THIS nested fn (not a global
                    // namesake) at every call site, and recursion type-checks. Declared BEFORE
                    // `check_fn_body`.
                    self.declare_unchecked(&decl.name, fn_value_ty(&sig));
                    self.kw_certain.insert(kw_key.clone(), Vec::new());
                    if !kw_was_written {
                        self.kw_written.remove(&kw_key);
                    }
                    // B3.3 (Task 2a): record the nested fn's non-sendable LOCAL captures keyed by its
                    // name (same free-var over-approximation as the runtime), so `spawn <name>()`
                    // rejects a captured `ref` at compile time. `decl.name` is bound BEFORE this so a
                    // self-recursive reference resolves; it (and the params) are subtracted as binds.
                    let mut bound: std::collections::HashSet<String> =
                        decl.params.iter().map(|p| p.name.clone()).collect();
                    bound.insert(decl.name.clone());
                    let free = crate::compiler::free_names_of_block(&decl.body, &bound);
                    self.record_closure_captures(&decl.name, &free);
                    self.check_fn_body(decl, None, sig);
                    return;
                }
                // Top-level fn: checked against its hoisted global sig.
                // `.get` (not index) is panic-safe even when a redeclaration left a different sig.
                if self.skips_body(stmt) {
                    return;
                }
                if let Some(sig) = self.functions.get(&decl.name).cloned() {
                    self.check_fn_body(decl, None, sig);
                }
            }
            StmtKind::Struct {
                name,
                name_span,
                type_params,
                fields,
                methods,
                doc,
            } => {
                let self_ty = self.struct_self_ty(name);
                // The struct's type parameters are in scope across its method bodies.
                let saved = self.enter_type_params(type_params);
                // Editor hover (decl-site): record the struct type at its declared-name token, with
                // the struct's doc-comment. Probe-gated no-op off the probe.
                if self.hover_probe.is_some() {
                    self.hover_record_at(*name_span, &self_ty, HoverKind::Struct, doc.clone());
                }
                // Editor hover: record each field's declared type at its DECL-site name span. Reads
                // the already-resolved field types out of `self.structs` (no re-`resolve_type`, so no
                // duplicate errors); gated on the probe so normal checks stay strictly zero-overhead.
                if self.hover_probe.is_some()
                    && let Some(info) = self.structs.get(&self.bare_key(name)).cloned()
                {
                    for field in fields {
                        if let Some((_, fty)) = info.fields.iter().find(|(n, _)| n == &field.name) {
                            self.hover_record_at(field.name_span, fty, HoverKind::Field, None);
                        }
                    }
                }
                // TICKET-201: the struct-body binders a default could name — every field that
                // carries a default and every method. Fields and methods interleave, so
                // `check_default_scope` compares spans to find those before the owner.
                let binders: Vec<(String, Span, String)> = fields
                    .iter()
                    .filter(|f| f.default.is_some())
                    .map(|f| (f.name.clone(), f.name_span, name.clone()))
                    .chain(method_binders(methods, name))
                    .collect();
                let saved_binders = std::mem::replace(&mut self.default_binders, binders);
                // A constant-literal field default must be assignable to the field's type (checked
                // here so a wrong-typed default is caught at the declaration, not only when omitted).
                for field in fields {
                    if let Some(def) = &field.default {
                        self.check_default_scope(def, field.name_span);
                        let expected = self.resolve_type_at(&field.ty, def.span, TypePos::Field);
                        // Same seeding, same gate, same reasons as the parameter default above.
                        let fseed = ty_concrete_but(&expected, &|n| self.rigid_param(n, &[]))
                            || self.generic_fn_value_sig(def).is_none();
                        let fhint = fseed.then(|| expected.clone());
                        let saved_dsd = std::mem::replace(&mut self.decl_site_default, true);
                        let actual = self.infer_arg(def, fhint.as_ref());
                        let actual = self.resolve_default_binders(&expected, actual);
                        self.decl_site_default = saved_dsd;
                        if !matches!(expected, Ty::Unknown) && !self.assignable(&expected, &actual)
                        {
                            let note = self.protocol_note(&expected, &actual);
                            let [expected_s, actual_s] = Ty::render_distinct([&expected, &actual]);
                            self.error(
                                def.span,
                                format!(
                                    "default value for field '{}': expected {expected_s}, found {actual_s}{note}{}",
                                    field.name,
                                    slot_note(&expected, &actual)
                                ),
                            );
                        }
                    }
                }
                // A struct with ≥1 `test fn` method is a test suite. Its lifecycle hooks
                // (before_all/after_all/before_each/after_each), when present, must be `fn name(self)`
                // returning nothing — validated here so the runner can trust the shape.
                let is_suite = methods.iter().any(|m| m.is_test);
                // The type's runtime key — the table the members' stored `witness_params` live under.
                let host_key = self.bare_key(name);
                for m in methods {
                    // TICKET-255 — a graph that builds no `Range` skips the prelude `Range` bodies.
                    if self.skip_range_bodies && name == crate::resolver::RANGE_STRUCT {
                        continue;
                    }
                    if m.is_test {
                        self.validate_test_fn_shape(m, Some(&host_key));
                    } else if is_suite && is_lifecycle_hook(&m.name) {
                        self.validate_lifecycle_hook(m, &host_key);
                    }
                    // Panic-safe: a redeclared struct name means `structs[key]` is a *different*
                    // struct whose method table may not contain `m.name`. Keyed by the runtime key
                    // (mirror the enum arm below) so the layout resolves in the multi-module path —
                    // otherwise a bare-`name` miss silently SKIPS body checking entirely.
                    // A duplicate-named method already produced a clear hoist-time "already defined"
                    // error; its body would be checked against the collapsed-map SURVIVOR's signature,
                    // emitting a misleading return-type mismatch. Skip so the dup error is the sole
                    // signal (guarded on count>1 so unique methods still get their bodies checked).
                    if methods.iter().filter(|x| x.name == m.name).count() > 1 {
                        continue;
                    }
                    if let Some(sig) = self
                        .structs
                        .get(&self.bare_key(name))
                        .and_then(|s| s.methods.get(&m.name))
                        .cloned()
                    {
                        self.record_method_decl_hover(m.name_span, &sig);
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
                self.default_binders = saved_binders;
                self.exit_type_params(saved);
            }
            // Enum methods' bodies are checked here (mirroring the struct path); the variant/payload
            // shapes are validated during hoisting.
            StmtKind::Enum {
                name,
                name_span,
                type_params,
                variants,
                methods,
                doc,
            } => {
                let self_ty = self.enum_self_ty(name);
                // The enum's type parameters are in scope across its method bodies.
                let saved = self.enter_type_params(type_params);
                // Editor hover (decl-site): record the enum type at its declared-name token + doc,
                // plus each variant's ctor signature at its declared-name token (`Val(int)` →
                // "fn(int) -> Col"). Mirrors the use-site hover at `infer_variant_call`: the enum's
                // declared `Ty::Param`s are preserved in the return type so a generic variant Displays
                // "fn(T) -> Box[T]". Probe-gated no-op (behavior-neutral), emits no error.
                if self.hover_probe.is_some() {
                    self.hover_record_at(*name_span, &self_ty, HoverKind::Struct, doc.clone());
                    let key = self.bare_key(name);
                    let targs_disp: Vec<Ty> = self
                        .enum_type_params
                        .get(&key)
                        .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
                        .unwrap_or_default();
                    for v in variants {
                        if let Some(vi) = self.variants.get(&(key.clone(), v.name.clone())).cloned()
                        {
                            let fty = Ty::Func {
                                params: vi.payload,
                                ret: Box::new(Ty::enum_ty(vi.enum_name, targs_disp.clone())),
                                labels: crate::checker::FnLabels::default(),
                                variadic: crate::ast::Variadic::No,
                            };
                            self.hover_record_at(v.name_span, &fty, HoverKind::Func, None);
                        }
                    }
                }
                let is_suite = methods.iter().any(|m| m.is_test);
                let host_key = self.bare_key(name);
                let saved_binders =
                    std::mem::replace(&mut self.default_binders, method_binders(methods, name));
                for m in methods {
                    if m.is_test {
                        self.validate_test_fn_shape(m, Some(&host_key));
                    } else if is_suite && is_lifecycle_hook(&m.name) {
                        self.validate_lifecycle_hook(m, &host_key);
                    }
                    // Skip a duplicate-named method's body check (see the struct arm) — its clear
                    // hoist-time dup error stands alone instead of a misleading return-type mismatch.
                    if methods.iter().filter(|x| x.name == m.name).count() > 1 {
                        continue;
                    }
                    if let Some(sig) = self
                        .enum_methods
                        .get(&self.bare_key(name))
                        .and_then(|ms| ms.get(&m.name))
                        .cloned()
                    {
                        self.record_method_decl_hover(m.name_span, &sig);
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
                self.default_binders = saved_binders;
                self.exit_type_params(saved);
            }
            // A protocol's method signatures are validated during hoisting; pass 2 only records its
            // decl-site hover (the protocol existential at the protocol-name token + doc).
            StmtKind::Protocol {
                name,
                name_span,
                doc,
                ..
            } => {
                if self.hover_probe.is_some() {
                    let ty = Ty::Protocol(
                        self.user_protocol_key(name).unwrap_or_else(|| name.clone()),
                        Vec::new(),
                    );
                    self.hover_record_at(*name_span, &ty, HoverKind::Struct, doc.clone());
                }
            }
            // A type alias is fully resolved during hoisting; pass 2 only records its decl-site hover.
            StmtKind::TypeAlias {
                name,
                name_span,
                doc,
                ty,
                ..
            } => {
                // A qualified body's binder is checked here, at the declaration, by the same
                // decider an annotation uses: `type R = b.P` with `b` bound by two imports is
                // ambiguous, as `b.P(1)` is (TICKET-180 P2).
                if let Type::Qualified { module, name, .. } = ty {
                    self.qualified_type_module(module, name, span);
                }
                // Editor hover (decl-site): record the ALIASED type at the alias-name token. Gated
                // strictly on the probe so the extra `resolve_type` never runs in normal checking; on
                // an invalid alias it may add a duplicate error, but hover returns None on ANY error,
                // so observable behavior is unchanged.
                if self.hover_probe.is_some()
                    && let Some(body) = self.aliases.get(name).cloned()
                {
                    let ty = self.resolve_ret_type(&body, *name_span);
                    self.hover_record_at(*name_span, &ty, HoverKind::Struct, doc.clone());
                }
            }
            // Imports, extern blocks, and native (fn/ctor/struct/enum) decls carry nothing to check in
            // pass 2 (native decls are fully validated + registered in `hoist`).
            StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. } => {}
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    self.expect_bool(cond, "if condition");
                    self.check_block(body);
                }
                if let Some(body) = else_block {
                    self.check_block(body);
                }
            }
            StmtKind::For { target, iter, body } => {
                let bindings = self.for_bindings(target, iter);
                self.push_scope();
                // The declare policy records each name's decl-site hover (`i` in `for i in …`).
                self.declare_target_leaves(&bindings, iter.span);
                for (name, _, _) in &bindings {
                    // Loop vars are rebound each iteration → immutable; reassigning one diverges
                    // across engines, so the checker forbids it (see `check_assign`).
                    self.mark_loop_var(name);
                }
                self.loop_depth += 1;
                for stmt in body {
                    self.check_stmt(stmt);
                }
                self.loop_depth -= 1;
                self.pop_scope();
            }
            StmtKind::While { cond, body } => {
                self.expect_bool(cond, "while condition");
                self.loop_depth += 1;
                self.check_block(body);
                self.loop_depth -= 1;
            }
            StmtKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms),
            StmtKind::Return(value) => self.check_return(value.as_ref(), span),
            StmtKind::Yield(e) => self.check_yield(e, span),
            StmtKind::Defer(DeferTarget::Call(e)) => {
                // Block-scoped defer: any indented block — including the module body — is a defer
                // scope, so top-level `defer` is legal (no `in_fn` requirement).
                // `defer` targets a method call or a call to a first-class callable value (a user
                // function/closure, or a name bound to one). Built-ins (`print`, `len`, …) and
                // struct/enum constructors are not first-class values — wrap them in a function.
                match &e.kind {
                    ExprKind::Call { callee, .. } => match &callee.kind {
                        // M24-5b — a dotted CONSTRUCTOR (`Color.Val(3)`, `lib.Point(3)`,
                        // `concurrency.Shared(0)`) is a constructor like the bare `P(3)` below, so
                        // it gets that rule and that message. A dotted STATIC METHOD (`H.build(3)`)
                        // and an ordinary module FUNCTION (`math.abs(-3)`) are ordinary calls and
                        // fall through to the accepting arm.
                        ExprKind::Field { .. } if self.dotted_ctor_target(callee) => {
                            self.error(
                                e.span,
                                "defer requires a function or method call (built-ins and \
                                 constructors must be wrapped in a function)",
                            );
                        }
                        ExprKind::Field { .. } => {} // method or static-method call
                        ExprKind::Ident(name)
                            if self.lookup(name).is_none()
                                && !self.functions.contains_key(name)
                                && !self.names_builtin_fn(name) =>
                        {
                            self.error(
                                e.span,
                                "defer requires a function or method call (built-ins and \
                                 constructors must be wrapped in a function)",
                            );
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
                        | ExprKind::Call { .. }
                        | ExprKind::Index { .. }
                        | ExprKind::Try(_)
                        | ExprKind::OptChain { .. }
                        | ExprKind::NullCoalesce { .. }
                        | ExprKind::Closure { .. }
                        | ExprKind::Match { .. }
                        | ExprKind::IfElse { .. }
                        | ExprKind::Recover(..)
                        | ExprKind::ElseGuard { .. } => {} // a name bound to a callable, or an arbitrary value-producing callee
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
                    | ExprKind::ElseGuard { .. } => {
                        self.error(e.span, "defer requires a function or method call")
                    }
                }
                // Type-check the call (and its args); the result is discarded, like an expr stmt.
                self.infer(e);
            }
            StmtKind::Defer(DeferTarget::Block(body)) => {
                // `defer:` block — an ordinary nested scope checked in place. Unlike a `spawn:` block
                // it runs in the same task (no thread airlock), so we push NO `capture_floor`: reads
                // of enclosing locals (even non-sendable ones) are fine, and — uniform by-reference
                // capture — the block shares the enclosing binding's cell, so writing back through a
                // captured local now mutates the shared cell (A2/A3/E1); no reassign gate is needed.
                // A `defer:` block compiles to a fresh child proto with an empty loop stack, so a
                // `break`/`continue` lexically nested in an enclosing loop but placed here is illegal
                // at runtime. Save-zero-restore `loop_depth` (mirroring `check_fn_body`) so the
                // `loop_depth == 0` guard at `StmtKind::Break`/`Continue` fires at check time; a
                // legitimate loop INSIDE the block re-increments from 0, keeping its own break legal.
                // A `return` here can never mean anything (Chezzi has no named return values and the
                // block is its own closure) — the compiler silently dropped it, so reject it, like
                // `recover:`. Only `return` is checked: the `loop_depth` reset owns break/continue.
                if let Some(sp) = self.block_flow(body).returns {
                    self.error(
                        sp,
                        "'return' is not allowed inside a defer block".to_string(),
                    );
                }
                let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
                // The `defer:` block is its own closure with a `?`-DISCARDING contract: a fired
                // Err/None short-circuits the block and is dropped, never reaching the enclosing
                // fn's return (`syntax.md`). So a `?` here must be typed against a defer-local
                // discarding context, NOT the enclosing `current_ret` (which over-rejects when the
                // fn returns nil/int, and mis-accepts-by-coincidence when it returns Result). Zero
                // `recover_depth` too — this closure boundary means a `?` in the block cannot target
                // an enclosing `recover:` boundary (a `recover:` nested INSIDE the block re-arms it).
                let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
                let saved_in_defer = std::mem::replace(&mut self.in_defer_block, true);
                // M24 Task 4: the witness scope carries in — `compile_defer`'s block arm appends the
                // enclosing frame's `$w:T` bindings to the child proto's capture entries.
                self.check_block(body);
                self.in_defer_block = saved_in_defer;
                self.recover_depth = saved_recover;
                self.loop_depth = saved_loop_depth;
            }
            StmtKind::Parallel { body } => {
                self.push_scope();
                for stmt in body {
                    self.check_stmt(stmt);
                }
                self.pop_scope();
            }
            StmtKind::Spawn(target) => {
                // M-C implicit nurseries: a bare `spawn` is legal anywhere in a function body and at
                // the module top level — every function body (and the module top level) is an
                // implicit nursery that joins at its `return`/end, so there is no longer a
                // nursery-depth gate. The function-boundary rule (a task can't outlive the function
                // that spawned it) is enforced at runtime by the per-function implicit nursery.
                match target {
                    SpawnTarget::Call(e) => {
                        // `spawn` targets a method call or a call to a first-class callable (a user
                        // function/closure, a name bound to one, or a first-class builtin fn value —
                        // `print`/`ord`/`chr`/`panic`, which cross the airlock by name). Other built-ins
                        // and struct/enum constructors are not first-class values — wrap them in a
                        // function. Mirrors `defer`'s guard so the two features agree.
                        if let ExprKind::Call { callee, .. } = &e.kind {
                            match &callee.kind {
                                // M24-5b — a dotted CONSTRUCTOR (user or native) belongs with the
                                // bare-constructor rule below; a dotted STATIC METHOD is an
                                // ordinary call.
                                ExprKind::Field { .. } if self.dotted_ctor_target(callee) => {
                                    self.error(
                                        e.span,
                                        "spawn requires a function or method call (built-ins and \
                                         constructors must be wrapped in a function)",
                                    );
                                }
                                ExprKind::Field { .. } => {} // method or static-method call
                                ExprKind::Ident(name)
                                    if self.lookup(name).is_none()
                                        && !self.functions.contains_key(name)
                                        && !self.names_builtin_fn(name) =>
                                {
                                    self.error(
                                        e.span,
                                        "spawn requires a function or method call (built-ins and \
                                         constructors must be wrapped in a function)",
                                    );
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
                                | ExprKind::Call { .. }
                                | ExprKind::Index { .. }
                                | ExprKind::Try(_)
                                | ExprKind::OptChain { .. }
                                | ExprKind::NullCoalesce { .. }
                                | ExprKind::Closure { .. }
                                | ExprKind::Match { .. }
                                | ExprKind::IfElse { .. }
                                | ExprKind::Recover(..)
                                | ExprKind::ElseGuard { .. } => {}
                            }
                        }
                        // Full type-check of the call (callee, arity, args) — the single source of
                        // type diagnostics for the sub-expressions.
                        self.infer(e);
                        // Every value crossing the airlock must be sendable: the arguments, and
                        // (for a method spawn) the receiver the task talks through. Re-inferring
                        // here would duplicate the type errors `infer(e)` already reported, so we
                        // truncate any errors this re-inference adds and keep only the sendability
                        // diagnostics.
                        if let ExprKind::Call {
                            callee,
                            args,
                            named,
                            ..
                        } = &e.kind
                        {
                            let checkpoint = self.diag_mark();
                            let mut bad: Vec<(Span, String)> = Vec::new();
                            if let ExprKind::Field { obj, .. } = &callee.kind {
                                let rty = self.infer(obj);
                                // A module-qualified callee (`lib.helper(3)`, `math.abs(-3)`) is a
                                // PLAIN CALL through a NAMESPACE, not a call on a receiver value —
                                // nothing about the module crosses the airlock, so asking whether it
                                // is sendable is a question about the wrong thing. `defer` already
                                // treats this shape as a plain call, and Go accepts `go pkg.F(x)`
                                // exactly as it accepts `defer pkg.F(x)`; one concept, one verdict
                                // (M24-5).
                                //
                                // The skip is keyed on the same thing the compiler's
                                // [`crate::compiler::Compiler::receiverless_call_head`] is — an
                                // UNBOUND module NAME — not on the resolved type being `Ty::Module`.
                                // A module bound to a local (`m := math`) is `Ty::Module` but is not
                                // a namespace head: the compiler lowers it as a real receiver, and
                                // the airlock then refuses the module handle at RUN time on a
                                // program `chezzi check` had just passed. Two halves of one rule, so
                                // they ask one question (M24-5b).
                                let namespace_head = matches!(rty, Ty::Module(_))
                                    && matches!(&obj.kind, ExprKind::Ident(n)
                                        if self.imported_modules.contains_key(n)
                                            && !self.head_is_value(n));
                                if !namespace_head && !self.sendable(&rty) {
                                    bad.push((
                                        obj.span,
                                        format!(
                                            "cannot spawn on a non-sendable receiver of type {rty}"
                                        ),
                                    ));
                                }
                            }
                            // Every argument crossing the airlock must be sendable — positional AND
                            // keyword (a value+keyword spawn, `spawn h(f=cb)`, lowers to the same
                            // positional SpawnCall, so a non-sendable value smuggled in by LABEL must
                            // be rejected exactly like the positional form).
                            // D4 (TICKET-189, TICKET-240): decide, once, the shape each operand
                            // crosses with, per bound slot (keyword args, default fills and a pack
                            // at their compiled position). For a `lib.f` / `lib.K.f` head the
                            // receiver is the namespace expression, which is `Marked`. The compiler
                            // stores the shapes; the runtime walks them.
                            let receiver = match &callee.kind {
                                ExprKind::Field { obj, .. } => Some(&**obj),
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
                                | ExprKind::Call { .. }
                                | ExprKind::Index { .. }
                                | ExprKind::Try(_)
                                | ExprKind::OptChain { .. }
                                | ExprKind::NullCoalesce { .. }
                                | ExprKind::Closure { .. }
                                | ExprKind::Match { .. }
                                | ExprKind::IfElse { .. }
                                | ExprKind::Recover(..)
                                | ExprKind::ElseGuard { .. } => None,
                            };
                            let slots = self.bound_slots(e.id, args, named);
                            let crossing = self.call_crossing(receiver, &slots);
                            if self.records_node(e.id) {
                                crate::checker::record_call_table_entry(
                                    &mut self.crossings,
                                    &mut self.table_conflicts,
                                    (self.graph_module_idx, e.id.0),
                                    crossing.clone(),
                                    "spawn crossing",
                                    e.span,
                                );
                            }
                            for arg in args.iter().chain(named.iter().map(|(_, v)| v)) {
                                let aty = self.infer(arg);
                                if !self.sendable(&aty) {
                                    bad.push((
                                        arg.span,
                                        format!("cannot pass a non-sendable value of type {aty} to a spawned task"),
                                    ));
                                }
                            }
                            self.diag_rollback(checkpoint);
                            for (sp, msg) in bad {
                                self.error(sp, msg);
                            }
                            // D4 layer A: the callee's certain writes, through the same slots
                            // the compiler encodes.
                            self.report_named_call_writes(callee, &slots, true);
                            // B3.3 (Task 2a): a closure/nested-fn VALUE at the callee or an arg crosses
                            // the airlock by value — reject each of its non-sendable LOCAL captures (a
                            // captured `ref` etc.) at compile time, matching the `spawn:` block form.
                            // The callee `f` in `spawn f()` and every positional/keyword arg are
                            // checked; a module-global capture is already excluded at record time.
                            let mut cap_errs: Vec<(Span, Vec<Capture>)> = Vec::new();
                            for ex in std::iter::once(&**callee)
                                .chain(args.iter())
                                .chain(named.iter().map(|(_, v)| v))
                            {
                                let caps = self.spawn_value_captures(ex);
                                if !caps.is_empty() {
                                    cap_errs.push((ex.span, caps));
                                }
                            }
                            for (sp, caps) in cap_errs {
                                self.emit_capture_errors(&caps, sp);
                            }
                            if let Some(written) = self.value_writes(callee).first() {
                                self.error(
                                    callee.span,
                                    format!("'{written}' {}", crate::base::words::COPY_WRITE_TAIL),
                                );
                            }
                        }
                    }
                    SpawnTarget::Block(body) => {
                        // Bindings visible now are captured by the task and are read-only inside
                        // it (the airlock); bindings the body declares (at this floor or deeper)
                        // are task-local. `enter`/`leave` is balanced even if checking errors.
                        let floor = self.scopes.len();
                        self.capture_floors.push(floor);
                        // A spawned task outlives the frame — there is nothing for a `return` here
                        // to return to (it was silently dropped). Reject it, like `recover:`.
                        // Only `return` is checked: the `loop_depth` reset below owns break/continue.
                        if let Some(sp) = self.block_flow(body).returns {
                            self.error(
                                sp,
                                "'return' is not allowed inside a spawn block".to_string(),
                            );
                        }
                        // A `spawn:` block compiles to a fresh child proto with an empty loop stack,
                        // so a `break`/`continue` lexically nested in an enclosing loop but placed
                        // here is illegal at runtime. Save-zero-restore `loop_depth` (mirroring
                        // `infer_closure`) so the `loop_depth == 0` guard at `StmtKind::Break`/
                        // `Continue` fires at check time; a legitimate loop INSIDE the block
                        // re-increments from 0, keeping its own break/continue legal.
                        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
                        // A spawned task is its OWN frame (the compiler emits a fresh child proto),
                        // so the `?`-targeting state belonging to the enclosing frame stops at this
                        // boundary: an enclosing `defer:`'s discard contract and an enclosing
                        // `recover:` boundary are zeroed here and restored below. `in_spawn_block`
                        // then says what the resulting context IS — a task has no caller, so a `?`
                        // in it is rejected with the spawn-specific diagnostic rather than one
                        // naming the enclosing fn (W7-48). A `defer:`/`recover:` nested INSIDE the
                        // task re-arms from zero, so its own (per-frame, correct) contract still
                        // wins — see the gate order in `infer_try`.
                        //
                        // `current_ret`/`in_fn_body` are deliberately NOT zeroed. `in_spawn_block`
                        // gates `infer_try` BEFORE it reads either, so a reset buys nothing there —
                        // and their only other reader is `check_return` (`:3366`), where a fake
                        // `Nil` made `spawn: return 5` inside `fn main() -> int` add a second,
                        // FALSE error ("function returns nothing, cannot return a value") on top of
                        // the correct "'return' is not allowed inside a spawn block" — the exact
                        // class of enclosing-fn lie this fix exists to remove.
                        let saved_in_defer = std::mem::replace(&mut self.in_defer_block, false);
                        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
                        let saved_in_spawn = std::mem::replace(&mut self.in_spawn_block, true);
                        // M24 Task 4: the witness scope carries in. `compile_spawn`'s block arm
                        // appends the enclosing frame's `$w:T` bindings to the capture entries, and
                        // a witness is a plain `str` — it deep-copies across the airlock like any
                        // other captured string, so nothing here needs a sendability exemption.
                        self.push_scope();
                        for stmt in body {
                            self.check_stmt(stmt);
                        }
                        self.pop_scope();
                        self.in_spawn_block = saved_in_spawn;
                        self.recover_depth = saved_recover;
                        self.in_defer_block = saved_in_defer;
                        self.loop_depth = saved_loop_depth;
                        self.capture_floors.pop();
                    }
                }
            }
            StmtKind::Wait { arms, else_block } => self.check_wait(arms, else_block.as_ref()),
            StmtKind::Break => {
                if self.loop_depth == 0 {
                    self.error(span, "break outside loop");
                }
            }
            StmtKind::Continue => {
                if self.loop_depth == 0 {
                    self.error(span, "continue outside loop");
                }
            }
            // `pass` — a no-op statement; nothing to check.
            StmtKind::Pass => {}
            StmtKind::Expr(e) => self.check_discarded(e),
            StmtKind::Assert { cond, msg } => {
                self.expect_bool(cond, "assert condition");
                if let Some(m) = msg {
                    let t = self.infer_value(m);
                    if t != Ty::Str && !t.is_unknown() {
                        self.error(m.span, format!("assert message must be str, found {t}"));
                    }
                }
            }
        }
    }

    /// Best-effort source span for a function declaration (FnDecl has no span of its own): the first
    /// body statement, since a test fn / lifecycle hook always has a non-empty body.
    pub(super) fn fn_span(decl: &FnDecl) -> Span {
        decl.body.first().map(|s| s.span).unwrap_or_default()
    }

    /// A `test fn` takes no parameters (free) or only `self` (method) and returns nothing. Hard
    /// errors here keep the runner's contract simple (it invokes tests with no args / only the
    /// instance). The body is still checked normally by the caller.
    pub(super) fn validate_test_fn_shape(&mut self, decl: &FnDecl, host: Option<&str>) {
        let span = Self::fn_span(decl);
        self.reject_witness_runner_target(decl, host, "a `test fn`", span);
        if host.is_some() {
            let ok = decl.params.len() == 1 && decl.params[0].is_self();
            if !ok {
                self.error(span, "test method must take only self".to_string());
            }
        } else if !decl.params.is_empty() {
            self.error(span, "test function must take no parameters".to_string());
        }
        if decl.ret.is_some() {
            self.error(span, "test function must not return a value".to_string());
        }
    }

    /// M24 — the ONE stored answer to "does this fn/member take hidden witness params", read BY NAME:
    /// [`FnSig::witness_params`], derived once by [`Self::witness_params_of`] at the hoist (whose
    /// fixpoint makes the forwarding half transitive). `host` is the type's runtime key for a member,
    /// `None` for a module-level free fn. Never re-derives — a fresh derivation from a different point
    /// in the pipeline is exactly the second answer this milestone forbids.
    pub(super) fn stored_witness_params(&self, host: Option<&str>, name: &str) -> Vec<String> {
        let sig = match host {
            None => self.functions.get(name),
            Some(k) => self
                .structs
                .get(k)
                .and_then(|s| s.methods.get(name))
                .or_else(|| self.enum_methods.get(k).and_then(|ms| ms.get(name))),
        };
        sig.map(|s| s.witness_params.clone()).unwrap_or_default()
    }

    /// M24 Task 5 — `chezzi test` invokes a `test fn` (and each lifecycle hook) BY NAME at a fixed
    /// arity — nothing, or the suite instance. So it can carry no hidden witness argument: the
    /// runner has no call site at which to pin `T`, and the slot would be read off the stack as a
    /// type key. Rejected at the declaration, where the fix is obvious.
    fn reject_witness_runner_target(
        &mut self,
        decl: &FnDecl,
        host: Option<&str>,
        what: &str,
        span: Span,
    ) {
        let w = self.stored_witness_params(host, &decl.name);
        if w.is_empty() {
            return;
        }
        self.error(
            span,
            format!(
                "{what} is invoked by the test runner with no arguments of its own, so '{}' cannot construct through its static-protocol bound ({}) — the hidden type witness has no call site to come from. Move the construction into a helper the test calls with a concrete type",
                decl.name,
                w.join(", ")
            ),
        );
    }

    /// A suite lifecycle hook (`before_all`/`after_all`/`before_each`/`after_each`) must be
    /// `fn name(self)` returning nothing — the runner invokes it with only the instance.
    pub(super) fn validate_lifecycle_hook(&mut self, decl: &FnDecl, host: &str) {
        let span = Self::fn_span(decl);
        self.reject_witness_runner_target(decl, Some(host), "a suite lifecycle hook", span);
        let ok = decl.params.len() == 1 && decl.params[0].is_self();
        if !ok {
            self.error(
                span,
                format!("lifecycle hook '{}' must take only self", decl.name),
            );
        }
        if decl.ret.is_some() {
            self.error(
                span,
                format!("lifecycle hook '{}' must not return a value", decl.name),
            );
        }
    }

    /// A live const cannot be re-declared away in its own scope. Shared by the single-name let and,
    /// per name, by `check_destructure`'s tuple arm. A second module-level binding of a name is
    /// `GlobalBinding::clashes`'s, so this judges scope depth 2 and more only.
    pub(super) fn reject_redeclare(&mut self, name: &str, span: Span) {
        // `X := 2` / `X: T = 2` after `X: const T = 1` in one fn scope would drop the const mark
        // (`declare`). An INNER-scope binding of the same name is a fresh shadow and is untouched
        // (the outer scope's const set is not `.last()`). Skipped during return inference, whose
        // truncate-and-rerun can re-walk a body within one open scope.
        if !self.inferring_ret
            && self.scopes.len() > 1
            && self.const_decls.last().is_some_and(|s| s.contains(name))
        {
            self.error(
                span,
                format!("cannot re-declare const binding '{name}' (a const cannot be rebound — not even with ':=' or a new typed let)"),
            );
        }
    }
}
