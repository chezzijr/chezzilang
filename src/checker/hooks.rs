//! Checker: operator hooks: hook verdicts and tables, conformance obligations, requirement
//! matching, native satisfaction.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::proto::HookMiss;
use super::proto::ReqMiss;
use super::*;
use std::collections::{HashMap, HashSet};

impl Checker {
    /// TICKET-250 — is method `method` of declared type `ty` the hook for the reserved
    /// `protocol`? [`Self::hook_verdict`]'s `Ok`: the method's signature with the owner's type
    /// arguments substituted in, and the protocol's type parameters as that signature binds them.
    pub(super) fn hook_method(
        &self,
        ty: &Ty,
        protocol: &str,
        method: &str,
    ) -> Option<(FnSig, HashMap<String, Ty>)> {
        self.hook_verdict(ty, protocol, method).ok()
    }

    /// Does the run time call `method` of `ty` for `protocol`'s operator? True when the method is
    /// the hook of the declaration: for this instantiation ([`Self::hook_method`]), or for the
    /// instantiations that satisfy the method's own `where` bound (`HookMiss::Bound`). The hook
    /// table has one bit per declared type, so a reader that grants a hookless path (a constant
    /// hash, structural equality, the other iteration hook) asks this, never `hook_method`.
    pub(super) fn declares_hook(&self, ty: &Ty, protocol: &str, method: &str) -> bool {
        match self.hook_verdict(ty, protocol, method) {
            Ok(_) | Err(HookMiss::Bound(_)) => true,
            Err(HookMiss::Absent | HookMiss::Static | HookMiss::Shape(_)) => false,
        }
    }

    /// The `Iterator[T]` bound has ONE element type for a type: `s.next()`, `for q in s`,
    /// `List(s)` and the recovery of `T` all read it. `for` and `List()` run `iter` where the
    /// declaration has that hook and `s.next()` runs `next`, so a declaration whose two hooks
    /// yield different element types has no such type: `Err` names both, read at the
    /// DECLARATION, so every instantiation gets one answer. `Ok` is what iterating `ty` yields
    /// ([`Checker::iterable_elem`]).
    pub(super) fn iterator_bound_elem(&self, ty: &Ty) -> Result<Option<Ty>, String> {
        let decl = self.declaration_of(ty).unwrap_or_else(|| ty.clone());
        let called = |protocol: &str, hook: Hook| {
            self.declares_hook(&decl, protocol, hook.method())
                .then(|| self.hook_candidate(&decl, hook))
                .flatten()
        };
        let next = called("Iterator", Hook::Next).and_then(|sig| Self::next_ret_elem(sig.ret));
        let iter = called("Iterable", Hook::Iter).and_then(|sig| Self::iter_ret_elem(sig.ret));
        match (next, iter) {
            (Some(next), Some(iter)) if next != iter => Err(format!(
                "('next' yields {next}, 'iter' yields {iter}); the bound needs one element type. \
                 Bound the parameter `Iterable[{iter}]`, or make both hooks yield the same type"
            )),
            (Some(_) | None, Some(_) | None) => Ok(self.iterable_elem(ty)),
        }
    }

    /// The declaration that the declared type `ty` instantiates (`Key[int]` -> `Key[T]`), or
    /// `None` when `ty` is that declaration itself or is not a struct or an enum.
    fn declaration_of(&self, ty: &Ty) -> Option<Ty> {
        let own = |tps: &[TyParam]| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect();
        let decl = match ty {
            Ty::Struct(key, _) => {
                Ty::Struct(key.clone(), own(&self.struct_shape(key)?.type_params))
            }
            Ty::Enum(key, _) => Ty::enum_ty(key.clone(), own(self.enum_type_params_of(key)?)),
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
            | Ty::Var(_) => return None,
        };
        (decl != *ty).then_some(decl)
    }

    /// The one decider: the protocol's own requirement run through
    /// [`Self::requirement_matches_at`]. An `Err` says why the method is not the hook.
    ///
    /// There is one verdict per (declared type, hook), and the DECLARATION decides it
    /// (`Key[T]`, never `Key[int]`): the hook table has one bit per declared type. A method
    /// whose signature conforms for one instantiation only (`fn hash(self) -> T`, an operand
    /// `Box[int]`) is the hook of no instantiation. An instantiation adds one question, the
    /// method's own `where` bound (`HookMiss::Bound`), and recovers the substituted types.
    pub(super) fn hook_verdict(
        &self,
        ty: &Ty,
        protocol: &str,
        method: &str,
    ) -> Result<(FnSig, HashMap<String, Ty>), HookMiss> {
        if let Some(decl) = self.declaration_of(ty) {
            match self.hook_verdict(&decl, protocol, method) {
                Ok(_) | Err(HookMiss::Bound(_)) => {}
                Err(miss @ (HookMiss::Absent | HookMiss::Static | HookMiss::Shape(_))) => {
                    return Err(miss);
                }
            }
        }
        let methods = self.nominal_method_table(ty).ok_or(HookMiss::Absent)?;
        let actual = methods.get(method).ok_or(HookMiss::Absent)?;
        let pinfo = self.protocol_shape(protocol).ok_or(HookMiss::Absent)?;
        let (_, req) = pinfo
            .methods
            .iter()
            .find(|(n, _)| n == method)
            .ok_or(HookMiss::Absent)?;
        let tymap = self.nominal_param_map(ty);
        let sig = FnSig {
            params: actual.params.iter().map(|t| subst(t, &tymap)).collect(),
            ret: subst(&actual.ret, &tymap),
            ..actual.clone()
        };
        let self_map = HashMap::from([("Self".to_string(), ty.clone())]);
        let mut bound: HashMap<String, Ty> = HashMap::new();
        if req.params.len() == sig.params.len() {
            for (want, got) in req.params.iter().zip(&sig.params) {
                unify(&subst(want, &self_map), got, &mut bound);
            }
            unify(&subst(&req.ret, &self_map), &sig.ret, &mut bound);
        }
        bound.retain(|name, _| pinfo.type_params.contains(name));
        let args: Vec<Ty> = pinfo
            .type_params
            .iter()
            .map(|name| bound.get(name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        match self.requirement_matches_at(ty, protocol, &args, pinfo, method, methods) {
            Ok(()) => Ok((sig, bound)),
            Err(_) if actual.is_static => Err(HookMiss::Static),
            Err(ReqMiss::Bound { why, .. }) => Err(HookMiss::Bound(why)),
            Err(miss @ ReqMiss::Shape(_)) => Err(HookMiss::Shape(miss.text())),
        }
    }

    /// TICKET-250 — fill [`HookTable`] for every struct and enum this module declares: one
    /// `hook_method` question per `HOOKS` row, asked inside the owner's type-parameter scope.
    /// Runs after `infer_returns`, so an inferred return type is final.
    pub(super) fn record_hook_table(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            let (name, type_params, methods, is_struct) = match &s.kind {
                StmtKind::Struct {
                    name,
                    type_params,
                    methods,
                    ..
                } => (name, type_params, methods, true),
                StmtKind::Enum {
                    name,
                    type_params,
                    methods,
                    ..
                } => (name, type_params, methods, false),
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
                | StmtKind::Expr(_) => continue,
            };
            let saved = self.enter_type_params(type_params);
            let ty = if is_struct {
                self.struct_self_ty(name)
            } else {
                self.enum_self_ty(name)
            };
            let mut set = HookSet::default();
            for row in HOOKS {
                // A conditional method (`fn compare(..) where T: Comparable`) is the hook of the
                // declaration: the table has one bit per declared type, and `HookMiss::Bound`
                // makes an operator on an instantiation that fails the bound a check error.
                match self.hook_verdict(&ty, row.protocol, row.method) {
                    // The same two verdicts `declares_hook` answers `true` for.
                    Ok(_) | Err(HookMiss::Bound(_)) => set.insert(row.hook),
                    Err(HookMiss::Shape(why)) => {
                        let span = methods
                            .iter()
                            .find(|m| m.name == row.method)
                            .map_or(s.span, |m| m.name_span);
                        self.warn_near_miss_hook(&ty, name, row, &why, span);
                    }
                    Err(HookMiss::Absent | HookMiss::Static) => {}
                }
            }
            if self.range_key_conflict(&ty) {
                let span = methods
                    .iter()
                    .find(|m| m.name == "index")
                    .map_or(s.span, |m| m.name_span);
                self.error(span, Self::range_key_conflict_text(name));
            }
            self.exit_type_params(saved);
            if let Ty::Struct(key, _) | Ty::Enum(key, _) = ty {
                self.hook_table.insert((self.graph_module_idx, key), set);
            }
        }
    }

    /// TICKET-250 (D4) — ask a conformance question a type spelling raises. Inside `hoist` it is
    /// deferred until every type of the module is inserted; anywhere else it is answered now.
    /// A new conformance report inside `resolve_type` goes through here, never to `self.error`.
    pub(super) fn conformance_obligation(&mut self, ob: Obligation) {
        match &mut self.deferred_obligations {
            Some(list) => list.push((ob, self.type_params.clone())),
            None => self.discharge_obligation(ob),
        }
    }

    pub(super) fn discharge_obligation(&mut self, ob: Obligation) {
        match ob {
            Obligation::Hashable { ty, what, span } => {
                if let Some(why) = self.key_ty_reject(&ty) {
                    self.error(span, format!("{what} {why}"));
                    // The literal sites read this right after `resolve_type` to skip their own
                    // copy of the report; a deferred obligation has no such reader.
                    if self.deferred_obligations.is_none() {
                        self.pending_key_reject = Some(why);
                    }
                }
            }
            Obligation::Bound {
                name,
                key,
                is_struct,
                args,
                span,
            } => {
                let tps = if is_struct {
                    self.structs.get(&key).map(|i| i.type_params.clone())
                } else {
                    self.enum_type_params.get(&key).cloned()
                };
                self.check_type_arity_and_bounds(&name, tps, &args, span);
            }
            Obligation::AtomicPayload { elem, span } => self.reject_eq_atomic_payload(&elem, span),
            Obligation::ExternMarshal {
                ty,
                name,
                span,
                allow_void,
            } => self.assert_marshallable(&ty, &name, span, allow_void),
        }
    }

    /// The declared method of `ty` that carries `hook`'s name, hook or not. It decides nothing:
    /// its readers need the candidate's own `where` bounds, and ask [`Self::hook_method`] for the
    /// verdict.
    pub(super) fn hook_candidate(&self, ty: &Ty, hook: Hook) -> Option<FnSig> {
        self.nominal_method_table(ty)?.get(hook.method()).cloned()
    }

    /// TICKET-250 — the near-miss warning. A receiver-first method that [`Self::hook_verdict`]
    /// rejects for its shape warns only where the operator still accepts the type and silently
    /// skips the method: the operator is total (`print`), or the type satisfies the protocol
    /// without the hook (structural `==`, a zero-field `Set` element, `for` through the other
    /// iteration hook). Where the operator rejects the type, that use is already a check error
    /// with the same reason, so nothing is silent and no warning fires. For a generic owner the
    /// question is asked with each of its type parameters carrying the protocol, the most an
    /// instantiation can supply. The text is the decider's own reject reason.
    fn warn_near_miss_hook(&mut self, ty: &Ty, owner: &str, row: &HookRow, why: &str, span: Span) {
        if self.current_module_is_stdlib {
            return;
        }
        let skipped = row.total || {
            let saved = self.type_params.clone();
            for bounds in self.type_params.values_mut() {
                bounds.push(TyBound {
                    name: row.protocol.to_string(),
                    args: Vec::new(),
                });
            }
            let ok = self.satisfies(ty, row.protocol).is_ok();
            self.type_params = saved;
            ok
        };
        if skipped {
            self.warn(
                span,
                format!(
                    "'{}' on {owner} is not the {} hook ({why}); {} will not call it",
                    row.method, row.protocol, row.operator
                ),
            );
        }
    }

    /// The hook a call to `method` through `protocol` is: `Some` when the nearest protocol that
    /// declares `method` (`protocol` itself, or one it embeds) is a reserved one. A requirement a
    /// user protocol declares itself is an ordinary method call.
    pub(super) fn hook_requirement(&self, protocol: &str, method: &str) -> Option<Hook> {
        let owner = self.requirement_owner(protocol, method, &mut HashSet::new())?;
        if !is_reserved_protocol(&owner) {
            return None;
        }
        HOOKS
            .iter()
            .find(|row| row.method == method)
            .map(|row| row.hook)
    }

    fn requirement_owner(
        &self,
        pname: &str,
        method: &str,
        seen: &mut HashSet<String>,
    ) -> Option<String> {
        if !seen.insert(pname.to_string()) {
            return None;
        }
        let pinfo = self.protocol_shape(pname)?;
        if pinfo.methods.iter().any(|(n, _)| n == method) {
            return Some(pname.to_string());
        }
        pinfo
            .embeds
            .iter()
            .find_map(|e| self.requirement_owner(&e.name, method, seen))
    }

    /// Record a method call typed through `protocol`'s requirement for the backend, when that
    /// requirement is a hook ([`Self::hook_requirement`]).
    pub(super) fn record_hook_call(
        &mut self,
        name_span: Span,
        protocol: &str,
        method: &str,
        span: Span,
    ) {
        let Some(hook) = self.hook_requirement(protocol, method) else {
            return;
        };
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        );
        crate::checker::record_call_table_entry(
            &mut self.hook_calls,
            &mut self.table_conflicts,
            key,
            hook,
            "hook call",
            span,
        );
    }

    /// One requirement of [`Self::satisfies_methods`]: does `methods` carry `mname` with the
    /// signature `protocol` declares for it? A requirement that is a `HOOKS` row is the hook of
    /// the declaration or of no instantiation ([`Self::hook_verdict`]), so an instantiation of a
    /// declared type takes the declaration's shape verdict before its own arguments are compared.
    pub(super) fn requirement_matches(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        mname: &str,
        methods: &HashMap<String, FnSig>,
    ) -> Result<(), ReqMiss> {
        if HOOKS
            .iter()
            .any(|row| row.protocol == protocol && row.method == mname)
            && let Some(decl) = self.declaration_of(ty)
            && let Err(HookMiss::Shape(why)) = self.hook_verdict(&decl, protocol, mname)
        {
            return Err(ReqMiss::Shape(why));
        }
        self.requirement_matches_at(ty, protocol, args, pinfo, mname, methods)
    }

    /// [`Self::requirement_matches`] for `ty` as written, with no question to its declaration.
    fn requirement_matches_at(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        mname: &str,
        methods: &HashMap<String, FnSig>,
    ) -> Result<(), ReqMiss> {
        // TICKET-027: `protocol` may carry a module-qualified KEY — render the BARE name in messages.
        let protocol_display = crate::base::words::bare_display(protocol);
        let pmap: HashMap<String, Ty> = pinfo
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        // The RECEIVING type's own param→arg substitution (e.g. `T→int` from `Box[int]`). The user's
        // stored methods carry the struct/enum's type params UNsubstituted (a method on
        // `Box[T]` is stored as `add(self, o: Box[T]) -> Box[T]`), so for a generic instantiation we
        // must bind those params to the instantiation's args before comparing against the (Self-bound)
        // protocol signature — otherwise `compatible(Box[int], Box[T])` fails and a perfectly good
        // operator method is rejected as "wrong signature". Non-generic types yield an empty map (no
        // params), and checking inside the generic's own def (`ty = Box[T]`) yields an identity map
        // `{T→T}` — both are no-op substitutions, so this only ever binds concrete args.
        // Each arm resolves the receiving type's params through the miss-only identity-key helpers
        // (gap #4), so a generic named-fn-imported instantiation (`Box[int]` from a factory whose TYPE
        // name is not imported) binds its params here identically to a whole-module import.
        let tymap: HashMap<String, Ty> = match ty {
            // A protocol existential witnessing another protocol (M22): bind its OWN params to its
            // carried args, and `Self` to the existential itself — its sigs spell `Self` exactly as
            // the requirement's do, and only the requirement side is `Self`-bound by `method_matches`.
            Ty::Protocol(name, targs) => {
                let mut m: HashMap<String, Ty> = self
                    .protocol_shape(name)
                    .map(|p| {
                        p.type_params
                            .iter()
                            .cloned()
                            .zip(targs.iter().cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                m.insert("Self".to_string(), ty.clone());
                m
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
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => self.nominal_param_map(ty),
        };
        let Some((_, msig)) = pinfo.methods.iter().find(|(n, _)| n == mname) else {
            return Ok(());
        };
        {
            let subst_params: Vec<Ty> = msig.params.iter().map(|t| subst(t, &pmap)).collect();
            let min_params = subst_params.len();
            let want = FnSig {
                summary: fn_writes::FnSummary::default(),
                labels: msig.labels.clone(),
                params: subst_params,
                ret: subst(&msig.ret, &pmap),
                type_params: Vec::new(),
                where_bounds: Vec::new(),
                min_params,
                // Carry the protocol requirement's static-ness (e.g. `Convert`'s `convert(x: S)` is
                // STATIC) so `method_matches` can reject an instance/self-slot witness of a static slot.
                // Instance-method requirements keep `is_static == false`, so this is inert for them.
                is_static: msig.is_static,
                doc: None,
                witness_params: Vec::new(),
                variadic: None,
                slots: None,
                c_variadic: false,
            };
            let msig = &want;
            // Pre-substitute the receiving type's params into the ACTUAL (user) method signature so
            // its `Box[T]` becomes `Box[int]` before the comparison. Only the actual side is bound —
            // a genuinely wrong sig (`add(self, o: int) -> int`) stays a mismatch, no laundering.
            let actual_owned = methods.get(mname).map(|actual| {
                if tymap.is_empty() {
                    actual.clone()
                } else {
                    FnSig {
                        params: actual.params.iter().map(|t| subst(t, &tymap)).collect(),
                        ret: subst(&actual.ret, &tymap),
                        ..actual.clone()
                    }
                }
            });
            match actual_owned.as_ref() {
                Some(actual) if self.method_matches(msig, actual, ty) => {
                    if let Some(mm) = param_name_mismatch(msig, actual) {
                        return Err(ReqMiss::Shape(format!(
                            "type {ty} does not satisfy {protocol_display} ({})",
                            name_mismatch_text(&protocol_display, mname, mm)
                        )));
                    }
                    // Conditional conformance: a method whose `where` bounds the RECEIVER's own type
                    // param (e.g. `compare(self, o: Box[T]) -> int where T: Comparable` on `Box[T]`)
                    // only makes the type satisfy `protocol` when that bound HOLDS for this concrete
                    // instantiation. Enforce it structurally here — so EVERY satisfies-based consumer
                    // (operator dispatch, generic bounds, protocol-typed params, `for`) is sound, not
                    // just explicit `.method()` calls (which enforce at the call site). `tymap` maps the
                    // receiver param (`T`) to the concrete arg; `Ty::Unknown` args defer (satisfies_args
                    // returns `Ok` on Unknown), matching `List.sort`'s late-inference behaviour.
                    // Recursion terminates by structural descent (each level checks a smaller type arg).
                    // Pre-conditional-methods code has no `where_bounds` on any method, so this loop is a
                    // no-op there — zero effect on existing structural conformance.
                    for wb in &actual.where_bounds {
                        let Some(concrete) = tymap.get(&wb.name) else {
                            continue;
                        };
                        for bound in &wb.bounds {
                            let bargs: Vec<Ty> =
                                bound.args.iter().map(|a| subst(a, &tymap)).collect();
                            if self.satisfies_args(concrete, &bound.name, &bargs).is_err() {
                                return Err(ReqMiss::Bound {
                                    ty: ty.to_string(),
                                    protocol: protocol_display,
                                    why: format!(
                                        "method '{mname}' requires {}: {}",
                                        concrete,
                                        crate::base::words::bare_display(&bound.name)
                                    ),
                                });
                            }
                        }
                    }
                }
                // Name the REAL reason when the witness is generic, rather than blaming a signature
                // that is otherwise correct — the same message `satisfies_native` gives for a generic
                // native method. See `method_matches` for why a generic method cannot witness.
                Some(actual) if !actual.type_params.is_empty() => {
                    return Err(ReqMiss::Shape(format!(
                        "type {ty} does not satisfy {protocol_display} (method '{mname}' is generic \
                         and cannot witness a protocol requirement)"
                    )));
                }
                Some(_) => {
                    return Err(ReqMiss::Shape(format!(
                        "type {ty} does not satisfy {protocol_display} (method '{mname}' has the wrong signature)"
                    )));
                }
                None => {
                    return Err(ReqMiss::Shape(format!(
                        "type {ty} does not satisfy {protocol_display} (missing method '{mname}')"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Does a built-in (`List`/`Map`/`Set`/`str`/`bytes`/`bytearray`, or a scalar with no method
    /// table) satisfy a USER protocol out of its own harvested `native struct` method table
    /// (TICKET-024, W8-32)? `key` is [`Checker::native_witness_key`]'s answer for `ty`.
    ///
    /// The receiver param is PREPENDED to each harvested sig before it reaches
    /// [`Checker::satisfies_methods`] — load-bearing, not cosmetic: `harvest_native_fn_sig` strips
    /// the leading bare `self` (`src/checker/setup.rs:673-717`) while a protocol requirement keeps
    /// it as a `Ty::Unknown` `params[0]` (`src/checker/setup.rs:887-894`), and `method_matches`
    /// (`src/checker/mod.rs`) compares `params.len()` first. Without the prepend every requirement
    /// is refused as `(method '<name>' has the wrong signature)`.
    pub(super) fn satisfies_native(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        key: &str,
    ) -> Result<(), String> {
        // TICKET-027: `protocol` may carry a module-qualified KEY — render the BARE name in messages.
        let protocol_display = crate::base::words::bare_display(protocol);
        let native_methods = self.structs.get(key).map(|i| &i.methods);
        let mut table: HashMap<String, FnSig> = HashMap::new();
        for (mname, _) in &pinfo.methods {
            if self.std_internal_hidden(mname) {
                continue;
            }
            let Some(sig) = native_methods.and_then(|m| m.get(mname)) else {
                continue; // left out of the table -- satisfies_methods reports "(missing method ...)"
            };
            if !sig.type_params.is_empty() {
                return Err(format!(
                    "type {ty} does not satisfy {protocol_display} (native method '{mname}' is generic \
                     and cannot witness a protocol requirement)"
                ));
            }
            if let Some(why) = self.native_dispatch_residual(ty, mname) {
                return Err(format!(
                    "type {ty} does not satisfy {protocol_display} ({why})"
                ));
            }
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            params.push(ty.clone());
            params.extend(sig.params.iter().cloned());
            let mut labels = Vec::with_capacity(sig.labels.len() + 1);
            labels.push(None);
            labels.extend(sig.labels.iter().cloned());
            table.insert(
                mname.clone(),
                FnSig {
                    params,
                    labels,
                    min_params: sig.min_params + 1,
                    ..sig.clone()
                },
            );
        }
        self.satisfies_methods(ty, protocol, args, pinfo, &table)
    }
}
