// checker::sig — split out of checker/mod.rs. `super::*` == the `checker` module.
// Function signatures and return-type inference passes.

use super::*;

impl Checker {
    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type. The decl's
    /// generic `type_params` are installed (so `T` in annotations resolves to `Ty::Param("T")`) and
    /// each declared bound is validated against the known protocols.
    pub(super) fn fn_sig(&mut self, decl: &FnDecl, span: Span) -> FnSig {
        // A free fn's or method's own type param `[U]` may not be named after a reserved builtin type
        // (`fn id[int]`). This is the SOLE funnel for free fns AND struct/enum/newtype methods (it is
        // hoist-only, so it fires once per decl — a method's `[U]` is checked here while the struct's
        // `[T]` is checked at the struct hoist, no overlap).
        self.reject_reserved_type_params(&decl.type_params);
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
        // A where entry naming the ENCLOSING struct/enum/newtype's own type param (in `self.type_params`,
        // not `decl.type_params`) is a CONDITIONAL METHOD: it constrains the RECEIVER's concrete type
        // arg, callable only when that arg satisfies the bound (mirrors native `List[T].sort`'s
        // `where T: Comparable`). Recorded on `receiver_bounds` → carried on the returned `FnSig`'s
        // `where_bounds`, enforced at the method-call dispatch arms (struct/enum/newtype) against the
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
                    let declared = self.type_params.get(&w.name).cloned().unwrap_or_default();
                    let novel: Vec<Bound> = w
                        .bounds
                        .iter()
                        .filter(|b| {
                            !declared
                                .iter()
                                .any(|e| e.name == b.name && e.args == b.args)
                        })
                        .cloned()
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
        let saved = self.enter_type_params(&merged);
        for tp in &merged {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
        // Validate a conditional method's receiver-bound protocol names too (the enclosing type's
        // param is already in scope via the hoist's `enter_type_params`).
        for tp in &receiver_bounds {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
        let params: Vec<Ty> = decl
            .params
            .iter()
            .map(|p| match &p.ty {
                // A `ref T` param is a `Ref[T]` box (its reads/writes were lowered to `.get()`/`.set()`
                // by desugar). `check_ref_ty` rejects a non-boxable pointee (e.g. a bare generic).
                Some(t) if p.is_ref => {
                    self.check_ref_ty(t, span);
                    self.resolve_type(
                        &Type::Generic("Ref".to_string(), vec![t.clone()], Span::default()),
                        span,
                    )
                }
                // A variadic param `...xs: T` collapses to the slot type `List[T]` — the desugar pass
                // sweeps the surplus positionals into a `List[T]` literal, so the checker sees an
                // ordinary `List[T]` argument for this slot.
                Some(t) if p.is_variadic => Ty::List(Box::new(self.resolve_type(t, span))),
                Some(t) => self.resolve_type(t, span),
                None if p.name == "self" => Ty::Unknown, // bound in check_fn_body
                None => {
                    self.error(
                        span,
                        format!("parameter '{}' needs a type annotation", p.name),
                    );
                    Ty::Unknown
                }
            })
            .collect();
        // No `-> T`: leave the return as `Unknown` for now — `infer_returns` (run after `hoist`)
        // walks the body and replaces it with the inferred type. `Unknown` is the safe placeholder
        // any *other* function's inference sees in the meantime (forward refs degrade silently
        // rather than to a confidently-wrong `Nil`).
        let ret = decl
            .ret
            .as_ref()
            .map(|t| self.resolve_type(t, span))
            .unwrap_or(Ty::Unknown);
        self.exit_type_params(saved);
        // STATIC classification (the "no self ⇒ static" rule): a method whose first param is NOT
        // named `self` — or which has no params at all — is a static (associated) method, dispatched
        // `Type.method(args)`. This flag is consulted only when the sig is reached as a struct/enum
        // method; for a free fn it is meaningless (free fns are never reached via the method maps).
        let is_static = decl.params.first().is_none_or(|p| p.name != "self");
        // Surface-only labels for a value form of this fn: the param NAMES (parallel to `params`),
        // with `self` contributing `None` (a value keyword call never names the receiver).
        let labels: Vec<Option<String>> = decl
            .params
            .iter()
            .map(|p| {
                if p.name == "self" {
                    None
                } else {
                    Some(p.name.clone())
                }
            })
            .collect();
        FnSig {
            min_params: params.len(),
            labels,
            params,
            ret,
            type_params: merged,
            // A method's OWN `[U]` where-bounds are merged into `type_params` above (enforced via the
            // ordinary generic-call path). `where_bounds` carries only CONDITIONAL-METHOD receiver
            // bounds — a `where` naming the enclosing type's param — enforced at the struct/enum/newtype
            // method-call dispatch arms against the receiver's concrete type arg (mirrors native sigs,
            // e.g. `List[T].sort`'s `where T: Comparable`). Empty for a free fn or a plain method.
            where_bounds: receiver_bounds,
            is_static,
            doc: decl.doc.clone(),
            variadic: decl.params.iter().position(|p| p.is_variadic),
        }
    }

    /// Pass-1.5: for every function/method that omitted `-> T`, infer its return type from the
    /// body and overwrite the provisional `Unknown` left by `fn_sig`. Runs after `hoist`, so all
    /// type names, variants, and (provisional) function sigs are already visible to the inference.
    ///
    /// Inference is ORDER-INDEPENDENT: a single source-order pass would bail to `Unknown` whenever
    /// the deciding return is a call to a not-yet-inferred function (a forward reference or mutual
    /// recursion), leaking an unsound permissive `Unknown` into a typed slot. Instead this runs the
    /// per-pass walk (`infer_returns_pass`) repeatedly to a FIXPOINT: each pass re-infers every
    /// un-annotated fn/method, and because a callee's resolved `FnSig.ret` is written back
    /// immediately, a later pass sees the earlier pass's resolutions. The iteration is MONOTONE — a
    /// pass only ever turns an `Unknown` ret into a concrete one (or detects a conflict via pass-2),
    /// and a concrete ret is never reverted to `Unknown` — so it converges. The cap
    /// (`un-annotated count + 1`) bounds the longest forward-ref resolution chain and guarantees
    /// termination on genuinely un-inferable cases (pure recursion / mutual recursion with no
    /// concrete base, where the ret stays `Unknown` forever). Such a residual `Unknown` stays
    /// permissive (same as the pre-fixpoint behavior) — it is NOT rejected here: a blanket
    /// "leftover Unknown ⇒ require annotation" check over-reaches, because a bare `Unknown` ret is
    /// also produced by non-recursive paths (e.g. `return x[0]` of an empty-collection literal) and
    /// by already-errored bodies. Rejecting the genuinely-un-inferable recursive case soundly needs
    /// call-graph cycle detection; tracked as a follow-up gap.
    pub(super) fn infer_returns(&mut self, stmts: &[Stmt]) {
        // Bound: each productive pass resolves at least one more `Unknown`→concrete; `+1` lets the
        // final pass confirm no change (the fixpoint). A non-productive pass breaks the loop early.
        let cap = self.count_uninferred(stmts) + 1;
        for _ in 0..cap {
            if !self.infer_returns_pass(stmts, false) {
                break;
            }
        }
        // FINALIZE: one last pass that folds ALL return branches (a top-level `Unknown` from a
        // forward-ref/recursive sibling is absorbed by `join_ret`), fills the `Result` E-slot default
        // (`Error`), and ERRORS on any residual un-inferable `Unknown` (an `Err`-only / `None`-only /
        // empty-`[]` return, or a genuinely baseless recursion). Kept SEPARATE from the fixpoint so
        // the passes above stay permissive: a callee's ret must be free to be `Unknown` mid-fixpoint
        // (it resolves on a later pass) without being prematurely rejected or E-defaulted.
        self.infer_returns_pass(stmts, true);
    }

    /// Count the un-annotated free fns + struct/enum methods that `infer_returns` infers — the
    /// fixpoint iteration bound. (Annotated decls are skipped by the pass, so they cannot extend the
    /// chain.)
    pub(super) fn count_uninferred(&self, stmts: &[Stmt]) -> usize {
        let mut n = 0;
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() => n += 1,
                StmtKind::Struct { methods, .. } | StmtKind::Enum { methods, .. } => {
                    n += methods.iter().filter(|m| m.ret.is_none()).count();
                }
                _ => {}
            }
        }
        n
    }

    /// One inference pass over every un-annotated fn/method. Re-infers each from the body (idempotent
    /// per the truncate-errors model in `infer_fn_ret`) and writes the result back into the stored
    /// `FnSig.ret` immediately, so a callee resolved earlier in THIS pass is already visible to a
    /// caller later in the pass. Returns `true` iff any stored ret changed (drives the fixpoint).
    pub(super) fn infer_returns_pass(&mut self, stmts: &[Stmt], finalize: bool) -> bool {
        let mut changed = false;
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() => {
                    let Some(sig) = self.functions.get(&decl.name).cloned() else {
                        continue;
                    };
                    let ret = self.infer_fn_ret(decl, None, &sig.params, finalize);
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
                        if m.ret.is_some() {
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
                            self.infer_fn_ret(m, Some(self_ty.clone()), &sig.params, finalize);
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
                        if m.ret.is_some() {
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
                            self.infer_fn_ret(m, Some(self_ty.clone()), &sig.params, finalize);
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
                _ => {}
            }
        }
        changed
    }

    /// Infer one function's return type by walking its body in inference mode: every `return`'s
    /// type is collected by `check_return` (with errors suppressed — pass 2 re-reports for real).
    /// The pick rule, in order:
    /// - first concrete non-`nil` return wins (pass 2 then validates the rest against it);
    /// - else, if any value-return was uncertain (`Unknown` — a forward ref to a not-yet-inferred
    ///   function, or a self-recursive call) → `Unknown` for THIS pass, so the function stays
    ///   permissive instead of producing spurious errors; the enclosing fixpoint (`infer_returns`)
    ///   then re-infers it on a later pass once the callee resolves;
    /// - else (only bare `return`s / no returns at all) → `nil` (void preserved).
    ///
    /// One pass is order-dependent (a call to a not-yet-inferred function yields `Unknown`), but
    /// `infer_returns` iterates this to a FIXPOINT, so the FINAL stored ret is order-independent: a
    /// forward-ref / mutually-recursive callee resolves on a later pass. Only a genuinely
    /// un-inferable function (no concrete base anywhere) stays `Unknown` after convergence — that
    /// residual stays permissive (not rejected; soundly rejecting it needs call-graph cycle
    /// detection — a follow-up).
    pub(super) fn infer_fn_ret(
        &mut self,
        decl: &FnDecl,
        self_ty: Option<Ty>,
        params: &[Ty],
        finalize: bool,
    ) -> Ty {
        let mark = self.errors.len();
        let saved_tps = self.enter_type_params(&decl.type_params);
        let saved_ret = std::mem::replace(&mut self.current_ret, Ty::Unknown);
        let saved_flag = std::mem::replace(&mut self.inferring_ret, true);
        let saved_rets = std::mem::take(&mut self.collected_rets);
        // A generator body's `yield`s must be legal (`in_generator`) and COLLECTED (`collected_yields`)
        // during inference; a non-generator resets both so a stray `yield` is diagnosed.
        let saved_ig = std::mem::replace(&mut self.in_generator, decl.is_generator);
        let saved_yields = std::mem::take(&mut self.collected_yields);
        self.push_scope();
        for (i, param) in decl.params.iter().enumerate() {
            let ty = if param.name == "self" {
                self_ty.clone().unwrap_or(Ty::Unknown)
            } else {
                params.get(i).cloned().unwrap_or(Ty::Unknown)
            };
            self.declare(&param.name, ty);
            if param.is_ref {
                self.declare_ref(&param.name);
            }
        }
        // An inline-expr body (`fn a(): <expr>`) implicitly returns its single expression, so its
        // type IS the inferred return (mirroring a closure body) — there is no `return` to collect.
        let inline_ret = if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            Some(self.infer(e))
        } else {
            for stmt in &decl.body {
                self.check_stmt(stmt);
            }
            None
        };
        self.pop_scope();
        let found = std::mem::replace(&mut self.collected_rets, saved_rets);
        let found_yields = std::mem::replace(&mut self.collected_yields, saved_yields);
        self.in_generator = saved_ig;
        self.inferring_ret = saved_flag;
        self.current_ret = saved_ret;
        self.exit_type_params(saved_tps);
        // Did the body inference itself emit an error (undefined name, bad call, …)? If so the real
        // diagnostic surfaces in pass 2, so a residual `Unknown`/conflict here is a CASCADE, not a
        // genuine un-inferable return — suppress the finalize error to avoid piling on.
        let body_had_err = self.errors.len() > mark;
        self.errors.truncate(mark); // discard inference-time errors; pass 2 re-reports them for real
        // A GENERATOR's return type is `Iterator[T]`, `T` inferred by strict-first-yield — NOT the
        // folded `return` branches (a generator's `return`s are bare, contributing only `Nil`). Route
        // to the dedicated helper before the value-return fold below.
        if decl.is_generator {
            return self.infer_generator_ret(found_yields, decl.name_span, finalize, body_had_err);
        }

        // Collect every return branch: an inline-expr body's single implicit return, else all the
        // `return`s (a bare `return` contributed `Nil`; no returns at all ⇒ an empty set ⇒ void).
        let branches: Vec<Ty> = match inline_ret {
            Some(t) => vec![t],
            None => found,
        };
        // Fold the branches with the JOIN function `J` (`join_ret`): a==b, the one int→float widen,
        // or slot-wise merge of a shared type-constructor; a top-level `Unknown` (forward ref /
        // recursion / cascade) is ABSORBED by the other side, so a recursive fn still resolves to its
        // concrete base during the fixpoint (matching the old first-concrete-wins timing). An empty
        // branch set is `Nil` (void). A conflict yields `Err((X, Y))`.
        let folded: Result<Ty, (Ty, Ty)> = {
            let mut iter = branches.into_iter();
            match iter.next() {
                None => Ok(Ty::Nil),
                Some(first) => iter.try_fold(first, |acc, b| self.join_ret(&acc, &b)),
            }
        };
        if !finalize {
            // Fixpoint pass: stay permissive. A conflict collapses to `Unknown` (suppressed; the
            // FINALIZE pass re-runs the fold and emits the real conflict diagnostic).
            return folded.unwrap_or(Ty::Unknown);
        }
        // FINALIZE pass: emit the conflict diagnostic, else fill the E-default / reject residual
        // un-inferable `Unknown`.
        match folded {
            Err((x, y)) => {
                if !body_had_err {
                    self.error(
                        decl.name_span,
                        format!(
                            "cannot infer return type: conflicting branches ({x} vs {y}); add a -> annotation"
                        ),
                    );
                }
                Ty::Unknown
            }
            Ok(t) => self.finalize_ret(&t, &decl.name, decl.name_span, body_had_err),
        }
    }

    /// The JOIN function `J` over two RETURN branch types. Pure (no `self`). Returns the merged type,
    /// or `Err((a, b))` on a genuine conflict (the caller renders `cannot infer return type:
    /// conflicting branches (a vs b)`). Rules, in order:
    /// 1. `a == b` → `a`.
    /// 2. a top-level `Unknown` is absorbed by the other side (a forward-ref / recursive / cascade
    ///    branch carries no information — it must not drag a concrete sibling to a conflict).
    /// 3. `{int, float}` → `float` (the ONE numeric widen — BARE SCALARS ONLY; it does NOT recurse
    ///    into type-arg slots, per `docs/spec.md` `float! = Ok(3)` already being a type error).
    /// 4. same type-constructor (Result/Option/List/Set/Map, or a same-name-same-arity Struct/Enum/
    ///    NewType) → MERGE SLOT-WISE via [`Self::join_slot`].
    /// 5. otherwise (incl. Nil-vs-value, two distinct structs) → CONFLICT. There is deliberately NO
    ///    common-supertype / protocol / `Any` search: a protocol return must be spelled explicitly.
    fn join_ret(&self, a: &Ty, b: &Ty) -> Result<Ty, (Ty, Ty)> {
        use Ty::*;
        if a == b {
            return Ok(a.clone());
        }
        let conflict = || (a.clone(), b.clone());
        match (a, b) {
            (Unknown, other) | (other, Unknown) => Ok(other.clone()),
            // NOTE: no `(Int, Float) -> Float` widen here. An inferred return type is NOT a widening
            // "sink" (spec.md: int->float widens only at an explicit sink — a typed binding/param/
            // `-> float` annotation, which emits `Op::CoerceFloat`). Inferring `float` from mixed
            // `return 3` / `return 4.0` branches would set the static type to float WITHOUT the
            // compiler emitting the coercion (compile_fn reads `decl.ret`, the annotation, not the
            // checker's inferred ret), leaving a runtime `int` under a `float` type — `x / 2` would
            // do integer division. So mixed int/float branches CONFLICT: annotate `-> float` (which
            // coerces correctly) to opt in.
            // Merge the T-slot (Ok payload) normally; merge the E-slot with `join_err_slot` — two
            // DIFFERENT `Err` payloads that BOTH satisfy `Error` do NOT conflict (they unify to the
            // uniform `Error` existential at finalize), but a non-`Error` payload keeps `join_slot`'s
            // equal-or-conflict semantics so a genuine mismatch is still reported. `fill_ret` decides
            // the final Error-default per-slot; a concrete E is honored via an explicit annotation.
            (Result(at, ae), Result(bt, be)) => Ok(Ty::Result(
                Box::new(Self::join_slot(at, bt).ok_or_else(conflict)?),
                Box::new(self.join_err_slot(ae, be).ok_or_else(conflict)?),
            )),
            (Option(x), Option(y)) => Ok(Ty::Option(Box::new(
                Self::join_slot(x, y).ok_or_else(conflict)?,
            ))),
            (List(x), List(y)) => Ok(Ty::List(Box::new(
                Self::join_slot(x, y).ok_or_else(conflict)?,
            ))),
            (Set(x), Set(y)) => Ok(Ty::Set(Box::new(
                Self::join_slot(x, y).ok_or_else(conflict)?,
            ))),
            (Map(k1, v1), Map(k2, v2)) => Ok(Ty::Map(
                Box::new(Self::join_slot(k1, k2).ok_or_else(conflict)?),
                Box::new(Self::join_slot(v1, v2).ok_or_else(conflict)?),
            )),
            (Struct(n1, a1), Struct(n2, a2)) if n1 == n2 && a1.len() == a2.len() => Ok(Ty::Struct(
                n1.clone(),
                Self::join_slots(a1, a2).ok_or_else(conflict)?,
            )),
            (Enum(n1, a1), Enum(n2, a2)) if n1 == n2 && a1.len() == a2.len() => Ok(Ty::Enum(
                n1.clone(),
                Self::join_slots(a1, a2).ok_or_else(conflict)?,
            )),
            (NewType(n1, a1), NewType(n2, a2)) if n1 == n2 && a1.len() == a2.len() => Ok(
                Ty::NewType(n1.clone(), Self::join_slots(a1, a2).ok_or_else(conflict)?),
            ),
            _ => Err(conflict()),
        }
    }

    /// Slot merge `S` for a single type-arg position INSIDE a shared constructor. `None` = conflict.
    /// One side `Unknown` → the concrete other (partial `Ok`/`Err`/`Some`/`[]` branches fill each
    /// other's slots); both `Unknown` → `Unknown` (finalize handles the residual). Both concrete →
    /// must be EQUAL else conflict: widening does NOT apply inside payloads/elements (`int` vs `float`
    /// here CONFLICTS), per `docs/spec.md` (`float! = Ok(3)` is already a type error). No recursion —
    /// a nested same-ctor mismatch (`Some(Ok(5))` vs `Some(Err("x"))`) conflicts rather than merges.
    fn join_slot(a: &Ty, b: &Ty) -> Option<Ty> {
        if a.is_unknown() {
            return Some(b.clone());
        }
        if b.is_unknown() {
            return Some(a.clone());
        }
        if a == b { Some(a.clone()) } else { None }
    }

    /// Merge two inferred `Result` **error slots**. Like [`Self::join_slot`] (equal → keep; one
    /// `Unknown` → the other), EXCEPT two DIFFERENT concrete payloads that BOTH satisfy the `Error`
    /// protocol merge to `Unknown` — `fill_ret` then unifies them to the uniform `Error` existential,
    /// so branches returning distinct *error* types (`Err(EA())` vs `Err(EB())`) don't spuriously
    /// conflict. A pair where at least one side is a non-`Error` concrete keeps `join_slot`'s strict
    /// equal-or-conflict rule (a real type mismatch is still reported; forcing `Error` there would be
    /// unsound). Equal concretes are kept as-is — `fill_ret` decides Error-defaulting per slot.
    fn join_err_slot(&self, a: &Ty, b: &Ty) -> Option<Ty> {
        if a == b {
            return Some(a.clone());
        }
        if a.is_unknown() {
            return Some(b.clone());
        }
        if b.is_unknown() {
            return Some(a.clone());
        }
        if self.assignable(&Ty::error_proto(), a) && self.assignable(&Ty::error_proto(), b) {
            return Some(Ty::Unknown);
        }
        None
    }

    /// Slot-merge two equal-length type-arg lists position-wise; `None` if any slot conflicts.
    fn join_slots(a: &[Ty], b: &[Ty]) -> Option<Vec<Ty>> {
        a.iter()
            .zip(b)
            .map(|(x, y)| Self::join_slot(x, y))
            .collect()
    }

    /// Infer an un-annotated generator's return type as `Iterator[T]` where `T` is the type of the
    /// FIRST `yield` (strict-first-yield — chosen over a JOIN so no int->float coercion is silently
    /// introduced at a `yield`, which has no `CoerceFloat`; pass-2 `check_yield` validates the rest of
    /// the yields against this `T`). On the FINALIZE pass, a residual un-inferable `Unknown` in the
    /// element (an empty generator whose only `yield` is `[]`, or one that reached no `yield` at all)
    /// is a clear ERROR — never a silent `Iterator[Unknown]` leak (the residual-Unknown type-check
    /// bypass class). A `body_had_err` cascade suppresses that diagnostic (the real error already fired
    /// in pass 2), mirroring `finalize_ret`.
    pub(super) fn infer_generator_ret(
        &mut self,
        yields: Vec<Ty>,
        span: Span,
        finalize: bool,
        body_had_err: bool,
    ) -> Ty {
        let elem = yields.into_iter().next().unwrap_or(Ty::Unknown);
        if !finalize {
            // Fixpoint pass: stay permissive — a first yield that is `Unknown` (forward-ref callee /
            // recursion) resolves on a later pass. Only the finalize pass rejects a residual.
            return Ty::Struct("Iterator".to_string(), vec![elem]);
        }
        let mut bad = false;
        let filled = self.fill_ret(&elem, &mut bad);
        if bad && !body_had_err {
            self.error(
                span,
                "cannot infer generator element type; annotate the return type as `Iterator[T]`"
                    .to_string(),
            );
        }
        Ty::Struct("Iterator".to_string(), vec![filled])
    }

    /// FINALIZE a folded return type after the fixpoint converges: default the `Result` E-slot to the
    /// `Error` protocol when it is `Unknown` or its payload satisfies `Error` (matching the `T!` /
    /// `Result[T]` shorthand — a concrete non-`Error` payload is preserved; a deliberate concrete E
    /// needs an explicit annotation) and REJECT any OTHER residual `Unknown` (top-level, a `Result`
    /// T-slot, an `Option` T-slot, a List/Set/Map element/key/value, or a Struct/Enum/NewType
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

    /// Recursive helper for [`Self::finalize_ret`]: rebuild `t` defaulting a `Result` E-slot to the
    /// `Error` protocol WHEN it is `Unknown` or its payload satisfies `Error` (a concrete non-`Error`
    /// payload is preserved — see the `Ty::Result` arm), and flagging (`*bad = true`) every OTHER
    /// residual `Unknown`. `Ty::Param` and all leaf types pass through unchanged.
    fn fill_ret(&self, t: &Ty, bad: &mut bool) -> Ty {
        match t {
            Ty::Unknown => {
                *bad = true;
                Ty::Unknown
            }
            // An inferred `Result` E-slot defaults to the `Error` protocol (the `T!` / single-arg
            // `Result[T]` semantics) when it is un-pinned (`Unknown`) OR the pinned payload actually
            // SATISFIES `Error` — so the common `Err("msg")` / custom-error branches unify to the
            // uniform `Error` existential. A concrete E that does NOT satisfy `Error` (a struct with
            // no `message`, or `int`) is PRESERVED — forcing it to `Error` would launder a non-Error
            // value into the `Error` existential (the pass-2 return check / a downstream method-call
            // check then rejects any Error-method use soundly). A deliberate concrete non-`Error` E
            // needs an explicit `-> Result[T, E]` annotation (resolved by `resolve_type`, a separate
            // path). The T-slot is an ordinary value slot — a residual `Unknown` there is
            // un-inferable → `bad` (preserving the `Err`-only / `None`-only / `[]` leak guards).
            Ty::Result(a, b) => {
                let na = self.fill_ret(a, bad);
                let nb = if b.is_unknown() || self.assignable(&Ty::error_proto(), b) {
                    Ty::error_proto()
                } else {
                    self.fill_ret(b, bad)
                };
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
            Ty::NewType(n, a) => {
                Ty::NewType(n.clone(), a.iter().map(|x| self.fill_ret(x, bad)).collect())
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
            } => Ty::Func {
                params: params.iter().map(|x| self.fill_ret(x, bad)).collect(),
                ret: Box::new(self.fill_ret(ret, bad)),
                labels: labels.clone(),
            },
            Ty::BuiltinFn { params, ret } => Ty::BuiltinFn {
                params: params.iter().map(|x| self.fill_ret(x, bad)).collect(),
                ret: Box::new(self.fill_ret(ret, bad)),
            },
            // Leaf types carry no inner `Ty` (nothing to fill or flag). `Ty::Param` is intentionally
            // passed through UNTOUCHED — generic fns / the proto.rs HOF loop-back own it, an `Unknown`
            // it later resolves is not this pass's concern. Enumerated exhaustively (NO catch-all) so
            // any FUTURE `Ty` variant carrying an inner type fails to compile here instead of silently
            // re-opening the residual-`Unknown` leak.
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Param(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Ptr
            | Ty::Module(_) => t.clone(),
        }
    }

    /// Validate the pointee type of a `ref T` binding/param. `ref` lowers to a `Ref[T]` box, so the
    /// pointee must be a concrete (monomorphic) type: a bare in-scope generic parameter is rejected
    /// (use a first-class `Ref[T]` field/param for a generic box). Other unboxable shapes do not arise
    /// — the parser already bars `ref` from collection-element / return / field positions.
    pub(super) fn check_ref_ty(&mut self, t: &Type, span: Span) {
        if let Type::Named { name: n, .. } = t
            && self.type_params.contains_key(n)
        {
            self.error(
                span,
                format!(
                    "`ref {n}` is not allowed: a `ref` binding cannot point at the generic type parameter '{n}' — use a first-class `Ref[{n}]` instead"
                ),
            );
        }
    }

    /// Resolve an AST `Type` annotation into a checker `Ty`, reporting unknown type names.
    /// The "unknown type T" message, with a module-scoped import hint when `T` is declared by some
    /// (un-imported) module: a type is private to its declaring module and must be imported. Picks
    /// the first declaring module in graph (deps-first) order.
    pub(super) fn unknown_type_msg(&self, n: &str) -> String {
        if let Some(mods) = self.types_by_name.get(n)
            && let Some(m) = mods.first()
        {
            format!("unknown type '{n}'; import it from {m} (`import {n} from {m}`)")
        } else {
            format!("unknown type '{n}'")
        }
    }

    /// True iff the bare runtime concurrency ctor/TYPE name `n` (`Shared`/`RwShared`/`Atomic`/
    /// `Executor`) is usable in the current module: either this module imported it from
    /// `std.concurrency`, or we're inside a privileged stdlib module (std/* may use the four bare —
    /// `std/cancel.chz`, `std/concurrency/collection.chz`). The four ALSO stay reserved names (can't be
    /// shadowed by a user `struct`); this gate is the SEPARATE "must import to USE" requirement.
    pub(super) fn concurrency_licensed(&self, n: &str) -> bool {
        self.imported_concurrency.contains(n) || self.current_module_is_stdlib
    }

    /// True iff the opcode-backed `timer` builtin name `n` is usable in the current module: either this
    /// module imported it from `std.time` (whole-module or per-name), or we're inside a privileged
    /// stdlib module (std/* may call `timer` bare — e.g. `std/cancel.chz`). `timer` ALSO stays a
    /// reserved name (can't be shadowed by a user `struct`/`fn`); this gate is the SEPARATE "must import
    /// to USE" requirement.
    pub(super) fn time_licensed(&self, n: &str) -> bool {
        self.imported_time.contains(n) || self.current_module_is_stdlib
    }

    /// True iff the std.net TCP handle TYPE name `n` (`Socket`/`Listener`) is usable in the current
    /// module: either this module imported it from `std.net` (whole-module or per-name), or we're
    /// inside a privileged stdlib module (std/* may use the two bare). The two ALSO stay reserved names
    /// (can't be shadowed by a user `struct`); this gate is the SEPARATE "must import to USE" requirement.
    pub(super) fn net_licensed(&self, n: &str) -> bool {
        self.imported_net.contains(n) || self.current_module_is_stdlib
    }

    /// True iff the std.ffi type-license NAME `n` (the opaque `ptr` handle or a fixed-width `int8..
    /// uint64`) is usable in the current module: either this module imported it from `std.ffi`
    /// (whole-module or per-name), or we're inside a privileged stdlib module (std/* may use them bare
    /// — e.g. `std/ffi.chz` itself, whose bodyless `native fn` sigs are written in terms of `ptr`).
    /// These ALSO stay reserved names; this gate is the SEPARATE "must import to USE" requirement,
    /// mirroring `concurrency_licensed`/`net_licensed`/`time_licensed`. NOTE: during the native-module
    /// harvest (`harvest_native_module`) `current_module_is_stdlib` is UNRELIABLE (harvest runs without
    /// `begin_module`), so that path relies on the transient `imported_ffi_types` license instead.
    pub(super) fn ffi_type_licensed(&self, n: &str) -> bool {
        self.imported_ffi_types.contains(n) || self.current_module_is_stdlib
    }

    /// Map an opaque/native builtin TYPE name (one that lives ONLY in its owning std module's
    /// `sig.types` — reserved, so no user module can export it) to its builtin `Ty`, given already-
    /// resolved type args. This is the additive "make a Rust type reachable by qualified path"
    /// lever: `concurrency.Shared[int]` -> `Ty::shared(int)`, `net.Socket` -> `Ty::Socket`,
    /// `ffi.int32` -> `Ty::Int`, etc. Returns `None` for a `sig.types` name that is NOT a builtin
    /// TYPE in type position (`timer`, a function — the caller emits the "function, not a type"
    /// error). Arity is NOT validated here — the mutable `resolve_type` caller emits arity
    /// diagnostics; the read-only `resolve_qualified_ro` caller is permissive (no errors). Shared
    /// by both qualified resolvers so the mapping can never drift between them.
    pub(super) fn qualified_builtin_ty(&self, name: &str, args: &[Ty]) -> Option<Ty> {
        let one = || args.first().cloned().unwrap_or(Ty::Unknown);
        match name {
            "Shared" => Some(Ty::shared(one())),
            "RwShared" => Some(Ty::rwshared(one())),
            "Atomic" => Some(Ty::atomic(one())),
            "Executor" => Some(Ty::Executor),
            "Socket" => Some(Ty::Socket),
            "Listener" => Some(Ty::Listener),
            "ptr" => Some(Ty::Ptr),
            _ if crate::native::ffi::TYPE_NAMES.contains(&name) => Some(Ty::Int),
            _ => None,
        }
    }

    /// Expected type-argument arity for a builtin TYPE reached by qualified path (the generic
    /// concurrency boxes take exactly one; every other in-scope native type is non-generic).
    pub(super) fn qualified_builtin_arity(name: &str) -> usize {
        match name {
            "Shared" | "RwShared" | "Atomic" => 1,
            _ => 0,
        }
    }

    /// Does `n` name a protocol with ANY STATIC method requirement (first param NOT `self`), directly OR
    /// through a transitively-embedded protocol? Such a protocol (e.g. `Convert`'s static-ctor
    /// `convert(x: S) -> Self`, or a bundle `protocol MakeInt: Convert[int]` that EMBEDS it) is
    /// witnessable only by a STATIC method — a VALUE cannot invoke it — so it is usable ONLY as a generic
    /// bound `[T: P]`, never as a value-annotation type. Keys on the STRUCTURAL property (own + flattened
    /// embed requirements), so a future/user static-ctor protocol — and any bundle embedding one — is
    /// gated the same way. Ordinary instance-method protocols (all `is_static == false`) return `false`
    /// and stay usable as value existentials (`c: Container[int]`).
    pub(super) fn protocol_has_static_method(&self, n: &str) -> bool {
        let Some(p) = self.protocols.get(n) else {
            return false;
        };
        if p.methods.iter().any(|(_, s)| s.is_static) {
            return true;
        }
        // Flatten the transitive embed method set (cycle-capped, read-only) and check it too, so a
        // bundle that pulls in a static-ctor requirement via `embeds` is gated the same as one declaring
        // it directly.
        let mut path = vec![n.to_string()];
        let (required, _cyclic, _conflict) = self.flatten_embed_methods(&p.embeds, &mut path);
        required.values().any(|s| s.is_static)
    }

    /// Find the first STATIC-CTOR protocol embedded ANYWHERE in a (possibly nested) already-resolved
    /// `Ty` — directly (`Convert[int]`), or nested under a container / `Option` / tuple / `Result` /
    /// `Func` / struct-or-enum type arg. Read-only. This is the value-position gate for a `Ty` that
    /// arrives ALREADY RESOLVED — a cross-module type-alias body, computed by the `&self` read-only
    /// resolver (`resolve_ty_ro_d`) which cannot emit — so the mutable `resolve_type` arm gate never
    /// sees it. `resolve_type` itself recurses through the arm gate for every FRESHLY-resolved position,
    /// so this only needs to run at the pre-resolved-body seams.
    fn first_static_ctor_protocol(&self, ty: &Ty) -> Option<String> {
        match ty {
            Ty::Protocol(n, args) => {
                if self.protocol_has_static_method(n) {
                    Some(n.clone())
                } else {
                    args.iter().find_map(|a| self.first_static_ctor_protocol(a))
                }
            }
            Ty::List(t)
            | Ty::Set(t)
            | Ty::Option(t)
            | Ty::Channel(t)
            | Ty::Shared(t)
            | Ty::Atomic(t)
            | Ty::RwShared(t) => self.first_static_ctor_protocol(t),
            Ty::Map(k, v) | Ty::Result(k, v) => self
                .first_static_ctor_protocol(k)
                .or_else(|| self.first_static_ctor_protocol(v)),
            Ty::Tuple(ts) | Ty::Struct(_, ts) | Ty::Enum(_, ts) | Ty::NewType(_, ts) => {
                ts.iter().find_map(|t| self.first_static_ctor_protocol(t))
            }
            Ty::Func { params, ret, .. } => params
                .iter()
                .find_map(|p| self.first_static_ctor_protocol(p))
                .or_else(|| self.first_static_ctor_protocol(ret)),
            _ => None,
        }
    }

    /// If a resolved value-position `ty` embeds a static-ctor protocol (arriving pre-resolved from a
    /// cross-module alias body, so the `resolve_type` arm gate never fired), reject it with the SAME
    /// bound-only error the arms emit and return `Ty::Unknown`; otherwise return `ty` unchanged.
    fn reject_static_protocol_value(&mut self, ty: Ty, span: Span) -> Ty {
        if let Some(n) = self.first_static_ctor_protocol(&ty) {
            self.error(
                span,
                format!(
                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                ),
            );
            Ty::Unknown
        } else {
            ty
        }
    }

    pub(super) fn resolve_type(&mut self, t: &Type, span: Span) -> Ty {
        match t {
            Type::Named {
                name: n,
                span: name_span,
            } => {
                let resolved = match n.as_str() {
                    "int" => Ty::Int,
                    "float" => Ty::Float,
                    "bool" => Ty::Bool,
                    "str" => Ty::Str,
                    "bytes" => Ty::Bytes,
                    "bytearray" => Ty::ByteArray,
                    "nil" => Ty::Nil,
                    // A generic type parameter (`T`) or `Self`, in scope while checking a generic fn
                    // signature/body or a protocol method. Resolved BEFORE every reserved/module name
                    // below (ptr / owned_str / Executor / Shared|RwShared|Atomic / Socket / Listener)
                    // and before user structs/aliases, so an in-scope type param uniformly shadows them
                    // (e.g. `fn id[Executor](x: Executor)`), instead of being hijacked by the builtin
                    // arm or mis-erroring with a bogus import hint. Placed below the scalar primitives
                    // (int/float/bool/str/…) so those keep resolving to their scalar even when used as a
                    // type-param name (`fn id[int](x: int)` → x is `int`), preserving existing behavior.
                    _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                    // An opaque C-ABI pointer handle — the marshalling primitive for `extern "lib":`
                    // signatures. Like the fixed-width FFI integer names below, `ptr` is NOT a global
                    // builtin: it resolves only in a module that imported `std.ffi` (whole-module
                    // `import std.ffi` OR selective `import ptr from std.ffi`), OR via a LICENSED
                    // transparent alias body (`ffi_alias_ok`). Since extern blocks use `ptr` pervasively,
                    // the hint points at the whole-module form. See `Ty::Ptr`.
                    "ptr" => {
                        if self.ffi_type_licensed("ptr")
                            || self
                                .alias_resolving
                                .last()
                                .is_some_and(|a| self.ffi_alias_ok.contains(a))
                        {
                            Ty::Ptr
                        } else {
                            self.error(
                                span,
                                "unknown type 'ptr' (import it from std.ffi: `import std.ffi`)"
                                    .to_string(),
                            );
                            Ty::Unknown
                        }
                    }
                    // A RETURN-ONLY C-ABI marshalling type name (sibling of `ptr`): an OWNED `char*`
                    // the runtime copies into a `str` and then frees. To the program it IS a plain `str`
                    // (the ownership/free is a runtime-only distinction the backends recover via
                    // `ctype_of`); the return-only-ness is enforced by a surface guard in the extern
                    // param loop (an `owned_str` parameter is rejected before this collapses to `Str`).
                    // Bare use OUTSIDE an extern signature is rejected: it is not a general type — left
                    // ungated it would silently collapse a `fn f(x: owned_str) -> owned_str` to `str`
                    // with no import (mirrors how `ptr` errors when used without `import std.ffi`).
                    "owned_str" => {
                        if self.in_extern_sig {
                            Ty::Str
                        } else {
                            self.error(
                            span,
                            "'owned_str' is a return-only extern marshalling type and cannot be \
                             used as a general type annotation"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // The C5 escape hatch handle, non-generic (a bare `Executor` type annotation). Like
                    // `Shared`/`RwShared`/`Atomic` below, `Executor` is NOT a global builtin: it resolves
                    // only in a module that imported `std.concurrency` (whole-module or per-name). The name
                    // STAYS reserved (no user `struct Executor`); this is the separate import requirement.
                    "Executor" => {
                        if self.concurrency_licensed("Executor") {
                            Ty::Executor
                        } else {
                            self.error(
                            span,
                            "unknown type 'Executor' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // A BARE (no type-arg) `Shared`/`RwShared`/`Atomic` annotation. Like `Executor`
                    // above these names are NOT global builtins and STAY reserved; but they are generic,
                    // so a bare write is either unlicensed (→ same import hint the `Shared[T]` arm below
                    // emits) or licensed-but-missing-its-type-arg (→ the same missing-type-arg message a
                    // bare user-generic struct/enum/newtype gets). Placed before the catch-all so it
                    // can't fall through to a hint-less "unknown type". (An in-scope type param of the
                    // same name, e.g. `fn f[Shared]`, was already resolved by the hoisted `type_params`
                    // arm above this match, so no per-arm guard is needed here.)
                    n @ ("Shared" | "RwShared" | "Atomic") => {
                        if self.concurrency_licensed(n) {
                            self.error(
                                span,
                                format!("type '{n}' expects 1 type argument(s), got 0"),
                            );
                        } else {
                            self.error(
                            span,
                            format!(
                                "unknown type '{n}' (import it from std.concurrency: `import std.concurrency`)"
                            ),
                        );
                        }
                        Ty::Unknown
                    }
                    // D6 — the std.net TCP handles, non-generic (bare `Socket` / `Listener` annotations).
                    // Like `Executor`/`Shared`/`ptr` above, these are NOT global builtins: they resolve
                    // only in a module that imported `std.net` (whole-module or per-name). The names STAY
                    // reserved (no user `struct Socket`); this is the separate import requirement. (An
                    // in-scope type param of the same name was already resolved by the hoisted `type_params`
                    // arm above this match.)
                    n @ ("Socket" | "Listener") => {
                        if self.net_licensed(n) {
                            if n == "Socket" {
                                Ty::Socket
                            } else {
                                Ty::Listener
                            }
                        } else {
                            self.error(
                                span,
                                format!(
                                    "unknown type '{n}' (import it from std.net: `import std.net`)"
                                ),
                            );
                            Ty::Unknown
                        }
                    }
                    // A transparent type alias resolves to its underlying type (recursively). The
                    // `alias_resolving` stack breaks cycles (`type A = B; type B = A`).
                    _ if self.aliases.contains_key(n) => {
                        if self.alias_resolving.iter().any(|a| a == n) {
                            self.error(span, format!("recursive type alias '{n}'"));
                            Ty::Unknown
                        } else {
                            let aliased = self.aliases[n].clone();
                            self.alias_resolving.push(n.clone());
                            let ty = self.resolve_type(&aliased, span);
                            self.alias_resolving.pop();
                            ty
                        }
                    }
                    // A `from`-imported type alias resolves to its pre-resolved body (computed in the
                    // defining module's scope). A licensed FFI-width alias was already re-seeded into
                    // `ffi_alias_ok`, but since the body is already a concrete `Ty` no width re-check is
                    // needed here.
                    _ if self.imported_alias_tys.contains_key(n) => {
                        // The body was resolved read-only in its defining module (no gate), so a
                        // static-ctor protocol could hide in it — re-gate it out of value position here.
                        let ty = self.imported_alias_tys[n].clone();
                        self.reject_static_protocol_value(ty, span)
                    }
                    // Fixed-width C-ABI integer marshalling type names (`int8`..`uint64`) — Chezzi's first
                    // type imports. Each resolves to a plain `int` (`Ty::Int`) — the width/signedness is a
                    // runtime-only marshalling distinction the backends recover via `ctype_of`, and they're
                    // BIDIRECTIONAL (valid as both param and return). But they are NOT global builtins: a
                    // width name resolves only in a module that imported it per-name from `std.ffi`
                    // (`import int32 from std.ffi` → `imported_ffi_types`). Otherwise it's an unknown type
                    // with an FFI-specific hint (matches the qualified-variant "write it qualified" style).
                    _ if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) => {
                        // Accept the width name if THIS module imported it, OR if we reached it by
                        // expanding a LICENSED transparent alias body — one whose defining module
                        // imported the width (`ffi_alias_ok`). A `type Len = int32` is a deliberate
                        // opt-in that stays valid wherever the alias is used, including cross-module
                        // (the alias is program-global but the per-module import set is not). A bare
                        // width name in ordinary code still needs the import — and crucially an alias
                        // whose module never imported the width does NOT launder it (the closed gate
                        // hole): only a licensed alias indirection bypasses the per-module requirement.
                        if self.ffi_type_licensed(n)
                            || self
                                .alias_resolving
                                .last()
                                .is_some_and(|a| self.ffi_alias_ok.contains(a))
                        {
                            Ty::Int
                        } else {
                            self.error(
                            span,
                            format!(
                                "unknown type '{n}' (import it from std.ffi: `import {n} from std.ffi`)"
                            ),
                        );
                            Ty::Unknown
                        }
                    }
                    _ if self.struct_names.contains(n) => {
                        // The layout is keyed by the runtime key (bare unless disambiguated); the written
                        // name's bare-visibility is the `struct_names` gate above. Carry the key on the Ty.
                        let key = self.bare_key(n);
                        // A generic struct written without type arguments is missing them.
                        let nparams = self.structs.get(&key).map_or(0, |i| i.type_params.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::strukt(key)
                    }
                    _ if self.enum_names.contains(n) => {
                        let key = self.bare_key(n);
                        // A generic enum written without type arguments is missing them.
                        let nparams = self.enum_type_params.get(&key).map_or(0, |tps| tps.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::Enum(key, Vec::new())
                    }
                    _ if self.newtype_names.contains(n) => {
                        let key = self.bare_key(n);
                        // A generic newtype written without type arguments is missing them.
                        let nparams = self
                            .newtype_type_params
                            .get(&key)
                            .map_or(0, |tps| tps.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::NewType(key, Vec::new())
                    }
                    // A protocol name used as a value type (existential), e.g. `Error`. BUT a protocol
                    // with a STATIC method requirement (`Convert`-style static ctor) is witnessable only
                    // by a static method — a VALUE can't invoke it — so it is BOUND-ONLY, rejected here.
                    _ if self.protocols.contains_key(n) => {
                        if self.protocol_has_static_method(n) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(n.clone(), Vec::new())
                        }
                    }
                    _ => {
                        self.error(span, self.unknown_type_msg(n));
                        Ty::Unknown
                    }
                };
                // Editor hover (LSP): record the resolved type at the name token's OWN span (NOT
                // the enclosing-annotation `span` param). Probe-gated so off-probe checks pay
                // nothing; gated on `!generic_arg_prepass` so the generic-arg unification prepass
                // can't first-hit-wins latch an incomplete type. Composite inner names
                // (`int` in `List[int]`) record via this same arm when resolve_type recurses.
                // A builtin/stdlib type name (`str`/`bytes`/bare `Shared`/…) carries its
                // `builtin_type_doc` usage+methods blurb (Tier C); a user type falls back to its
                // `name_docs` docstring — for an imported non-generic type used as `x: Foo`, the
                // import-line binding seeded `name_docs[Foo]` (its decl docstring or a kind+module
                // blurb). A current-module user type's own docstring also surfaces here (additive;
                // the Tier-A decl-name hover already covers the decl site). `builtin_type_doc` stays
                // FIRST so a builtin name is never shadowed by a same-named `name_docs` entry.
                if self.hover_probe.is_some() && !self.generic_arg_prepass {
                    // Suppress the `name_docs` fallback when the name resolved to a type PARAMETER
                    // (a generic `[T]` in scope) that merely SHADOWS a same-named top-level decl —
                    // the param is an unrelated entity and must not borrow that decl's docstring
                    // (mirrors the value-ident hover's scope guard). `builtin_type_doc` is unaffected
                    // (it never names a param). A real generic head is never a `Ty::Param`, so this
                    // is a no-op at the head site.
                    let doc = builtin_type_doc(n).or_else(|| {
                        if matches!(resolved, Ty::Param(_)) {
                            None
                        } else {
                            self.name_docs.get(n).cloned()
                        }
                    });
                    self.hover_record_at(*name_span, &resolved, HoverKind::Type, doc);
                }
                resolved
            }
            Type::Func {
                params,
                ret,
                labels,
            } => Ty::Func {
                params: params.iter().map(|p| self.resolve_type(p, span)).collect(),
                ret: Box::new(self.resolve_type(ret, span)),
                // Carry the annotation's optional labels onto the type (surface-only), so a value call
                // through e.g. a HOF param `f: fn(name: str) -> nil` can resolve `f(name="X")`.
                labels: FnLabels(labels.clone()),
            },
            Type::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve_type(t, span)).collect()),
            Type::Generic(n, args, head_span) => {
                // TYPE-IDENTITY source for the generic container ctors: the `List`/`Map`/`Set` arms
                // below turn `List[int]` into `Ty::List(Int)` etc. — NOT expressible as a flat `FnSig`,
                // so it stays HERE. Their `CallBuiltin` DISPATCH + name-set is table-sourced (the
                // `Intrinsic::Ctor` PRELUDE rows); `builtin_container_sig` = flat display/placeholder.
                let resolved = match (n.as_str(), args.as_slice()) {
                    ("List", [inner]) => Ty::list(self.resolve_type(inner, span)),
                    ("Result", [inner]) => Ty::result(self.resolve_type(inner, span)),
                    ("Result", [t, e]) => {
                        Ty::result_e(self.resolve_type(t, span), self.resolve_type(e, span))
                    }
                    ("Option", [inner]) => Ty::option(self.resolve_type(inner, span)),
                    // `Iterator[T]` as a *value* type — the result of calling a generator function.
                    // Represented as `Ty::Struct("Iterator", [T])`, an existential iterator whose element
                    // type `iter_elem` recovers (so `for`-loops and `[S: Iterator[T]]` bounds accept it).
                    // Experimental: only generators produce these; ordinary code still uses adapter
                    // structs / built-in collections.
                    ("Iterator", [elem]) => {
                        Ty::Struct("Iterator".to_string(), vec![self.resolve_type(elem, span)])
                    }
                    ("Channel", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if !self.sendable(&elem) {
                            self.error(
                                span,
                                format!("Channel element type must be sendable, found {elem}"),
                            );
                        }
                        Ty::channel(elem)
                    }
                    // `Shared[T]` (C3): the cross-task mutable box. Unlike a `Channel`, its element type
                    // isn't gated on sendability — the value lives in one owner and is copied in/out
                    // through `get`/`set`; the *handle* is what crosses (always sendable). NOT a global
                    // builtin: requires `import std.concurrency` (the inner type is still resolved on the
                    // unlicensed path so a nested error surfaces; the name STAYS reserved). Same for the
                    // RwShared/Atomic siblings below.
                    ("Shared", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if self.concurrency_licensed("Shared") {
                            Ty::shared(elem)
                        } else {
                            self.error(
                            span,
                            "unknown type 'Shared' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // `RwShared[T]` (type annotation): the cross-task read-write box. Like `Shared`, its
                    // element type isn't gated on sendability — the handle is what crosses.
                    ("RwShared", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if self.concurrency_licensed("RwShared") {
                            Ty::rwshared(elem)
                        } else {
                            self.error(
                            span,
                            "unknown type 'RwShared' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // `Atomic[T]` (type annotation): the cross-task atomic box. Like `Shared`, its element
                    // type isn't gated on sendability — the handle is what crosses.
                    ("Atomic", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if self.concurrency_licensed("Atomic") {
                            Ty::atomic(elem)
                        } else {
                            self.error(
                            span,
                            "unknown type 'Atomic' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    ("Map", [k, v]) => {
                        let key = self.resolve_type(k, span);
                        let value = self.resolve_type(v, span);
                        if !self.is_hashable_key(&key) {
                            self.error(
                            span,
                            format!("Map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {key}"),
                        );
                        }
                        Ty::map(key, value)
                    }
                    ("Set", [t]) => {
                        let elem = self.resolve_type(t, span);
                        if !self.is_hashable_key(&elem) {
                            self.error(
                            span,
                            format!("Set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {elem}"),
                        );
                        }
                        Ty::set(elem)
                    }
                    // A user-defined generic struct instantiated with type arguments: `Pair[int, str]`.
                    _ if self.struct_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        // Clone the param list out so the borrow on `self.structs` is dropped before
                        // the `satisfies`/`error` calls below.
                        let tps = self.structs.get(&key).map(|i| i.type_params.clone());
                        if let Some(tps) = tps {
                            if tps.len() != resolved.len() {
                                self.error(
                                    span,
                                    format!(
                                        "type '{n}' expects {} type argument(s), got {}",
                                        tps.len(),
                                        resolved.len()
                                    ),
                                );
                            }
                            // Enforce each type parameter's protocol bounds against its argument.
                            for (tp, arg) in tps.iter().zip(&resolved) {
                                for bound in &tp.bounds {
                                    if let Err(msg) = self.satisfies(arg, &bound.name) {
                                        self.error(span, msg);
                                    }
                                }
                            }
                        }
                        Ty::Struct(key, resolved)
                    }
                    // A user-defined generic enum instantiated with type arguments: `Tree[int]`.
                    _ if self.enum_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let tps = self.enum_type_params.get(&key).cloned();
                        if let Some(tps) = tps {
                            if tps.len() != resolved.len() {
                                self.error(
                                    span,
                                    format!(
                                        "type '{n}' expects {} type argument(s), got {}",
                                        tps.len(),
                                        resolved.len()
                                    ),
                                );
                            }
                            for (tp, arg) in tps.iter().zip(&resolved) {
                                for bound in &tp.bounds {
                                    if let Err(msg) = self.satisfies(arg, &bound.name) {
                                        self.error(span, msg);
                                    }
                                }
                            }
                        }
                        Ty::Enum(key, resolved)
                    }
                    // A parameterized protocol used as a value type (`Container[int]`): resolve the
                    // args, arity-check against the protocol's own type params, and carry them on the
                    // existential. Conformance is witnessed at every store/pass boundary (via
                    // `assignable` → `satisfies_args`); the args are then erased at runtime. Mirrors
                    // the struct/enum parameterized arms above.
                    _ if self.protocols.contains_key(n) => {
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let nparams = self.protocols.get(n).map_or(0, |p| p.type_params.len());
                        if nparams != resolved.len() {
                            self.error(
                                span,
                                format!(
                                    "type '{n}' expects {nparams} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        // A static-method (static-ctor) protocol like `Convert[int]` is BOUND-ONLY —
                        // no value can witness a static requirement, so it is rejected in value position.
                        if self.protocol_has_static_method(n) {
                            self.error(
                                span,
                                format!(
                                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                                ),
                            );
                            Ty::Unknown
                        } else {
                            Ty::Protocol(n.clone(), resolved)
                        }
                    }
                    // A user-defined generic newtype instantiated with type arguments: `Stack[int]`.
                    _ if self.newtype_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let tps = self.newtype_type_params.get(&key).cloned();
                        if let Some(tps) = tps {
                            if tps.len() != resolved.len() {
                                self.error(
                                    span,
                                    format!(
                                        "type '{n}' expects {} type argument(s), got {}",
                                        tps.len(),
                                        resolved.len()
                                    ),
                                );
                            }
                            for (tp, arg) in tps.iter().zip(&resolved) {
                                for bound in &tp.bounds {
                                    if let Err(msg) = self.satisfies(arg, &bound.name) {
                                        self.error(span, msg);
                                    }
                                }
                            }
                        }
                        Ty::NewType(key, resolved)
                    }
                    _ => {
                        self.error(span, format!("unknown generic type '{n}'"));
                        Ty::Unknown
                    }
                };
                // Editor hover (LSP, Tier C): record the resolved type at the GENERIC HEAD-name
                // token's OWN span (`List`/`Heap` in `List[int]`/`Heap[int]`) — the gap the bare
                // `Type::Named` arm already covers for non-generic heads. A builtin/stdlib head
                // (`List`/`Map`/`Set`/`Channel`/…) carries its `builtin_type_doc` usage+methods
                // blurb; a user head (an imported generic struct/enum/newtype like `Heap`) falls back
                // to its `name_docs` docstring (the import-line binding seeds that entry). Probe-gated
                // so off-probe checks pay nothing; gated `!generic_arg_prepass` so the generic-arg
                // unification prepass can't first-hit-wins latch an incomplete type.
                if self.hover_probe.is_some() && !self.generic_arg_prepass {
                    // Suppress the `name_docs` fallback when the name resolved to a type PARAMETER
                    // (a generic `[T]` in scope) that merely SHADOWS a same-named top-level decl —
                    // the param is an unrelated entity and must not borrow that decl's docstring
                    // (mirrors the value-ident hover's scope guard). `builtin_type_doc` is unaffected
                    // (it never names a param). A real generic head is never a `Ty::Param`, so this
                    // is a no-op at the head site.
                    let doc = builtin_type_doc(n).or_else(|| {
                        if matches!(resolved, Ty::Param(_)) {
                            None
                        } else {
                            self.name_docs.get(n).cloned()
                        }
                    });
                    self.hover_record_at(*head_span, &resolved, HoverKind::Type, doc);
                }
                resolved
            }
            // A module-qualified type `module.Type[args]` (mirrors how a function is reached via its
            // bound module name). Resolve `module` in `imported_modules` → the target's `ModuleSig`,
            // confirm the type exists there, and return the matching `Ty`. Enforces arity for generic
            // struct/enum targets.
            Type::Qualified { module, name, args } => {
                let resolved: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, span)).collect();
                let Some(mid) = self.imported_modules.get(module).cloned() else {
                    self.error(
                        span,
                        format!("unknown module '{module}' (import it to use `{module}.{name}`)"),
                    );
                    return Ty::Unknown;
                };
                let Some(sig) = self.module_sigs.get(&mid).cloned() else {
                    self.error(span, format!("module '{module}' has no type '{name}'"));
                    return Ty::Unknown;
                };
                // A RESERVED native type (std.concurrency's `Shared`/`RwShared`/`Atomic`/`Executor`,
                // std.net's `Socket`/`Listener`) now ALSO has a `sig.struct_defs` entry — for its
                // harvested METHOD table — but it is NOT a nominal struct: it must resolve to the opaque
                // reserved `Ty::Shared`/etc via the `sig.types` → `qualified_builtin_ty` branch below,
                // NOT `Ty::Struct(...)`. Skip the struct_defs arm for these so `concurrency.Shared[int]`
                // (a qualified annotation / `type` alias / `newtype` body) keeps its reserved `Ty`
                // (matching the bare-after-import path); otherwise it would mint a divergent
                // `Ty::Struct("Shared", …)` that fails to unify with a `Shared(v)` ctor's `Ty::shared`.
                if let Some(info) = sig.struct_defs.get(name)
                    && self.qualified_builtin_ty(name, &[]).is_none()
                {
                    if info.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                info.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::Struct(self.type_key(&mid, name), resolved)
                } else if let Some(edef) = sig.enum_defs.get(name) {
                    if edef.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                edef.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::Enum(self.type_key(&mid, name), resolved)
                } else if let Some(ntdef) = sig.newtype_defs.get(name) {
                    if ntdef.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                ntdef.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::NewType(self.type_key(&mid, name), resolved)
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    // Pre-resolved in the exporting module by the read-only resolver (no gate) — re-gate
                    // a static-ctor protocol out of value position (`import a; c: a.Foo`).
                    let body = asig.body.clone();
                    self.reject_static_protocol_value(body, span)
                } else if sig.types.contains(name) {
                    // An opaque/native builtin TYPE reached by qualified path (`concurrency.Shared[int]`,
                    // `net.Socket`, `ffi.int32`/`ffi.ptr`). These names live ONLY in their owning std
                    // module's `sig.types` (reserved — no user module can export them), so this branch
                    // fires solely for native builtins; user struct/enum/newtype/alias names were already
                    // consumed by the def-map arms above. The arm required the module be imported (the
                    // `imported_modules.get` above), so the import gate is unchanged: a non-imported
                    // module still hit the `unknown module` error before reaching here. ADDITIVE — the
                    // bare-after-import path (`imported_concurrency`/`_net`/`_ffi_types`) is untouched.
                    if name == "timer" {
                        self.error(
                            span,
                            "'timer' is a function, not a type — call it: `time.timer(ms)`"
                                .to_string(),
                        );
                        Ty::Unknown
                    } else if let Some(ty) = self.qualified_builtin_ty(name, &resolved) {
                        let expected = Self::qualified_builtin_arity(name);
                        if resolved.len() != expected {
                            self.error(
                                span,
                                format!(
                                    "type '{module}.{name}' expects {expected} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        ty
                    } else {
                        self.error(span, format!("module '{module}' has no type '{name}'"));
                        Ty::Unknown
                    }
                } else {
                    self.error(span, format!("module '{module}' has no type '{name}'"));
                    Ty::Unknown
                }
            }
        }
    }

    // ===== pass 2: check statements =====

    pub(super) fn check_block(&mut self, block: &Block) {
        // PERSISTENT refine-on-first-use (scope-wide first-use pinning): `check_block` runs every
        // CONDITIONALLY-executed STATEMENT body (an `if`/`elif`/`else` branch, a `while` body, a
        // `defer:` block). A refine-on-first-use narrowing of an OUTER binding performed inside this
        // body PERSISTS — the first mutating op that fixes an empty collection's element/key/value
        // type pins it for the binding's whole scope, even across sibling branches and past the
        // branch. `repin` writes the pin to the binding's OWNING scope, so it survives `pop_scope`
        // (which only removes inner-block-declared bindings, not the outer owner). Building a
        // heterogeneous collection split across branches/arms is therefore now a type error, exactly
        // like the literal `[1, "s"]`. Lexical scoping is intact: a binding DECLARED in this block is
        // still removed by `pop_scope`; only an OUTER binding's first-use pin persists. (Expression-
        // position arms — `infer_if_else`/`infer_match` — keep their snapshot/restore barrier: a pin
        // in one value-arm must not leak to a sibling value-arm, that being the narrow residual.)
        self.push_scope();
        for stmt in block {
            self.check_stmt(stmt);
        }
        self.pop_scope();
    }

    pub(super) fn check_stmt(&mut self, stmt: &Stmt) {
        let span = stmt.span;
        match &stmt.kind {
            StmtKind::Let {
                names,
                name_spans,
                ty,
                value,
                is_ref,
                ..
            } => {
                let is_ref = *is_ref;
                // A closure bound to a `fn`-typed annotation is inferred in checking-mode (source #1):
                // resolve the annotation first so its unannotated params bind to the slot's param
                // types. Only the single-name, non-`ref`, `fn`-typed case (a `ref` pointee is never a
                // closure; destructuring never binds one). Otherwise ordinary bottom-up inference.
                let val_ty = match ty {
                    Some(t)
                        if !is_ref
                            && names.len() == 1
                            && matches!(value.kind, ExprKind::Closure { .. }) =>
                    {
                        let expected = self.resolve_type(t, span);
                        if matches!(expected, Ty::Func { .. }) {
                            self.infer_arg(value, Some(&expected))
                        } else {
                            self.infer_value(value)
                        }
                    }
                    // Expected-type checking-mode for a NON-closure value bound to an annotation:
                    // thread the annotation as a hint into the value's inference so a generic
                    // ctor / generic fn-call pre-seeds its type params from it — `a: Heap[int] =
                    // Heap([], fn(x, y): x < y)` pins `T=int`, which then pins the comparator's
                    // params. A `ref` binding is excluded (its init was desugared to a `Ref(...)`
                    // wrapper whose param is the pointee, not this annotation). `infer_call` clears
                    // the hint, but pair the set with an immediate clear so a non-call value never
                    // leaks it into the next statement.
                    Some(t) if !is_ref && names.len() == 1 => {
                        let expected = self.resolve_type(t, span);
                        self.expected_hint = Some(expected);
                        let vt = self.infer_value(value);
                        self.expected_hint = None;
                        vt
                    }
                    _ => self.infer_value(value),
                };
                if names.len() > 1 {
                    // destructuring let `a, b := expr` — `expr` must be a tuple of matching arity.
                    self.check_destructure(names, name_spans, &val_ty, value.span);
                    return;
                }
                let name = &names[0];
                let declared = match ty {
                    Some(t) => {
                        // A `ref T` binding is, at runtime, a `Ref[T]` box (the desugar pass lowered
                        // the init to `Ref(v)`/alias and every read to `.get()`); so the binding's
                        // checker type is `Ref[T]`, not `T`. The pointee element type `T` is rejected
                        // if it is not boxable (e.g. a bare unconstrained generic) by `check_ref_ty`.
                        let expected = if is_ref {
                            self.check_ref_ty(t, span);
                            self.resolve_type(
                                &Type::Generic("Ref".to_string(), vec![t.clone()], Span::default()),
                                span,
                            )
                        } else {
                            self.resolve_type(t, span)
                        };
                        if !self.assignable_w(&expected, &val_ty, true) {
                            // Transparency: a `ref T` binding's mismatch renders `ref T`, not `Ref[T]`.
                            let exp = if is_ref {
                                ref_display(&expected)
                            } else {
                                expected.to_string()
                            };
                            self.error(
                                value.span,
                                format!("cannot assign {val_ty} to variable of type {exp}"),
                            );
                        }
                        expected
                    }
                    None => val_ty,
                };
                // PART A: an UN-annotated empty literal (`b := []`/`{}`/`Set()`) whose element/key/value
                // slot is still `Unknown` records a pending site; if no later op constrains it, the
                // end-of-scope finalize requires an annotation. Gated on `!inferring_ret` so the
                // return-inference passes (whose errors are truncated + re-run) don't record duplicates.
                // The annotated branch never reaches here as an unrefined-empty (a `List[int]`
                // annotation leaves no `Unknown`-in-slot), and an expression-position literal (`f([])`,
                // `return []`) binds no local, so the false-positive guards fall out structurally.
                if ty.is_none() && !self.inferring_ret && Self::is_unrefined_empty_coll(&declared) {
                    self.empty_coll_sites
                        .push((self.scopes.len() - 1, name.clone(), span));
                } else if ty.is_some()
                    && !contains_unknown_in_slot(&declared)
                    && let ExprKind::Ident(src) = &value.kind
                {
                    // PART A: binding a bare empty-collection ident into a CONCRETE-typed annotated
                    // let (`c: List[int] = b`) constrains `b`'s element type — drop its pending
                    // requirement (the typed-binding false-positive guard, one binding away from the
                    // direct-literal `b: List[int] = []`). Gated on the annotation being fully
                    // concrete so `c: List[?] = b` does not spuriously satisfy the requirement.
                    self.drop_empty_site(src);
                }
                // An empty binding read as the let VALUE escapes into the new binding (alias `c := b`
                // or nested `c := [b]`) — drop the source's pending site (the alias records its own if
                // it stays unrefined). Runs for every binding kind; only an active site is affected.
                self.drop_value_escape_sites(value);
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
                    self.hover_record_binding(span, &declared, name, HoverKind::Local, doc);
                }
                self.declare(name, declared);
                if is_ref {
                    self.declare_ref(name);
                }
            }
            StmtKind::Assign { target, op, value } => {
                // Checking-mode: a closure assigned to a `fn`-typed lvalue (a struct fn-field or a
                // fn-typed variable) binds its unannotated params from the target's type (source #1).
                let val_ty = if matches!(value.kind, ExprKind::Closure { .. }) {
                    // Discover the target's type for checking-mode WITHOUT diagnosing it: inferring
                    // an lvalue as an rvalue would run read-side gates (non-sendable-captured-binding
                    // read) and double-infer a Field/Index receiver. `check_assign` below is the sole
                    // validator of the target, so snapshot+truncate any errors this probe produces
                    // (mirrors the generic-arg recovery idiom).
                    let mark = self.errors.len();
                    let target_ty = self.infer(target);
                    self.errors.truncate(mark);
                    if matches!(target_ty, Ty::Func { .. }) {
                        self.infer_arg(value, Some(&target_ty))
                    } else {
                        self.infer_value(value)
                    }
                } else {
                    self.infer_value(value)
                };
                // An empty binding read as the assignment VALUE escapes into the target slot (`c = b`,
                // `bx.items = b`) — drop the source's pending empty-collection site, mirroring the
                // typed-binding-value guard for `c: List[int] = b`. Covers every target shape.
                self.drop_value_escape_sites(value);
                self.check_assign(target, *op, val_ty, span);
            }
            StmtKind::Fn(decl) => {
                if decl.is_test {
                    self.validate_test_fn_shape(decl, false);
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
                    // Name-resolution parity guard: the compiler resolves reserved builtins /
                    // container-runtime ctors (`print`/`range`/`List`/`Channel`/…), bare struct &
                    // newtype constructors, and the bare BUILTIN variant ctors (`Ok`/`Err`/`Some`/
                    // `None`) BEFORE a local value-call (`compile_call`). Declaring a nested fn with
                    // one of those names would type calls to the nested fn while the VM runs the
                    // builtin/ctor — the exact check-OK/run-divergent hole this feature exists to
                    // close. Reject cleanly (mirrors the top-level `is_reserved_name` hoist guard,
                    // extended to the ctor families a nearest-scope local can shadow but the backend
                    // can't). User-enum variants are NOT bare-callable, so they aren't guarded (no
                    // divergence + a nested fn may legitimately share a user variant's name).
                    let nm = decl.name.as_str();
                    if crate::checker::is_reserved_name(nm)
                        || self.struct_names.contains(nm)
                        || self.newtype_names.contains(nm)
                        || matches!(nm, "Ok" | "Err" | "Some" | "None")
                    {
                        self.error(
                            decl.name_span,
                            format!(
                                "nested function name '{nm}' is reserved (a builtin or constructor the runtime resolves before a local)"
                            ),
                        );
                        return;
                    }
                    let mut sig = self.fn_sig(decl, decl.name_span);
                    // No `-> T`: infer the return from the body (mirrors the top-level single-fn
                    // inference). Declare a PROVISIONAL `Ty::Func` first so a self-recursive call
                    // inside inference resolves as an arity-checked value-call (not a global namesake
                    // / `unknown name`). A residual `Unknown` for a purely-recursive un-annotated
                    // nested fn stays permissive — a v1 limit, only its own call sites degrade.
                    if decl.ret.is_none() && matches!(sig.ret, Ty::Unknown) {
                        self.declare(
                            &decl.name,
                            Ty::Func {
                                params: sig.params.clone(),
                                ret: Box::new(Ty::Unknown),
                                labels: crate::checker::FnLabels(sig.labels.clone()),
                            },
                        );
                        sig.ret = self.infer_fn_ret(decl, None, &sig.params, true);
                    }
                    // Nearest-scope binding: the name resolves to THIS nested fn (not a global
                    // namesake) at every call site, and recursion type-checks. Declared BEFORE
                    // `check_fn_body`.
                    self.declare(
                        &decl.name,
                        Ty::Func {
                            params: sig.params.clone(),
                            ret: Box::new(sig.ret.clone()),
                            labels: crate::checker::FnLabels(sig.labels.clone()),
                        },
                    );
                    self.check_fn_body(decl, None, sig);
                    return;
                }
                // Top-level fn: checked against its hoisted global sig.
                // `.get` (not index) is panic-safe even when a redeclaration left a different sig.
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
                // A constant-literal field default must be assignable to the field's type (checked
                // here so a wrong-typed default is caught at the declaration, not only when omitted).
                for field in fields {
                    if let Some(def) = &field.default {
                        let expected = self.resolve_type(&field.ty, def.span);
                        let actual = self.infer(def);
                        if !matches!(expected, Ty::Unknown)
                            && !self.assignable_w(&expected, &actual, true)
                        {
                            self.error(
                                def.span,
                                format!(
                                    "default value for field '{}': expected {expected}, found {actual}",
                                    field.name
                                ),
                            );
                        }
                    }
                }
                // A struct with ≥1 `test fn` method is a test suite. Its lifecycle hooks
                // (before_all/after_all/before_each/after_each), when present, must be `fn name(self)`
                // returning nothing — validated here so the runner can trust the shape.
                let is_suite = methods.iter().any(|m| m.is_test);
                for m in methods {
                    if m.is_test {
                        self.validate_test_fn_shape(m, true);
                    } else if is_suite && is_lifecycle_hook(&m.name) {
                        self.validate_lifecycle_hook(m);
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
                // "fn(T) -> Box[T]". Probe-gated no-op (parity-neutral), emits no error.
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
                                ret: Box::new(Ty::Enum(vi.enum_name, targs_disp.clone())),
                                labels: crate::checker::FnLabels::default(),
                            };
                            self.hover_record_at(v.name_span, &fty, HoverKind::Func, None);
                        }
                    }
                }
                let is_suite = methods.iter().any(|m| m.is_test);
                for m in methods {
                    if m.is_test {
                        self.validate_test_fn_shape(m, true);
                    } else if is_suite && is_lifecycle_hook(&m.name) {
                        self.validate_lifecycle_hook(m);
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
                self.exit_type_params(saved);
            }
            // Newtype method bodies are checked here, mirroring the enum path (`self` is the newtype).
            StmtKind::NewType {
                name,
                name_span,
                type_params,
                methods,
                doc,
                ..
            } => {
                let self_ty = self.newtype_self_ty(name);
                let key = self.bare_key(name);
                // The newtype's type parameters are in scope across its method bodies (like the
                // struct/enum path), so a generic `fn peek(self) -> Option[T]` resolves `T`.
                let saved = self.enter_type_params(type_params);
                // Editor hover (decl-site): record the newtype at its declared-name token + doc.
                if self.hover_probe.is_some() {
                    self.hover_record_at(*name_span, &self_ty, HoverKind::Struct, doc.clone());
                }
                for m in methods {
                    if m.is_test {
                        // Parser rejects `test fn` in a newtype body, so this is unreachable; guard
                        // anyway to keep the suite invariants explicit.
                        self.validate_test_fn_shape(m, true);
                    }
                    // Skip a duplicate-named method's body check (see the struct arm) — its clear
                    // hoist-time dup error stands alone instead of a misleading return-type mismatch.
                    if methods.iter().filter(|x| x.name == m.name).count() > 1 {
                        continue;
                    }
                    if let Some(sig) = self
                        .newtype_defs
                        .get(&key)
                        .and_then(|(_, ms)| ms.get(&m.name))
                        .cloned()
                    {
                        self.record_method_decl_hover(m.name_span, &sig);
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
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
                    let ty = Ty::Protocol(name.clone(), Vec::new());
                    self.hover_record_at(*name_span, &ty, HoverKind::Struct, doc.clone());
                }
            }
            // A type alias is fully resolved during hoisting; pass 2 only records its decl-site hover.
            StmtKind::TypeAlias {
                name,
                name_span,
                doc,
                ..
            } => {
                // Editor hover (decl-site): record the ALIASED type at the alias-name token. Gated
                // strictly on the probe so the extra `resolve_type` never runs in normal checking; on
                // an invalid alias it may add a duplicate error, but hover returns None on ANY error,
                // so observable behavior is unchanged.
                if self.hover_probe.is_some()
                    && let Some(body) = self.aliases.get(name).cloned()
                {
                    let ty = self.resolve_type(&body, *name_span);
                    self.hover_record_at(*name_span, &ty, HoverKind::Struct, doc.clone());
                }
            }
            // Imports, extern blocks, and native (fn/ctor/struct/enum) decls carry nothing to check in
            // pass 2 (native decls are fully validated + registered in `hoist`).
            StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. } => {}
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
            StmtKind::For {
                vars,
                var_spans,
                iter,
                body,
            } => {
                let bindings = self.for_bindings(vars, iter);
                // PERSISTENT refine-on-first-use (see `check_block`): a refine-on-first-use pin of an
                // OUTER empty collection inside the loop body PERSISTS past the loop. We accept the
                // zero-trip / always-runs over-approximation by design — `xs:=[]; for i in []:
                // xs.push(1); xs.push("s")` REJECTS even though the body never runs at runtime; a
                // sound static over-approximation, matching "first statement that fixes the element
                // type records it". (No snapshot/restore here, so the pin written to the binding's
                // OWNING scope by `repin` survives `pop_scope`, which only removes the loop vars.)
                self.push_scope();
                // `bindings` is parallel to `vars` (and thus `var_spans`); zip truncates safely if the
                // lengths ever diverge (a binding's hover is dropped, never a panic).
                for ((name, ty), span) in bindings.into_iter().zip(var_spans.iter()) {
                    // EDITOR HOVER: the loop binding (`i` in `for i in …`) is a NAME, not an `Expr` the
                    // probe visits during `infer`; record its decl-site hover here (no-op unless armed).
                    self.hover_record_at(*span, &ty, HoverKind::Local, None);
                    self.declare(&name, ty);
                    // Loop vars are rebound each iteration → immutable; reassigning one diverges
                    // across engines, so the checker forbids it (see `check_assign`).
                    self.mark_loop_var(&name);
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
                    ExprKind::Call { callee, named, .. } => match &callee.kind {
                        ExprKind::Field { .. } => {} // method call
                        ExprKind::Ident(name)
                            if self.lookup(name).is_none()
                                && !self.functions.contains_key(name)
                                && !is_firstclass_builtin_fn(name) =>
                        {
                            self.error(
                                e.span,
                                "defer requires a function or method call (built-ins and \
                                 constructors must be wrapped in a function)",
                            );
                        }
                        // A deferred first-class builtin runs via its VALUE form, which cannot carry
                        // named args — `sep=`/`end=` are direct-call-only (the specialized `print`
                        // opcode). Reject them rather than silently dropping them: the direct-call
                        // typing would otherwise accept `defer print(a, sep="-")` and then print `a`
                        // with the default separator, a wrong result vs. the accepted contract.
                        ExprKind::Ident(name)
                            if is_firstclass_builtin_fn(name) && !named.is_empty() =>
                        {
                            self.error(
                                e.span,
                                "named arguments (sep=/end=) are only supported on a direct \
                                 print(...) call, not a deferred one",
                            );
                        }
                        _ => {} // a name bound to a callable, or an arbitrary value-producing callee
                    },
                    _ => self.error(e.span, "defer requires a function or method call"),
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
                // in both engines. Save-zero-restore `loop_depth` (mirroring `check_fn_body`) so the
                // `loop_depth == 0` guard at `StmtKind::Break`/`Continue` fires at check time; a
                // legitimate loop INSIDE the block re-increments from 0, keeping its own break legal.
                let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
                self.check_block(body);
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
                        if let ExprKind::Call { callee, named, .. } = &e.kind {
                            match &callee.kind {
                                ExprKind::Field { .. } => {} // method call
                                ExprKind::Ident(name)
                                    if self.lookup(name).is_none()
                                        && !self.functions.contains_key(name)
                                        && !is_firstclass_builtin_fn(name) =>
                                {
                                    self.error(
                                        e.span,
                                        "spawn requires a function or method call (built-ins and \
                                         constructors must be wrapped in a function)",
                                    );
                                }
                                // A spawned first-class builtin runs via its VALUE form (fixed sep/end),
                                // which cannot carry `sep=`/`end=` — reject rather than silently drop
                                // (mirrors the deferred-builtin guard).
                                ExprKind::Ident(name)
                                    if is_firstclass_builtin_fn(name) && !named.is_empty() =>
                                {
                                    self.error(
                                        e.span,
                                        "named arguments (sep=/end=) are only supported on a direct \
                                         print(...) call, not a spawned one",
                                    );
                                }
                                _ => {}
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
                            let checkpoint = self.errors.len();
                            let mut bad: Vec<(Span, String)> = Vec::new();
                            if let ExprKind::Field { obj, .. } = &callee.kind {
                                let rty = self.infer(obj);
                                if !self.sendable(&rty) {
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
                            for arg in args.iter().chain(named.iter().map(|(_, v)| v)) {
                                let aty = self.infer(arg);
                                if !self.sendable(&aty) {
                                    bad.push((
                                        arg.span,
                                        format!("cannot pass a non-sendable value of type {aty} to a spawned task"),
                                    ));
                                }
                            }
                            self.errors.truncate(checkpoint);
                            for (sp, msg) in bad {
                                self.error(sp, msg);
                            }
                        }
                    }
                    SpawnTarget::Block(body) => {
                        // Bindings visible now are captured by the task and are read-only inside
                        // it (the airlock); bindings the body declares (at this floor or deeper)
                        // are task-local. `enter`/`leave` is balanced even if checking errors.
                        let floor = self.scopes.len();
                        self.capture_floors.push(floor);
                        // A `spawn:` block compiles to a fresh child proto with an empty loop stack,
                        // so a `break`/`continue` lexically nested in an enclosing loop but placed
                        // here is illegal in both engines. Save-zero-restore `loop_depth` (mirroring
                        // `infer_closure`) so the `loop_depth == 0` guard at `StmtKind::Break`/
                        // `Continue` fires at check time; a legitimate loop INSIDE the block
                        // re-increments from 0, keeping its own break/continue legal.
                        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
                        self.push_scope();
                        for stmt in body {
                            self.check_stmt(stmt);
                        }
                        self.pop_scope();
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
            StmtKind::Expr(e) => {
                self.infer(e);
            }
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
        decl.body
            .first()
            .map(|s| s.span)
            .unwrap_or(Span { line: 0, col: 0 })
    }

    /// A `test fn` takes no parameters (free) or only `self` (method) and returns nothing. Hard
    /// errors here keep the runner's contract simple (it invokes tests with no args / only the
    /// instance). The body is still checked normally by the caller.
    pub(super) fn validate_test_fn_shape(&mut self, decl: &FnDecl, is_method: bool) {
        let span = Self::fn_span(decl);
        if is_method {
            let ok = decl.params.len() == 1 && decl.params[0].name == "self";
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

    /// A suite lifecycle hook (`before_all`/`after_all`/`before_each`/`after_each`) must be
    /// `fn name(self)` returning nothing — the runner invokes it with only the instance.
    pub(super) fn validate_lifecycle_hook(&mut self, decl: &FnDecl) {
        let span = Self::fn_span(decl);
        let ok = decl.params.len() == 1 && decl.params[0].name == "self";
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

    /// Check a destructuring let `a, b, … := value`. The value's type must be a tuple whose arity
    /// matches the binding count; each name is then declared with its element type. An `Unknown`
    /// value (an already-reported error) declares all names `Unknown` so no cascade follows.
    pub(super) fn check_destructure(
        &mut self,
        names: &[String],
        name_spans: &[Span],
        val_ty: &Ty,
        span: Span,
    ) {
        match val_ty {
            Ty::Unknown => {
                for name in names {
                    self.declare(name, Ty::Unknown);
                }
            }
            Ty::Tuple(elems) if elems.len() == names.len() => {
                for ((name, ty), name_span) in names.iter().zip(elems).zip(name_spans.iter()) {
                    // EDITOR HOVER: each destructure target (`a`/`b` in `a, b := (1,2)`) is a NAME,
                    // not an `Expr` the probe visits; record its tuple-element type at its own span
                    // (no-op unless a probe is armed → zero overhead on normal checks).
                    self.hover_record_at(*name_span, ty, HoverKind::Local, None);
                    self.declare(name, ty.clone());
                }
            }
            Ty::Tuple(elems) => {
                self.error(
                    span,
                    format!(
                        "destructuring binds {} name(s), but the tuple has {} element(s)",
                        names.len(),
                        elems.len()
                    ),
                );
                for name in names {
                    self.declare(name, Ty::Unknown);
                }
            }
            other => {
                self.error(
                    span,
                    format!("cannot destructure non-tuple value of type {other}"),
                );
                for name in names {
                    self.declare(name, Ty::Unknown);
                }
            }
        }
    }

    pub(super) fn check_assign(&mut self, target: &Expr, op: AssignOp, val_ty: Ty, span: Span) {
        match &target.kind {
            ExprKind::Ident(name) => {
                let Some(var_ty) = self.lookup(name) else {
                    self.error(
                        span,
                        format!("cannot assign to undeclared variable '{name}'"),
                    );
                    return;
                };
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
                // the one deliberate divergence from Go). A captured MODULE GLOBAL stays rejected: it
                // is frozen under `--parallel`, and writing it would diverge serial (shared global)
                // from M:N (worker's snapshot copy).
                if self.is_captured(name) && !self.is_local_capture(name) {
                    self.error(
                        target.span,
                        format!("cannot reassign the captured module global '{name}' inside a spawned task (module globals are frozen under --parallel — use a Shared or Channel)"),
                    );
                    return;
                }
                // A `defer:` block runs in the SAME task (no airlock), so it shares the enclosing
                // binding's cell — reassigning a captured local mutates the shared cell (A2/A3/E1).
                // Editor hover (decl-site): a reassignment's LHS `i` is an `Ident` lvalue the probe
                // does NOT visit via `infer` (it's the assignment TARGET, not an evaluated expr), so
                // record its type at the target span here. Probe-gated no-op off the probe; mirrors
                // the let-binding/for-binding `Local` hover. Simple-Ident lvalue only (Index/Field
                // targets are handled in their own arms below, where the receiver IS inferred).
                self.hover_record_at(target.span, &var_ty, HoverKind::Local, None);
                self.check_assign_value(&var_ty, op, &val_ty, target.span);
                // PART A: a whole-binding (re)assignment / compound-assign / tuple-assign element that
                // supplies a CONCRETE-typed value into an unrefined empty-collection binding constrains
                // its element type — clear the pending annotation requirement (the binding IS
                // constrained, just not through the two refine-on-first-use mutator gates). Gated on the
                // value being fully concrete (`!contains_unknown_in_slot`) so reassigning ANOTHER empty
                // literal (`b = []`, still `List[Unknown]`) does NOT drop the requirement. No re-pin —
                // the binding's stored type is intentionally left permissive (behavior-preserving;
                // matches base, which never narrowed on plain `=`).
                if !contains_unknown_in_slot(&val_ty) {
                    self.drop_empty_site(name);
                }
            }
            // `xs[i] = v` — only lists are mutable by index. Strings are immutable; other types
            // aren't indexable. (`infer_index` would green-light a str index — handle it here.)
            ExprKind::Index { obj, index } => {
                // Refine-on-first-use for `m[k]=v` / `xs[i]=v`: when `obj` is a simple variable whose
                // type has an `Unknown` key/value/element slot (an empty `{}`/`[]`), the supplied
                // (idx_ty, val_ty) makes the slot concrete — re-pin the binding so a later conflicting
                // assign is a normal mismatch. The match below then re-reads the refined type from
                // scope. (Same simple-variable-only limitation as `refine_receiver`.)
                self.refine_index_receiver(obj, index, &val_ty);
                match self.infer(obj) {
                    Ty::Map(k, v) => {
                        let idx_ty = self.infer(index);
                        if !compatible(&k, &idx_ty) {
                            self.error(index.span, format!("map key must be {k}, found {idx_ty}"));
                        }
                        // Direct insertion-site Hashable / float-key ban: reject a non-Hashable key
                        // expr even when the map's key type is still `Unknown` (an empty `{}`), so
                        // `m:={}; m[1.5]=..` faults here (mirrors the literal `{1.5:..}` ban) rather
                        // than slipping past check.
                        if !idx_ty.is_unknown() && !self.is_hashable_key(&idx_ty) {
                            self.error(
                                index.span,
                                format!("map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {idx_ty}"),
                            );
                        }
                        self.check_assign_value(&v, op, &val_ty, target.span);
                    }
                    Ty::List(elem) => {
                        self.expect_int(index, "index");
                        self.check_assign_value(&elem, op, &val_ty, target.span);
                    }
                    // `ba[i] = x` — the MUTABLE sibling of bytes. Int index, int value (0–255
                    // validated at runtime). Bytes has NO arm here (immutable); bytearray adds one.
                    Ty::ByteArray => {
                        self.expect_int(index, "index");
                        self.check_assign_value(&Ty::Int, op, &val_ty, target.span);
                    }
                    Ty::Str => {
                        self.expect_int(index, "index");
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
                        if let Some((k, v)) = self.param_indexset_kv(&name, target.span) {
                            let idx_ty = self.infer(index);
                            if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                                self.error(
                                    index.span,
                                    format!("index must be {k}, found {idx_ty}"),
                                );
                            }
                            self.check_assign_value(&v, op, &val_ty, target.span);
                        } else {
                            self.error(target.span, format!("cannot index-assign into {name}"));
                        }
                    }
                    other => {
                        // A struct satisfying `IndexSet` (has `index` + `set_index`) is mutable by index.
                        if let Some((k, v)) = self.index_set_kv(&other) {
                            let idx_ty = self.infer(index);
                            if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                                self.error(
                                    index.span,
                                    format!("index must be {k}, found {idx_ty}"),
                                );
                            }
                            self.check_assign_value(&v, op, &val_ty, target.span);
                        } else {
                            self.expect_int(index, "index");
                            self.error(target.span, format!("cannot index-assign into {other}"));
                        }
                    }
                }
            }
            // `p.x = v` — only data fields of a struct are assignable (not methods, not module
            // members). `infer_field` would accept those, so check the field kind here.
            ExprKind::Field { obj, name, .. } => {
                let obj_ty = self.infer(obj);
                match &obj_ty {
                    Ty::Struct(sname, targs) => {
                        let field_ty = self.struct_shape(sname).and_then(|info| {
                            info.fields
                                .iter()
                                .find(|(f, _)| f == name)
                                .map(|(_, ty)| subst(ty, &struct_param_map(info, targs)))
                        });
                        match field_ty {
                            Some(ty) => self.check_assign_value(&ty, op, &val_ty, target.span),
                            None => self.error(
                                target.span,
                                format!(
                                    "cannot assign to '{name}': type {obj_ty} has no field '{name}'"
                                ),
                            ),
                        }
                    }
                    Ty::Unknown => {}
                    other => self.error(
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
                    self.check_assign(t, AssignOp::Eq, ety, span);
                }
            }
            _ => self.error(
                target.span,
                "invalid assignment target (only variables can be assigned)",
            ),
        }
    }

    pub(super) fn check_assign_value(
        &mut self,
        target_ty: &Ty,
        op: AssignOp,
        val_ty: &Ty,
        span: Span,
    ) {
        match op {
            AssignOp::Eq => {
                if !self.assignable(target_ty, val_ty) {
                    self.error(span, format!("cannot assign {val_ty} to {target_ty}"));
                }
            }
            // Numeric compound ops `+= -= *= /= %=` (and str+str for `+=`). No implicit widening:
            // `int <op> float` yields a float, which can't flow back into a concrete int slot —
            // reject it (gap #9), mirroring strict `=` (`x = 1.5`). `/=` inherits this rule, so
            // `int /= float` is rejected (true division would widen the slot).
            AssignOp::PlusEq
            | AssignOp::MinusEq
            | AssignOp::StarEq
            | AssignOp::SlashEq
            | AssignOp::PercentEq => {
                let str_ok = op == AssignOp::PlusEq && *target_ty == Ty::Str && *val_ty == Ty::Str;
                let widens = *target_ty == Ty::Int && *val_ty == Ty::Float;
                let num_ok = target_ty.is_numeric() && val_ty.is_numeric() && !widens;
                // Collection forms mirror `infer_binary`: `list += list` (concat), `list *= int`
                // (repeat), `set -= set` (difference). Compound-assign lowers through the same
                // `Op::Add`/`Op::Mul`/`Op::Sub` opcodes the binary form uses, so the runtime already
                // handles these — only the checker had to be taught to accept them.
                let coll_ok = match (op, target_ty, val_ty) {
                    (AssignOp::PlusEq, Ty::List(a), Ty::List(b)) => compatible(a, b),
                    (AssignOp::StarEq, Ty::List(_), Ty::Int) => true,
                    (AssignOp::MinusEq, Ty::Set(a), Ty::Set(b)) => compatible(a, b),
                    _ => false,
                };
                let known = !target_ty.is_unknown() && !val_ty.is_unknown();
                if known && !str_ok && !num_ok && !coll_ok {
                    let sym = match op {
                        AssignOp::PlusEq => "+=",
                        AssignOp::MinusEq => "-=",
                        AssignOp::StarEq => "*=",
                        AssignOp::SlashEq => "/=",
                        _ => "%=",
                    };
                    self.error(
                        span,
                        format!("cannot apply {sym} to {target_ty} and {val_ty}"),
                    );
                }
            }
            // Bitwise/shift compound ops `&= |= ^= <<= >>=` — int-only, EXCEPT `&= |= ^=` also do
            // set algebra on two `set[T]` (mirrors `infer_binary`'s bitwise arm; `<<= >>=` stay
            // strictly int). Lowers through the same `Op::BitOr`/etc opcodes as the binary form.
            AssignOp::AmpEq
            | AssignOp::PipeEq
            | AssignOp::CaretEq
            | AssignOp::ShlEq
            | AssignOp::ShrEq => {
                let int_ok = *target_ty == Ty::Int && *val_ty == Ty::Int;
                let set_ok = matches!(op, AssignOp::AmpEq | AssignOp::PipeEq | AssignOp::CaretEq)
                    && matches!((target_ty, val_ty), (Ty::Set(a), Ty::Set(b)) if compatible(a, b));
                let known = !target_ty.is_unknown() && !val_ty.is_unknown();
                if known && !int_ok && !set_ok {
                    let sym = match op {
                        AssignOp::AmpEq => "&=",
                        AssignOp::PipeEq => "|=",
                        AssignOp::CaretEq => "^=",
                        AssignOp::ShlEq => "<<=",
                        _ => ">>=",
                    };
                    self.error(
                        span,
                        format!("bitwise operator {sym} requires int operands or two sets, found {target_ty} and {val_ty}"),
                    );
                }
            }
        }
    }

    pub(super) fn check_return(&mut self, value: Option<&Expr>, span: Span) {
        // Pass-1 inference mode: record the return's type, don't diagnose. A bare `return`
        // contributes `Nil`. (Separate flag + field so we don't borrow `collected_rets` across
        // the `&mut self` call to `infer`.)
        if self.inferring_ret {
            let ty = match value {
                Some(e) => self.infer(e),
                None => Ty::Nil,
            };
            self.collected_rets.push(ty);
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
                // Checking-mode: a closure returned into a `fn`-typed return slot binds its
                // unannotated params from the declared return type (source #1). A NON-closure return
                // keeps plain `infer` (NOT `infer_value`): returning `nil` just makes a void fn — it
                // is not "using nil as a value", so it must not get `infer_value`'s nil-rejection on
                // top of check_return's own `function returns nothing` diagnostic.
                let ty = if matches!(e.kind, ExprKind::Closure { .. }) {
                    self.infer_arg(e, Some(&ret))
                } else {
                    // Expected-type checking-mode: thread the declared return type as a hint so a
                    // returned generic ctor / generic fn-call pre-seeds its type params from it —
                    // `fn mk() -> Heap[int]: return Heap([], fn(x, y): x < y)` pins `T=int`. `unify`
                    // no-ops on a `Nil` (void) ret, so setting it unconditionally is safe; pair with
                    // an immediate clear so a non-call return value never leaks the hint.
                    self.expected_hint = Some(ret.clone());
                    let t = self.infer(e);
                    self.expected_hint = None;
                    t
                };
                if ret == Ty::Nil {
                    self.error(e.span, "function returns nothing, cannot return a value");
                } else if !self.assignable_w(&ret, &ty, true) {
                    self.error(e.span, format!("expected return type {ret}, found {ty}"));
                } else if let ExprKind::Ident(name) = &e.kind
                    && !contains_unknown_in_slot(&ret)
                {
                    // PART A: returning a bare empty-collection binding into a CONCRETE collection
                    // return type constrains its element type (the typed-return false-positive guard,
                    // one binding away from the direct-literal `return []`). Drop its pending
                    // annotation requirement.
                    self.drop_empty_site(name);
                }
            }
            None => {
                if ret != Ty::Nil {
                    self.error(span, format!("expected a return value of type {ret}"));
                }
            }
        }
    }

    /// Experimental generators do not support the structured-concurrency / cleanup statements whose
    /// state (nurseries, frame defers) the suspendable generator context does not manage. Reject them
    /// with a clear message rather than mis-execute. Recurses through nested control-flow blocks but
    /// not into nested `fn` definitions (those have their own generator status).
    pub(super) fn check_generator_restrictions(&mut self, body: &[Stmt]) {
        for s in body {
            // A restricted statement can also hide in a `recover:` block in expression position
            // (`x := recover: … defer … `), which the statement structure does not reach — descend
            // into those too so the ban can't be bypassed. (Mirrors the parser's yield detection.)
            let mut recover_blocks = Vec::new();
            crate::ast::stmt_expr_recover_blocks(s, &mut recover_blocks);
            for b in recover_blocks {
                self.check_generator_restrictions(b);
            }
            match &s.kind {
                StmtKind::Defer(_) => self.error(
                    s.span,
                    "`defer` is not supported inside a generator (experimental)",
                ),
                StmtKind::Spawn(_) => self.error(
                    s.span,
                    "`spawn` is not supported inside a generator (experimental)",
                ),
                StmtKind::Parallel { .. } => self.error(
                    s.span,
                    "`parallel:` is not supported inside a generator (experimental)",
                ),
                StmtKind::Wait { .. } => self.error(
                    s.span,
                    "`wait:` is not supported inside a generator (experimental)",
                ),
                StmtKind::If {
                    branches,
                    else_block,
                } => {
                    for (_, b) in branches {
                        self.check_generator_restrictions(b);
                    }
                    if let Some(b) = else_block {
                        self.check_generator_restrictions(b);
                    }
                }
                StmtKind::For { body, .. } | StmtKind::While { body, .. } => {
                    self.check_generator_restrictions(body)
                }
                StmtKind::Match { arms, .. } => {
                    for a in arms {
                        self.check_generator_restrictions(&a.body);
                    }
                }
                _ => {}
            }
        }
    }

    /// Sound "this block provably cannot fall off its end" analysis, used to enforce that a function
    /// with a *declared* non-void return type returns a value on every control-flow path (Option B).
    /// Conservative by design: returns `true` only when a path PROVABLY diverges or returns a value,
    /// so it can never false-positive on valid code (which would break the build). A genuine
    /// fall-through that this misses is an acceptable false-negative (misses the error), not a hazard.
    ///
    /// A block terminates iff ANY statement in it terminates (the first terminator dominates; no
    /// dead-code diagnosis — out of scope).
    pub(super) fn block_terminates(body: &[Stmt]) -> bool {
        body.iter().any(Self::stmt_terminates)
    }

    pub(super) fn stmt_terminates(s: &Stmt) -> bool {
        match &s.kind {
            // `return <expr>` and bare `return` both leave the function (a bare `return` under a
            // non-nil signature is already its own error in `check_return`; don't double-report).
            StmtKind::Return(_) => true,
            // An `if` terminates only with an `else` AND every branch body + the else body terminate.
            StmtKind::If {
                branches,
                else_block: Some(eb),
            } => {
                branches.iter().all(|(_, b)| Self::block_terminates(b))
                    && Self::block_terminates(eb)
            }
            // No `else` -> the all-conditions-false path falls through.
            StmtKind::If {
                else_block: None, ..
            } => false,
            // A `match` terminates iff every arm body terminates. Exhaustiveness (coverage by the
            // unguarded arms) is enforced separately by the match checker, so once every arm
            // terminates the eventually-chosen arm terminates too.
            StmtKind::Match { arms, .. } => arms.iter().all(|a| Self::block_terminates(&a.body)),
            // `while true:` with no reachable `break` loops forever (never falls through).
            StmtKind::While { cond, body } => {
                matches!(cond.kind, ExprKind::Bool(true)) && !Self::block_has_break(body)
            }
            // A trailing `exit(...)` / `panic(...)` diverges (neither returns to the caller). A
            // narrow, syntactic special-case on the callee name; a user shadowing the name only
            // causes an acceptable false-negative (missed error), never a false-positive.
            StmtKind::Expr(e) => Self::expr_is_diverging_call(e),
            _ => false,
        }
    }

    /// Whether `e` is a call to a diverging builtin — `exit` (`std.os.exit`, typed `nil`, never
    /// returns) or `panic` (raises a recoverable `RuntimeError`, bottom-typed, never returns
    /// normally). Matches both a bare `exit(...)`/`panic(...)` and the module-qualified
    /// `os.exit(...)` form. A narrow, syntactic special-case: a user shadowing the name only causes
    /// an acceptable false-negative (a missed error), never a false-positive that breaks a valid build.
    pub(super) fn expr_is_diverging_call(e: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &e.kind {
            match &callee.kind {
                ExprKind::Ident(name) => name == "exit" || name == "panic",
                // Only `exit` has a module-qualified form (`os.exit`); `panic` is bare-call only.
                // A user method named `panic` (`obj.panic()`) compiles to CallMethod and RETURNS
                // normally, so treating it as divergence would suppress missing-return and let a
                // typed body fall through to nil. Keep the Field arm to `exit`.
                ExprKind::Field { name, .. } => name == "exit",
                _ => false,
            }
        } else {
            false
        }
    }

    /// Whether `body` contains a `break` that targets THIS loop level — descends into `if`/`match`
    /// arms (a `break` there exits the enclosing loop) but NOT into nested `while`/`for` loops (their
    /// `break` is theirs) nor into closures/nested fns (those open a fresh loop context).
    pub(super) fn block_has_break(body: &[Stmt]) -> bool {
        body.iter().any(Self::stmt_has_break)
    }

    pub(super) fn stmt_has_break(s: &Stmt) -> bool {
        match &s.kind {
            StmtKind::Break => true,
            StmtKind::If {
                branches,
                else_block,
            } => {
                branches.iter().any(|(_, b)| Self::block_has_break(b))
                    || else_block.as_deref().is_some_and(Self::block_has_break)
            }
            StmtKind::Match { arms, .. } => arms.iter().any(|a| Self::block_has_break(&a.body)),
            // A nested `while`/`for` owns its own `break`; do not descend.
            _ => false,
        }
    }

    /// `yield <expr>` — legal only inside a generator function (one whose return type is
    /// `Iterator[T]`); the operand must be assignable to the element type `T`.
    pub(super) fn check_yield(&mut self, e: &Expr, span: Span) {
        let ty = self.infer(e);
        // `in_generator` (not `yield_ty.is_some()`) is the in-bounds signal: during return-type
        // inference the element type is not yet pinned (`yield_ty` is `None`) but a `yield` is still
        // legal and its type must be COLLECTED to seed the inferred `Iterator[T]`.
        if !self.in_generator {
            self.error(span, "`yield` can only appear inside a generator function");
            return;
        }
        // Inference mode: gather every yield's type; the first pins `T` (strict-first-yield), the
        // rest are validated in pass 2 (below) once `yield_ty` is seeded from the inferred sig.
        if self.inferring_ret {
            self.collected_yields.push(ty);
            return;
        }
        // Pass 2: validate each yield against the pinned element type `T`. Plain `assignable` (NOT a
        // widening variant): there is no `CoerceFloat` emitted at a `yield`, so an `int` yielded under
        // an inferred/annotated `float` `T` would run int-under-float — a strict `assignable` (which
        // makes `int` vs `float` incompatible) rejects it instead of silently coercing.
        if let Some(elem) = self.yield_ty.clone()
            && !self.assignable(&elem, &ty)
        {
            self.error(e.span, format!("expected yield type {elem}, found {ty}"));
        }
    }

    pub(super) fn check_fn_body(&mut self, decl: &FnDecl, self_ty: Option<Ty>, sig: FnSig) {
        // Enter the sig's type params (NOT `decl.type_params`): `fn_sig` folds any `where T: Bound`
        // clause into them, so the body sees a `where`-bounded param as satisfying its bound (e.g. a
        // `where T: Comparable` param may use `<` in the body). Same names as `decl.type_params`
        // (the merge only adds bounds), so the reserved/shadow checks in `fn_sig` still cover them.
        let saved_tps = self.enter_type_params(&sig.type_params);
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
                    if !bounds.iter().any(|e| e.name == b.name && e.args == b.args) {
                        bounds.push(b.clone());
                    }
                }
            }
        }
        let saved_ret = std::mem::replace(&mut self.current_ret, sig.ret.clone());
        // A generator (`is_generator`, i.e. its body contains `yield`) has return type `Iterator[T]` —
        // either declared explicitly (`-> Iterator[T]`) or INFERRED by strict-first-yield (stored back
        // into `sig.ret` by `infer_generator_ret`). Recover `T` as the per-yield element type. The `_`
        // arm now fires only for a WRONG EXPLICIT annotation (`-> int`): inference always yields an
        // `Iterator[T]` sig, so an un-annotated generator can no longer reach it.
        let new_yield_ty = if decl.is_generator {
            match &sig.ret {
                Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                    Some(args[0].clone())
                }
                _ => {
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
        // A nested function checked while pass-1 is inferring an *outer* function's return must not
        // feed the outer `collected_rets` — this body's `return`s are diagnosed, not collected.
        let saved_inferring = std::mem::replace(&mut self.inferring_ret, false);
        // A function body opens a fresh loop context: a loop enclosing this fn's *definition* must
        // not make a `break`/`continue` in the body legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        // A nested fn opens a fresh `?`-target context: a `?` in this body targets this function,
        // not an enclosing recover at the definition site.
        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
        self.push_scope();
        // Editor hover: record the function's OWN signature at its decl-site name token (no-op
        // off-probe; parity-neutral — `name_span` is runtime-inert). Covers free fns AND methods,
        // both routed through here. For a method, `record_method_decl_hover` (called from the
        // struct/enum/newtype arm BEFORE this) already latched the receiver-stripped sig first
        // (first-hit-wins in `hover_record_at`), so this is a harmless no-op there; for a free fn
        // there is no prior record, so this produces the previously-missing fn-name hover.
        if self.hover_probe.is_some() {
            let fty = Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
                labels: crate::checker::FnLabels::default(),
            };
            self.hover_record_at(decl.name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
        for (i, param) in decl.params.iter().enumerate() {
            let ty = if param.name == "self" {
                self_ty.clone().unwrap_or(Ty::Unknown)
            } else {
                sig.params.get(i).cloned().unwrap_or(Ty::Unknown)
            };
            // A constant-literal default must itself be assignable to the parameter's type — checked
            // here (where type params are in scope) so a wrong-typed default is caught at the
            // declaration even when every call overrides it.
            if let Some(def) = &param.default {
                let actual = self.infer(def);
                // One-way int→float widening (scalar sink): a `float` param accepts an int default,
                // coerced to f64 at the callee prologue (the default is desugar-spliced into the call
                // when omitted). Mirrors the typed-`let`/arg/return/struct-field sinks.
                if !matches!(ty, Ty::Unknown) && !self.assignable_w(&ty, &actual, true) {
                    self.error(
                        def.span,
                        format!(
                            "default value for parameter '{}': expected {ty}, found {actual}",
                            param.name
                        ),
                    );
                }
            }
            // Editor hover: record the param's declared type at its DECL-site name span (no-op
            // off-probe; covers free fns AND methods, both routed through check_fn_body).
            self.hover_record_at(param.name_span, &ty, HoverKind::Param, None);
            self.declare(&param.name, ty);
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
            let ty = self.infer(e);
            if ret == Ty::Nil {
                // A NON-nil expr against `-> nil` is a void fn that actually returns a value —
                // reject it, mirroring the multiline `return <expr>` path. A nil-typed inline expr
                // (e.g. a bare void call) implicitly returns nil and stays legal.
                if ty != Ty::Nil && !ty.is_unknown() {
                    self.error(e.span, "function returns nothing, cannot return a value");
                }
            } else if !self.assignable_w(&ret, &ty, true) {
                self.error(e.span, format!("expected return type {ret}, found {ty}"));
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
        if !decl.is_generator
            && !decl.inline_expr_body
            && decl.ret.is_some()
            && sig.ret != Ty::Nil
            && !Self::block_terminates(&decl.body)
            && let Some(span) = decl.body.first().map(|s| s.span)
        {
            let ret = &sig.ret;
            self.error(
                span,
                format!(
                    "function '{}' has return type {ret} but can fall off the end without returning a value; add an explicit `return`, or use a closure `fn() -> {ret}: <expr>` which implicitly returns its expression body",
                    decl.name
                ),
            );
        }
        self.finalize_empty_coll_sites();
        self.finalize_hover_pending();
        self.pop_scope();
        self.current_ret = saved_ret;
        self.yield_ty = saved_yield;
        self.in_generator = saved_ig;
        self.inferring_ret = saved_inferring;
        self.loop_depth = saved_loop_depth;
        self.recover_depth = saved_recover;
        self.exit_type_params(saved_tps);
    }

    /// The element type produced by iterating `iter` in a `for` loop.
    /// The per-iteration bindings of a `for` loop: one name for the common form, or two
    /// (`for k, v in m:`) to destructure a map's entries. A range/list/str binds a single value; a
    /// map binds its key (1 name) or key+value (2 names). Any other arity/iterand combination is an
    /// error (a dummy `Unknown` binding is returned per name so checking continues).
    /// If `ty` is a user struct with a method `next(self) -> Option[E]` (self-only, no extra params),
    /// return the element type `E` (with the struct's type arguments substituted in). This is the
    /// structural "iterator protocol": such a struct is iterable in a `for`. Mirrors the type-arg
    /// substitution `infer_method_call` does for the `Ty::Struct` arm.
    pub(super) fn struct_iter_elem(&self, ty: &Ty) -> Option<Ty> {
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
        let info = self.structs.get(name)?;
        let sig = info.methods.get("next")?;
        if sig.params.len() != 1 {
            return None; // (self) only — no extra args
        }
        let Ty::Option(inner) = &sig.ret else {
            return None;
        };
        let map = struct_param_map(info, targs);
        Some(subst(inner, &map))
    }

    /// The element type a user struct's structural `iter(self) -> Iterator[E]` produces, or `None`.
    /// Sibling of [`struct_iter_elem`](Self::struct_iter_elem); used so a struct with `iter` but no
    /// `next` is recognised as `Iterable` and bound in `for`. `Iterator` is not a registered struct,
    /// so this only matches real user structs.
    pub(super) fn struct_iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
        if name == "Iterator" {
            return None; // the existential cursor — handled by `iter_elem`, not as a user struct
        }
        let info = self.structs.get(name)?;
        let sig = info.methods.get("iter")?;
        if sig.params.len() != 1 {
            return None; // (self) only
        }
        let Ty::Struct(rname, rargs) = &sig.ret else {
            return None;
        };
        if rname != "Iterator" || rargs.len() != 1 {
            return None; // must declare `-> Iterator[E]`
        }
        let map = struct_param_map(info, targs);
        Some(subst(&rargs[0], &map))
    }

    /// The element type of ANY `Iterable` value — the single source of truth for `Iterable`
    /// conformance and the `Iterable`-driven `for`. A built-in collection, an `Iterator[T]`
    /// existential, or a struct with structural `next` all flow through [`iter_elem`](Self::iter_elem)
    /// (every `Iterator` is `Iterable` via `iter() == self`); a struct with only `iter` flows through
    /// [`struct_iterable_elem`](Self::struct_iterable_elem). `None` ⇒ not iterable.
    pub(super) fn iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        self.iter_elem(ty).or_else(|| self.struct_iterable_elem(ty))
    }

    /// What iterating `ty` yields per step — the `Iterator` element type. Built-in collections yield
    /// intrinsically (list/set → element, str → str, map → key, matching the single-variable `for`);
    /// a user struct yields via its structural `next(self) -> Option[E]`. `None` ⇒ not iterable. This
    /// is the single source of truth shared by `for`-binding, `satisfies(Iterator)`, and the
    /// `Iterator[T]` element-recovery in `infer_generic_call`.
    pub(super) fn iter_elem(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::List(e) | Ty::Set(e) => Some((**e).clone()),
            Ty::Str => Some(Ty::Str),
            // `bytes`/`bytearray` iterate to `int` (0–255), like Python.
            Ty::Bytes | Ty::ByteArray => Some(Ty::Int),
            Ty::Map(k, _) => Some((**k).clone()),
            // `Iterator[T]` value (a generator result): element type is its single type argument.
            Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                Some(args[0].clone())
            }
            _ => self.struct_iter_elem(ty),
        }
    }

    /// The `(key, value)` types of `obj[k]` — the `Index` protocol's args. Built-in collections
    /// intrinsically (list/str index by int, map by its key); a user struct via its structural
    /// `index(self, K) -> V`. Single source of truth for `Index` conformance, `infer_index`, and the
    /// `Index[K,V]` arg-recovery in generic calls. `None` ⇒ not indexable.
    pub(super) fn index_kv(&self, ty: &Ty) -> Option<(Ty, Ty)> {
        match ty {
            Ty::List(e) => Some((Ty::Int, (**e).clone())),
            Ty::Str => Some((Ty::Int, Ty::Str)),
            // `bytes[i]`/`bytearray[i]` yield an `int` (0–255).
            Ty::Bytes | Ty::ByteArray => Some((Ty::Int, Ty::Int)),
            Ty::Map(k, v) => Some(((**k).clone(), (**v).clone())),
            Ty::Struct(name, targs) => {
                let info = self.structs.get(name)?;
                let sig = info.methods.get("index")?;
                if sig.params.len() != 2 {
                    return None; // (self, key)
                }
                let map = struct_param_map(info, targs);
                Some((subst(&sig.params[1], &map), subst(&sig.ret, &map)))
            }
            _ => None,
        }
    }

    /// The `(key, value)` types of a mutable `obj[k] = v` — the `IndexSet` protocol's args. Built-in
    /// `list`/`map` are mutable intrinsically (handled directly in `check_assign`); this resolves the
    /// struct case via `set_index(self, K, V)`. `IndexSet` *requires* `index` too (Rust `IndexMut: Index`):
    /// a plain `=` only calls `set_index`, but a compound `b[k] += v` reads via `index` first, so a
    /// struct missing `index` would type-check then crash. `None` ⇒ not index-assignable.
    pub(super) fn index_set_kv(&self, ty: &Ty) -> Option<(Ty, Ty)> {
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
        let info = self.structs.get(name)?;
        let sig = info.methods.get("set_index")?;
        if sig.params.len() != 3 {
            return None; // (self, key, val)
        }
        // Must also be readable — `index(self, key) -> val` — or compound index-assign would crash.
        let read = info.methods.get("index")?;
        if read.params.len() != 2 {
            return None; // (self, key)
        }
        let map = struct_param_map(info, targs);
        Some((subst(&sig.params[1], &map), subst(&sig.params[2], &map)))
    }

    /// The result type of `obj[a..b]` — the `Slice` protocol's arg. `list[T] → list[T]`, `str → str`;
    /// a user struct via `slice(self, int, int) -> R`. `None` ⇒ not sliceable.
    pub(super) fn slice_result(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            // `bytes[a:b:c]` yields a new `bytes`; `bytearray` slices to a new `bytearray`;
            // `list`/`str` slice to themselves.
            Ty::List(_) | Ty::Str | Ty::Bytes | Ty::ByteArray => Some(ty.clone()),
            Ty::Struct(name, targs) => {
                let info = self.structs.get(name)?;
                let sig = info.methods.get("slice")?;
                // The `Slice` protocol fixes the bounds: `slice(self, int? , int?, int?) -> R`.
                // Both engines always pass three `Option[int]` components (start/end/step, each
                // `None` when omitted), so a non-conforming signature (wrong arity or non-`int?`
                // bounds) is not a valid `Slice` impl — reject rather than green-light a crash.
                let opt_int = Ty::option(Ty::Int);
                if sig.params.len() != 4 || sig.params[1..=3].iter().any(|p| *p != opt_int) {
                    return None;
                }
                let map = struct_param_map(info, targs);
                Some(subst(&sig.ret, &map))
            }
            _ => None,
        }
    }

    /// Resolve a bound's type argument (the `T` in `Iterator[T]`) to a `Ty` with the *callee's* type
    /// parameters in scope, so a bare param name becomes `Ty::Param` even at a call site where those
    /// params aren't otherwise visible. Restores the prior scope before returning.
    pub(super) fn resolve_bound_arg(&mut self, arg: &Type, tps: &[TypeParam], span: Span) -> Ty {
        let saved = self.enter_type_params(tps);
        let ty = self.resolve_type(arg, span);
        self.exit_type_params(saved);
        ty
    }

    pub(super) fn for_bindings(&mut self, vars: &[String], iter: &Expr) -> Vec<(String, Ty)> {
        let unknowns = |vars: &[String]| vars.iter().map(|v| (v.clone(), Ty::Unknown)).collect();
        // Ranges are syntactic and always yield a single int.
        if let ExprKind::Range { start, end } = &iter.kind {
            self.expect_int(start, "range bound");
            self.expect_int(end, "range bound");
            if vars.len() != 1 {
                self.error(
                    iter.span,
                    "a range binds a single loop variable; `for k, v` needs a map",
                );
                return unknowns(vars);
            }
            return vec![(vars[0].clone(), Ty::Int)];
        }
        let it = self.infer(iter);
        match &it {
            Ty::Map(k, v) => match vars.len() {
                1 => vec![(vars[0].clone(), (**k).clone())],
                2 => vec![
                    (vars[0].clone(), (**k).clone()),
                    (vars[1].clone(), (**v).clone()),
                ],
                _ => {
                    self.error(
                        iter.span,
                        "a `for` over a map binds one (key) or two (key, value) names",
                    );
                    unknowns(vars)
                }
            },
            // Tuple-destructuring `for`: over a `list[(A, B, …)]` with N>1 names, bind each name to
            // the matching tuple element. One name still binds the whole tuple (the `Ty::List` arm
            // below). A list of non-tuples (or an arity mismatch) with N>1 names is an error.
            Ty::List(inner) if vars.len() > 1 => match &**inner {
                Ty::Tuple(ts) if ts.len() == vars.len() => {
                    vars.iter().cloned().zip(ts.iter().cloned()).collect()
                }
                Ty::Tuple(ts) => {
                    self.error(iter.span, format!(
                        "tuple-destructuring `for` binds {} names but the element has {} ({inner})",
                        vars.len(), ts.len()
                    ));
                    unknowns(vars)
                }
                Ty::Unknown => unknowns(vars),
                _ => {
                    self.error(
                        iter.span,
                        format!("`for k, v` requires a map or a list of tuples, found {it}"),
                    );
                    unknowns(vars)
                }
            },
            Ty::Str | Ty::Bytes | Ty::ByteArray | Ty::Set(_) | Ty::Channel(_)
                if vars.len() != 1 =>
            {
                if matches!(it, Ty::Channel(_)) {
                    self.error(iter.span, "a channel iterator binds a single loop variable");
                } else {
                    self.error(iter.span, format!("`for k, v` requires a map, found {it}"));
                }
                unknowns(vars)
            }
            Ty::List(inner) => vec![(vars[0].clone(), (**inner).clone())],
            Ty::Set(elem) => vec![(vars[0].clone(), (**elem).clone())],
            Ty::Str => vec![(vars[0].clone(), Ty::Str)],
            // `for x in bytes:`/`for x in bytearray:` bind a single `int` (0–255).
            Ty::Bytes | Ty::ByteArray => vec![(vars[0].clone(), Ty::Int)],
            // `for v in ch:` over a `Channel[T]` blocks for each value and ends when the channel is
            // closed-and-drained (Go's `for v := range ch`). Binds a single element of type `T`.
            Ty::Channel(elem) => vec![(vars[0].clone(), (**elem).clone())],
            Ty::Unknown => unknowns(vars),
            Ty::Param(name) => {
                // A type parameter bounded `S: Iterator[T]` is iterable; bind the loop var to its
                // declared element type `T` (resolved with the surrounding params in scope).
                let arg = self.type_params.get(name).and_then(|bs| {
                    // `S: Iterator[T]` OR `S: Iterable[T]` is for-iterable; both carry the element as
                    // their single bound arg (an `Iterable` is driven through a one-time `.iter()`).
                    bs.iter()
                        .find(|b| b.name == "Iterator" || b.name == "Iterable")
                        .and_then(|b| b.args.first().cloned())
                });
                match arg {
                    Some(_) if vars.len() != 1 => {
                        self.error(iter.span, "a struct iterator binds a single loop variable");
                        unknowns(vars)
                    }
                    Some(t) => vec![(vars[0].clone(), self.resolve_type(&t, iter.span))],
                    None => {
                        self.error(iter.span, format!("cannot iterate over {it}"));
                        unknowns(vars)
                    }
                }
            }
            // A generator result `Iterator[T]` (experimental, VM-only) binds a single element of T.
            Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                if vars.len() != 1 {
                    self.error(
                        iter.span,
                        "a generator iterator binds a single loop variable",
                    );
                    return unknowns(vars);
                }
                vec![(vars[0].clone(), args[0].clone())]
            }
            _ if self.struct_iter_elem(&it).is_some() => {
                // A user struct with `next(self) -> Option[E]` is iterable; it binds a single element.
                // Checked FIRST so a struct with BOTH `next` and `iter` keeps the existing `next()`
                // fast path (back-compat precedence).
                let elem = self
                    .struct_iter_elem(&it)
                    .expect("guarded by the match arm");
                if vars.len() != 1 {
                    self.error(iter.span, "a struct iterator binds a single loop variable");
                    return unknowns(vars);
                }
                vec![(vars[0].clone(), elem)]
            }
            _ if self.struct_iterable_elem(&it).is_some() => {
                // A pure-`Iterable` struct: `iter(self) -> Iterator[E]` but NO `next`. Driven by a
                // one-time `.iter()` then the cursor's `next()` (the additive `Iterable` for-case).
                let elem = self
                    .struct_iterable_elem(&it)
                    .expect("guarded by the match arm");
                if vars.len() != 1 {
                    self.error(iter.span, "a struct iterator binds a single loop variable");
                    return unknowns(vars);
                }
                vec![(vars[0].clone(), elem)]
            }
            other => {
                self.error(iter.span, format!("cannot iterate over {other}"));
                unknowns(vars)
            }
        }
    }

    /// How a `match` is being checked, derived from the scrutinee's type.
    ///
    /// `patterns` are the arms' top-level patterns — used ONLY to recover exhaustiveness when the
    /// scrutinee is un-inferable (`Ty::Unknown`, e.g. an unannotated closure param): a `Skip` there
    /// would bypass coverage entirely (soundness hole — `match x: E.A: ..` over `enum E{A,B}` checked
    /// ok then trapped at runtime). See `reconstruct_unknown_kind`.
    pub(super) fn match_kind(&mut self, scrutinee: &Expr, patterns: &[&Pattern]) -> MatchKind {
        let sty = self.infer(scrutinee);
        match &sty {
            Ty::Enum(name, targs) => {
                let map = self.enum_param_map(name, targs);
                let variants = self
                    .enums
                    .get(name)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| {
                        let payload = self.variants[&(name.clone(), v.clone())]
                            .payload
                            .iter()
                            .map(|p| subst(p, &map))
                            .collect();
                        (v, payload)
                    })
                    .collect();
                MatchKind::Variants {
                    label: name.clone(),
                    variants,
                }
            }
            Ty::Result(ok, err) => MatchKind::Variants {
                label: "Result".into(),
                variants: HashMap::from([
                    ("Ok".into(), vec![(**ok).clone()]),
                    ("Err".into(), vec![(**err).clone()]),
                ]),
            },
            Ty::Option(inner) => MatchKind::Variants {
                label: "Option".into(),
                variants: HashMap::from([
                    ("Some".into(), vec![(**inner).clone()]),
                    ("None".into(), vec![]),
                ]),
            },
            // int/str/bool scrutinees match against literal patterns (+ a `_` wildcard).
            Ty::Int => MatchKind::Literal(Ty::Int),
            Ty::Str => MatchKind::Literal(Ty::Str),
            Ty::Bool => MatchKind::Literal(Ty::Bool),
            Ty::Tuple(tys) => MatchKind::Tuple(tys.clone()),
            // Un-inferable scrutinee: rather than skip exhaustiveness outright (a soundness hole),
            // reconstruct a concrete kind from the arm patterns when they unambiguously name a single
            // known enum or are literals — so the normal coverage check applies. Genuinely unknowable
            // cases still fall back to `Skip`.
            Ty::Unknown => self.reconstruct_unknown_kind(patterns, scrutinee.span),
            other => {
                self.error(
                    scrutinee.span,
                    format!("cannot match on non-enum type {other}"),
                );
                MatchKind::Skip
            }
        }
    }

    /// Classify a match over a residual un-inferable (`Ty::Unknown`) scrutinee — one that §3 closure
    /// inference did NOT pin (e.g. a tuple-element binding `a` from `(a, b)`, or an `Unknown`-typed
    /// expression). The top-level scrutinee arm goes through this path (not `bind_subpattern`), so it
    /// is the OTHER half of the §4.1 structural-over-`Unknown` reject (the nested half is in
    /// `bind_subpattern`):
    /// - **Any STRUCTURAL arm** (a tuple, or a real variant — qualified, builtin `Ok`/`Err`/`Some`/
    ///   `None`, an owned bare variant, or a `Name(..)` with a payload) tests a shape/tag against a
    ///   value whose type we can't prove → it traps at runtime on a wrong shape (a trailing `_` cannot
    ///   rescue it). Reject; annotate the enclosing param. Returns `Skip` afterwards so the arm bodies
    ///   still bind permissively (no cascade).
    /// - **Only literal/range arms** → `Literal(first-arm scalar)`: the existing literal-domain rule
    ///   then makes a non-`_` match non-exhaustive AND a heterogeneous arm (`1` + `"b"`) a literal
    ///   mismatch — restoring the pre-`OpenScrutinee` behaviour.
    /// - **Only bindings / `_`** → `Skip` (a bare-ident catch-all closes it; nothing to enforce).
    ///
    /// Top-level `Or` alternatives are flattened first. `&mut self` (it may emit the §4.1 reject).
    pub(super) fn reconstruct_unknown_kind(
        &mut self,
        patterns: &[&Pattern],
        span: Span,
    ) -> MatchKind {
        // Flatten top-level or-alternatives into a flat head list.
        let mut heads: Vec<&Pattern> = Vec::new();
        for p in patterns {
            match p {
                Pattern::Or(alts) => heads.extend(alts.iter()),
                other => heads.push(other),
            }
        }
        let mut lit_ty: Option<Ty> = None;
        // The kind word of the FIRST structural arm seen (`"tuple"` / `"variant"`), if any.
        let mut structural: Option<&'static str> = None;
        for h in &heads {
            match h {
                Pattern::Tuple(_) => {
                    structural.get_or_insert("tuple");
                }
                Pattern::Variant {
                    name,
                    enum_name,
                    module_name,
                    bindings,
                } => {
                    // A real variant pattern (qualified, builtin, owned, or carrying a payload) is
                    // structural; a bare name that is NOT a known variant and binds nothing is a
                    // catch-all binding → not structural.
                    let is_variant = module_name.is_some()
                        || enum_name.is_some()
                        || matches!(name.as_str(), "Ok" | "Err" | "Some" | "None")
                        || self.variant_owners.contains_key(name)
                        || !bindings.is_empty();
                    if is_variant {
                        structural.get_or_insert("variant");
                    }
                }
                Pattern::Literal(lit) => {
                    if lit_ty.is_none() {
                        lit_ty = Some(lit_pattern_ty(lit));
                    }
                }
                Pattern::Range { .. } if lit_ty.is_none() => {
                    lit_ty = Some(Ty::Int);
                }
                // Ident/Wildcard/(nested Or) — no structural or literal signal.
                _ => {}
            }
        }
        if let Some(kind) = structural {
            self.error(
                span,
                format!(
                    "cannot match a {kind} pattern on a value of un-inferable type; annotate it"
                ),
            );
            return MatchKind::Skip;
        }
        if let Some(t) = lit_ty {
            return MatchKind::Literal(t);
        }
        MatchKind::Skip
    }

    /// The substitution from a generic enum's type parameters to a concrete instantiation's type
    /// arguments (`Tree[int]` ⇒ `{T: int}`). Empty for a non-generic enum.
    pub(super) fn enum_param_map(&self, name: &str, targs: &[Ty]) -> HashMap<String, Ty> {
        // `enum_type_params_of` adds the miss-only owning-module fallback (gap #4) so a named-fn-
        // imported enum value binds its params identically to a whole-module import.
        self.enum_type_params_of(name)
            .map(|tps| {
                tps.iter()
                    .map(|tp| tp.name.clone())
                    .zip(targs.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The variant→payload map for an enum/Option/Result type, else `None`. Shared by `match_kind`
    /// and the nested-pattern checker (gap #15) so they agree on what counts as a variant.
    pub(super) fn variants_of(&self, ty: &Ty) -> Option<HashMap<String, Vec<Ty>>> {
        match ty {
            Ty::Enum(name, targs) => {
                let map = self.enum_param_map(name, targs);
                let vs = self.enums.get(name)?;
                Some(
                    vs.iter()
                        .map(|v| {
                            let payload = self.variants[&(name.clone(), v.clone())]
                                .payload
                                .iter()
                                .map(|p| subst(p, &map))
                                .collect();
                            (v.clone(), payload)
                        })
                        .collect(),
                )
            }
            Ty::Result(ok, err) => Some(HashMap::from([
                ("Ok".into(), vec![(**ok).clone()]),
                ("Err".into(), vec![(**err).clone()]),
            ])),
            Ty::Option(inner) => Some(HashMap::from([
                ("Some".into(), vec![(**inner).clone()]),
                ("None".into(), vec![]),
            ])),
            _ => None,
        }
    }
}
