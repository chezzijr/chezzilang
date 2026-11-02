//! Checker: method calls.
//! Split out of `src/checker/expr.rs`. `super::*` is the parent module.

use super::expr::NativeHandleMethod;
use super::expr::RWSHARED_VIEWS;
use super::expr::RWSHARED_WALKS;
use super::expr::callable_slots;
use super::*;
use crate::base::builtin_method::RwSharedPrim;

impl Checker {
    /// Type-check an instance method call `obj.method(args)`. `type_args` are the explicit
    /// member-level turbofish (`obj.method[A, B](x, y)`) — non-empty only when the parser stole a
    /// type list after the `.method`; they seed a generic method's own `[U]` params. A method-level
    /// turbofish on a BUILTIN or non-generic member (no own type params) is rejected — including the
    /// `.iter` fast-path (`xs.iter[int]()`), guarded BEFORE that fast-path runs.
    #[allow(clippy::too_many_arguments)] // receiver + method + name span + args + targs + span + hint
    pub(super) fn infer_method_call(
        &mut self,
        obj: &Expr,
        method: &str,
        // Source span of the method-NAME token (the `Field` callee's `name_span`), used ONLY to
        // anchor the editor hover record at the method name; never affects checking.
        name_span: Span,
        args: &[Expr],
        type_args: &[Ty],
        span: Span,
        // The expected-type hint of the enclosing `let`/annotation, threaded through to a generic
        // MODULE fn call so a return-type-only type param can be solved from it. `None` everywhere
        // else (a struct/enum method call ignores it, as before).
        hint: Option<&Ty>,
    ) -> Ty {
        if method == "len" {
            self.range_literal_fault(obj, true);
        }
        let obj_ty = self.infer(obj);
        // M3.1/M3.4 — an aggregate mutated in a task (`xs.push(v)`, …), a module global or a
        // captured local, is legal: the task writes the one object (Go). A member-level turbofish (`obj.method[A, B](...)`) is
        // only valid on a USER method that declares its OWN `[U]` type params. On a builtin
        // (`xs.len[int]()`, `xs.iter[int]()`) or a non-generic user method it is an arity error —
        // checked BEFORE the `.iter` fast-path below so `xs.iter[int]()` is rejected like
        // `xs.len[int]()` (it was silently swallowed otherwise).
        if !type_args.is_empty() && self.member_own_type_params(&obj_ty, method) == Some(false) {
            self.infer_all(args);
            self.error(
                span,
                format!("method '{method}' takes no type argument(s) (it declares no own type parameters)"),
            );
            return Ty::Unknown;
        }
        // `.iter()` on a BUILTIN receiver (a collection, or an `Iterator[T]` cursor, where it
        // returns the cursor itself): a fresh cursor typed `Iterator[T]`. A struct, an enum and a
        // protocol slot are not typed here; each reaches its own arm of the dispatch below.
        if method == Hook::Iter.method()
            && args.is_empty()
            && let Some(elem) = self.builtin_iter_elem(&obj_ty)
        {
            return Ty::Struct("Iterator".to_string(), vec![elem]);
        }
        // `c.contains(x)` on a builtin container asks the same needle rule as `x in c` and the
        // `Contains[X]` bound (`builtin_contains_verdict`). bytes and bytearray have no such method.
        if self.hook_requirement("Contains", method).is_some()
            && args.len() == 1
            && !matches!(obj_ty, Ty::Bytes | Ty::ByteArray)
        {
            let item = self.infer(&args[0]);
            if let Some(verdict) = self.builtin_contains_verdict(&obj_ty, &item) {
                match verdict {
                    Ok(()) => {}
                    Err(None) => {
                        let [i_s, r_s] = Ty::render_distinct([&item, &obj_ty]);
                        self.error(span, format!("cannot test membership of {i_s} in {r_s}"));
                    }
                    Err(Some(why)) => self.error(
                        span,
                        format!("contains() compares {obj_ty} elements for equality — {why}"),
                    ),
                }
                return Ty::Bool;
            }
        }
        match &obj_ty {
            // `module.fn(args)` is a plain call on the member — no `self`.
            Ty::Module(mname) => {
                let sig = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id));
                // TICKET-196: the module's one export record for the slot, cloned here so the
                // `sig` borrow ends before the first `&mut self` call below.
                let member = sig.and_then(|s| s.member(method)).cloned();
                let fsig = member.as_ref().and_then(|m| m.certain_fn.clone());
                let vty = member.as_ref().map(|m| m.ty.clone());
                // Editor hover (CASE 2): record `module.fn`'s native signature at the method name —
                // covers plain and generic module fns.
                if let Some(f) = &fsig {
                    self.record_method_hover(name_span, f);
                }
                if let Some(fsig) = fsig {
                    let Some(bound) = self.bind_call(fsig.slots.as_deref(), method, args, 0, span)
                    else {
                        return Ty::Unknown;
                    };
                    let args: &[Expr] = &bound;
                    // A generic module function (`cmp.max`): infer its type parameters from the
                    // arguments, enforce bounds, and substitute into the return type.
                    if !fsig.type_params.is_empty() {
                        // A module-qualified generic fn call (`m.f(...)`): thread BOTH the member-level
                        // turbofish (`m.f[int]()`) and the expected-type hint — a type param that
                        // appears ONLY in the return type (`fn empty_list[T]() -> List[T]`) is otherwise
                        // unsolvable and leaked `List[T]` into a user-facing type.
                        // M24 Task 3: a witness-needing callee records here exactly like a bare one —
                        // the entry is keyed by the CALLING module, and the compiler resolves the
                        // callee's hidden arity through this same module bind.
                        // The pin the "not determined here" diagnostic may suggest is spelled with
                        // the MODULE prefix (`lib.empty[Counter]()`); the bare form parses as a
                        // free call and dead-ends on "'empty' takes no type arguments".
                        let recv = WitnessCallee::Dotted(mname.clone());
                        return self.infer_generic_call(
                            method, &fsig, args, type_args, name_span, span, hint, recv,
                        );
                    }
                    // Float params are coerced at the callee's prologue. Honor an optional trailing
                    // tail (`min_params < params.len()`, e.g. `request.get(url, timeout_ms?)`); for
                    // plain sigs `min_params == params.len()`, identical to the old exact check.
                    if fsig.c_variadic {
                        self.check_c_variadic_args(method, &fsig.params, args, span);
                    } else {
                        self.check_args_range(method, &fsig.params, fsig.min_params, args, span);
                    }
                    return fsig.ret;
                }
                // W7-21 — a module GLOBAL that HOLDS a function value is callable through the module
                // (`m.G()`), like CPython's `m.G()` and Go's `pkg.G()`. Editor hover, same as the `fsig` path above: the
                // member's own `Ty::Func` IS what `record_method_hover` would build from an `FnSig`,
                // so record it directly (no doc — a slot without `certain_fn` carries none).
                if self.hover_probe.is_some()
                    && let Some(t @ Ty::Func { .. }) = &vty
                {
                    let t = t.clone();
                    self.hover_record_at(name_span, &t, HoverKind::Func, None);
                }
                match vty {
                    // STRICT — a module global holds a function VALUE, and a `Ty::Func` does not say
                    // which declaration it came from (a generic fn instantiated at float has an erased
                    // `T` param and coerces nothing). Same rule as the fn-value and fn-field call
                    // paths: no int→float widening through a function value.
                    // TICKET-196: it has no declaration to bind labels from, so `apply_value_call` with
                    // no callee refuses a keyword call through it.
                    Some(t @ Ty::Func { .. }) => {
                        let named = self
                            .call_ctx
                            .as_ref()
                            .map(|c| c.named.clone())
                            .unwrap_or_default();
                        return self.apply_value_call(None, Some(method), t, args, &named, span);
                    }
                    // The member's own initializer already errored (`X := k.nope`), so its type is
                    // `Unknown`. Stay SILENT — the checker's Unknown-suppression convention (the
                    // `Ty::Unknown` arm of `infer_call`): one root-cause error, no cascade asserting a
                    // type nobody knows. Matches what the two-step spelling (`f := l.X; f()`) reports.
                    Some(Ty::Unknown) => {
                        self.infer_all(args);
                        return Ty::Unknown;
                    }
                    // The member EXISTS, it just isn't callable — say that, rather than denying it.
                    Some(t) => {
                        self.infer_all(args);
                        self.error(
                            span,
                            format!(
                                "module '{mname}' member '{method}' is not callable (it has type {t})"
                            ),
                        );
                        return Ty::Unknown;
                    }
                    None => {}
                }
                self.infer_all(args);
                let names = self.module_member_names(mname);
                self.error_help(
                    name_span,
                    format!("module '{mname}' has no member '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            // A protocol existential (e.g. `Error`, or a parameterized `Container[int]`): the
            // protocol's own methods AND everything its embeds require are callable (M22 — the
            // embed set is flattened at every use site, not just at a bound).
            Ty::Protocol(pname, pargs) => {
                // W8-22 — `line()`/`col()`/`file()` on a bare `Error` existential read the fault's
                // origin span (stamped by `recover:`, absent on a user-constructed `Err`). These are
                // NOT `Error` protocol requirements: `ProtocolInfo::methods` is the SATISFACTION set
                // (`satisfies_methods` demands every entry), so adding them there would un-satisfy
                // every struct error type that has only `message()`. Special-cased here instead,
                // matching the hardcoded `Iterator`/`next` arm just below.
                if pname == "Error" && pargs.is_empty() && matches!(method, "line" | "col" | "file")
                {
                    self.check_args(method, &[], args, span);
                    return Ty::option(if method == "file" { Ty::Str } else { Ty::Int });
                }
                let found = self.protocol_method_sig(pname, method).map(|msig| {
                    let ptps = self
                        .protocol_shape(pname)
                        .map(|p| p.type_params.clone())
                        .unwrap_or_default();
                    (msig, ptps)
                });
                if let Some((msig, ptps)) = found {
                    self.record_hook_call(name_span, pname, method, span);
                    // OBJECT SAFETY — `Self` in a parameter slot is un-dispatchable through a
                    // protocol value: it erases which witness it holds, so `a.add(b)` over two
                    // `Vecish` values could hand a `W` to `V::add`. Rejected with the remedy, not
                    // silently mis-typed. `Self` in the RETURN stays fine (it widens to `obj_ty`).
                    if self_in_param_position(&msig) {
                        self.infer_all(args);
                        self.error(
                            span,
                            format!(
                                "method '{method}' is not callable through the protocol value \
                                 {pname} — its signature takes `Self`, and a protocol value erases \
                                 which type it holds. Bind the receiver with a generic parameter \
                                 instead: `[T: {pname}]`"
                            ),
                        );
                        return Ty::Unknown;
                    }
                    // DECISION-2 element RECOVERY: substitute the protocol's own type params → the
                    // carried concrete args into the method's params AND return, so `c.get(0)` on a
                    // `Container[int]` witnesses `i: int` and yields `int` (not the bare param `T`).
                    // Empty `pargs` (a bare existential like `Error`) yields an empty map — a no-op
                    // that reproduces the prior bare behaviour. This is SOUND because the store/pass
                    // boundary already witnessed conformance with these same args.
                    let mut pmap: HashMap<String, Ty> =
                        ptps.iter().cloned().zip(pargs.iter().cloned()).collect();
                    // `Self` in the RETURN means the receiver — here the existential itself, so
                    // `fn neg(self) -> Self` on a `Negish` value yields `Negish` rather than leaking
                    // the bare param out (the `Ty::Param` arm below binds it the same way).
                    pmap.insert("Self".to_string(), obj_ty.clone());
                    // First param is the implicit receiver; explicit args correspond to params[1..].
                    let expected: Vec<Ty> = msig
                        .params
                        .get(1..)
                        .unwrap_or(&[])
                        .iter()
                        .map(|t| subst(t, &pmap))
                        .collect();
                    // The receiver's label is dropped with its param.
                    let slots = callable_slots(
                        msig.labels.get(1..).unwrap_or(&[]),
                        expected.len(),
                        expected.len(),
                        None,
                    );
                    let Some(bound) = self.bind_call(Some(&slots), method, args, 0, span) else {
                        return Ty::Unknown;
                    };
                    self.check_args_subst(method, &expected, expected.len(), &bound, span);
                    return subst(&msig.ret, &pmap);
                }
                self.infer_all(args);
                let mut names = self.protocol_method_names(pname);
                if pname == "Error" && pargs.is_empty() {
                    names.extend(["line", "col", "file"].iter().map(|s| s.to_string()));
                }
                self.error_help(
                    name_span,
                    format!("type {pname} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            // An `Iterator[T]` value (a generator result) exposes the protocol's one method,
            // `next(self) -> Option[T]`, so it is drivable by explicit `.next()` as well as `for`.
            // (There is no registered struct named `Iterator`, so this must be handled here.)
            Ty::Struct(sname, targs) if sname == "Iterator" && targs.len() == 1 => {
                if method == Hook::Next.method() {
                    self.check_args(method, &[], args, span);
                    return Ty::option(targs[0].clone());
                }
                self.infer_all(args);
                self.error_help(
                    name_span,
                    format!(
                        "type {obj_ty} has no method '{method}' (an iterator only has `next()`)"
                    ),
                    suggest::did_you_mean(method, &[Hook::Next.method().to_string()]),
                );
                Ty::Unknown
            }
            Ty::Struct(sname, targs) => {
                // Substitute the struct's type arguments into the method signature, so calling
                // `Stack[int].push(x)` checks `x` against `int`, not the parameter `T`.
                let hover = self.hover_probe.is_some();
                let resolved = self.struct_shape(sname).and_then(|info| {
                    info.methods.get(method).map(|sig| {
                        let map = struct_param_map(info, targs);
                        let inst = instantiate_method(sig, &map);
                        // Editor hover only: the method's CALL type, receiver stripped.
                        let hover_ty = (hover && !sig.is_static && !inst.params.is_empty())
                            .then(|| crate::checker::fn_value_ty(&inst.receiverless()));
                        (
                            inst.params,
                            // the DECLARED (pre-substitution) param types — the widen license (a `T`
                            // slot instantiated at float is erased in the backend, so it cannot widen)
                            sig.params.clone(),
                            inst.ret,
                            inst.type_params,
                            // M24 Task 5 — the METHOD's own witnessed params (never the struct's)
                            inst.witness_params,
                            sig.is_static,
                            // Trailing parameters the CALLEE fills; the receiver slot is dropped
                            // below, so this is compared against `args.len() + 1`.
                            sig.min_params,
                            sig.doc.clone(),
                            // The DECLARED receiver `where` bounds and method params: `enforce_bounds`
                            // substitutes them with `map` once. `inst.where_bounds` is already
                            // substituted, and a second pass rewrites a caller param that shares a
                            // receiver param's name (TICKET-202).
                            (sig.where_bounds.clone(), sig.type_params.clone()),
                            map,
                            hover_ty,
                        )
                    })
                });
                if let Some((
                    params,
                    declared,
                    ret,
                    mtps,
                    mwitness,
                    is_static,
                    mminp,
                    mdoc,
                    (where_bounds, where_owner),
                    rmap,
                    hover_ty,
                )) = resolved
                {
                    // A STATIC method (no `self`) is NOT callable on a value — it is reached only as
                    // `Type.method(...)`. Reject the instance call with a pointer at the right form.
                    if is_static {
                        self.infer_all(args);
                        let disp = crate::base::words::bare_display(sname);
                        self.error(
                            span,
                            format!("'{method}' is a static method of '{disp}'; call it as `{disp}.{method}(...)`"),
                        );
                        return ret;
                    }
                    // Conditional method: enforce any receiver-param `where` bound against the
                    // receiver's concrete type arg (`{T -> concrete}`). Placed AFTER the is_static
                    // rejection so a static-method-called-on-a-value yields only the single
                    // static-method diagnostic (not a spurious bound error); a static method's own
                    // receiver bound is enforced on the static-dispatch path (`infer_static_call`).
                    // No-op when `where_bounds` empty. Mirrors the native `Ty::List` enforcement.
                    self.enforce_bounds(&where_bounds, &where_owner, &rmap, span);
                    let slots = self.method_slots(&obj_ty, method);
                    let Some(bound) = self.bind_call(slots.as_deref(), method, args, 0, span)
                    else {
                        return ret;
                    };
                    let args: &[Expr] = &bound;
                    // A generic method introduces its own type params `[U]` (beyond the struct's
                    // `[T]`, already substituted above). Infer them from the call arguments —
                    // mirrors the free generic-fn path (`infer_generic_call`).
                    if !mtps.is_empty() {
                        return self.infer_generic_method(
                            method, &params, &declared, &ret, &mtps, &mwitness, &obj_ty, type_args,
                            args, mminp, name_span, span, hint,
                        );
                    }
                    // The first param is the receiver (bound implicitly from `obj`), so the call's
                    // explicit args correspond to params[1..]. A method with NO params has no
                    // receiver slot — the runtime prepends the receiver and would error at runtime,
                    // so reject the call here instead. (A zero-param method is classified static
                    // above, so this `None` arm is now defensive — kept for the FnSig that omits the
                    // static flag, e.g. a protocol-derived sig.)
                    match params.split_first() {
                        Some((_receiver, expected)) => {
                            // Editor hover (probe-gated no-op): record the method's CALL signature
                            // (receiver stripped → "fn(int) -> int") at the method-name token, so
                            // hovering `c.foo(2)`'s `foo` yields the signature. Pure side effect.
                            if let Some(fty) = &hover_ty {
                                self.hover_record_at(name_span, fty, HoverKind::Func, mdoc.clone());
                            }
                            self.check_args_subst(
                                method,
                                expected,
                                mminp.saturating_sub(1),
                                args,
                                span,
                            )
                        }
                        None => {
                            self.error(
                                span,
                                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
                            );
                            self.infer_all(args);
                        }
                    }
                    return ret;
                }
                // No method named `method`: fall back to a function-typed *field* of the same name —
                // `recv.f(x)` where `f: fn(T) -> U` is field-access-then-call. (Parsed as a method
                // call; the desugar pass leaves fn-field names un-normalized so no method default is
                // injected here.)
                let field_fn = self.data_member_ty(&obj_ty, method);
                if let Some(t @ Ty::Func { .. }) = field_fn {
                    // STRICT — a fn-typed FIELD holds a function VALUE: `apply_value_call` checks it
                    // like every other value (no int→float widening through a function value).
                    let named = self
                        .call_ctx
                        .as_ref()
                        .map(|c| c.named.clone())
                        .unwrap_or_default();
                    return self.apply_value_call(None, Some(method), t, args, &named, span);
                }
                // TICKET-030 — every struct gets an intrinsic, MISS-ONLY `copy()`: it is checked here
                // only after the declared-method lookup and the fn-typed-field fallback above both
                // failed, so a struct declaring its own `copy` or holding a fn-typed `copy` field keeps
                // it. Also miss-only against a NON-fn field named `copy` — the fn-typed-field fallback
                // above only matches `Ty::Func`, so a plain `copy: int` field falls through to here;
                // without this guard the checker accepts `s.copy()` but the VM's field fallback faults
                // with "'{}' is not callable" (review finding on `3faa6948`). Returns the receiver type
                // UNCHANGED (not just `sname`), so a generic struct's type arguments survive. Mirrors
                // the runtime arm in `Vm::do_method_call`.
                let has_copy_field = self
                    .struct_shape(sname)
                    .is_some_and(|info| info.fields.iter().any(|(f, _)| f == "copy"));
                if method == "copy" && self.struct_shape(sname).is_some() && !has_copy_field {
                    self.check_args(method, &[], args, span);
                    return obj_ty.clone();
                }
                self.infer_all(args);
                // TICKET-079/TICKET-080 (c): a name that exists as a FIELD is never suggested for a
                // method miss of the same name — `did you mean 'copy'?` would answer with the exact
                // spelling the user already typed. Report the kind instead, mirroring the reverse
                // case (a field miss naming a method) at `pattern.rs`'s field lookup.
                if self.field_names(sname).iter().any(|f| f == method) {
                    self.error(
                        name_span,
                        format!(
                            "type {obj_ty} has no method '{method}' ('{method}' is a field -- read \
                             it without parentheses)"
                        ),
                    );
                    return Ty::Unknown;
                }
                // TICKET-030 — DEC-007: a `HashMap`-drawn candidate list must be sorted before scoring
                // so a distance tie doesn't depend on hash order. `copy` is callable on every struct
                // WITHOUT a `copy` field, so it belongs in this near-miss set only then.
                let mut names = self.method_names(sname);
                if !has_copy_field {
                    names.push("copy".to_string());
                }
                names.sort();
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            // Enum methods (name-resolved exactly like struct methods). Substitute the enum's type
            // arguments into the method signature, so `Box[int].get()` returns `int`, not `T`.
            Ty::Enum(ename, targs) => {
                let resolved = self.enum_methods_of(ename).and_then(|ms| {
                    ms.get(method).map(|sig| {
                        let map: HashMap<String, Ty> = self
                            .enum_type_params_of(ename)
                            .map(|tps| {
                                tps.iter()
                                    .map(|tp| tp.name.clone())
                                    .zip(targs.iter().cloned())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let inst = instantiate_method(sig, &map);
                        (
                            inst.params,
                            // the DECLARED (pre-substitution) param types — see the struct arm.
                            sig.params.clone(),
                            inst.ret,
                            inst.type_params,
                            // M24 Task 5 — the METHOD's own witnessed params (never the host type's)
                            inst.witness_params,
                            sig.is_static,
                            // Trailing parameters the CALLEE fills; the receiver slot is dropped
                            // below, so this is compared against `args.len() + 1`.
                            sig.min_params,
                            (sig.where_bounds.clone(), sig.type_params.clone()),
                            map,
                        )
                    })
                });
                if let Some((
                    params,
                    declared,
                    ret,
                    mtps,
                    mwitness,
                    is_static,
                    mminp,
                    (where_bounds, where_owner),
                    rmap,
                )) = resolved
                {
                    // A STATIC enum method is reached only as `Enum.method(...)`; reject the call on a
                    // value with a pointer at the right form (mirrors the struct arm).
                    if is_static {
                        self.infer_all(args);
                        let disp = crate::base::words::bare_display(ename);
                        self.error(
                            span,
                            format!("'{method}' is a static method of '{disp}'; call it as `{disp}.{method}(...)`"),
                        );
                        return ret;
                    }
                    // Conditional method: enforce any receiver-param `where` bound against the enum's
                    // concrete type arg (mirrors the struct arm). Placed after the is_static rejection
                    // so a static-on-value call stays single-diagnostic; a static enum method's own
                    // receiver bound is enforced on the static-dispatch path (`infer_static_call`).
                    // No-op when `where_bounds` empty.
                    self.enforce_bounds(&where_bounds, &where_owner, &rmap, span);
                    let slots = self.method_slots(&obj_ty, method);
                    let Some(bound) = self.bind_call(slots.as_deref(), method, args, 0, span)
                    else {
                        return ret;
                    };
                    let args: &[Expr] = &bound;
                    if !mtps.is_empty() {
                        return self.infer_generic_method(
                            method, &params, &declared, &ret, &mtps, &mwitness, &obj_ty, type_args,
                            args, mminp, name_span, span, hint,
                        );
                    }
                    match params.split_first() {
                        Some((_receiver, expected)) => self.check_args_subst(
                            method,
                            expected,
                            mminp.saturating_sub(1),
                            args,
                            span,
                        ),
                        None => {
                            self.error(
                                span,
                                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
                            );
                            self.infer_all(args);
                        }
                    }
                    return ret;
                }
                self.infer_all(args);
                let names = self.enum_method_names(ename);
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            // Core-type methods (M6): built-in methods on `str` and `list[T]`.
            Ty::Str => {
                // The sigs are harvested from `std/prelude.chz`'s `native struct str` (re-seeded by
                // `seed_stdlib_structs`); `str` is non-generic so no type args are substituted.
                if let Some(sig) = self.native_method_sig("str", method, &[]) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("str");
                let message = format!("type str has no method '{method}'");
                self.error_help(name_span, message, suggest::did_you_mean(method, &names));
                Ty::Unknown
            }
            Ty::List(elem) => {
                // `sum` is harvested from `std/prelude.chz` as `sum(self) -> T where T: Add`, but the
                // `where T: Add` bound alone is SEMIGROUP (structural `add`) and thus too broad: a plain
                // Add-satisfying struct has no zero/identity for the EMPTY-list case, and the engine's
                // `sum` (`vm/call.rs` do_builtin) has no way to mint one. So `sum`'s true requirement
                // is MONOID (Add + zero), which is not a declarable protocol here; we keep this
                // DISPATCH-TIME gate as the residual that enforces it (a struct with a structural
                // `add` still reports the numeric diagnostic, NOT check-ok/run-error). The
                // `where T: Add` in the decl is documentation of the necessary-but-insufficient bound.
                //
                // A `List[float]` records a `0.0` seed here: an EMPTY list carries no element to
                // read a kind off, and the backend is type-blind. BOTH verdicts are recorded, so an
                // aliased key is a hard error instead of one site's seed reaching another.
                let elem = (**elem).clone();
                if method == "sum" {
                    let seed = self.sum_seed(&elem);
                    if seed.is_none() && !(elem.is_numeric() || elem.is_unknown()) {
                        self.infer_all(args);
                        self.error(
                            span,
                            format!("sum() requires a numeric list, found List[{elem}]"),
                        );
                        return Ty::Unknown;
                    }
                    self.record_sum_seed(name_span, seed, span);
                }
                // **W7-45**, the same dispatch-time residual one line up, for the same reason. These
                // four have a RUNTIME of `values_equal` (`vm/call.rs` contains / index_of /
                // unique+dedup) but a harvested sig validated by `assignable` alone, so the W7-41
                // `==` guard never sees them: `[a].contains(b)` over a `Box[Tag]` check-cleaned and
                // then faulted with *"struct 'Tag' has no 'compare' method"*. The element type is
                // not expressible as a `where` bound on the decl — `List` puts no bound on `T` at
                // all — so it is enforced here, exactly where `sum`'s numeric requirement is.
                // The four are exhaustive against the `native struct List[T]` surface: `count` and
                // `position` take a PREDICATE (`fn(T) -> bool`) so they never compare, and there is
                // `remove` takes an index, so it never compares. NOT erased (W7-53): a free `elem` must
                // carry `Eq` among its declared bounds, same as the `==`/`in` gates — `eq_bounds_unsatisfied`.
                if matches!(method, "contains" | "index_of" | "dedup" | "unique")
                    && let Some(why) = self.eq_bounds_unsatisfied(&elem)
                {
                    self.infer_all(args);
                    self.error(
                        span,
                        format!("{method}() compares List[{elem}] elements for equality — {why}"),
                    );
                    return Ty::Unknown;
                }
                // Every other method's sig is harvested from `std/prelude.chz`'s `native struct List[T]`
                // (re-seeded by `seed_stdlib_structs`); the element type is substituted for `Ty::Param`.
                // A `where T: Comparable` bound (harvested onto the sig's `where_bounds`) is enforced
                // here via `enforce_bounds` with the `T -> elem` substitution — the file-backed
                // replacement for the retired bespoke Comparable arm (int/float/str intrinsically, a
                // struct via its `compare` method; `Ty::Unknown` tolerated). `sort`/`min`/`max`
                // (`Comparable`) and `sum` (`Add`) are the List methods carrying a where-clause today —
                // `min`/`max` pair one with an `Option[T]` return; it is a no-op for every other method.
                if let Some(sig) =
                    self.native_method_sig("List", method, std::slice::from_ref(&elem))
                {
                    self.record_method_hover(name_span, &sig);
                    // A method carrying its OWN `[U]` params (`map`/`fold`/`sort_by_key`, the
                    // closure-result HOFs that retired `infer_list_hof`) needs the generic solver so its
                    // return-position param is recovered from the closure body (the loop-back). The
                    // harvest STRIPS `self`, but `infer_generic_method` expects `params[0]` to be the
                    // receiver — so PREPEND the concrete receiver `List[elem]`. Non-generic methods
                    // (`filter`/`sort_by`/every flat method) keep the fixed-arity path.
                    if !sig.type_params.is_empty() {
                        let mut params = Vec::with_capacity(sig.params.len() + 1);
                        params.push(Ty::list(elem.clone()));
                        params.extend(sig.params.iter().cloned());
                        return self.infer_generic_method(
                            method,
                            &params,
                            &[],
                            &sig.ret,
                            &sig.type_params,
                            &[], // native `List` methods take no witness
                            &Ty::list(elem.clone()),
                            type_args,
                            args,
                            // The harvest strips `self` and the receiver is prepended just above, so
                            // the harvested minimum shifts by one to line up with `params`.
                            sig.min_params.saturating_add(1),
                            name_span,
                            span,
                            hint,
                        );
                    }
                    self.check_args_range_coll(method, &sig.params, sig.min_params, args, span);
                    self.enforce_bounds(
                        &sig.where_bounds,
                        &sig.type_params,
                        &HashMap::from([("T".to_string(), elem.clone())]),
                        span,
                    );
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("List");
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            // `bytes` core methods (immutable byte sequence): only `decode() -> str` (UTF-8).
            Ty::Bytes => {
                // The sigs are harvested from `std/prelude.chz`'s `native struct bytes` (re-seeded by
                // `seed_stdlib_structs`); `bytes` is non-generic so no type args are substituted.
                if let Some(sig) = self.native_method_sig("bytes", method, &[]) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("bytes");
                self.error_help(
                    name_span,
                    format!("type bytes has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            // `bytearray` core methods (mutable buffer): `len`, `push(int)`, `pop() -> Option[int]`,
            // `extend(bytes|bytearray|list[int])`. `extend` is handled here (not the fixed sig table)
            // because its argument may be any of the three byte-sequence shapes.
            Ty::ByteArray => {
                if method == "extend" {
                    self.check_arity("extend", 1, args, span);
                    if let Some(a) = args.first() {
                        match self.infer_value(a) {
                            Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                            Ty::List(elem) if matches!(elem.scalar(), Ty::Int | Ty::Unknown) => {}
                            other @ Ty::Int | other @ Ty::Float | other @ Ty::Bool | other @ Ty::Str | other @ Ty::Nil | other @ Ty::List(_) | other @ Ty::Map(..) | other @ Ty::Set(_) | other @ Ty::Func { .. } | other @ Ty::Tuple(_) | other @ Ty::Struct(..) | other @ Ty::Enum(..) | other @ Ty::Param(_) | other @ Ty::Result(..) | other @ Ty::Option(_) | other @ Ty::Channel(_) | other @ Ty::Shared(_) | other @ Ty::Atomic(_) | other @ Ty::AtomicInt | other @ Ty::RwShared(_) | other @ Ty::Executor | other @ Ty::Socket | other @ Ty::Listener | other @ Ty::Writer | other @ Ty::Reader | other @ Ty::Ptr | other @ Ty::Width(_) | other @ Ty::Protocol(..) | other @ Ty::Module(_) | other @ Ty::Var(_) => self.error(
                                a.span,
                                format!("extend() expects a bytes, a bytearray, or a List[int], got {other}"),
                            ),
                        }
                    }
                    return Ty::Nil;
                }
                // Every other method's sig is harvested from `std/prelude.chz`'s `native struct
                // bytearray` (re-seeded by `seed_stdlib_structs`); `bytearray` is non-generic so no type
                // args are substituted. `extend` is handled above (its arg may be any of three
                // byte-sequence shapes, not a flat FnSig).
                if let Some(sig) = self.native_method_sig("bytearray", method, &[]) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("bytearray");
                self.error_help(
                    name_span,
                    format!("type bytearray has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            Ty::Map(k, v) => {
                // The sigs are harvested from `std/prelude.chz`'s `native struct Map[K, V]` (re-seeded by
                // `seed_stdlib_structs`); the key/value types are substituted for `Ty::Param("K")`/
                // `Ty::Param("V")` here (DECLARATION order — `[k, v]`).
                let targs = [(**k).clone(), (**v).clone()];
                if let Some(sig) = self.native_method_sig("Map", method, &targs) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args_range(method, &sig.params, sig.min_params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("Map");
                let message = format!("type {obj_ty} has no method '{method}'");
                self.error_help(name_span, message, suggest::did_you_mean(method, &names));
                Ty::Unknown
            }
            Ty::Set(elem) => {
                // The sigs are harvested from `std/prelude.chz`'s `native struct Set[T]` (re-seeded by
                // `seed_stdlib_structs`); the element type is substituted for `Ty::Param("T")` here.
                let elem = (**elem).clone();
                if let Some(sig) =
                    self.native_method_sig("Set", method, std::slice::from_ref(&elem))
                {
                    self.record_method_hover(name_span, &sig);
                    self.check_args_range_coll(method, &sig.params, sig.min_params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("Set");
                let message = format!("type {obj_ty} has no method '{method}'");
                self.error_help(name_span, message, suggest::did_you_mean(method, &names));
                Ty::Unknown
            }
            Ty::Channel(elem) => {
                // The sigs are harvested from `std/prelude.chz`'s `native struct Channel[T]` (re-seeded
                // by `seed_stdlib_structs`); the element type is substituted for `Ty::Param("T")` here.
                // `send(v)` hands `v` over by handle (M3.2); `check_args` enforces it matches the
                // element type `T`.
                let elem = (**elem).clone();
                if let Some(sig) =
                    self.native_method_sig("Channel", method, std::slice::from_ref(&elem))
                {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    // `trip()`'s `where T: bool` (harvested onto `where_bounds`) is enforced here with
                    // the `T -> elem` substitution — `trip` is level-trigger-latch-only and always
                    // delivers `bool true`, so it's sound only on `Channel[bool]`. No-op for every
                    // other Channel method (empty `where_bounds`). Mirrors the `Ty::List` arm.
                    self.enforce_bounds(
                        &sig.where_bounds,
                        &sig.type_params,
                        &HashMap::from([("T".to_string(), elem.clone())]),
                        span,
                    );
                    return sig.ret;
                }
                self.infer_all(args);
                let names = self.method_names("Channel");
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            Ty::Shared(elem) => {
                // `get()->T`, `set(T)->nil`, `update(fn(T)->T)->nil` — the same box API as `Ref[T]`,
                // but reachable across tasks. The sigs are harvested from `std/concurrency.chz` into the
                // `Shared` method table (re-seeded by `seed_stdlib_structs`); the box's element type is
                // substituted for the sig's `Ty::Param("T")` here.
                let elem = (**elem).clone();
                match self.resolve_native_handle_method(
                    "Shared",
                    method,
                    std::slice::from_ref(&elem),
                    name_span,
                    &obj_ty,
                    type_args,
                    args,
                    span,
                    hint,
                ) {
                    NativeHandleMethod::Generic(t) => t,
                    NativeHandleMethod::Concrete(sig) => {
                        self.check_args_range(method, &sig.params, sig.min_params, args, span);
                        if method == "read" {
                            return self.recover_read_ret(args);
                        }
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        let names = self.method_names("Shared");
                        self.error_help(
                            name_span,
                            format!("type {obj_ty} has no method '{method}'"),
                            suggest::did_you_mean(method, &names),
                        );
                        Ty::Unknown
                    }
                }
            }
            Ty::RwShared(elem) => {
                // `get()->T`, `set(T)->nil`, `write(fn(T)->T)->nil` mirror `Shared`. `read` is the
                // read-only twin: `read(fn(T)->R)->R` — R-polymorphic in the closure's return type.
                // The harvested table (via `attach_native_module_metadata`) types `read`'s param as
                // `fn(T)->Unknown` (any closure return is accepted) and ret `Unknown`; here we recover R
                // from the supplied closure so the call site sees the real return type (not `Unknown`).
                // `read`'s sig ret is the placeholder `Unknown` (the real R is recovered below) —
                // hover (inside the helper) shows the declared `fn(fn(T) -> ?) -> ?` shape, the sig
                // of record.
                let elem = (**elem).clone();
                // The std-internal `_` primitives of a CONTAINER element (`List[E]`, `Map[K, V]`,
                // `Set[E]`; `RWSHARED_VIEWS`). E/K/V are not nameable in `RwShared[T]`'s harvested
                // surface, so these sigs are built here from the concrete container. A read copies
                // only what it returns; a writer is the plain container operation under the box's
                // update guard (`rwshared_method` in `src/vm/netio.rs`). A walk is not a primitive:
                // the collections write it as `inner.get()` plus a loop.
                let view = RWSHARED_VIEWS.iter().find(|(head, _, _)| {
                    matches!(
                        (&elem, *head),
                        (Ty::List(_), "List") | (Ty::Map(_, _), "Map") | (Ty::Set(_), "Set")
                    )
                });
                let prim = RwSharedPrim::from_name(method).filter(|p| {
                    view.is_some_and(|(_, _, names)| names.contains(p))
                        && !self.std_internal_hidden(method)
                });
                match &elem {
                    _ if prim.is_none() => {}
                    Ty::List(e) => {
                        let e = (**e).clone();
                        let Some(prim) = prim else {
                            unreachable!("the guard arm answers a method with no primitive")
                        };
                        match prim {
                            RwSharedPrim::Push => {
                                self.check_args_range("_push", &[e], 1, args, span);
                                return Ty::Nil;
                            }
                            RwSharedPrim::Len => {
                                self.check_args_range("_len", &[], 0, args, span);
                                return Ty::Int;
                            }
                            // `at(i) -> Option[E]`: out of range is `None`, never a fault, matching the
                            // language's other named accessors (`get_key -> Option[V]` below,
                            // `std.json.at -> Option[Json]`). `RwShared` has no `[]` of its own (it
                            // does not satisfy `Index`), so this is its only read accessor.
                            RwSharedPrim::At => {
                                self.check_args_range("_at", &[Ty::Int], 1, args, span);
                                return Ty::option(e);
                            }
                            RwSharedPrim::Slice => {
                                let range = self.range_ty();
                                self.check_args_range("_slice", &[range], 1, args, span);
                                return Ty::list(e);
                            }
                            RwSharedPrim::SetKey
                            | RwSharedPrim::RemoveKey
                            | RwSharedPrim::GetOrInsert
                            | RwSharedPrim::Add
                            | RwSharedPrim::Remove
                            | RwSharedPrim::Contains
                            | RwSharedPrim::Has
                            | RwSharedPrim::GetKey => {
                                unreachable!(
                                    "RWSHARED_VIEWS lists a primitive this arm does not type"
                                )
                            }
                        }
                    }
                    Ty::Map(k, v) => {
                        let k = (**k).clone();
                        let v = (**v).clone();
                        let Some(prim) = prim else {
                            unreachable!("the guard arm answers a method with no primitive")
                        };
                        match prim {
                            RwSharedPrim::Len => {
                                self.check_args_range("_len", &[], 0, args, span);
                                return Ty::Int;
                            }
                            RwSharedPrim::GetKey => {
                                self.check_args_range("_get_key", &[k], 1, args, span);
                                return Ty::option(v);
                            }
                            RwSharedPrim::Has => {
                                self.check_args_range("_has", &[k], 1, args, span);
                                return Ty::Bool;
                            }
                            // TICKET-192: the single-entry writers (update guard, O(1) expected).
                            RwSharedPrim::SetKey => {
                                self.check_args_range("_set_key", &[k, v.clone()], 2, args, span);
                                return Ty::option(v);
                            }
                            RwSharedPrim::RemoveKey => {
                                self.check_args_range("_remove_key", &[k], 1, args, span);
                                return Ty::option(v);
                            }
                            RwSharedPrim::GetOrInsert => {
                                self.check_args_range(
                                    "_get_or_insert",
                                    &[k, v.clone()],
                                    2,
                                    args,
                                    span,
                                );
                                return v;
                            }
                            RwSharedPrim::Push
                            | RwSharedPrim::At
                            | RwSharedPrim::Slice
                            | RwSharedPrim::Add
                            | RwSharedPrim::Remove
                            | RwSharedPrim::Contains => {
                                unreachable!(
                                    "RWSHARED_VIEWS lists a primitive this arm does not type"
                                )
                            }
                        }
                    }
                    Ty::Set(e) => {
                        let e = (**e).clone();
                        let Some(prim) = prim else {
                            unreachable!("the guard arm answers a method with no primitive")
                        };
                        match prim {
                            RwSharedPrim::Len => {
                                self.check_args_range("_len", &[], 0, args, span);
                                return Ty::Int;
                            }
                            RwSharedPrim::Contains => {
                                self.check_args_range("_contains", &[e], 1, args, span);
                                return Ty::Bool;
                            }
                            RwSharedPrim::Add | RwSharedPrim::Remove => {
                                self.check_args_range(prim.name(), &[e], 1, args, span);
                                return Ty::Bool;
                            }
                            RwSharedPrim::Push
                            | RwSharedPrim::At
                            | RwSharedPrim::Slice
                            | RwSharedPrim::SetKey
                            | RwSharedPrim::RemoveKey
                            | RwSharedPrim::GetOrInsert
                            | RwSharedPrim::Has
                            | RwSharedPrim::GetKey => {
                                unreachable!(
                                    "RWSHARED_VIEWS lists a primitive this arm does not type"
                                )
                            }
                        }
                    }
                    Ty::Int
                    | Ty::Float
                    | Ty::Bool
                    | Ty::Str
                    | Ty::Bytes
                    | Ty::ByteArray
                    | Ty::Nil
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
                    | Ty::Protocol(..)
                    | Ty::Module(_)
                    | Ty::Unknown
                    | Ty::Var(_) => {}
                }
                match self.resolve_native_handle_method(
                    "RwShared",
                    method,
                    std::slice::from_ref(&elem),
                    name_span,
                    &obj_ty,
                    type_args,
                    args,
                    span,
                    hint,
                ) {
                    NativeHandleMethod::Generic(t) => t,
                    NativeHandleMethod::Concrete(sig) => {
                        self.check_args_range(method, &sig.params, sig.min_params, args, span);
                        if method == "read" {
                            return self.recover_read_ret(args);
                        }
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        // A container primitive (public or `_` spelling) names its collection
                        // before any near-miss: `set_key` is not a typo of `set`.
                        let under = format!("_{method}");
                        let help = match view {
                            Some((head, coll, names))
                                if names
                                    .iter()
                                    .any(|p| p.name() == method || p.name() == under)
                                    || RWSHARED_WALKS
                                        .iter()
                                        .any(|(h, walks)| h == head && walks.contains(&method)) =>
                            {
                                Some(format!("use {coll} from std.concurrency.collection"))
                            }
                            _ => {
                                let mut names = self.method_names("RwShared");
                                names.retain(|n| !self.std_internal_hidden(n));
                                suggest::did_you_mean(method, &names)
                            }
                        };
                        self.error_help(
                            name_span,
                            format!("type {obj_ty} has no method '{method}'"),
                            help,
                        );
                        Ty::Unknown
                    }
                }
            }
            Ty::Atomic(elem) => {
                // `load()->T`, `store(T)`, `exchange(T)->T`, `cas(T,T)->bool`; `add(T)->T`/`sub(T)->T`
                // only when `T` is numeric. The sigs are harvested from `std/concurrency.chz`; the
                // numeric gate on `add`/`sub` is a DISPATCH-TIME constraint a plain sig cannot express,
                // so it stays a residual here — for a non-numeric element those two are gated out so the
                // call reports "no method 'add'" (matching the retired `atomic_method_sig`).
                let elem = (**elem).clone();
                // The numeric gate on `add`/`sub` is a DISPATCH-TIME constraint a plain sig cannot
                // express — it must stay BEFORE the lookup so a non-numeric element short-circuits to
                // the "no method" path (matching the retired `atomic_method_sig`).
                let numeric_gated = matches!(method, "add" | "sub") && !elem.is_numeric();
                // TICKET-144 (W14-24): `cas` refuses a payload that holds a `fn` value: Go's func is
                // uncomparable, and `atomic.Value.CompareAndSwap` panics `comparing uncomparable type`
                // there. `load`/`store`/`exchange` stay legal; a `Param`/protocol payload hides the fn,
                // so the runtime `cas` backstops it.
                if method == "cas"
                    && let Some(f) = self.reaches_func(&elem, &mut Vec::new())
                {
                    self.error(
                        name_span,
                        format!(
                            "Atomic[{elem}].cas cannot compare a payload that holds a function value ('{f}'): a function value is not comparable (Go: func is uncomparable); use store/exchange instead"
                        ),
                    );
                }
                let resolved = if numeric_gated {
                    NativeHandleMethod::Miss
                } else {
                    self.resolve_native_handle_method(
                        "Atomic",
                        method,
                        std::slice::from_ref(&elem),
                        name_span,
                        &obj_ty,
                        type_args,
                        args,
                        span,
                        hint,
                    )
                };
                match resolved {
                    NativeHandleMethod::Generic(t) => t,
                    NativeHandleMethod::Concrete(sig) => {
                        self.check_args_range(method, &sig.params, sig.min_params, args, span);
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        let names = self.method_names("Atomic");
                        self.error_help(
                            name_span,
                            format!("type {obj_ty} has no method '{method}'"),
                            suggest::did_you_mean(method, &names),
                        );
                        Ty::Unknown
                    }
                }
            }
            Ty::AtomicInt => {
                // `load()->int`, `store(int)`, `exchange(int)->int`, `cas(int,int)->bool`,
                // `add(int)->int`, `sub(int)->int`. Monomorphic int — NO element type (`&[]`) and NO
                // numeric gate (int is ALWAYS numeric — the whole reason for monomorphizing). Sigs are
                // harvested from `std/concurrency.chz` as concrete int sigs (no generic solving).
                match self.resolve_native_handle_method(
                    "AtomicInt",
                    method,
                    &[],
                    name_span,
                    &obj_ty,
                    type_args,
                    args,
                    span,
                    hint,
                ) {
                    NativeHandleMethod::Generic(t) => t,
                    NativeHandleMethod::Concrete(sig) => {
                        self.check_args_range(method, &sig.params, sig.min_params, args, span);
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        let names = self.method_names("AtomicInt");
                        self.error_help(
                            name_span,
                            format!("type {obj_ty} has no method '{method}'"),
                            suggest::did_you_mean(method, &names),
                        );
                        Ty::Unknown
                    }
                }
            }
            Ty::Executor => {
                // `shutdown()->nil`, `cancel()->nil` and the bodied generic `submit[T](f) -> Task[T]`
                // (C5 escape hatch). No element type to substitute (`&[]`). The generic `submit`
                // routes through the helper's generic solver (infer T from the closure return);
                // the inner `self._submit_settled(..)` that its body emits is the `Concrete` path.
                match self.resolve_native_handle_method(
                    "Executor",
                    method,
                    &[],
                    name_span,
                    &obj_ty,
                    type_args,
                    args,
                    span,
                    hint,
                ) {
                    NativeHandleMethod::Generic(t) => t,
                    NativeHandleMethod::Concrete(sig) => {
                        self.check_args_range(method, &sig.params, sig.min_params, args, span);
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        let mut names = self.method_names("Executor");
                        names.retain(|n| !self.std_internal_hidden(n));
                        self.error_help(
                            name_span,
                            format!("type {obj_ty} has no method '{method}'"),
                            suggest::did_you_mean(method, &names),
                        );
                        Ty::Unknown
                    }
                }
            }
            // D6 — `Socket` / `Listener` (std.net): a small fixed method set. The runtime parks the
            // fiber on a would-block `read`/`write`/`accept`; from the type system they just return
            // their `Result`.
            Ty::Socket => self.infer_fixed_native_handle_method(
                "Socket", method, name_span, &obj_ty, type_args, args, span,
            ),
            Ty::Listener => self.infer_fixed_native_handle_method(
                "Listener", method, name_span, &obj_ty, type_args, args, span,
            ),
            // R2 — `Writer` (std.io): a small fixed write-only method set (`write`/`write_bytes`/
            // `flush`/`close`). Method table harvested from `std/io.chz`, looked up here like Socket.
            Ty::Writer => self.infer_fixed_native_handle_method(
                "Writer", method, name_span, &obj_ty, type_args, args, span,
            ),
            // R2b — `Reader` (std.io): a small fixed read-only method set (`read_line`/`read_bytes`/
            // `close`, plus the bodied `lines`). Method table harvested from `std/io.chz`.
            Ty::Reader => self.infer_fixed_native_handle_method(
                "Reader", method, name_span, &obj_ty, type_args, args, span,
            ),
            // A bound generic type parameter exposes its protocol's methods (e.g. `a.compare(b)`
            // where `a: T` and `T: Comparable`).
            Ty::Param(pname) => {
                // Search the param's bounds for a protocol that declares `method` (multi-bound
                // `T: Add + Mul` exposes the union of both protocols' methods).
                // `Self` maps to the receiver `Ty::Param(pname)`, and the parameterized protocol's
                // own params to the bound's args (`Container[int]` ⇒ `T ↦ int`).
                if let Some((proto, msig, map)) = self.bound_method(pname, method, &|_| true) {
                    self.record_hook_call(name_span, &proto.name, method, span);
                    let expected: Vec<Ty> = match msig.params.split_first() {
                        Some((_recv, rest)) => rest.iter().map(|t| subst(t, &map)).collect(),
                        None => Vec::new(),
                    };
                    // The receiver's label is dropped with its param.
                    let slots = callable_slots(
                        msig.labels.get(1..).unwrap_or(&[]),
                        expected.len(),
                        expected.len(),
                        None,
                    );
                    let Some(bound) = self.bind_call(Some(&slots), method, args, 0, span) else {
                        return Ty::Unknown;
                    };
                    self.check_args_subst(method, &expected, expected.len(), &bound, span);
                    return self.bound_method_ret(&msig, &map);
                }
                self.infer_all(args);
                let bounds = self.type_params.get(pname).cloned().unwrap_or_default();
                let mut names: Vec<String> = bounds
                    .iter()
                    .flat_map(|b| self.protocol_method_names(&b.name))
                    .collect();
                names.sort();
                names.dedup();
                self.error_help(
                    name_span,
                    format!("type parameter {pname} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            Ty::Unknown => {
                self.infer_all(args);
                Ty::Unknown
            }
            other @ Ty::Int
            | other @ Ty::Float
            | other @ Ty::Bool
            | other @ Ty::Nil
            | other @ Ty::Func { .. }
            | other @ Ty::Tuple(_)
            | other @ Ty::Result(..)
            | other @ Ty::Option(_)
            | other @ Ty::Ptr
            | other @ Ty::Width(_)
            | other @ Ty::Var(_) => {
                self.infer_all(args);
                self.error(name_span, format!("type {other} has no method '{method}'"));
                Ty::Unknown
            }
        }
    }

    // ===== small helpers =====
}
