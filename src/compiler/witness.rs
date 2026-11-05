//! Compiler: call arguments and witnesses: crossing masks, sum seeds, carrier wraps, static calls,
//! witness args.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{Expr, ExprKind, Span};
use crate::base::op::{CapEntry, Op};
use crate::checker::Resolution;

impl Compiler {
    /// Compile a positional argument list onto the stack. Replaces the flat
    /// `for a in args { compile_expr }` loop repeated at every call/ctor/variant/static/defer emit
    /// site. No argument is coerced: an int never reaches a `float` slot (D3, TICKET-138).
    pub(super) fn compile_args(
        &mut self,
        fc: &mut FnComp,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        for a in args {
            self.compile_expr(fc, a)?;
        }
        Ok(())
    }

    /// M24 Task 5 — the hidden witness params a MEMBER's proto carries, from the checker's `fns`
    /// table. Keyed `<type key>.<method>` under the DECLARING module, which cannot collide with a
    /// free fn's entry (no fn name contains a `.`).
    pub(super) fn member_witnesses(
        &self,
        module_idx: usize,
        type_key: &str,
        method: &str,
    ) -> Vec<String> {
        self.witnesses
            .fns
            .get(&(module_idx, format!("{type_key}.{method}")))
            .cloned()
            .unwrap_or_default()
    }

    /// The seed a `xs.sum()` site needs, per the checker's [`crate::checker::SumSeedTable`] --
    /// `Some(SumSeed::Float)` for a plain `List[float]`. A MISS, or a recorded `None`, means the plain numeric sum (the pre-fix
    /// lowering), so a stale/absent entry can only under-apply. Keyed on the method-NAME token,
    /// which is distinct per link of a postfix/pipe chain (the call node's span is not -- see
    /// [`crate::checker::CarrierKey`]).
    pub(super) fn sum_seed(
        &self,
        name: &str,
        args: &[Expr],
        name_span: Span,
    ) -> Option<crate::checker::SumSeed> {
        if name != "sum" || !args.is_empty() {
            return None;
        }
        self.sum_seeds
            .get(&crate::checker::carrier_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                name_span,
            ))?
            .clone()
    }

    /// Emit `sum`'s hidden seed argument: a bare `0.0` for a plain `List[float]`. The value IS the
    /// empty list's answer, and its TAG tells the runtime fold which kind to accumulate in.
    pub(super) fn emit_sum_seed(
        &self,
        fc: &mut FnComp,
        seed: &crate::checker::SumSeed,
        span: Span,
    ) {
        match seed {
            crate::checker::SumSeed::Float => fc.emit(Op::ConstFloat(0.0), span),
        }
    }

    /// W8-21 — emit the `Op::NewEnum` wrap a declared `T?`/`T!E` return sink's success-coercion
    /// verdict calls for, over a value already on the stack (`WrapOkNil` additionally needs
    /// `Op::Nil` pushed by its caller BEFORE this runs — mirrors the written zero-arg `Ok()` site
    /// above). A miss or `NoWrap` is a no-op: the pre-fix lowering.
    pub(super) fn emit_ret_coerce(
        &mut self,
        fc: &mut FnComp,
        span: Span,
    ) -> Result<(), CompileError> {
        let key = crate::checker::ret_coerce_key(
            self.current_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            span,
        );
        match self.ret_coerce.get(&key) {
            None | Some(crate::checker::RetCoerce::NoWrap) => Ok(()),
            Some(crate::checker::RetCoerce::WrapOkNil) => {
                self.emit_carrier_wrap(fc, "Result", "Ok", span)
            }
        }
    }

    /// Build the carrier variant `enum_key.variant` around the stack top, for a wrap the CHECKER
    /// decided (`FallOff::OkNil`, `Wrap`, `!e`, `RetCoerce`) — a verdict, never a name in source.
    pub(super) fn emit_carrier_wrap(
        &mut self,
        fc: &mut FnComp,
        enum_key: &str,
        variant: &str,
        span: Span,
    ) -> Result<(), CompileError> {
        if self.variant_id_of_key(enum_key, variant) == crate::base::op::VID_NONE {
            return Err(CompileError {
                message: format!(
                    "internal: no '{variant}' variant registered for a carrier value \
                     -- the type-checker and the backend disagree"
                ),
                span,
            });
        }
        self.emit_new_enum(fc, enum_key, variant, 1, span);
        Ok(())
    }

    /// A variant constructor call `enum_key.variant(args)`, qualified or through a bare imported
    /// name. Zero-arg `Result.Ok()` is `Result[nil, E]`'s success value (DEC-017, the checker rule
    /// is in `infer_variant_call`): the `nil` payload is synthesized.
    pub(super) fn compile_variant_call(
        &mut self,
        fc: &mut FnComp,
        enum_key: &str,
        variant: &str,
        args: &[Expr],
        span: Span,
    ) -> Result<(), CompileError> {
        if args.is_empty() && (enum_key, variant) == ("Result", "Ok") {
            fc.emit(Op::Nil, span);
            self.emit_new_enum(fc, enum_key, variant, 1, span);
            return Ok(());
        }
        self.compile_args(fc, args)?;
        self.emit_new_enum(fc, enum_key, variant, args.len(), span);
        Ok(())
    }

    pub(super) fn member_witness_srcs(
        &self,
        name_span: Span,
    ) -> Option<&Vec<crate::checker::WitnessSrc>> {
        self.witnesses.calls.get(&crate::checker::witness_key(
            self.current_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        ))
    }

    /// The internal name of the `convert` overload the checker picked for the call or value read
    /// at `name_span` (`WitnessTable::overload_calls`); `None` for a name with one method.
    pub(super) fn overload_pick(&self, name_span: Span) -> Option<String> {
        self.witnesses
            .overload_calls
            .get(&crate::checker::witness_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                name_span,
            ))
            .cloned()
    }

    /// Push `args` and emit `Op::CallStatic` for a `Type.method(args)` static call. The six call
    /// sites differ only in how they derive `type_key` (bare / qualified / turbofish); this collapses
    /// the identical push-then-emit tail — including the hidden witness arguments of a generic static
    /// method (M24 Task 5), which ride LAST so no declared slot index moves.
    pub(super) fn emit_call_static(
        &mut self,
        fc: &mut FnComp,
        type_key: String,
        name: &str,
        call: (crate::ast::NodeId, &Expr, &[Expr], &[(String, Expr)]),
        name_span: Span,
        span: Span,
    ) -> Result<(), CompileError> {
        let (call_id, callee, args, named) = call;
        let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
        let w = match self.member_witness_srcs(name_span).cloned() {
            Some(srcs) => self.emit_witness_args(fc, &srcs, name, span)?,
            None => 0,
        };
        // A `convert` overload: the checker's pick for this call, by the witness key.
        let method = self
            .overload_pick(name_span)
            .unwrap_or_else(|| name.to_string());
        fc.emit(
            Op::CallStatic {
                type_key,
                method,
                argc: argc + w,
            },
            span,
        );
        Ok(())
    }

    /// M24-5b — does this `spawn`/`defer` call target have a TYPE, rather than a value, at its head?
    /// `Op::SpawnRecv`/`DeferMethod` record a RECEIVER value plus a member name, and
    /// `Type.static_method(..)` has no receiver: compiling the head as a value panicked in
    /// [`Self::global_slot`] (a bare type name has no global slot) or — for a `from`-imported type,
    /// whose slot exists and holds `Nil` — faulted with `type nil has no method`. Both are answered
    /// by [`Self::compile_receiverless_target`] instead.
    ///
    /// The question asked is deliberately "does the head NAME A TYPE OR A MODULE" — a NAMESPACE
    /// rather than a value — not "which of [`Self::compile_call`]'s receiverless arms matches": one
    /// stable question rather than a second copy of that arm list to drift out of sync with. A head
    /// that is a local/capture (including one SHADOWING a type or module name) or any other value
    /// never answers yes, so every genuine receiver shape keeps its `SpawnRecv`/`DeferMethod`
    /// lowering.
    pub(super) fn receiverless_call_head(&self, callee: &Expr) -> Result<bool, CompileError> {
        // A tuple slot (`t.0`) is a value, never a name (`compile_call` skips it the same way).
        match &callee.kind {
            ExprKind::Field { name, .. } if !crate::ast::is_tuple_index(name) => {}
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
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => return Ok(false),
        }
        // A type member, a native ctor or a module member is receiverless; a value's member (a
        // local that merely SHADOWS a type or module name included) is a genuine receiver.
        Ok(matches!(
            self.resolution(callee)?,
            Resolution::MethodFn { .. }
                | Resolution::NativeStatic(_)
                | Resolution::Variant { .. }
                | Resolution::VariantFn { .. }
                | Resolution::Fn { .. }
                | Resolution::Decode(_)
                | Resolution::StructCtor(_)
                | Resolution::WitnessStatic(_)
                | Resolution::ParamMethodFn { .. }
                | Resolution::Builtin(_)
                | Resolution::ModuleMember { .. }
        ))
    }

    /// M24-5b — lower a receiverless `defer Type.m(a, b)` / `spawn Type.m(a, b)`. The arguments are
    /// evaluated EAGERLY here (Go semantics — `defer pkg.F(x)` snapshots `x`, and so does every other
    /// Chezzi `defer`/`spawn` argument) and handed to a synthetic wrapper proto whose body REPLAYS the
    /// original callee through [`Self::compile_call`] with the parameters standing in for the
    /// arguments. So every static spelling — bare, type-level turbofish, `from`-imported,
    /// module-qualified, combined turbofish, and the `$w:T` witness call — reaches exactly the
    /// bytecode its eager form emits, and there is no second dispatch list to keep in sync.
    ///
    /// The wrapper is a `MakeClosure` rather than a `MakeFunc` only so it can reach the enclosing
    /// frame's `$w:T` witnesses the way a `defer:` block does ([`Self::with_witness_captures`]); nothing
    /// else is captured, so a witness-free target is still a capture-free callee. Returns the operand count for `DeferCall`/`SpawnCall`.
    pub(super) fn compile_receiverless_target(
        &mut self,
        fc: &mut FnComp,
        call: (crate::ast::NodeId, &Expr, &[Expr], &[(String, Expr)]),
        span: Span,
    ) -> Result<usize, CompileError> {
        let (call_id, callee, args, named) = call;
        // The arguments -- default fills included -- are evaluated here, at the statement; the
        // replay is a positional call of that many values with no plan (SYNTH id).
        let argc = self
            .call_plans
            .get(&(self.current_module_idx, call_id.0))
            .map_or(args.len(), Vec::len);
        let snap = fc.snapshot_entries();
        // No free-name walk exists for a SYNTHESIZED body: the wrapper replays `callee(args)`, which
        // is a call site of the ENCLOSING body — so every witness rides (`None`), and the replay's
        // own `compile_call` finds whatever it needs.
        let entries = self.with_witness_captures(&snap, Vec::new(), None);
        let mut child = FnComp::new("<deferred static call>".to_string(), argc, false);
        child.captured_names = entries.iter().map(|e| e.name.clone()).collect();
        // One unspellable parameter per argument, in source order, so the replayed call reads them
        // back as ordinary locals. `$` is not an identifier character (same trick as `$w:`).
        let params: Vec<Expr> = (0..argc)
            .map(|i| {
                let n = format!("$a{i}");
                child.add_local(n.clone());
                Expr {
                    id: crate::ast::NodeId::SYNTH,
                    kind: ExprKind::Ident(n),
                    span,
                }
            })
            .collect();
        self.compile_call(
            &mut child,
            callee,
            &params,
            &[],
            None,
            span,
            crate::ast::NodeId::SYNTH,
        )?;
        child.emit(Op::Return, span);
        let pid = self.finish(child);
        fc.emit(Op::MakeClosure(pid, entries), span);
        self.compile_call_args(fc, call_id, callee, args, named, span)?;
        Ok(argc)
    }

    /// M24-5 — the hidden witness arguments a `spawn`/`defer` VALUE-callee target must push on top
    /// of its already-pushed declared args; the count widens `Op::SpawnCall`/`Op::DeferCall`'s
    /// `argc`. Same by-name shape rule as the eager `Op::Call` site: only a bare `Ident` naming a
    /// module-level fn can be a witness call, so a chained head link (`defer mk(c)(5)`, whose callee
    /// is itself a `Call`) never reads its head's entry.
    pub(super) fn emit_indirect_witness_args(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        span: Span,
    ) -> Result<usize, CompileError> {
        let ExprKind::Ident(fname) = &callee.kind else {
            return Ok(0);
        };
        match self.witness_srcs(callee, fname, span)? {
            Some(srcs) => {
                let fname = fname.clone();
                self.emit_witness_args(fc, &srcs, &fname, span)
            }
            None => Ok(0),
        }
    }

    /// M24-5 — the same, for a MEMBER target (`spawn h.make(c)`, `defer lib.reset(c)`). The
    /// module-QUALIFIED spelling goes through [`Self::witness_srcs`] so its stray-entry guard still
    /// applies; an instance method has no `witness_fn_key`, so it reads the checker's record at the
    /// method-NAME token directly ([`Self::member_witness_srcs`]) — the same key `Op::CallMethod`
    /// uses for the eager form.
    pub(super) fn emit_member_witness_args(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        name: &str,
        name_span: Span,
        span: Span,
    ) -> Result<usize, CompileError> {
        let srcs = match self.witness_srcs(callee, name, span)? {
            Some(srcs) => Some(srcs),
            None => self.member_witness_srcs(name_span).cloned(),
        };
        match srcs {
            Some(srcs) => self.emit_witness_args(fc, &srcs, name, span),
            None => Ok(0),
        }
    }

    /// M24 Task 3 — does the fn a call site NAMES take hidden trailing witness params?
    /// [`crate::checker::WitnessTable::fns`] is keyed by the module that DECLARES the fn, so the
    /// call site's own index is only right for a local callee; a `from`-imported one resolves through
    /// [`Self::imported_fns`] and a qualified one (`lib.reset(...)`) through the module bind. Callees
    /// this cannot classify (a value, a method, a local shadow) answer `false` — and the stray-entry
    /// guard in [`Self::witness_srcs`] is what keeps such a miss loud instead of one `argc` short.
    fn callee_takes_witnesses(&self, callee: &Expr) -> Result<bool, CompileError> {
        Ok(self
            .witness_fn_key(callee)?
            .is_some_and(|k| self.witnesses.fns.contains_key(&k)))
    }

    /// The `(module index, fn name)` [`crate::checker::WitnessTable::fns`] would key this callee
    /// under — the module that DECLARES it and the name it is DECLARED as (an `import reset as again`
    /// binding is keyed `reset`, not `again`) — or `None` when the callee is not a by-name call on a
    /// module-level fn.
    fn witness_fn_key(&self, callee: &Expr) -> Result<Option<(usize, String)>, CompileError> {
        Ok(match &callee.kind {
            ExprKind::Ident(_) => match self.resolution(callee)? {
                Resolution::Fn { module, name } => Some((*module, name.clone())),
                Resolution::Local
                | Resolution::Global { .. }
                | Resolution::Builtin(_)
                | Resolution::StructCtor(_)
                | Resolution::Provider
                | Resolution::ParamMethodFn { .. }
                | Resolution::IndexCall
                | Resolution::Variant { .. }
                | Resolution::MethodFn { .. }
                | Resolution::NativeStatic(_)
                | Resolution::VariantFn { .. }
                | Resolution::WitnessStatic(_)
                | Resolution::ModuleMember { .. }
                | Resolution::Module(_)
                | Resolution::Member
                | Resolution::PatBinding
                | Resolution::PatStruct(_)
                | Resolution::Decode(_) => None,
            },
            ExprKind::Field { name, .. } if !crate::ast::is_tuple_index(name) => {
                match self.resolution(callee)? {
                    Resolution::ModuleMember { module, name } | Resolution::Fn { module, name } => {
                        Some((*module, name.clone()))
                    }
                    Resolution::Local
                    | Resolution::Global { .. }
                    | Resolution::Builtin(_)
                    | Resolution::StructCtor(_)
                    | Resolution::Provider
                    | Resolution::ParamMethodFn { .. }
                    | Resolution::IndexCall
                    | Resolution::Variant { .. }
                    | Resolution::MethodFn { .. }
                    | Resolution::NativeStatic(_)
                    | Resolution::VariantFn { .. }
                    | Resolution::WitnessStatic(_)
                    | Resolution::Module(_)
                    | Resolution::Member
                    | Resolution::PatBinding
                    | Resolution::PatStruct(_)
                    | Resolution::Decode(_) => None,
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
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        })
    }

    /// The module-resolution half of [`Self::witness_fn_key`], for a callee already reduced to the
    /// `(module, name)` pair a [`CallSite`] carries. `None` for a qualified head that names no
    /// imported module (a receiver value, or a type).
    fn witness_fn_key_named(&self, module: Option<&str>, name: &str) -> Option<(usize, String)> {
        match module {
            None => Some(
                self.imported_fns
                    .get(name)
                    .cloned()
                    .unwrap_or((self.current_module_idx, name.to_string())),
            ),
            Some(m) => self
                .imported_modules
                .get(m)
                .map(|&idx| (idx, name.to_string())),
        }
    }

    /// M24-2 — could a call the free-name walk recorded as `name(…)` / `head.name(…)` take hidden
    /// witness arguments? The [`nested_body_needs_witness`] lookup.
    ///
    /// A FIELD-HEADED call always answers yes. What the head `X` in `X.name(…)` denotes — an imported
    /// module, a struct/enum TYPE, a receiver value, or a module name shadowed by an enclosing
    /// binding — is a question about the enclosing scopes and the type namespace that this syntactic
    /// walk does not carry, and two rounds of guessing at it each shipped a check-ok/run-fault (a
    /// shadowing param, then a type named like an imported module). Answering yes for every head
    /// retires the whole class by construction: there is no head shape left to classify.
    ///
    /// So a NO comes only from a BARE-IDENT callee, resolved through the real
    /// [`crate::checker::WitnessTable::fns`] table — a call the compiler itself would thread no
    /// witness into at a real call site.
    fn call_may_take_witnesses(&self, module: Option<&str>, name: &str) -> bool {
        module.is_some()
            || self
                .witness_fn_key_named(None, name)
                .is_some_and(|k| self.witnesses.fns.contains_key(&k))
    }

    /// M24 Task 4 — append the enclosing frame's hidden `$w:T` bindings to a nested body's capture
    /// entries (closure, nested `fn`, `spawn:`/`defer:` block). They can never survive the
    /// free-variable filter — `$` is unspellable, so no source name makes one free — so a child proto
    /// reaches its witness only because this puts it back. `snapshot` is the SAME `snapshot_entries()`
    /// the free filter ran over, so a `$w:T` the enclosing frame itself only CAPTURED (a closure in a
    /// closure) rides along with its `CapSrc::Captured` stamp already correct.
    ///
    /// M24-2 — a witness rides only when the body can REACH it ([`nested_body_needs_witness`], which
    /// states the invariant that keeps `FnComp::witness_ref` a superset of the checker's
    /// `witness_scope`). `free` is that body's already-computed free-name walk; `None` means the body
    /// is SYNTHESIZED rather than written — the receiverless `defer Type.m(…)` wrapper replays a call
    /// that is in no walk at all — so every witness rides, as it did everywhere before.
    pub(super) fn with_witness_captures(
        &self,
        snapshot: &[CapEntry],
        mut entries: Vec<CapEntry>,
        free: Option<&FreeNames>,
    ) -> Vec<CapEntry> {
        entries.extend(
            snapshot
                .iter()
                .filter(|e| match e.name.strip_prefix(WITNESS_PREFIX) {
                    // The suffix is the slot's tag (`witness_local` formats exactly `$w:<tag>`),
                    // whose type param `witness_tag_param` reads back.
                    Some(tag) => free.is_none_or(|f| {
                        let t = crate::checker::witness_tag_param(tag);
                        nested_body_needs_witness(f, t, &|m, n| self.call_may_take_witnesses(m, n))
                    }),
                    None => false,
                })
                .cloned(),
        );
        entries
    }

    /// M24 — the witness arguments this call site must push, `None` when the callee takes none.
    /// BOTH directions are hard errors, never a silent short `argc`: a callee that needs witnesses
    /// with no recorded entry, and a recorded entry at a call the compiler would not thread (the
    /// checker recorded a witness the backend is about to drop).
    pub(super) fn witness_srcs(
        &self,
        callee: &Expr,
        fname: &str,
        span: Span,
    ) -> Result<Option<Vec<crate::checker::WitnessSrc>>, CompileError> {
        // Only a BY-NAME call on a module-level fn can be a witness call, and only such a callee is
        // held to the stray-entry half of the guard below. A chained postfix link shares its
        // primary expression's span (`lib.reset(c).tag(1)` keys where `lib.reset(c)` recorded), so a
        // blanket check would read the head link's entry and reject a legal program.
        if self.witness_fn_key(callee)?.is_none() {
            return Ok(None);
        }
        let key = crate::checker::witness_key(
            self.current_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            crate::checker::witness_key_span(callee, span),
        );
        let recorded = self.witnesses.calls.get(&key);
        match (self.callee_takes_witnesses(callee)?, recorded) {
            (true, Some(srcs)) => Ok(Some(srcs.clone())),
            (true, None) => Err(CompileError {
                message: format!(
                    "internal: no static-type witness recorded for the call to '{fname}'"
                ),
                span,
            }),
            (false, Some(_)) => Err(CompileError {
                message: format!(
                    "internal: a static-type witness is recorded for the call to '{fname}', which \
                     this call site does not thread"
                ),
                span,
            }),
            (false, None) => Ok(None),
        }
    }

    /// M24 — push one argument per witness slot, on top of the already-pushed declared args: a
    /// `ConstStr` type-identity key for a CONCRETE witness, or a load of the caller's own `$w:p` local
    /// for a FORWARDED one. Returns how many were pushed, which widens the call's `argc`.
    pub(super) fn emit_witness_args(
        &mut self,
        fc: &mut FnComp,
        srcs: &[crate::checker::WitnessSrc],
        fname: &str,
        span: Span,
    ) -> Result<usize, CompileError> {
        for src in srcs {
            match src {
                // The witness value is the static-method key: the type AND the method the caller
                // picked (a `convert` overload), which `CallStaticDyn` splits.
                crate::checker::WitnessSrc::Concrete { type_key, method } => {
                    fc.emit(Op::ConstStr(static_key(type_key, method)), span)
                }
                // FORWARDING: the caller's own witness for `p` and this requirement IS the argument
                // — its `$w:` local, or (Task 4, inside a nested body) the capture of it. The checker
                // records this only where that witness is reachable (`Checker::witness_scope`), so a
                // missing one is an internal invariant break, never a short `argc`.
                crate::checker::WitnessSrc::Forward { param: p, req } => {
                    let Some(w) = fc.witness_ref(&crate::checker::witness_tag(p, req)) else {
                        return Err(CompileError {
                            message: format!(
                                "internal: no type witness in scope to forward as '{p}' into \
                                 the call to '{fname}'"
                            ),
                            span,
                        });
                    };
                    fc.emit_witness(w, span);
                }
            }
        }
        Ok(srcs.len())
    }
}
