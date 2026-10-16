//! Compiler: fn bodies: `compile_fn`, default and box parameter prologues, `finish`.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{FnDecl, Span, Stmt, StmtKind};
use crate::base::op::{Op, Proto, ProtoId};

impl Compiler {
    /// Compile a named function / method to its own proto. `params` occupy slots `0..arity`.
    pub(super) fn compile_fn(
        &mut self,
        decl: &FnDecl,
        _is_method: bool,
    ) -> Result<ProtoId, CompileError> {
        self.compile_fn_captured(decl, Vec::new())
    }

    /// Compile a function body to a child proto. `captured_names` is empty for a top-level/method fn
    /// (a plain `MakeFunc` proto — byte-identical to the pre-nested-fn path) and NON-empty for a
    /// NESTED fn compiled as a closure-with-a-name (the enclosing-frame binding snapshot, in slot
    /// order), so the body's free names (captured outer locals AND the recursive self-name) resolve
    /// via `GetCaptured` against the cells `MakeClosure` populates.
    pub(super) fn compile_fn_captured(
        &mut self,
        decl: &FnDecl,
        captured_names: Vec<String>,
    ) -> Result<ProtoId, CompileError> {
        // The fn's OWN generic params join the enclosing type's for the duration of this body: they
        // shadow any same-named module-level float alias at every coercion site below (and inside any
        // nested closure, which compiles within this same scope).
        // M24 — this body's OWN hidden witness params. `pending_witnesses` is set ONLY at the
        // module-level `fn`/member emit site, so a nested `fn` takes the empty default: it declares
        // none, and reaches the enclosing frame's through the `$w:T` capture entries
        // `with_witness_captures` appended at its `MakeClosure` (Task 4).
        let witnesses = std::mem::take(&mut self.pending_witnesses);
        let prev_w = std::mem::replace(&mut self.witness_locals, witnesses);
        let r = self.compile_fn_body(decl, captured_names);
        self.witness_locals = prev_w;
        r
    }

    fn compile_fn_body(
        &mut self,
        decl: &FnDecl,
        captured_names: Vec<String>,
    ) -> Result<ProtoId, CompileError> {
        // M24: the hidden trailing witness params sit AFTER the declared params, so every existing
        // slot index (float/box prologues, capture entries) is untouched and only the arity grows.
        let mut fc = FnComp::new(
            decl.name.clone(),
            decl.params.len() + self.witness_locals.len(),
            false,
        );
        fc.captured_names = captured_names;
        // Uniform by-reference capture (Task A): compute this frame's boxed-name set (unwired).
        fc.boxed_names = captured_names_of_body(&decl.body, &decl.params);
        fc.is_generator = decl.is_generator;
        fc.is_test = decl.is_test;
        fc.decl_span = decl.name_span;
        if decl.is_generator
            && let Some(names) = self
                .gen_crossings
                .frames
                .get(&(self.current_module_idx, decl.name_span))
        {
            fc.gen_slots = names.clone();
        }
        for p in &decl.params {
            fc.add_frame_local(p.name.clone());
        }
        for w in &self.witness_locals {
            fc.add_local(witness_local(w));
        }
        // Callee-side default fill FIRST, so a filled value is boxed like a supplied one.
        self.emit_default_param_prologue(&mut fc, &decl.params)?;
        // Uniform by-reference capture: box any param captured by a nested closure.
        self.emit_box_param_prologue(&mut fc, &decl.params);
        // An inline-expr body (`fn a(): <expr>`) implicitly returns its single expression — exactly
        // like a closure `fn(x): expr` (see `compile_closure`): compile the expr and emit `Return`
        // instead of evaluating-then-discarding it and falling through to `Nil`/`Return`. An inline
        // body cannot hold a bare `spawn` (spawn is a statement, not an expression), so the implicit-
        // nursery dance below never applies to it.
        if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            self.compile_expr(&mut fc, e)?;
            fc.emit(Op::Return, e.span);
            return Ok(self.finish(fc));
        }
        // M-C: if the body has a bare `spawn` (not inside an explicit `parallel:`), open an implicit
        // nursery at entry. It is NOT joined here — `do_return` joins it at every `return`/`?`/end (a
        // single join site), so we only emit the opening `EnterNursery` and flag the proto.
        let implicit = block_has_bare_spawn(&decl.body);
        if implicit {
            fc.has_implicit_nursery = true;
            // TICKET-048 — an implicit nursery has no keyword of its own, so it carries the span of
            // the first statement of the block it covers; the deadlock headline renders this.
            fc.emit(
                Op::EnterNursery,
                decl.body.first().map(|s| s.span).unwrap_or(Span::RUNTIME),
            );
            fc.nursery_scopes += 1;
        }
        self.compile_block_scoped(&mut fc, &decl.body)?;
        if implicit {
            fc.nursery_scopes -= 1;
        }
        // TICKET-184: the checker proved this end unreachable, so reaching it means the proof is
        // wrong — trap loudly instead of returning a silent `nil`. Fix `checker/flow.rs`, never this.
        let fall_off = self
            .fall_off
            .get(&(self.current_module_idx, decl.name_span))
            .copied();
        if fall_off == Some(crate::checker::FallOff::Trap) {
            fc.emit(
                Op::ConstStr(format!(
                    "internal: function '{}' fell off the end, but the type-checker proved every path returns",
                    decl.name
                )),
                Span::RUNTIME,
            );
            fc.emit(Op::CallBuiltin(Builtin::Panic, 1), Span::RUNTIME);
        }
        // Fall off the end → return Nil (do_return joins the implicit nursery); a `None!E` fn
        // returns `Ok(nil)` (TICKET-227).
        fc.emit(Op::Nil, Span::RUNTIME);
        if fall_off == Some(crate::checker::FallOff::OkNil) {
            self.emit_carrier_wrap(&mut fc, "Result", "Ok", Span::RUNTIME)?;
        }
        fc.emit(Op::Return, Span::RUNTIME);
        Ok(self.finish(fc))
    }

    /// **Callee-side default fill.** For each trailing parameter that carries a default, emit
    ///
    /// ```text
    ///     JumpIfProvided(slot, after)
    ///     <the default expression>
    ///     SetLocal(slot)
    ///   after:
    /// ```
    ///
    /// and lower [`FnComp::min_arity`] accordingly, so the runtime arity checks admit a call that
    /// omits them. The default is compiled HERE, in the module that declares the function, which is
    /// what makes this correct for the shapes no call-site rewrite can reach: a call through a
    /// first-class function value (the caller has no signature to consult) and a default whose type
    /// mentions `Self` or an enclosing type parameter (unspellable in the free `fn` a provider is).
    ///
    /// Runs BEFORE [`Compiler::emit_float_param_prologue`] so a filled `int` default still widens
    /// into a `float` parameter, and therefore before [`Compiler::emit_box_param_prologue`] so a
    /// filled value is boxed like a supplied one.
    ///
    /// **Emits nothing, and leaves `min_arity == arity`, unless every condition holds:**
    /// the parameters carrying defaults form a SUFFIX (a short call can only ever drop a suffix, so
    /// a hole before a supplied argument is not expressible), the proto carries no hidden trailing
    /// WITNESS parameters (they live past the declared ones, so a short declared count would land a
    /// witness in a defaulted slot), and no parameter is VARIADIC (its surplus collapse is a
    /// call-site rewrite the callee cannot reconstruct). A function with no defaults is
    /// byte-identical to before.
    fn emit_default_param_prologue(
        &mut self,
        fc: &mut FnComp,
        params: &[crate::ast::Param],
    ) -> Result<(), CompileError> {
        // ONE shared predicate with the checker — see `ast::min_callable_params` for why they must
        // agree by construction rather than by hand-sync.
        let first = crate::ast::min_callable_params(params, !self.witness_locals.is_empty());
        if first == params.len() {
            return Ok(()); // no short entry for this shape (no trailing defaults, variadic, witnesses)
        }
        for (i, p) in params.iter().enumerate().skip(first) {
            let Some(d) = &p.default else { continue };
            let span = d.span;
            // Patched below once the body's length is known.
            let jump_at = fc.code.len();
            fc.emit(Op::JumpIfProvided(i as u32, usize::MAX), span);
            // **The default is evaluated in MODULE scope, not the callee's.** `docs/reference/syntax/`: "a
            // default is evaluated on its own, where parameters are not in scope" — and the provider
            // `fn` that serves every reachable call site is a free top-level function, so it resolves
            // its free names against the module's globals. This prologue must give the SAME answer, or
            // one function's default means two different things depending on whether the caller could
            // see the definer. Measured before this hid the locals, `n := 100` at module level and
            // `fn f(n: int, x: str = "n={n}")`: a direct `f(3)` printed `n=100` (provider, module
            // scope) while `g := f; g(3)` printed `n=3` (prologue, callee scope) — the same default,
            // two answers. The frame is hidden from name lookup and closure capture for the duration
            // of this one expression by `name_floor` (read through `FnComp::visible_locals`), never
            // by replacing `locals`: a binder the default declares (a comprehension variable) must
            // take its slot from the same `slot_count` as every other local, so `locals[i]` stays
            // slot `i`.
            let saved_floor = std::mem::replace(&mut fc.name_floor, fc.locals.len());
            let saved_caps = std::mem::take(&mut fc.captured_names);
            let compiled = self.compile_expr(fc, d);
            debug_assert!(
                compiled.is_err() || fc.locals.len() == fc.name_floor,
                "a default left a binder in scope"
            );
            fc.name_floor = saved_floor;
            fc.captured_names = saved_caps;
            // Propagate rather than swallow: the checker has ALREADY advertised short entry for this
            // signature (`FnSig::min_params`), so silently declining to emit the fill would leave a
            // call the checker accepts and the runtime rejects — the exact checker⊆compiler violation
            // `ast::min_callable_params` exists to make impossible.
            compiled?;
            fc.emit_set_local_raw(i, span);
            let after = fc.code.len();
            if let Op::JumpIfProvided(_, t) = &mut fc.code[jump_at] {
                *t = after;
            }
        }
        fc.min_arity = first;
        Ok(())
    }

    /// Uniform by-reference capture — box every param captured by a nested closure/`spawn`/`defer`.
    /// After arg binding, replace the raw arg in the boxed slot with a fresh cell wrapping it:
    /// `GetLocal(slot); NewCell; SetLocal(slot)`. The param slots are `0..params.len()` in
    /// declaration order. A fn with no captured params emits nothing.
    pub(super) fn emit_box_param_prologue(
        &mut self,
        fc: &mut FnComp,
        params: &[crate::ast::Param],
    ) {
        for i in 0..params.len() {
            if fc.is_boxed_slot(i) {
                let span = Span::RUNTIME;
                fc.emit_get_local_raw(i, span);
                fc.emit(Op::NewCell, span);
                fc.emit_set_local_raw(i, span);
            }
        }
    }

    pub(super) fn finish(&mut self, fc: FnComp) -> ProtoId {
        let claims = |c: &[Option<bool>]| c.iter().map(|&c| c == Some(true)).collect::<Vec<_>>();
        let private = crate::base::crossing::slot_bits(&claims(&fc.slot_private));
        let deep = private & crate::base::crossing::slot_bits(&claims(&fc.slot_deep));
        let pid = self.program.protos.len();
        // M19: peephole pass — const-fold + superinstruction fusion, with jump relocation.
        let (code, lines) = peephole::optimize(fc.code, fc.lines);
        self.program.protos.push(Proto {
            name: fc.name,
            arity: fc.arity,
            min_arity: fc.min_arity,
            n_slots: fc.max_slots,
            code,
            lines,
            has_implicit_nursery: fc.has_implicit_nursery,
            is_generator: fc.is_generator,
            is_test: fc.is_test,
            decl_span: fc.decl_span,
            // Lever #3: cold-path capture-name metadata in slot order (empty for non-closures).
            capture_names: fc.captured_names,
            private_slots: private,
            deep_slots: deep,
        });
        pid
    }

    // ----- statements -----
}
