// checker::sig — split out of checker/mod.rs. `super::*` == the `checker` module.
// Function signatures: receivers, `fn_sig`, parameter slots. Return inference is in `infer_ret.rs`.

use super::proto::HookMiss;
use super::*;

/// Where a spelled type sits: `None` ("returns nothing") is a type only in a `Void` position.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum TypePos {
    /// A value position: a parameter, a field, an element, a type argument.
    Value,
    /// A declared return, a fn-type return, the success side of a carrier.
    Void,
    /// A parameter of an `extern` or `native` fn signature, or of a fn type written in one.
    CParam,
    /// The return of an `extern` or `native` fn signature, or of a fn type written in one.
    CRet,
    /// A struct field's own type (not its type arguments).
    Field,
}

impl TypePos {
    #[cfg(test)]
    pub(super) const ALL: [TypePos; 5] = [
        TypePos::Value,
        TypePos::Void,
        TypePos::CParam,
        TypePos::CRet,
        TypePos::Field,
    ];

    /// TICKET-262 — may a `std.ffi` width type be written here? Owner rule 1: only at the C boundary.
    pub(super) fn width_ok(self) -> bool {
        match self {
            TypePos::CParam | TypePos::CRet | TypePos::Field => true,
            TypePos::Value | TypePos::Void => false,
        }
    }

    /// May `None` ("returns nothing") be written here?
    pub(super) fn void_ok(self) -> bool {
        match self {
            TypePos::Void | TypePos::CRet => true,
            TypePos::Value | TypePos::CParam | TypePos::Field => false,
        }
    }

    /// The positions of the params and the return of a fn type written at `self`: a callback type
    /// in a C signature stays at the C boundary; anywhere else it is a program fn type.
    pub(super) fn func_parts(self) -> (TypePos, TypePos) {
        match self {
            TypePos::CParam | TypePos::CRet => (TypePos::CParam, TypePos::CRet),
            TypePos::Value | TypePos::Void | TypePos::Field => (TypePos::Value, TypePos::Void),
        }
    }
}

/// TICKET-262 — the text of the owner-rule-1 error for the width type `name`.
pub(super) fn width_outside_c_msg(name: &str) -> String {
    format!(
        "'{name}' describes the C boundary only: write it in an extern fn signature or a struct \
         field, and use `{}` here",
        ffi_width_ty(name).scalar()
    )
}

impl Checker {
    /// The seed at the decl-site default resolves a provider's binders only where the hint actually
    /// REACHES — it is a single slot, drained by the first consumer, so in `idl(mkl())` the outer
    /// call takes it and the inner provider keeps spelling its own `Z`. This is the compare-time
    /// backstop: pass the default's final inferred type as `unify`'s PATTERN, which treats every
    /// `Ty::Param` on that side as a variable and so freshens ALL remaining binders in one step — no
    /// gensym pass, no list to enumerate, no site to miss. One-directional: nothing on the DECLARED
    /// side is rewritten, so a genuinely wrong default still fails the assignability check.
    ///
    /// **It is not redundant with the seed, and was briefly deleted as if it were.** The two cover
    /// different halves: the seed makes the binding exist INSIDE the call, which is the only way
    /// `enforce_bounds` can see it; this makes the final COMPARISON independent of spelling even when
    /// the hint never arrived. Measured with only the seed: `xs: List[G[T]] = idl(mkl())` was `ok`
    /// for `mkl[T]` and *default value for parameter 'xs': expected List[G[T]], found List[G[Z]]* for
    /// `mkl[Z]`. The battery that pronounced this redundant contained only BARE provider calls, where
    /// the hint always arrives — `widening-untested-by-its-own-suite`, applied to a deletion.
    pub(super) fn resolve_default_binders(&self, declared: &Ty, actual: Ty) -> Ty {
        let mut map: HashMap<String, Ty> = HashMap::new();
        unify(&actual, declared, &mut map);
        if map.is_empty() {
            actual
        } else {
            subst(&actual, &map)
        }
    }

    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type. The decl's
    /// generic `type_params` are installed (so `T` in annotations resolves to `Ty::Param("T")`) and
    /// each declared bound is validated against the known protocols.
    /// TICKET-250 — the one answer to "is this parameter the receiver". `Ok(Some(owner))` when
    /// `params[0]` is named `self`, has no default, and is un-annotated or annotated with exactly
    /// `owner` (the struct, enum or protocol the method is declared in). `Ok(None)` when no
    /// parameter is named `self`. Every other parameter named `self` is the error.
    pub(super) fn receiver(
        &mut self,
        params: &[Param],
        owner: Option<&Ty>,
        span: Span,
    ) -> Result<Option<Ty>, String> {
        let mut found = None;
        for (i, p) in params.iter().enumerate() {
            if !p.is_self() {
                continue;
            }
            let ok = match owner {
                Some(o) if i == 0 && p.default.is_none() => {
                    p.ty.as_ref()
                        .is_none_or(|t| self.resolve_type(t, span) == *o)
                }
                _ => false,
            };
            if !ok {
                return Err(
                    "'self' is only allowed as the first parameter of a method declared in a \
                     struct, enum or protocol body"
                        .to_string(),
                );
            }
            found = owner.cloned();
        }
        Ok(found)
    }

    /// [`Checker::receiver`], reporting its error at `span`.
    pub(super) fn receiver_or_report(
        &mut self,
        params: &[Param],
        owner: Option<&Ty>,
        span: Span,
    ) -> Option<Ty> {
        match self.receiver(params, owner, span) {
            Ok(r) => r,
            Err(e) => {
                self.error(span, e);
                None
            }
        }
    }

    pub(super) fn fn_sig(&mut self, decl: &FnDecl, span: Span, owner: Option<&Ty>) -> FnSig {
        // A method's own `[U]` may not reuse a type parameter already in scope (the struct's `[T]`):
        // it would be a confusing double-binding. `self.type_params` is empty for a free fn, so this
        // only fires for methods declared inside a generic struct.
        for tp in &decl.type_params {
            if self.type_params.contains_key(&tp.name) {
                self.error(
                    span,
                    format!(
                        "method type parameter '{}' shadows the struct's type parameter '{}'",
                        tp.name, tp.name
                    ),
                );
            }
        }
        // Fold any `where T: Bound` clauses into the matching declared type parameter's bounds, so the
        // existing generic-call bound-enforcement path (`infer_generic_call` → `enforce_bounds`) handles
        // them with zero new machinery. A where entry naming the method's OWN `[U]` merges as above.
        // A where entry naming the ENCLOSING struct/enum's own type param (in `self.type_params`,
        // not `decl.type_params`) is a CONDITIONAL METHOD: it constrains the RECEIVER's concrete type
        // arg, callable only when that arg satisfies the bound (mirrors native `List[T].sort`'s
        // `where T: Comparable`). Recorded on `receiver_bounds` → carried on the returned `FnSig`'s
        // `where_bounds`, enforced at the method-call dispatch arms (struct/enum) against the
        // receiver's substitution — exactly like the native `Ty::List` arm. A where entry naming NEITHER
        // an own param NOR a receiver param is still an error (e.g. a free fn's `where Q:`).
        let mut merged = decl.type_params.clone();
        let mut receiver_bounds: Vec<TypeParam> = Vec::new();
        for w in &decl.where_bounds {
            match merged.iter_mut().find(|tp| tp.name == w.name) {
                // Dedup: a bound repeated across `[T: X]` and `where T: X` must not double the
                // enforcement (else a failing call emits the identical "does not satisfy X" twice).
                Some(tp) => {
                    for b in &w.bounds {
                        if !tp
                            .bounds
                            .iter()
                            .any(|e| e.name == b.name && e.args == b.args)
                        {
                            tp.bounds.push(b.clone());
                        }
                    }
                }
                // Not one of the method's own `[U]` params. If it names the enclosing type's param
                // (present in `self.type_params` while a method sig is built inside a generic type),
                // record it as a receiver-bound; otherwise it is genuinely unknown → error.
                None if self.type_params.contains_key(&w.name) => {
                    // DEDUP against the enclosing param's DECLARED bounds (`struct Box[T: Bound]`):
                    // the static-dispatch path (`infer_static_call`) enforces BOTH the enclosing
                    // param's bounds (`tps`) AND the method's receiver `where_bounds`, so a bound
                    // named in both would fire the identical "does not satisfy" diagnostic twice.
                    // Keep only the bounds the enclosing param does NOT already declare (the
                    // still-in-scope enclosing param is entered by the type's hoist). If nothing is
                    // novel, record no receiver-bound at all (the declared bound already covers it).
                    // Compared RESOLVED (TICKET-202): the declared side is already a `TyBound`.
                    let declared = self.type_params.get(&w.name).cloned().unwrap_or_default();
                    let resolved = self.resolve_bounds(std::slice::from_ref(w), span);
                    let novel: Vec<Bound> = w
                        .bounds
                        .iter()
                        .zip(&resolved[0].bounds)
                        .filter(|(_, r)| !declared.contains(r))
                        .map(|(b, _)| b.clone())
                        .collect();
                    if !novel.is_empty() {
                        receiver_bounds.push(TypeParam {
                            name: w.name.clone(),
                            name_span: w.name_span,
                            bounds: novel,
                        });
                    }
                }
                None => self.error(
                    span,
                    format!("unknown type parameter '{}' in where-clause", w.name),
                ),
            }
        }
        let saved = self.declare_type_params(&merged, span);
        // Validate a conditional method's receiver-bound protocol names too (the enclosing type's
        // param is already in scope via the hoist's `declare_type_params`).
        self.check_param_bounds(&receiver_bounds, span);
        let receiver = self.receiver_or_report(&decl.params, owner, span);
        let params: Vec<Ty> = decl
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| match &p.ty {
                _ if i == 0 && receiver.is_some() => receiver.clone().unwrap_or(Ty::Unknown),
                // A variadic param `...xs: T` collapses to the slot type `List[T]` — the desugar pass
                // sweeps the surplus positionals into a `List[T]` literal, so the checker sees an
                // ordinary `List[T]` argument for this slot.
                Some(t) if p.is_variadic => Ty::List(Box::new(self.resolve_type(t, span))),
                Some(t) => self.resolve_type(t, span),
                None => {
                    self.error(
                        span,
                        format!("parameter '{}' needs a type annotation", p.name),
                    );
                    Ty::Unknown
                }
            })
            .collect();
        // No `-> T`: a block body returns nothing. An inline expression body takes the type of its
        // one expression, which `infer_returns` writes. This is the one place that decides it.
        let ret = decl
            .ret
            .as_ref()
            .map(|t| self.resolve_ret_type(t, span))
            .unwrap_or(if decl.inline_expr_body {
                Ty::Unknown
            } else {
                Ty::Nil
            });
        // TICKET-202: bounds resolve HERE, in the declaration's scope (the receiver's params are
        // entered by the type hoist), and never again at a use site.
        let type_params = self.resolve_bounds(&merged, span);
        let where_bounds = self.resolve_bounds(&receiver_bounds, span);
        self.exit_type_params(saved);
        // STATIC classification: a method with no receiver is a static (associated) method,
        // dispatched `Type.method(args)`. `Checker::receiver` is the one answer. This flag is
        // consulted only when the sig is reached as a struct/enum method; for a free fn it is
        // meaningless (free fns are never reached via the method maps).
        let is_static = receiver.is_none();
        // Surface-only labels for a value form of this fn: the param NAMES (parallel to `params`),
        // with `self` contributing `None` (a value keyword call never names the receiver).
        let labels: Vec<Option<String>> = decl
            .params
            .iter()
            .map(|p| {
                if p.is_self() {
                    None
                } else {
                    Some(p.name.clone())
                }
            })
            .collect();
        let wparams = self.witness_reqs(&self.witness_params_of(decl), &type_params);
        FnSig {
            // Trailing defaulted parameters are filled by the CALLEE's own prologue, so a call may
            // omit them. Same predicate the compiler sizes `Proto::min_arity` with — and because
            // this reading DEPENDS on `wparams`, which the hoist fixpoint re-derives (non-monotone:
            // it adds and removes charges), the fixpoint re-derives this alongside it. Neither may
            // be written without the other.
            min_params: crate::ast::min_callable_params(&decl.params, !wparams.is_empty()),
            labels,
            params,
            ret,
            // TICKET-027: a stored bound must cross a module boundary keyed, not bare, so a whole-module
            // import re-spells it correctly against the importer's own `Checker::protocols`.
            type_params,
            // A method's OWN `[U]` where-bounds are merged into `type_params` above (enforced via the
            // ordinary generic-call path). `where_bounds` carries only CONDITIONAL-METHOD receiver
            // bounds — a `where` naming the enclosing type's param — enforced at the struct/enum
            // method-call dispatch arms against the receiver's concrete type arg (mirrors native sigs,
            // e.g. `List[T].sort`'s `where T: Comparable`). Empty for a free fn or a plain method.
            where_bounds,
            is_static,
            doc: decl.doc.clone(),
            // M24 — computed HERE (the one site with the declaration's body in hand) so every
            // consumer reads this one answer instead of re-deriving it.
            witness_params: wparams,
            variadic: decl.params.iter().position(|p| p.is_variadic),
            slots: Some(self.decl_param_slots(decl, span.file, 0)),
            native: None,
            c_variadic: false,
        }
    }

    /// The call-binding slots of `decl`, named as `desugar::synthesize_providers` named its
    /// providers: owner `f` for a free fn, `S.m` for a method of the host `current_self_ty`, with
    /// `nth` the count of earlier same-named methods ([`crate::base::provider_name::method_owner`]).
    pub(super) fn decl_param_slots(
        &self,
        decl: &FnDecl,
        file: u32,
        nth: usize,
    ) -> Vec<crate::desugar::SlotSpec> {
        let (owner, bare, host_tps) = match &self.current_self_ty {
            Some(Ty::Struct(k, a) | Ty::Enum(k, a)) => {
                let bare = k.rsplit("::").next().unwrap_or(k.as_str()).to_string();
                let tps: Vec<String> = a
                    .iter()
                    .filter_map(|t| match t {
                        Ty::Param(p) => Some(p.clone()),
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
                        | Ty::Var(_) => None,
                    })
                    .collect();
                let owner = crate::base::provider_name::method_owner(&bare, &decl.name, nth);
                (owner, Some(bare), tps)
            }
            _ => (decl.name.clone(), None, Vec::new()),
        };
        let self_ty = bare.as_deref().filter(|_| host_tps.is_empty());
        crate::desugar::param_slots(decl, file, &owner, self_ty, &host_tps)
    }
}

/// The Chezzi type of a `std.ffi` width name (a `native type NAME = int|float` decl of
/// `std/ffi.chz`): `Ty::Width` carrying its `cffi::width_ctype` — a slot tag whose value is `float`
/// for `float32` and `int` for every integer width (`Ty::scalar`). The one builder of `Ty::Width`:
/// the qualified, bare and alias-export resolvers all call it, so an alias's `Ty` agrees with its
/// `CType`.
pub(super) fn ffi_width_ty(name: &str) -> Ty {
    Ty::Width(
        crate::native::cffi::width_ctype(name).expect("ffi_width_ty: a declared std.ffi width"),
    )
}

/// TICKET-201 — each method of type `owner` as a struct-body binder for
/// [`Checker::check_default_scope`]: `(name, name span, owner)`.
pub(super) fn method_binders(methods: &[FnDecl], owner: &str) -> Vec<(String, Span, String)> {
    methods
        .iter()
        .map(|m| (m.name.clone(), m.name_span, owner.to_string()))
        .collect()
}

/// M24-5b — the bare type NAME a dotted callee's head spells, peeling either type-level turbofish
/// carrier (`Enum[T]` and `E[T, U]` both parse as an `Index` over the name).
pub(super) fn bare_head_name(kind: &ExprKind) -> Option<&str> {
    match kind {
        ExprKind::Ident(n) => Some(n),
        ExprKind::Index { obj, .. } => match obj.kind.as_ident() {
            Some(n) => Some(n),
            None => None,
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
        | ExprKind::Field { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => None,
    }
}

/// M24-5b — the `(module, type)` a dotted callee's head spells, peeling the same turbofish carrier
/// (`module.Enum[T]` is an `Index` over the `Field`).
pub(super) fn qualified_head_names(kind: &ExprKind) -> Option<(&str, &str)> {
    let field = match kind {
        ExprKind::Index { obj, .. } => &obj.kind,
        k @ ExprKind::Int(_)
        | k @ ExprKind::Float(_)
        | k @ ExprKind::Str(_)
        | k @ ExprKind::Interp(_)
        | k @ ExprKind::Bytes(_)
        | k @ ExprKind::RawStr(_)
        | k @ ExprKind::Bool(_)
        | k @ ExprKind::NoneLit
        | k @ ExprKind::Pass
        | k @ ExprKind::Ident(_)
        | k @ ExprKind::List(..)
        | k @ ExprKind::Tuple(_)
        | k @ ExprKind::Map(_)
        | k @ ExprKind::Set(_)
        | k @ ExprKind::Comprehension { .. }
        | k @ ExprKind::Unary { .. }
        | k @ ExprKind::Binary { .. }
        | k @ ExprKind::Compare { .. }
        | k @ ExprKind::Range { .. }
        | k @ ExprKind::Call { .. }
        | k @ ExprKind::Field { .. }
        | k @ ExprKind::Try(_)
        | k @ ExprKind::OptChain { .. }
        | k @ ExprKind::NullCoalesce { .. }
        | k @ ExprKind::Closure { .. }
        | k @ ExprKind::Match { .. }
        | k @ ExprKind::IfElse { .. }
        | k @ ExprKind::Recover(..)
        | k @ ExprKind::ElseGuard { .. } => k,
    };
    match field {
        ExprKind::Field { obj, name, .. } => match obj.kind.as_ident() {
            Some(m) => Some((m, name)),
            None => None,
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
    }
}

impl Checker {
    /// The `param_name_mismatch` verdict of hook `sig` against the reserved `protocol`'s own
    /// `method` requirement; `None` when the protocol declares no such method.
    fn hook_name_mismatch(
        &self,
        protocol: &str,
        method: &str,
        sig: &FnSig,
    ) -> Option<(usize, String, String)> {
        let pinfo = self.protocol_shape(protocol)?;
        let (_, req) = pinfo.methods.iter().find(|(n, _)| n == method)?;
        param_name_mismatch(req, sig)
    }

    /// TICKET-197 — why an operator or hook on `ty` failed, when the reason is a misnamed
    /// parameter: ` (method 'add' parameter 1 is named 'o', but Add declares 'other')` for a
    /// struct or enum whose `hook` method misnames a parameter of its protocol, else empty.
    pub(super) fn hook_name_note(&self, ty: &Ty, hook: Hook) -> String {
        let (protocol, method) = (hook.protocol(), hook.method());
        // A conditional hook whose `where` bound this instantiation fails: name the bound.
        if let Err(HookMiss::Bound(why)) = self.hook_verdict(ty, hook) {
            return format!(" ({why})");
        }
        let methods = match ty {
            Ty::Struct(name, _) => self.structs.get(name).map(|info| &info.methods),
            Ty::Enum(name, _) => self.enum_methods_of(name),
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
            | Ty::Var(_) => None,
        };
        methods
            .and_then(|ms| ms.get(method))
            .and_then(|sig| self.hook_name_mismatch(protocol, method, sig))
            .map(|mm| format!(" ({})", name_mismatch_text(protocol, method, mm)))
            .unwrap_or_default()
    }

    /// [`Self::hook_name_note`] for iteration: the `iter` hook's reason, else the `next` hook's.
    pub(super) fn iter_hook_note(&self, ty: &Ty) -> String {
        let note = self.hook_name_note(ty, Hook::Iter);
        if note.is_empty() {
            self.hook_name_note(ty, Hook::Next)
        } else {
            note
        }
    }
}

/// Does the SYNTACTIC type `ty` mention the type-parameter name `t` anywhere? A plain occurs-check
/// over the AST type — no resolution, so it is usable before/independently of `resolve_type`. Read by
/// [`Checker::ty_param_in_sig`] and by [`Checker::member_call_forwards_a_witness`], which asks it of a
/// member call's TYPE ARGUMENTS as well as of a parameter's annotation.
pub(super) fn ty_mentions(ty: &crate::ast::Type, t: &str) -> bool {
    match ty {
        crate::ast::Type::Nil(_) => false,
        crate::ast::Type::Named { name, .. } => name == t,
        crate::ast::Type::Qualified { args, .. } => args.iter().any(|a| ty_mentions(a, t)),
        crate::ast::Type::Generic(head, args, _) => {
            head == t || args.iter().any(|a| ty_mentions(a, t))
        }
        crate::ast::Type::Func { params, ret, .. } => {
            params.iter().any(|p| ty_mentions(p, t)) || ty_mentions(ret, t)
        }
        crate::ast::Type::Tuple(items) => items.iter().any(|i| ty_mentions(i, t)),
    }
}

/// M24 — charge every static-bounded CANDIDATE (`cands`) that an explicit turbofish (`type_args`)
/// names, into `out`. Only candidates are matched, never every in-scope name: a turbofish naming a
/// concrete type is not a forward, and charging for one would cost the enclosing fn its
/// function-value position for nothing.
pub(super) fn turbofish_charges(
    type_args: &[crate::ast::Type],
    cands: &[String],
    out: &mut std::collections::HashSet<String>,
) {
    out.extend(
        cands
            .iter()
            .filter(|t| type_args.iter().any(|ty| ty_mentions(ty, t)))
            .cloned(),
    );
}
