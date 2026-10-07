// checker::expr — split out of checker/mod.rs. `super::*` == the `checker` module.
// Expression & call inference, keyword calls, type application, indexing.

use super::pattern::WrittenTypeArgs;
use super::resolve::PathPos;
use super::setup::{HeadBinding, TypeHead, TypeHeadKind};
use super::*;

/// Result of resolving a bodied/native method on one of the reserved native handles
/// (`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`Writer`/`Reader`). The shared
/// `resolve_native_handle_method` helper folds the byte-identical lookup + hover + generic branch;
/// each arm keeps ITS residual (numeric gate, submit capture-floor, `read` R-recovery) inline.
enum NativeHandleMethod {
    /// A harvested method carrying its OWN `[U]` params — already routed through the generic
    /// solver; the returned `Ty` is the arm's result.
    Generic(Ty),
    /// A non-generic sig; the caller runs its own residual (`check_args_range` + any special case).
    Concrete(Box<FnSig>),
    /// Lookup miss; the caller runs `infer_all(args)` + "no method" error.
    Miss,
}

impl Checker {
    #[allow(clippy::too_many_arguments)] // the call node's parts, its bracket reading, span and id
    pub(super) fn infer_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        type_args: &[Type],
        bracket: Option<&Expr>,
        span: Span,
        call_id: crate::ast::NodeId,
    ) -> Ty {
        let ctx = CallCtx {
            id: call_id,
            named: named.to_vec(),
            pack_origin: crate::checker::witness_key_span(callee, span),
            consumed: false,
            span,
            bracket: bracket.cloned(),
        };
        let saved = self.call_ctx.replace(ctx);
        let t = self.infer_call_dispatch(callee, args, named, type_args, span);
        // D4 layer A inside a `spawn:` body, through the plan the dispatch just bound.
        let slots = self.bound_slots(call_id, args, named);
        self.report_named_call_writes(callee, &slots, None, false);
        // TICKET-190: a call that creates a generator stamps its param slots with how each
        // argument crosses (a fresh argument is private to the new frame). A bare-name callee
        // stamps only through `labels_certain`, settled at its binding's `pop_scope`: a written
        // binding may hold another fn at the call, so that call gets no stamp.
        if self.records_node(call_id)
            && self
                .named_fn_summary(callee)
                .is_some_and(|(_, s)| s.is_generator)
        {
            let crossing = self.call_crossing(None, &slots);
            let call = (self.graph_module_idx, call_id.0);
            match &callee.kind {
                ExprKind::Ident(name) => {
                    if let Ok(key) = self.labels_certain(name) {
                        let pending = (key, super::globals::KwUse::GenStamp(call, crossing, span));
                        if !self.kw_pending.contains(&pending) {
                            self.kw_pending.push(pending);
                        }
                    }
                }
                _ => crate::checker::record_call_table_entry(
                    &mut self.gen_crossings.calls,
                    &mut self.table_conflicts,
                    call,
                    crossing,
                    "generator crossing",
                    span,
                ),
            }
        }
        let consumed = std::mem::replace(&mut self.call_ctx, saved).is_some_and(|c| c.consumed);
        // A call whose named arguments no binder and no refusal consumed: its callee binds none.
        if !named.is_empty() && !consumed {
            self.error(
                span,
                "named arguments are only supported on functions, struct constructors, and struct methods"
                    .to_string(),
            );
            for (_, v) in named {
                self.infer_value(v);
            }
        }
        t
    }

    fn infer_call_dispatch(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        type_args: &[Type],
        span: Span,
    ) -> Ty {
        // Consume the expected-type hint FIRST (before any argument is inferred): it belongs to THIS
        // call only. `take()` clears the slot so a nested arg call (inferred later inside the ctor/call
        // dispatchers) sees `None` — the hint never leaks past the outermost call it was set for. It is
        // threaded into the generic ctor / generic fn-call dispatchers below to pre-seed `T`.
        let expected = self.expected_hint.take();
        let expected = expected.as_ref();
        if self.in_spawn_block
            && let ExprKind::Ident(name) = &callee.kind
            && self.is_captured(name)
            && let Some(written) = self.value_writes(callee).first()
        {
            self.error(
                callee.span,
                format!("'{written}' {}", crate::vm::COPY_WRITE_TAIL),
            );
        }
        // What the callee names: ONE answer from the rule table (`resolve.rs`). Every arm below takes
        // its branch from it and none re-decides what the path is.
        let res = self.resolve_path(callee, PathPos::Callee);
        // `print(..., sep=, end=)`: its only named arguments are `sep` and `end`, typed `str`.
        if !named.is_empty()
            && let Some(Resolution::Builtin(b)) = &res
            && b == "print"
        {
            self.consume_named();
            let mut seen: Vec<&str> = Vec::new();
            for (k, _) in named {
                if !matches!(k.as_str(), "sep" | "end") || seen.contains(&k.as_str()) {
                    self.error(
                        span,
                        "print() only accepts the named arguments 'sep' and 'end'".to_string(),
                    );
                    break;
                }
                seen.push(k);
            }
            for a in args {
                self.infer_value(a);
            }
            for (_, v) in named {
                let t = self.infer_value(v);
                if t != Ty::Str && !t.is_unknown() {
                    self.error(v.span, format!("print() sep/end must be str, found {t}"));
                }
            }
            return Ty::Nil;
        }
        // An **out-of-closure default provider** call. `desugar` lowers every omitted non-literal
        // default to a call on the hidden zero-arg provider `fn` its defining module declares. When
        // the definer IS in this module's transitive import closure a synthetic `from` import binds
        // that name into `self.functions` (`setup.rs`'s `Import::From` arm), and the call resolves
        // on the ordinary, fully-typed path below. When it is NOT — the name-keyed METHOD path can
        // reach a definer this module has no relation to at all, which is the ordinary
        // protocol/implementation split — no import may be synthesized (it would outrun load order),
        // so there is no local symbol and the compiler emits a direct, call-time reference to the
        // definer's proto instead. Type it as the parameter slot it fills: `infer_arg` has already
        // threaded that slot's declared type in as the expected-type hint.
        //
        // **Sound, not a bypass.** The two checks that matter have already run, elsewhere: the
        // DEFINER's own module type-checks the default expression against the declared parameter
        // type (`check_fn_body`'s decl-site copy — *"default value for parameter 'x': expected …,
        // found …"*), and protocol conformance forces the protocol's declared parameter type to
        // match the implementor's (`method_matches`). A provider name is unspellable by a user
        // (`$def$…`), so this arm can only ever see an expression `desugar` synthesized.
        if let Some(Resolution::Provider) = res {
            return expected.cloned().unwrap_or(Ty::Unknown);
        }
        // `head[k](args)` with a head that denotes data: index, then call the element (Go,
        // CPython). The one choice between that reading and the type-argument reading.
        if type_args.len() == 1
            && let Some(ix) = self.call_ctx.as_ref().and_then(|c| c.bracket.clone())
            && self.bracket_is_index(callee)
        {
            if let Some(id) = self.call_ctx.as_ref().map(|c| c.id) {
                self.record_index_call(id, span);
            }
            let callee_ty = self.index_value(callee, &ix, true);
            return self.apply_value_call(callee, callee_ty, args, named, span);
        }
        // Explicit call-site type arguments `name[T, …](…)`. Resolved once here; only generic
        // by-name calls (fn / struct / variant constructors) can consume them.
        let targs: Vec<Ty> = type_args
            .iter()
            .map(|t| self.resolve_type(t, span))
            .collect();
        // TICKET-187: `.decode[T](s)` is an ordinary member call; only `decode` on the `std.json`
        // module is JSON decode, recorded as `Resolution::Decode` on the callee for the compiler.
        // The rule table leaves it unnamed (its entry is the descriptor, DEC-214).
        if res.is_none()
            && let ExprKind::Field { obj, name, .. } = &callee.kind
            && let ExprKind::Ident(m) = &obj.kind
            && self.json_decode_member(m, name)
        {
            let sig = super::pattern::json_decode_sig();
            if args.len() != sig.params.len()
                || type_args.len() != sig.type_params.len()
                || !named.is_empty()
            {
                self.consume_named();
                self.infer_all(args);
                self.error(
                    span,
                    "decode takes one type argument and one str argument: `json.decode[T](s)`"
                        .to_string(),
                );
                return Ty::Unknown;
            }
            return self.infer_decode(callee.id, obj, &type_args[0], &args[0], span);
        }
        // Method call: `obj.method(args)`.
        if let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &callee.kind
            && !crate::ast::is_tuple_index(name)
        {
            return self.infer_member_path_call(
                callee, res, obj, name, *name_span, args, named, &targs, span, expected,
            );
        }
        if let ExprKind::Ident(name) = &callee.kind {
            match &res {
                // A type parameter: the one bare name the rule table leaves unnamed. It is erased at
                // runtime and has no constructor; rustc rejects the shape too (E0308 `expected type
                // parameter Item, found struct Item` on `let _y: Item = Item(99)`).
                None => {
                    self.infer_all(args);
                    return self.type_param_shadow_error(
                        name,
                        &format!(
                            "a type parameter is erased at runtime, so it has no constructor; take a factory function (a `fn(...) -> {name}` parameter), or bound '{name}' by a protocol with a static requirement and call `{name}.<method>(...)`"
                        ),
                        span,
                    );
                }
                Some(
                    r
                    @ (Resolution::Builtin(_) | Resolution::StructCtor(_) | Resolution::Fn { .. }),
                ) => {
                    if let Some(ty) =
                        self.infer_item_call(callee, name, r, args, named, &targs, span, expected)
                    {
                        return ty;
                    }
                }
                // A bare user-variant constructor (`Circle(5)`) is no longer allowed — variants are
                // scoped under their enum and must be written qualified (`Shape.Circle(5)`). The
                // table answers `Global` for a name no scope binds.
                Some(Resolution::Global { .. })
                    if self.lookup(name).is_none() && self.variant_owners.contains_key(name) =>
                {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                    for a in args {
                        self.infer_value(a);
                    }
                    return Ty::Unknown;
                }
                // A value the scopes hold, a module-level binding outside them, or `None`: the
                // value call below.
                Some(_) => {}
            }
        }
        // A value-call (closure / arbitrary expr) cannot take explicit type arguments.
        if !targs.is_empty() {
            let label = match &callee.kind {
                ExprKind::Ident(n) => format!("'{n}'"),
                _ => "this expression".to_string(),
            };
            self.error(span, format!("{label} takes no type arguments"));
        }
        // Fall back: the callee is an arbitrary expression; it must evaluate to a function.
        let callee_ty = self.infer(callee);
        self.apply_value_call(callee, callee_ty, args, named, span)
    }

    /// A call whose callee is the member path `obj.name`, dispatched on `res`, the rule table's
    /// answer for the callee (`resolve.rs`).
    #[allow(clippy::too_many_arguments)] // callee shape + its resolution + call shape + hint
    fn infer_member_path_call(
        &mut self,
        callee: &Expr,
        res: Option<Resolution>,
        obj: &Expr,
        name: &str,
        name_span: Span,
        args: &[Expr],
        named: &[(String, Expr)],
        targs: &[Ty],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        // A type parameter under a TYPE-LEVEL turbofish (`Item[int].tag()`, in either carrier) is
        // the parameter, which takes no type arguments (rustc E0109). A reject before any reading.
        if let Some(tname) = type_apply_param_head(obj)
            && self.shadowing_type_param(&tname)
        {
            self.infer_all(args);
            return self.type_param_shadow_error(
                &tname,
                &format!(
                    "a type parameter takes no type arguments and cannot be indexed (`{tname}[…]`)"
                ),
                span,
            );
        }
        match res {
            // `module.Struct(args)` / `module.Alias(args)`: a struct constructed through a
            // whole-module import (TICKET-172 for the exported alias, with its pinned arguments).
            Some(Resolution::StructCtor(key)) => {
                let ExprKind::Ident(mname) = &obj.kind else {
                    self.infer_all(args);
                    return Ty::Unknown;
                };
                let spelled = format!("{mname}.{name}");
                if let Some(target @ Ty::Struct(..)) = self.qualified_alias_ty(mname, name) {
                    return self
                        .infer_alias_ctor_call(&target, &spelled, args, targs, span, expected);
                }
                let Some(info) = self
                    .imported_modules
                    .get(mname)
                    .and_then(|mid| self.module_sigs.get(mid))
                    .and_then(|sig| sig.struct_defs.get(name))
                    .cloned()
                else {
                    self.infer_all(args);
                    return Ty::Unknown;
                };
                self.infer_qualified_struct_call(&info, name, &key, args, targs, span, expected)
            }
            // `module.Ctor(args)` — a qualified native builtin CONSTRUCTOR (`concurrency.Shared(0)`,
            // aliased `c.Shared(0)`, `time.timer(100)`): the same inference and license check the
            // bare name takes.
            Some(Resolution::Builtin(b)) => {
                if self.reject_item_targs(&b, args, targs, span) {
                    return Ty::Unknown;
                }
                self.infer_builtin_call(&b, args, targs, span, expected)
                    .unwrap_or(Ty::Unknown)
            }
            // `T.member(args)` where `T` is an in-scope generic TYPE PARAMETER and `member` is no
            // instance method of its bounds. M24 — the STATIC-WITNESS call: legal exactly when one
            // of `T`'s bounds declares `member` as a STATIC requirement AND the enclosing fn's
            // hidden `$w:T` witness local is reachable here; a miss reports itself.
            Some(Resolution::WitnessStatic(tname)) => {
                self.infer_witness_static_call(&tname, name, args, span)
            }
            // A bound's INSTANCE method through the parameter (`T.get(v)`, Rust's
            // `T::get(&v)`) is the path value `T.get` applied.
            Some(Resolution::ParamMethodFn { .. }) => {
                let callee_ty = self.infer(callee);
                self.apply_value_call(callee, callee_ty, args, named, span)
            }
            // A call through a type path: a variant constructor or a method of the type.
            Some(
                r @ (Resolution::Variant { .. }
                | Resolution::VariantFn { .. }
                | Resolution::MethodFn { .. }),
            ) => {
                let Some((th, head_args)) = self.peel_type_path(obj) else {
                    self.infer_all(args);
                    return Ty::Unknown;
                };
                let variant = !matches!(r, Resolution::MethodFn { .. });
                self.infer_type_member_call(
                    obj, &th, head_args, name, variant, name_span, args, targs, span, expected,
                )
            }
            // A struct or enum type path the table found no member on: the static call reports
            // the miss ("type 'E' has no static method 'm'").
            None => {
                if let Some((th, head_args)) = self.peel_type_path(obj)
                    && matches!(th.kind, TypeHeadKind::Struct | TypeHeadKind::Enum)
                {
                    return self.infer_type_member_call(
                        obj, &th, head_args, name, false, name_span, args, targs, span, expected,
                    );
                }
                self.infer_method_call(obj, name, name_span, args, targs, span, expected)
            }
            Some(_) => {
                // A type-only native name (Socket/Listener/FFI width/ptr) — no from-nothing ctor.
                // Gated on `qualified_builtin_ty` so this fires ONLY for genuine native types; a
                // (non-builtin) user `sig.types` name — e.g. an exported type alias used as a bogus
                // `mod.Alias(x)` ctor — falls through to `infer_method_call` (its original error),
                // not this native-specific message.
                if let Some(Resolution::ModuleMember { .. }) = res
                    && let ExprKind::Ident(mname) = &obj.kind
                    && self
                        .imported_modules
                        .get(mname)
                        .and_then(|mid| self.module_sigs.get(mid))
                        .is_some_and(|sig| sig.types.contains(name))
                    && self.qualified_builtin_ty(name, &[]).is_some()
                {
                    self.infer_all(args);
                    self.error(
                        span,
                        format!(
                            "'{mname}.{name}' has no constructor — it is a type-only native type (a value is obtained from the module's functions, e.g. net.connect/net.listen for a Socket)"
                        ),
                    );
                    return Ty::Unknown;
                }
                self.infer_method_call(obj, name, name_span, args, targs, span, expected)
            }
        }
    }

    /// A call through the type path `obj.name` whose head is `th`: a variant constructor when
    /// `variant` (the rule table answered `Variant`/`VariantFn`), else the static call, which also
    /// reports a miss. The head's written type arguments (`Box[int].Has(5)`), an alias head's pinned
    /// ones (`type IS = Box[str]; IS.of(3)`) and the member-level turbofish (`Box.make[U](x)`, the
    /// method's OWN `[U]`) compose here.
    #[allow(clippy::too_many_arguments)] // head + member + call shape + hint
    fn infer_type_member_call(
        &mut self,
        obj: &Expr,
        th: &TypeHead,
        head_args: Option<WrittenTypeArgs>,
        name: &str,
        variant: bool,
        name_span: Span,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        let vinfo = if variant {
            self.variants
                .get(&(th.key.clone(), name.to_string()))
                .cloned()
        } else {
            None
        };
        // `Type[T…].member(args)` — declaration-site turbofish for a generic TYPE.
        if let Some((type_exprs, _)) = head_args {
            let written: Vec<Ty> = type_exprs
                .iter()
                .map(|t| self.resolve_type(t, span))
                .collect();
            let Some(resolved) = self.written_head_args(
                &th.spelled,
                self.type_param_count(&th.key),
                th.pinned.clone(),
                written,
                span,
            ) else {
                self.infer_all(args);
                return Ty::Unknown;
            };
            if let Some(v) = vinfo {
                // A variant ctor takes NO method-level type args (`Box[int].Has[str](5)`).
                if !targs.is_empty() {
                    self.error(
                        span,
                        format!(
                            "variant '{name}' of '{}' takes no method type arguments",
                            th.name
                        ),
                    );
                }
                return self
                    .infer_variant_call(&v, name, args, &resolved, name_span, span, expected);
            }
            return self.infer_static_call(
                th, &th.name, name, args, &resolved, targs, name_span, span, expected,
            );
        }
        // An alias head (`Tone.Val(3)`, `lib.Tone.of(3)`, TICKET-172) threads the alias body's
        // pinned type arguments through both readings.
        if let Some(pinned) = th.pinned.clone() {
            if let Some(v) = vinfo {
                if !targs.is_empty() {
                    self.infer_all(args);
                    self.error(
                        span,
                        format!(
                            "variant '{name}' of '{}' takes no method type arguments",
                            th.spelled
                        ),
                    );
                    return Ty::Unknown;
                }
                return self.infer_variant_call(&v, name, args, &pinned, name_span, span, expected);
            }
            return self.infer_static_call(
                th,
                &th.spelled,
                name,
                args,
                &pinned,
                targs,
                name_span,
                span,
                expected,
            );
        }
        // Editor hover (probe-gated no-op): record the receiver `Col` of `Col.Val(3)` /
        // `Foo.default()` as its type.
        if self.hover_probe.is_some() && matches!(obj.kind, ExprKind::Ident(_)) {
            let ty = match th.kind {
                TypeHeadKind::Enum => Ty::Enum(th.key.clone(), Vec::new()),
                _ => Ty::Struct(th.key.clone(), Vec::new()),
            };
            self.hover_record_at(obj.span, &ty, HoverKind::Other, None);
        }
        if let Some(v) = vinfo {
            // The OLD gliding form `Enum.Variant[T](args)` (type args on the VARIANT) is removed —
            // explicit type args go on the TYPE: `Enum[T].Variant(args)`.
            if !targs.is_empty() {
                self.infer_all(args);
                self.error(
                    span,
                    format!(
                        "put the type arguments on the type: {}[{}].{name}(...)",
                        th.spelled,
                        render_targs(targs)
                    ),
                );
                return Ty::Unknown;
            }
            return self.infer_variant_call(&v, name, args, &[], name_span, span, expected);
        }
        // A STATIC (or receiver-first instance) method through the type. The member-level
        // turbofish is the bare carrier of the method's OWN `[U]` args (PART 2).
        self.infer_static_call(
            th,
            &th.spelled,
            name,
            args,
            &[],
            targs,
            name_span,
            span,
            expected,
        )
    }

    /// The "takes no type arguments" reject for a by-name item call (a builtin, a struct
    /// constructor, a fn). An alias of a struct passes: the constructor decides through
    /// `written_head_args` (TICKET-180 P2, TICKET-172). `true` when it reported.
    fn reject_item_targs(&mut self, name: &str, args: &[Expr], targs: &[Ty], span: Span) -> bool {
        let alias_takes_targs = matches!(self.alias_body_ty(name), Some(Ty::Struct(..)));
        if targs.is_empty() || self.name_is_generic(name) || alias_takes_targs {
            return false;
        }
        self.error(span, format!("'{name}' takes no type arguments"));
        for a in args {
            self.infer(a);
        }
        true
    }

    /// A call of the bare name `name` that the rule table answered `r` for: a builtin, a struct
    /// constructor or a module-level fn. `None` for a first-class builtin fn with no call arm of
    /// its own (the value call types it).
    #[allow(clippy::too_many_arguments)] // callee + resolution + call shape + hint
    fn infer_item_call(
        &mut self,
        callee: &Expr,
        name: &str,
        r: &Resolution,
        args: &[Expr],
        named: &[(String, Expr)],
        targs: &[Ty],
        span: Span,
        expected: Option<&Ty>,
    ) -> Option<Ty> {
        // A DIRECT call of a from-imported fn (`h()`) above its own `import` is the same
        // use-before-import as the bare read (`g := h`), but a direct callee never reaches
        // `infer_ident` — so the guard is repeated at this funnel, or the two spellings of one
        // concept would disagree. A same-module fn — never in `import_binds` — is untouched.
        if let Resolution::Fn { .. } = r {
            self.reject_read_above_import(name, true, callee.span);
        }
        // Editor hover (probe-gated no-op): record a DISPLAY function type at the callee token so
        // hovering a CALL's callee yields its signature — the callee never reaches
        // `infer()`/`hover_record_expr`. A free fn → its declared `FnSig`; a struct ctor →
        // fields-to-`Struct`; a reserved builtin → its `builtin_sig` display sig. Emits no error.
        if self.hover_probe.is_some()
            && let Some(fty) = self.callee_display_ty(name)
        {
            // doc: a user-defined free fn owns its `FnSig::doc` and NOTHING else — an
            // undocumented user fn must NOT fall through to a builtin blurb.
            let doc = if let Some(sig) = self.functions.get(name) {
                sig.doc.clone()
            } else {
                self.name_docs
                    .get(name)
                    .cloned()
                    .or_else(|| builtin_type_doc(name))
            };
            self.hover_record_at(callee.span, &fty, HoverKind::Func, doc);
        }
        if !named.is_empty() && self.lookup(name).is_none() && super::is_reserved_name(name) {
            self.refuse_named(name, named, span);
        }
        if self.reject_item_targs(name, args, targs, span) {
            return Some(Ty::Unknown);
        }
        match r {
            Resolution::Builtin(b) => self.infer_builtin_call(b, args, targs, span, expected),
            Resolution::StructCtor(key) => {
                Some(self.infer_struct_ctor_call(name, key, args, targs, span, expected))
            }
            _ => Some(self.infer_fn_call(name, args, targs, callee.span, span, expected)),
        }
    }

    /// A call of an arbitrary callee expression whose type is `callee_ty`: it must evaluate to a
    /// function. The value-call fallback of `infer_call_dispatch`, shared by the index-then-call
    /// reading of a call bracket and by a call through a type parameter's member (`T.get(v)`).
    fn apply_value_call(
        &mut self,
        callee: &Expr,
        callee_ty: Ty,
        args: &[Expr],
        named: &[(String, Expr)],
        span: Span,
    ) -> Ty {
        match callee_ty {
            // A user fn / closure VALUE carrying keyword arguments (`g := greet; g(name="Bob")`):
            // resolve each label to a positional slot against the value's surface labels (Swift-style
            // keyword args through a value). Positional-only value calls skip this entirely (hot path).
            // TICKET-187: a variadic value packs its surplus the same way, under the same certainty.
            Ty::Func {
                params,
                ret,
                labels,
            } if !named.is_empty()
                || (labels.variadic().is_some()
                    && matches!(&callee.kind, ExprKind::Ident(n) if self.labels_certain(n).is_ok())) =>
            {
                // TICKET-139 (W14-2) — labels are surface-only (equality-neutral, DEC-108), so a
                // callee that is not certain to hold ONE known function may bind a permuted set of
                // names: `fs := [f, ren]; fs[1](a=1, b=2)` bound `a`/`b` by `f`'s labels and ran
                // `ren`. Only a `kw_certain` `Ident` binding keeps keyword calls; everything else
                // is a compile error (positional calls are unaffected).
                // TICKET-186: `labels_certain` is the one decider, shared with the by-name path.
                let key = match &callee.kind {
                    ExprKind::Ident(n) => self.labels_certain(n),
                    _ => Err(super::globals::KwDeny::NotOneFn),
                };
                self.consume_named();
                let key = match key {
                    Ok(key) => key,
                    Err(deny) => {
                        for a in args {
                            self.infer(a);
                        }
                        for (_, v) in named {
                            self.infer(v);
                        }
                        let name = match &callee.kind {
                            ExprKind::Ident(n) => Some(n.as_str()),
                            _ => None,
                        };
                        self.error(span, super::globals::kw_deny_msg(name, deny));
                        return *ret;
                    }
                };
                // Settled at the binding's `pop_scope` (a write may come after this call). Nothing
                // is recorded under the generic-arg prepass, and a closure body inferred more than
                // once (DEC-025) records each call once.
                let pending = (key, super::globals::KwUse::Keyword(span));
                if !self.generic_arg_prepass && !self.kw_pending.contains(&pending) {
                    self.kw_pending.push(pending);
                }
                // A value read from one declaration binds that declaration's slots (TICKET-197),
                // so it fills defaults and packs a variadic like a direct call. Any other value's
                // slots are its labels, callee-filled from `min_params` on (a trailing run).
                let minp = labels.min_or(params.len());
                let value_slots = match &labels.slots {
                    Some(slots) => slots.to_vec(),
                    None => callable_slots(&labels.names, minp, params.len()),
                };
                let Some(bound) = self.bind_call(Some(&value_slots), "closure", args, 0, span)
                else {
                    return *ret;
                };
                self.check_args_range("closure", &params, minp, &bound, span);
                *ret
            }
            // A first-class builtin fn value (`f := ord; f("a")`) checks its args against the builtin's
            // canonical signature, exactly like a closure/fn value. `print`'s value form is thus a
            // fixed 1-arg call — the variadic / `sep=`/`end=` surface stays direct-call-only. Its value
            // form takes NO keyword arguments (labels are a user-fn surface).
            Ty::BuiltinFn { params: _, ret } if !named.is_empty() => {
                self.consume_named();
                for a in args {
                    self.infer(a);
                }
                for (_, v) in named {
                    self.infer(v);
                }
                self.error(
                    span,
                    "a first-class builtin function value takes no keyword arguments (labels apply only to user function values)"
                        .to_string(),
                );
                *ret
            }
            Ty::Func {
                params,
                ret,
                labels,
            } => {
                // STRICT — an int never widens into a `float` param through a function VALUE (D3, like
                // every other sink) — write `f(1.0)`.
                //
                // Arity is a RANGE when the underlying declaration's trailing parameters carry
                // defaults: the callee fills the omitted ones itself, so `f := g; f()` is legal and so
                // is `f(1)`. `min_or` collapses to exact arity for everything else (a bare `fn(T)`
                // annotation, a closure, a builtin) — those carry no optional tail.
                let minp = labels.min_or(params.len());
                self.check_args_range("closure", &params, minp, args, span);
                *ret
            }
            Ty::BuiltinFn { params, ret } => {
                self.check_args("closure", &params, args, span);
                *ret
            }
            Ty::Unknown => {
                self.consume_named();
                for a in args {
                    self.infer(a);
                }
                Ty::Unknown
            }
            // A module bind lands in the VALUE namespace, so `import lib.Point` beats a same-named
            // USER `struct`/`enum` ctor in expression position (`is_reserved_module_bind` gates only
            // the RESERVED names). The call is a hard error either way — name the collision instead of
            // leaving the user to wonder where their ctor went.
            Ty::Module(m)
                if self.structs.contains_key(&self.bare_key(m.as_str()))
                    || self.enums.contains_key(&self.bare_key(m.as_str())) =>
            {
                let m = m.clone();
                for a in args {
                    self.infer(a);
                }
                self.error(
                    span,
                    format!(
                        "module bind '{m}' shadows the same-named type '{m}' — alias the import: `import ... as {}`",
                        m.to_lowercase()
                    ),
                );
                Ty::Unknown
            }
            other => {
                for a in args {
                    self.infer(a);
                }
                self.error(span, format!("{other} is not callable"));
                Ty::Unknown
            }
        }
    }

    /// The one refusal for a callee that takes no named arguments: a native, builtin or `extern`
    /// callee, whose sig has no [`FnSig::slots`]. Infers each named value once (so its own errors
    /// still surface) and marks the call's named arguments consumed.
    pub(super) fn refuse_named(&mut self, head: &str, named: &[(String, Expr)], span: Span) {
        if !self.consume_named() {
            return;
        }
        self.error(
            span,
            format!("'{head}' takes no named arguments; pass its arguments positionally"),
        );
        for (_, v) in named {
            self.infer_value(v);
        }
    }

    /// **The one call binder.** Binds `args` and `named` against the slots of the declaration (or
    /// `kw_certain` function value) the checker's own dispatch already picked, and returns the
    /// positional argument list for the caller's existing arg check:
    ///
    /// * `Some(Borrowed(args))` — nothing to bind (a positional call that fills no default), or a
    ///   native callee (`slots` `None`), whose named arguments [`Self::refuse_named`] reports;
    /// * `Some(Owned(bound))` — the bound list: supplied arguments in slot order, each omitted
    ///   default as a synthetic fill node, the variadic surplus packed into one `List`. Its
    ///   [`ArgFill`] plan is recorded in [`Self::call_plans`] for the compiler;
    /// * `None` — a binding error was reported and every argument inferred once; the caller
    ///   returns `Ty::Unknown` and adds no arity error.
    ///
    /// The call's named arguments and NodeId come from [`Self::call_ctx`], which `infer_call` sets
    /// for the call it dispatches. `n_targs` is the count of type arguments the call forwards to a
    /// generic field provider (DEC-035): a `Dflt::GenericProvider` slot without a full turbofish
    /// stays unfilled.
    pub(super) fn bind_call<'a>(
        &mut self,
        slots: Option<&[crate::desugar::SlotSpec]>,
        head: &str,
        args: &'a [Expr],
        n_targs: usize,
        span: Span,
    ) -> Option<std::borrow::Cow<'a, [Expr]>> {
        use std::borrow::Cow;
        let (call_id, named, pack_origin) = match &self.call_ctx {
            Some(c) => (c.id, c.named.clone(), c.pack_origin),
            None => (crate::ast::NodeId::SYNTH, Vec::new(), span),
        };
        let named = &named[..];
        let Some(slots) = slots else {
            if !named.is_empty() {
                self.refuse_named(head, named, span);
            }
            return Some(Cow::Borrowed(args));
        };
        if !named.is_empty() {
            self.consume_named();
        }
        let plan = match self.plan_call(slots, args, named, n_targs, span) {
            Err((msg, at)) => {
                self.error(at, msg);
                for a in args {
                    self.infer_value(a);
                }
                for (_, v) in named {
                    self.infer_value(v);
                }
                return None;
            }
            Ok(None) => return Some(Cow::Borrowed(args)),
            Ok(Some(plan)) => plan,
        };
        let pick = |ci: usize| -> Expr {
            if ci < args.len() {
                args[ci].clone()
            } else {
                named[ci - args.len()].1.clone()
            }
        };
        let bound: Vec<Expr> = plan
            .iter()
            .map(|f| match f {
                ArgFill::Arg(ci) => pick(*ci),
                ArgFill::Pack(cis) => Expr {
                    id: crate::ast::NodeId::SYNTH,
                    kind: ExprKind::List(
                        cis.iter().map(|&ci| pick(ci)).collect(),
                        Some(pack_origin),
                    ),
                    span,
                },
                ArgFill::Provider(name) => Expr {
                    id: crate::ast::NodeId::SYNTH,
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            id: crate::ast::NodeId::SYNTH,
                            kind: ExprKind::Ident(name.clone()),
                            span,
                        }),
                        args: Vec::new(),
                        named: Vec::new(),
                        type_args: Vec::new(),
                        bracket: None,
                    },
                    span,
                },
                // The checker types a renumbered clone; the compiler compiles the declaration's
                // own node, in its declaring module.
                ArgFill::Inline { expr, .. } => {
                    let mut e = expr.clone();
                    crate::ast::renumber_expr(&mut e);
                    e
                }
            })
            .collect();
        if let Some((from, _)) = self
            .current_provider
            .clone()
            .filter(|_| self.records_node(call_id))
        {
            for f in &plan {
                if let ArgFill::Provider(to) = f {
                    let e = self.provider_edges.entry(from.clone()).or_default();
                    if !e.0.contains(to) {
                        e.0.push(to.clone());
                    }
                }
            }
        }
        if self.records_node(call_id) {
            crate::checker::record_call_table_entry(
                &mut self.call_plans,
                &mut self.table_conflicts,
                (self.graph_module_idx, call_id.0),
                plan,
                "call-plan",
                span,
            );
        }
        Some(Cow::Owned(bound))
    }

    /// Mark the named arguments of the call being dispatched ([`Self::call_ctx`]) consumed: a binder
    /// or a refusal answered them. Returns `false` when they already were.
    pub(super) fn consume_named(&mut self) -> bool {
        match &mut self.call_ctx {
            Some(c) => !std::mem::replace(&mut c.consumed, true),
            None => true,
        }
    }

    /// The call-binding slots of `method` on a struct or enum receiver.
    fn method_slots(&self, recv: &Ty, method: &str) -> Option<Vec<crate::desugar::SlotSpec>> {
        match recv {
            Ty::Struct(k, _) => self.struct_shape(k)?.methods.get(method)?.slots.clone(),
            Ty::Enum(k, _) => self.enum_methods_of(k)?.get(method)?.slots.clone(),
            _ => None,
        }
    }

    /// [`Self::native_handle_method`] for a call: a builtin receiver method takes no named
    /// arguments, so the call's named arguments get [`Self::refuse_named`].
    pub(super) fn native_method_sig(
        &mut self,
        key: &str,
        method: &str,
        targs: &[Ty],
    ) -> Option<FnSig> {
        let sig = self.native_handle_method(key, method, targs)?;
        if let Some(c) = self.call_ctx.clone()
            && !c.named.is_empty()
        {
            self.refuse_named(method, &c.named, c.span);
        }
        Some(sig)
    }

    /// The ONE map from a classified default to its fill: `S(...)` and `json.decode[S]` both read
    /// it (TICKET-198). `None` is a hole the callee's prologue fills (legal only as a trailing run).
    pub(super) fn default_fill(&self, d: &crate::desugar::Dflt, n_targs: usize) -> Option<ArgFill> {
        use crate::desugar::Dflt;
        match d {
            Dflt::CalleeFilled => None,
            Dflt::GenericProvider { tps, .. } if n_targs != *tps => None,
            Dflt::Provider { name, .. } | Dflt::GenericProvider { name, .. } => {
                Some(ArgFill::Provider(name.clone()))
            }
            Dflt::Inline(e) => Some(ArgFill::Inline {
                module: self
                    .module_idx_of_file
                    .get(&e.span.file)
                    .copied()
                    .unwrap_or(self.graph_module_idx),
                expr: e.clone(),
            }),
        }
    }

    /// [`Self::bind_call`]'s decision: `Ok(None)` when the call needs no binding (no named
    /// argument, not variadic, and some omitted slot has no default), else the slot plan, or the
    /// binding error and its span. A message about one named argument points at its value.
    fn plan_call(
        &self,
        slots: &[crate::desugar::SlotSpec],
        args: &[Expr],
        named: &[(String, Expr)],
        n_targs: usize,
        span: Span,
    ) -> Result<Option<Vec<ArgFill>>, (String, Span)> {
        let np = args.len();
        let n = slots.len();
        let slot_name = |i: usize| -> String {
            slots[i]
                .name
                .clone()
                .unwrap_or_else(|| format!("#{}", i + 1))
        };
        let unknown = |k: &str| -> String {
            let known: Vec<&str> = slots.iter().filter_map(|s| s.name.as_deref()).collect();
            if known.is_empty() {
                format!("unknown named argument '{k}'")
            } else {
                format!(
                    "unknown named argument '{k}' (its parameters are: {})",
                    known.join(", ")
                )
            }
        };
        let variadic = slots.iter().position(|s| s.is_variadic);
        if variadic.is_none() {
            let fillable = np < n && (np..n).all(|i| slots[i].default.is_some());
            if named.is_empty() && !fillable {
                return Ok(None);
            }
            if np > n {
                return Err((
                    format!("too many arguments: expected at most {n}, got {np}"),
                    span,
                ));
            }
        }
        // `by_slot[i]` = the combined index (`args ++ named values`) supplied for slot `i`.
        let mut by_slot: Vec<Option<usize>> = vec![None; n];
        let pos_limit = variadic.unwrap_or(n);
        for (i, s) in by_slot.iter_mut().enumerate().take(np.min(pos_limit)) {
            *s = Some(i);
        }
        for (j, (k, v)) in named.iter().enumerate() {
            let Some(idx) = slots.iter().position(|s| s.name.as_deref() == Some(k)) else {
                return Err((unknown(k), v.span));
            };
            if Some(idx) == variadic {
                return Err((
                    format!(
                        "argument '{k}' is the variadic parameter and cannot be passed by name (it collects the surplus positionals)"
                    ),
                    v.span,
                ));
            }
            if idx < np.min(pos_limit) {
                return Err((
                    format!("argument '{k}' specified both positionally and by name"),
                    v.span,
                ));
            }
            if by_slot[idx].is_some() {
                return Err((format!("duplicate named argument '{k}'"), v.span));
            }
            by_slot[idx] = Some(np + j);
        }
        let mut out: Vec<Option<ArgFill>> = Vec::with_capacity(n);
        for (i, s) in by_slot.into_iter().enumerate() {
            if Some(i) == variadic {
                out.push(Some(ArgFill::Pack((i..np).collect())));
                continue;
            }
            match (s, &slots[i].default) {
                (Some(ci), _) => out.push(Some(ArgFill::Arg(ci))),
                (None, Some(d)) => out.push(self.default_fill(d, n_targs)),
                (None, None) => {
                    let kind = if variadic.is_some_and(|v| i > v) {
                        "missing required keyword argument"
                    } else {
                        "missing required argument"
                    };
                    return Err((format!("{kind} '{}'", slot_name(i)), span));
                }
            }
        }
        // A trailing run of callee-filled slots is what a short call encodes.
        while matches!(out.last(), Some(None)) {
            out.pop();
        }
        // Anything still unfilled sits BEFORE a supplied argument, which a short call cannot
        // express: it pushes fewer values, it cannot leave a gap.
        if let Some(i) = out.iter().position(Option::is_none) {
            let later = (i + 1..out.len())
                .find(|&j| out[j].is_some())
                .map(slot_name)
                .unwrap_or_default();
            let p = slot_name(i);
            return Err((
                format!(
                    "the default for '{p}' is filled by the callee and can only be omitted from the END of a call, but '{later}' is supplied after it — pass '{p}' explicitly"
                ),
                span,
            ));
        }
        Ok(Some(
            out.into_iter()
                .map(|f| f.expect("no hole remains"))
                .collect(),
        ))
    }

    /// The member a call `recv.member(..)` reads as data, not as a method: a struct field when the
    /// struct has no method of that name (a method wins), or a module global that holds no fn.
    /// `None` for every other receiver and member.
    pub(super) fn data_member_ty(&self, recv_ty: &Ty, member: &str) -> Option<Ty> {
        match recv_ty {
            Ty::Struct(sname, targs) => {
                if self
                    .struct_shape(sname)
                    .is_some_and(|info| info.methods.contains_key(member))
                {
                    return None;
                }
                self.struct_field_ty(sname, targs, member)
            }
            Ty::Module(mname) => self
                .imported_modules
                .get(mname)
                .and_then(|id| self.module_sigs.get(id))
                .and_then(|sig| sig.member(member))
                .filter(|m| !m.holds_fn())
                .map(|m| m.ty.clone()),
            _ => None,
        }
    }

    /// The one answer to whether a call's one-arg bracket indexes its head (`fs[k](10)`) or passes
    /// a type argument (`idt[int](3)`). Never an index when the head is a fn-like path; a bare name
    /// indexes when it binds a local or a global; a member head indexes when its member is data
    /// ([`Self::data_member_ty`]) for the receiver's type, learned by a speculative probe.
    fn bracket_is_index(&mut self, callee: &Expr) -> bool {
        if self.path_fn(callee).is_some() {
            return false;
        }
        match &callee.kind {
            ExprKind::Ident(n) => {
                matches!(
                    self.head_binding(n),
                    HeadBinding::Local | HeadBinding::Global
                )
            }
            ExprKind::Field { obj, name, .. } if !crate::ast::is_tuple_index(name) => {
                let mark = self.diag_mark();
                let recv = self.infer(obj);
                self.diag_rollback(mark);
                self.data_member_ty(&recv, name).is_some()
            }
            _ => false,
        }
    }

    /// Type-check a call through a type, `Type.method(args)`: the path value `Type.method` applied.
    /// `th` is the type head; `tname` its display name. Reads the sig from `type_member_fn`, so an
    /// instance method takes its receiver first (`P.get(p)`); rejects a refused member or an unknown
    /// method. The enclosing type's params (`targs`, from `Box[int].empty()`) AND the method's OWN
    /// `[U]` params (`mtargs`, from the combined `Box[int].make[U](x)`) compose into ONE by-name
    /// substitution map: enclosing seeded from `targs`, method from `mtargs`, then `hint` (a `let`/return
    /// annotation, `b: Box[int] = Box.empty()`) seeds any still-free ENCLOSING param, the rest inferred by
    /// unifying the declared param types against the argument types (like a generic free fn). A method-OWN
    /// `[U]` left un-inferred degrades to `Ty::Unknown` (genuinely unconstrained, refines on use); an
    /// un-inferred ENCLOSING param (no turbofish, no arg binding, no hint) DELIBERATELY leaks as a
    /// `Ty::Param` so the first mismatching use routes to the "un-inferred type parameter … bind it at the
    /// construction site" diagnostic — parity with the generic free-fn path, and closing the soundness hole
    /// where `Ty::Unknown` swallowed any later argument. Mirrors the instance-method arms minus the receiver.
    #[allow(clippy::too_many_arguments)] // enclosing key/name + method + args + enclosing & method targs + span + hint
    pub(super) fn infer_static_call(
        &mut self,
        th: &TypeHead,
        // How the TYPE was spelled at this call site — bare (`Holder`) or module-qualified
        // (`lib.Holder`, or an alias). Diagnostics only, and that includes the witness pin advice,
        // which must name a spelling that compiles.
        tname: &str,
        method: &str,
        args: &[Expr],
        targs: &[Ty],
        mtargs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        // The call through a type is the path's value applied: `type_member_fn` is the value
        // side's answer, so an instance method takes its receiver first (`P.get(p)`).
        let Some(pf) = self.type_member_fn(th, None, method) else {
            self.infer_all(args);
            if !self.type_member_refusal(th, method, name_span) {
                self.error(
                    span,
                    format!("type '{tname}' has no static method '{method}'"),
                );
            }
            return Ty::Unknown;
        };
        let tps = pf.head_decl.clone();
        let mut sig = pf.sig;
        sig.type_params = sig.type_params.split_off(pf.head_params);
        // Editor hover (probe-gated no-op): record the static method's declared call signature at the
        // method-name token (`Foo.default()` → "fn() -> Foo"), mirroring the free-fn convention
        // (declared `FnSig` params/ret verbatim). Emits no error, changes no checking result.
        if self.hover_probe.is_some() {
            let fty = Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
                labels: crate::checker::FnLabels::default(),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
        // A method-level turbofish (`Box.empty[int]()`) on a static method that declares NO own
        // `[U]` is an arity error: it takes no method type arguments.
        if !mtargs.is_empty() && sig.type_params.is_empty() {
            self.infer_all(args);
            self.error(
                span,
                format!(
                    "method '{method}' takes no type argument(s) (it declares no own type parameters)"
                ),
            );
            return Ty::Unknown;
        }
        let Some(bound) = self.bind_call(sig.slots.as_deref(), method, args, 0, span) else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        // Non-generic static (no enclosing AND no method params): the simple substitution-free path.
        if tps.is_empty() && sig.type_params.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{tname}' takes no type arguments"));
            }
            self.check_args(method, &sig.params, args, span);
            return sig.ret;
        }
        // Generic: ONE by-name substitution map over BOTH the enclosing type's params and the
        // method's own `[U]` params. Seed each from its respective turbofish, then infer the rest by
        // unifying the declared param types (which may carry either set of `Ty::Param`s) against the
        // argument types — exactly like the struct ctor + a generic free fn.
        let arg_tys = self.infer_generic_arg_tys(args, &sig.params, &[]);
        if arg_tys.len() != sig.params.len() {
            self.check_arity(method, sig.params.len(), args, span);
        }
        let mut sub = self.seed_targs(tname, &tps, targs, span);
        let msub = self.seed_targs(method, &sig.type_params, mtargs, span);
        sub.extend(msub);
        for (decl, actual) in sig.params.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        self.recover_iter_elems(&sig.type_params, &mut sub, span);
        self.recover_index_args(&sig.type_params, &mut sub, span);
        // Expected-type checking-mode: a `let`/return annotation seeds any ENCLOSING type param the
        // arguments left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing)
        // against the hint — so `b: Box[int] = Box.empty()` pins the return-only `T`. Runs AFTER
        // arg-unification + iter/index recovery and BEFORE the per-arg check + bounds enforcement, so
        // precedence stays turbofish > args > annotation (`seed_from_hint`'s `unify` binds only a param
        // still FREE) and bounds enforce against the hint-pinned concrete type (mirrors the free-fn
        // path, `infer_generic_call`).
        seed_from_hint(hint, &sig.ret, &mut sub);
        let all_tps: Vec<TyParam> = tps.iter().chain(sig.type_params.iter()).cloned().collect();
        self.widen_targs_from_hint(
            hint,
            &sig.ret,
            &all_tps,
            &sig.params,
            &arg_tys,
            !targs.is_empty() || !mtargs.is_empty(),
            &mut sub,
            span,
        );
        for (decl, (actual, arg)) in sig.params.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(method, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &all_tps, &sub, span);
        self.enforce_bounds(&sig.type_params, &all_tps, &sub, span);
        // Conditional method: a STATIC method may carry a receiver-param `where` bound (naming the
        // enclosing type's own param) too — `fn_sig` is shared and records it regardless of `self`.
        // Enforce it against the enclosing param's inferred concrete type here (the sub map already
        // holds `{T -> concrete}`), mirroring the instance-method dispatch arms. Without this a
        // conditional factory `Box.of(Q(1))` (Q non-Comparable) would be silently accepted. No-op
        // when `where_bounds` is empty. (The non-generic fast path above cannot carry receiver
        // bounds: `where_bounds` non-empty ⟹ the enclosing type has params ⟹ `tps` non-empty.)
        self.enforce_bounds(&sig.where_bounds, &all_tps, &sub, span);
        // M24 Task 5 — half two of the contract for a STATIC method that declares its own witnessed
        // `[T]` (`Holder.build(c)`). Recorded BEFORE the degrade below so an un-inferable `T` reports
        // "is not determined here" (its own diagnostic) rather than looking bound to `Unknown`.
        // Only the METHOD's params can be witnessed, so the ENCLOSING type's `tps` are never here.
        if !sig.witness_params.is_empty() {
            let wparams = sig.witness_params.clone();
            // The pin goes AFTER the member name (`Holder.build[Counter]()`); the bare
            // `build[Counter](...)` the free spelling suggests is read as a free call.
            let recv = WitnessCallee::Dotted(tname.to_string());
            self.record_witness_call(method, &wparams, &sub, name_span, span, recv);
        }
        // A method-OWN `[U]` param still un-inferred (no method turbofish, unbindable from args, e.g.
        // `make[U]() -> List[U]`) degrades to the refinable `Ty::Unknown` — a method-local `[U]` with
        // nothing to bind it is genuinely unconstrained, and downstream use refines it cleanly.
        //
        // The ENCLOSING type's params (`tps`, the `T` of `Box[T]`) are DELIBERATELY NOT degraded: an
        // enclosing `T` left un-inferred (no type-level turbofish `Box[int].empty()`, no argument
        // binding `T`, no annotation/return hint `b: Box[int] = Box.empty()`) is the SAME "you must pin
        // `T` at the construction site" situation that already rejects bare container ctors (`[]`) and
        // generic FREE-function returns (`mkbox()`). Leaving it as a leaked `Ty::Param` (via `subst`
        // below) routes the first mismatching/mutating use to the existing diagnostics — a `List[T]`
        // field mutator hits the "un-inferred type parameter … bind it at the construction site" hint,
        // a `T`-param user method gets the base "expected T, found <ty>" — instead of `Ty::Unknown`
        // silently swallowing any later argument and defeating homogeneity checking (the soundness hole).
        for tp in sig.type_params.iter() {
            sub.entry(tp.name.clone()).or_insert(Ty::Unknown);
        }
        subst(&sig.ret, &sub)
    }

    /// Resolve a by-name call (builtin / constructor / variant / global fn). Returns `None` if
    /// `name` is none of those, so the caller can treat it as a value-call.
    /// Type-check an enum-variant constructor `Enum.Variant(args)` given its resolved `VariantInfo`.
    /// Handles both non-generic and generic enums (type args from explicit `[T]` or inferred from the
    /// payload). Shared by the qualified-call fast path and reachable only once the `(enum, variant)`
    /// pair is known.
    #[allow(clippy::too_many_arguments)] // variant shape + call args + span + expected-type hint
    pub(super) fn infer_variant_call(
        &mut self,
        v: &VariantInfo,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let tps = self
            .enum_type_params
            .get(&v.enum_name)
            .cloned()
            .unwrap_or_default();
        // Editor hover (probe-gated no-op): record the variant's ctor signature at the variant-name
        // token (`Col.Val(3)` → "fn(int) -> Col"). The enum's declared `Ty::Param`s are preserved in
        // the return type, so a generic variant Displays "fn(T) -> Box[T]". Emits no error.
        if self.hover_probe.is_some() {
            let targs_disp: Vec<Ty> = tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect();
            let fty = Ty::Func {
                params: v.payload.clone(),
                ret: Box::new(Ty::Enum(v.enum_name.clone(), targs_disp)),
                labels: crate::checker::FnLabels::default(),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, None);
        }
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            self.check_args(name, &v.payload, args, span);
            return Ty::Enum(v.enum_name.clone(), Vec::new());
        }
        // Generic enum: type arguments come from explicit call-site args (`Tree.Node[int](…)`) when
        // given, else are inferred by unifying the variant's declared payload types (which contain
        // the enum's `Ty::Param`s) against the argument types, then check each argument against the
        // substituted payload.
        let hints = self.ctor_arg_hints(
            hint,
            &Ty::Enum(v.enum_name.clone(), param_shape(&tps)),
            &tps,
            &v.payload,
            targs,
        );
        let arg_tys = self.infer_generic_arg_tys(args, &v.payload, &hints);
        if arg_tys.len() != v.payload.len() {
            self.check_arity(name, v.payload.len(), args, span);
        }
        let mut sub = self.seed_targs(name, &tps, targs, span);
        for (decl, actual) in v.payload.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: an annotation (`let`/return/param `Enum[int]`) seeds any type
        // param the args left FREE — unify the declared enum SHAPE (Param-bearing) against the hint
        // AFTER arg-unification, so turbofish/args win and the seed only breaks a genuine deadlock.
        seed_from_hint(
            hint,
            &Ty::Enum(v.enum_name.clone(), param_shape(&tps)),
            &mut sub,
        );
        self.widen_targs_from_hint(
            hint,
            &Ty::Enum(v.enum_name.clone(), param_shape(&tps)),
            &tps,
            &v.payload,
            &arg_tys,
            !targs.is_empty(),
            &mut sub,
            span,
        );
        for (decl, (actual, arg)) in v.payload.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Enum(v.enum_name.clone(), targs_out)
    }

    /// `module.Alias(args)` through an exported alias whose `target` is a struct
    /// (TICKET-172): the target's constructor, with the alias body's pinned type arguments. An alias
    /// that pins its arguments takes no more, exactly like the bare alias constructor.
    pub(super) fn infer_alias_ctor_call(
        &mut self,
        target: &Ty,
        spelled: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        expected: Option<&Ty>,
    ) -> Ty {
        let Ty::Struct(key, head_targs) = target else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let Some(targs) = self.written_head_args(
            spelled,
            self.type_param_count(key),
            Some(head_targs.clone()),
            targs.to_vec(),
            span,
        ) else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let targs = targs.as_slice();
        let Some(info) = self.struct_shape(key).cloned() else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        self.infer_qualified_struct_call(&info, spelled, key, args, targs, span, expected)
    }

    /// Type-check a module-qualified struct constructor `module.Struct(args)` from the struct's
    /// resolved `StructInfo` (held in the defining module's `ModuleSig`), mirroring the bare struct
    /// path in `infer_named_call`. Returns a `Ty::Struct` keyed by the DECLARING module's runtime key
    /// (`key`, bare in the common case, `<dotted>::Name` on a genuine cross-module clash) so the value
    /// resolves its fields/methods against the right module's layout — and so it unifies with the
    /// declaring module's own signatures (which carry the same key).
    #[allow(clippy::too_many_arguments)] // struct shape + call args + span + expected-type hint
    pub(super) fn infer_qualified_struct_call(
        &mut self,
        info: &StructInfo,
        name: &str,
        key: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let tps = info.type_params.clone();
        let field_tys: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
        let Some(bound) =
            self.bind_call(info.field_slots.as_deref(), name, args, targs.len(), span)
        else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            // Struct ctor float fields are coerced per-field by the backend's `NewStruct` site.
            self.check_args(name, &field_tys, args, span);
            return Ty::strukt(key.to_string());
        }
        let hints = self.ctor_arg_hints(
            hint,
            &Ty::Struct(key.to_string(), param_shape(&tps)),
            &tps,
            &field_tys,
            targs,
        );
        let arg_tys = self.infer_generic_arg_tys(args, &field_tys, &hints);
        self.check_ctor_arity(
            name,
            &tps,
            &info.fields,
            &info.defaulted_fields,
            targs,
            args,
            span,
        );
        let mut sub = self.seed_targs(name, &tps, targs, span);
        for (decl, actual) in field_tys.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: a `let`/return/param annotation (`Heap[int]`) seeds any type
        // param the args left FREE, BEFORE the deadlock probe — so the annotation breaks the
        // `Heap([], fn(a, b): a < b)` deadlock (it pins `T`, which then pins the closure params).
        seed_from_hint(
            hint,
            &Ty::Struct(key.to_string(), param_shape(&tps)),
            &mut sub,
        );
        self.widen_targs_from_hint(
            hint,
            &Ty::Struct(key.to_string(), param_shape(&tps)),
            &tps,
            &field_tys,
            &arg_tys,
            !targs.is_empty(),
            &mut sub,
            span,
        );
        // Same un-inferable closure-param deadlock guard as the bare struct-ctor / free-fn paths
        // (`Heap([], fn(a, b): a < b)` via a module-qualified ctor): report the cause and bind the
        // params to Unknown BEFORE the per-arg closure body is checked, so it doesn't leak a
        // misleading "cannot compare T and T" from inside the lambda.
        self.report_uninferable_closure_params(name, &tps, &field_tys, args, &mut sub, span);
        for (decl, (actual, arg)) in field_tys.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Struct(key.to_string(), targs_out)
    }

    /// Reject a scalar cast (`int`/`float`/`bool`) applied to an arg OUTSIDE the scalar-cast domain
    /// at check time. The domain is `int`/`float`/`bool`/`str` (`spec.md`); `List`/`Map`/`Set`/tuple/
    /// struct/enum/function are all outside it and always fault at runtime — `Vm::builtin_int` /
    /// `builtin_float` / `builtin_bool` (`src/vm/stmt.rs:1695`, `:1735`, `:1772`) handle only inline
    /// scalars and `Obj::Str`, then fall through to the error
    /// (`{cast}() cannot convert List`). Catching it here turns a check-OK-then-run-fault into a
    /// clean compile error (the value a statically-typed language adds over Python's runtime `TypeError`).
    /// Also covers `Ty::Option`/`Ty::Result` (both `Obj::Enum` at runtime, so both report `enum`),
    /// `Ty::Bytes`, `Ty::ByteArray`, and the `Shared`/`Channel` handle family (TICKET-020).
    /// `Ty::Protocol` and `Ty::Module` stay excluded — see `## Decisions` in TICKET-020.
    pub(super) fn reject_non_scalar_cast(&mut self, cast: &str, aty: &Ty, span: Span) {
        let kind = match aty {
            Ty::List(_) => "List",
            Ty::Map(..) => "Map",
            Ty::Set(_) => "Set",
            Ty::Tuple(_) => "tuple",
            Ty::Struct(..) => "struct",
            Ty::Enum(..) => "enum",
            Ty::Func { .. } | Ty::BuiltinFn { .. } => "function",
            // `Option`/`Result` are their own `Ty` variants, not `Ty::Enum`, but both are
            // `Obj::Enum` at runtime, so `Vm::type_name` renders both as `enum`.
            Ty::Option(_) | Ty::Result(..) => "enum",
            Ty::Bytes => "bytes",
            Ty::ByteArray => "bytearray",
            Ty::Shared(_) => "Shared",
            Ty::Channel(_) => "Channel",
            Ty::Atomic(_) => "Atomic",
            Ty::AtomicInt => "AtomicInt",
            Ty::RwShared(_) => "RwShared",
            Ty::Executor => "Executor",
            Ty::Socket => "Socket",
            Ty::Listener => "Listener",
            Ty::Writer => "Writer",
            Ty::Reader => "Reader",
            Ty::Ptr => "ptr",
            _ => return,
        };
        self.error(
            span,
            format!(
                "{cast}() cannot convert {kind} — its argument must be int, float, bool, or str"
            ),
        );
    }

    #[allow(clippy::too_many_arguments)] // call name/args/targs/spans + enum-qual + expected-type hint
    /// TICKET-180 — record what the name head `id` denotes. A compiler-synthesized node
    /// ([`crate::ast::NodeId::SYNTH`]) is never recorded, and neither is the generic-arg prepass (a
    /// closure body is walked there with its params unbound, DEC-025). Two different answers for one
    /// node are a hard `TableConflicts` error ([`crate::checker::record_call_table_entry`]).
    /// Whether this walk may write a per-node side table ([`Self::resolve_path`], the call
    /// plans of [`Self::bind_call`]): never for a synthesized node, the generic-arg prepass, or the
    /// return-inference walk, whose scope is not the final one.
    pub(super) fn records_node(&self, id: crate::ast::NodeId) -> bool {
        id.0 != crate::ast::NodeId::SYNTH.0 && !self.generic_arg_prepass && !self.resolving_returns
    }

    /// TICKET-184 — whether a callee resolved to `r` never returns: the `panic` builtin, or a fn of a
    /// native module that [`is_diverging_native`] names (`std.os.exit`, under any bound name). Never
    /// a name test: a user fn, method, local or parameter named `exit`/`panic` returns normally.
    pub(super) fn resolution_diverges(&self, r: &Resolution) -> bool {
        match r {
            Resolution::Builtin(n) => n == "panic",
            Resolution::Fn { module, name } | Resolution::ModuleMember { module, name } => self
                .module_idx_of
                .iter()
                .any(|(id, i)| *i == *module && is_diverging_native(id, name)),
            _ => false,
        }
    }

    /// TICKET-184 — whether `e` is a call whose resolved callee never returns (the divergence oracle
    /// of `flow::stmt`). Reads what [`Self::resolve_path`] recorded for the callee.
    pub(super) fn call_diverges(&self, e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Call { callee, .. }
            if self.callee_diverges.get(&(self.graph_module_idx, callee.id.0)) == Some(&true))
    }

    /// The [`Resolution::Fn`] of a module-level fn bound as `name` in the current module.
    pub(super) fn fn_resolution(&self, name: &str) -> Resolution {
        let (module, name) = self
            .fn_homes
            .get(name)
            .cloned()
            .unwrap_or((self.graph_module_idx, name.to_string()));
        Resolution::Fn { module, name }
    }

    /// Record `Resolution::Variant` for a variant pattern head `Enum.V`.
    pub(super) fn record_variant(
        &mut self,
        id: crate::ast::NodeId,
        enum_key: &str,
        variant: &str,
        span: Span,
    ) {
        let r = Resolution::Variant {
            enum_key: enum_key.to_string(),
            variant: variant.to_string(),
        };
        self.record_pattern_head(id, r, span);
    }

    /// What a bare value head that a scope holds names: a scope >= 1 binding, a module-level fn
    /// slot, a whole-module import, or another module-scope value.
    pub(super) fn value_head_resolution(&self, name: &str) -> Resolution {
        match self.head_binding(name) {
            HeadBinding::Local => Resolution::Local,
            _ if self.slot_holds_fn_decl(name) => self.fn_resolution(name),
            HeadBinding::Module => match self
                .imported_modules
                .get(name)
                .and_then(|mid| self.module_idx_of.get(mid))
            {
                Some(&idx) => Resolution::Module(idx),
                None => Resolution::Global {
                    module: self.graph_module_idx,
                    name: name.to_string(),
                },
            },
            _ => Resolution::Global {
                module: self.graph_module_idx,
                name: name.to_string(),
            },
        }
    }

    /// What `obj.name` names when `obj` is not a type head: a member of a whole-module import when
    /// `obj` is that import's name, else a member of a value.
    pub(super) fn member_resolution(&self, obj: &Expr, name: &str) -> Resolution {
        if let ExprKind::Ident(m) = &obj.kind
            && !self.head_is_value(m)
            && let Some(mid) = self.imported_modules.get(m)
            && let Some(&module) = self.module_idx_of.get(mid)
        {
            return Resolution::ModuleMember {
                module,
                name: name.to_string(),
            };
        }
        Resolution::Member
    }

    /// Does the slot `name` of the whole-module import bound as `mname` hold a function? A
    /// same-named fn (or a `:=` fn value, TICKET-196) replaces a type's ctor (DEC-029/055/172), so
    /// `lib.Q(..)` calls the slot.
    pub(super) fn module_declares_fn(&self, mname: &str, name: &str) -> bool {
        self.imported_modules
            .get(mname)
            .and_then(|mid| self.module_sigs.get(mid))
            .is_some_and(|sig| sig.member(name).is_some_and(MemberSig::holds_fn))
    }

    /// A call of the builtin `name` (the rule table answered `Resolution::Builtin`). The
    /// `range`/`List`/`Set`/`Map` arms are the ctor-RETURN-TYPE / generic-inference source for the
    /// container ctors (arity/overload check, element-type inference) — NOT a flat `FnSig`, so it
    /// stays HERE. Their `CallBuiltin` DISPATCH is table-sourced (the `Intrinsic::Ctor` PRELUDE
    /// rows); `builtin_container_sig` supplies only the flat display/placeholder sig. See
    /// `Intrinsic`. `None` for a first-class builtin fn with no arm here: the value call types it.
    fn infer_builtin_call(
        &mut self,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Option<Ty> {
        match name {
            "print" => {
                for a in args {
                    self.infer_value(a);
                }
                Some(Ty::Nil)
            }
            // `panic(msg)` raises the same recoverable `RuntimeError` the runtime uses for overflow/
            // OOB/decode faults (caught by the nearest `recover:` as `Err`, else aborts the program).
            // It never returns, so it is bottom-typed (`Ty::Unknown`): in value position it absorbs
            // into the other branch's concrete type via `unify_branch`, and in tail position
            // `flow::stmt` (via `call_diverges` on this Resolution) treats it as a divergence.
            "panic" => {
                self.check_arity("panic", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("panic() expects a str, got {other}")),
                    }
                }
                Some(Ty::Unknown)
            }
            "range" => {
                for a in args {
                    self.expect_int_val(a);
                }
                if args.is_empty() || args.len() > 3 {
                    self.error(
                        span,
                        "range() expects range(end), range(start, end), or range(start, end, step)",
                    );
                }
                Some(Ty::list(Ty::Int))
            }
            "int" => {
                self.check_arity("int", 1, args, span);
                if let Some(a) = args.first() {
                    let aty = self.infer_value(a);
                    self.reject_non_scalar_cast("int", &aty, a.span);
                }
                self.infer_all(args.get(1..).unwrap_or(&[]));
                Some(Ty::Int)
            }
            "float" => {
                self.check_arity("float", 1, args, span);
                if let Some(a) = args.first() {
                    let aty = self.infer_value(a);
                    self.reject_non_scalar_cast("float", &aty, a.span);
                }
                self.infer_all(args.get(1..).unwrap_or(&[]));
                Some(Ty::Float)
            }
            "bool" => {
                self.check_arity("bool", 1, args, span);
                // `bool(x)` is a total truthiness cast over the scalars (int/float/bool/str) —
                // like `str`, it accepts any SCALAR. But an AGGREGATE arg
                // is outside the domain and faults at runtime — reject it at check.
                if let Some(a) = args.first() {
                    let aty = self.infer_value(a);
                    self.reject_non_scalar_cast("bool", &aty, a.span);
                }
                self.infer_all(args.get(1..).unwrap_or(&[]));
                Some(Ty::Bool)
            }
            "str" => {
                self.check_arity("str", 1, args, span);
                // `str` is the Stringable display cast (accepts anything).
                self.infer_all(args);
                Some(Ty::Str)
            }
            "ord" => {
                self.check_arity("ord", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("ord() expects a str, got {other}")),
                    }
                }
                Some(Ty::Int)
            }
            "chr" => {
                self.check_arity("chr", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Int | Ty::Unknown => {}
                        other => self.error(a.span, format!("chr() expects an int, got {other}")),
                    }
                }
                Some(Ty::Str)
            }
            // `list(it)` → a list from ANY iterable VALUE (list/set/str/bytes/bytearray/map-keys/
            // Iterator). The element type flows through `iter_elem`. NB a bare RANGE is not one: it
            // has no runtime value, so `List(0..3)` is rejected (see `RANGE_NOT_A_VALUE`) — the
            // `range(a, b)` builtin is the materializer, and `List(range(0, 3))` works. The argument
            // is REQUIRED: an empty list is the `[]` literal (zero args can't infer T).
            "List" => {
                // `List[T]()` — explicit element type (turbofish), bare `List()` — empty list whose
                // element type is refined from the expected type / first use (mirrors `Set()`), and
                // `List(it)` builds from any for-iterable. With a turbofish AND an iterable, the
                // iterable's elements are checked against `T`.
                let targ_elem = match targs {
                    [] => None,
                    [t] => Some(t.clone()),
                    _ => {
                        self.error(span, "List[T]() takes exactly one type argument");
                        Some(Ty::Unknown)
                    }
                };
                match args.len() {
                    0 => Some(Ty::list(targ_elem.unwrap_or(Ty::Unknown))),
                    1 => {
                        let it = self.infer_value(&args[0]);
                        let elem = match self.iter_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!("List() expects an iterable, got {it}"),
                                );
                                Ty::Unknown
                            }
                        };
                        match targ_elem {
                            Some(t) => {
                                self.check_const_fits(&Ty::list(t.clone()), &args[0]);
                                if !t.is_unknown()
                                    && !elem.is_unknown()
                                    && !self.assignable(&t, &elem)
                                {
                                    let note = self.protocol_note(&t, &elem);
                                    let [t_s, elem_s] = Ty::render_distinct([&t, &elem]);
                                    self.error(
                                        args[0].span,
                                        format!(
                                            "List[{t_s}]() expected elements of type {t_s}, found {elem_s}{note}"
                                        ),
                                    );
                                }
                                Some(Ty::list(t))
                            }
                            None => Some(Ty::list(elem)),
                        }
                    }
                    _ => {
                        self.error(
                            span,
                            "List() takes at most one iterable argument — use [] for an empty list",
                        );
                        Some(Ty::list(targ_elem.unwrap_or(Ty::Unknown)))
                    }
                }
            }
            "Set" => {
                // `Set()`/`Set[T]()` → empty set (element from the turbofish, else inferred from
                // later use, like `{}` for maps); `Set(it)` → a set from ANY iterable VALUE
                // (broadened from list-only), deduped. The element type flows through `iter_elem`;
                // it must be Hashable. A bare RANGE is not a value, so `Set(0..3)` is rejected —
                // use `Set(range(0, 3))`. With a turbofish AND an iterable, elements check against `T`.
                let targ_elem = match targs {
                    [] => None,
                    [t] => Some(t.clone()),
                    _ => {
                        self.error(span, "Set[T]() takes exactly one type argument");
                        Some(Ty::Unknown)
                    }
                };
                let elem = match args.len() {
                    0 => targ_elem.unwrap_or(Ty::Unknown),
                    1 => {
                        let it = self.infer_value(&args[0]);
                        let elem = match self.iter_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!("Set() expects an iterable, got {it}"),
                                );
                                Ty::Unknown
                            }
                        };
                        match targ_elem {
                            Some(t) => {
                                self.check_const_fits(&Ty::set(t.clone()), &args[0]);
                                if !t.is_unknown()
                                    && !elem.is_unknown()
                                    && !self.assignable(&t, &elem)
                                {
                                    let note = self.protocol_note(&t, &elem);
                                    let [t_s, elem_s] = Ty::render_distinct([&t, &elem]);
                                    self.error(
                                        args[0].span,
                                        format!(
                                            "Set[{t_s}]() expected elements of type {t_s}, found {elem_s}{note}"
                                        ),
                                    );
                                }
                                t
                            }
                            None => elem,
                        }
                    }
                    _ => {
                        self.error(span, "Set() expects Set() or Set(iterable)");
                        targ_elem.unwrap_or(Ty::Unknown)
                    }
                };
                if !elem.is_unknown()
                    && let Some(why) = self.key_ty_reject(&elem)
                {
                    self.error(span, format!("Set element type {why}"));
                }
                Some(Ty::set(elem))
            }
            // `map(it)` → a map from an iterable of EXACTLY 2-tuples `(K, V)`. A non-2-tuple element is
            // a STATIC error here (not a runtime surprise). K must be Hashable. Last-wins on duplicate
            // keys (like the `{k: v}` literal). The argument is REQUIRED: an empty map is the `{}`
            // literal. (Free-call `map(it)` is a distinct namespace from the `xs.map(f)` list HOF.)
            "Map" => {
                // `Map[K, V]()` → typed empty map (turbofish); bare `Map()` → empty map refined from
                // the expected type / first use (mirrors the `{}` literal and `Set()`); `Map(it)` →
                // a map from an iterable of EXACTLY 2-tuples. With a turbofish AND an iterable, the
                // tuple parts are checked against `[K, V]`.
                let targ_kv = match targs {
                    [] => None,
                    [k, v] => Some((k.clone(), v.clone())),
                    _ => {
                        self.error(span, "Map[K, V]() takes exactly two type arguments");
                        Some((Ty::Unknown, Ty::Unknown))
                    }
                };
                let (k, v) = match args.len() {
                    0 => targ_kv.clone().unwrap_or((Ty::Unknown, Ty::Unknown)),
                    1 => {
                        let it = self.infer_value(&args[0]);
                        let elem = match self.iter_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!("Map() expects an iterable, got {it}"),
                                );
                                Ty::Unknown
                            }
                        };
                        let (k, v) = match elem {
                            Ty::Tuple(ref parts) if parts.len() == 2 => {
                                (parts[0].clone(), parts[1].clone())
                            }
                            Ty::Unknown => (Ty::Unknown, Ty::Unknown),
                            other => {
                                self.error(
                                    args[0].span,
                                    format!("Map() expects an iterable of (key, value) 2-tuples, found element {other}"),
                                );
                                (Ty::Unknown, Ty::Unknown)
                            }
                        };
                        if let Some((tk, tv)) = &targ_kv {
                            self.check_const_fits(
                                &Ty::list(Ty::Tuple(vec![tk.clone(), tv.clone()])),
                                &args[0],
                            );
                            if !tk.is_unknown() && !k.is_unknown() && !self.assignable(tk, &k) {
                                let note = self.protocol_note(tk, &k);
                                let [tk_s, tv_s, k_s] = Ty::render_distinct([tk, tv, &k]);
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map[{tk_s}, {tv_s}]() expected keys of type {tk_s}, found {k_s}{note}"
                                    ),
                                );
                            }
                            if !tv.is_unknown() && !v.is_unknown() && !self.assignable(tv, &v) {
                                let note = self.protocol_note(tv, &v);
                                let [tk_s, tv_s, v_s] = Ty::render_distinct([tk, tv, &v]);
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map[{tk_s}, {tv_s}]() expected values of type {tv_s}, found {v_s}{note}"
                                    ),
                                );
                            }
                            (tk.clone(), tv.clone())
                        } else {
                            (k, v)
                        }
                    }
                    _ => {
                        self.error(
                            span,
                            "Map() takes at most one iterable argument — use {} for an empty map",
                        );
                        targ_kv.clone().unwrap_or((Ty::Unknown, Ty::Unknown))
                    }
                };
                if !k.is_unknown()
                    && let Some(why) = self.key_ty_reject(&k)
                {
                    self.error(span, format!("Map key type {why}"));
                }
                Some(Ty::map(k, v))
            }
            // `bytearray(...)` — the MUTABLE byte buffer (constructor-only, no literal). Four forms:
            // `bytearray()` (empty), `bytearray(N)` (N zero bytes), `bytearray(b)` (from a `bytes`,
            // mutable copy), `bytearray([ints])` (from a `list[int]`, each 0–255 validated at runtime),
            // and `bytearray(ba)` (copy). Always infers `bytearray`.
            "bytearray" => {
                match args.len() {
                    0 => {}
                    1 => match self.infer_value(&args[0]) {
                        Ty::Int | Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                        Ty::List(elem) if matches!(elem.scalar(), Ty::Int | Ty::Unknown) => {}
                        other => self.error(
                            args[0].span,
                            format!("bytearray() expects an int size, a bytes, a bytearray, or a List[int], got {other}"),
                        ),
                    },
                    _ => self.error(span, "bytearray() expects bytearray(), bytearray(int), bytearray(bytes|bytearray), or bytearray(List[int])"),
                }
                Some(Ty::ByteArray)
            }
            // `bytes(...)` — the conversion bridge to the IMMUTABLE form (also constructor-only; the
            // `b"..."` literal is the other way to make a `bytes`). `bytes(ba)` snapshots a `bytearray`,
            // `bytes(b)` copies a `bytes`, `bytes([ints])` builds from a `list[int]`. Infers `bytes`.
            "bytes" => {
                match args.len() {
                    1 => match self.infer_value(&args[0]) {
                        Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                        Ty::List(elem) if matches!(elem.scalar(), Ty::Int | Ty::Unknown) => {}
                        other => self.error(
                            args[0].span,
                            format!(
                                "bytes() expects a bytes, a bytearray, or a List[int], got {other}"
                            ),
                        ),
                    },
                    _ => self.error(
                        span,
                        "bytes() expects bytes(bytes|bytearray) or bytes(List[int])",
                    ),
                }
                Some(Ty::Bytes)
            }
            "Channel" => {
                // `Channel[T]()` — an unbounded mailbox; `Channel[T](cap)` — a bounded FIFO whose
                // `send` blocks when `cap` messages are queued. The element type comes from the explicit
                // type argument (it can't be inferred), and must be sendable. The optional capacity is a
                // runtime int expr (validated `> 0` at runtime, so it can't be arity-checked away here).
                if args.len() > 1 {
                    self.error(span, "Channel[T]() takes an optional capacity argument");
                } else if args.len() == 1 {
                    self.expect_int(&args[0], "Channel capacity");
                }
                let elem = match targs {
                    [t] => t.clone(),
                    // TICKET-142 (W14-33): an expected `Channel[T]` (a let annotation, a call
                    // argument's declared param) supplies the element, as `l: List[float] = []` does.
                    [] if let Some(Ty::Channel(e)) = hint => (**e).clone(),
                    [] => {
                        self.error(span, "Channel() needs an element type — write Channel[T]()");
                        Ty::Unknown
                    }
                    _ => {
                        self.error(span, "Channel[T]() takes exactly one type argument");
                        Ty::Unknown
                    }
                };
                if !elem.is_unknown() && !self.sendable(&elem) {
                    let hint = self.sendable_error_hint(&elem);
                    self.error(
                        span,
                        format!("Channel element type must be sendable (able to cross a task boundary), found {elem}{hint}"),
                    );
                }
                Some(Ty::channel(elem))
            }
            "Shared" => {
                // `Shared(v)` — a fresh cross-task box initialised with `v`. The element type is
                // inferred from the value (value-first, unlike `Channel[T]()`); an OPTIONAL `[T]`
                // turbofish pins it and is checked against the value's type (`Shared[str](0)` rejects).
                // NOT a global builtin: requires `import std.concurrency` (the arg is still inferred on
                // the unlicensed path so a nested error surfaces; the name STAYS reserved). Same for
                // RwShared/Atomic/Executor below.
                let inferred = self.one_arg("Shared", args, span);
                let elem = self.concurrency_turbofish_elem("Shared", targs, inferred, span);
                if self.concurrency_licensed("Shared") {
                    Some(Ty::shared(elem))
                } else {
                    self.error(
                        span,
                        "unknown type 'Shared' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "RwShared" => {
                // `RwShared(v)` — a fresh cross-task read-write box initialised with `v`. The element
                // type is inferred from the value (value-first, like `Shared`); an OPTIONAL `[T]`
                // turbofish pins it and is checked against the value's type.
                let inferred = self.one_arg("RwShared", args, span);
                let elem = self.concurrency_turbofish_elem("RwShared", targs, inferred, span);
                if self.concurrency_licensed("RwShared") {
                    Some(Ty::rwshared(elem))
                } else {
                    self.error(
                        span,
                        "unknown type 'RwShared' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "Atomic" => {
                // `Atomic(v)` — a fresh cross-task atomic box initialised with `v`. Value-first like
                // `Shared`; an OPTIONAL `[T]` turbofish pins the element type and is checked against
                // the value's type.
                let inferred = self.one_arg("Atomic", args, span);
                let elem = self.concurrency_turbofish_elem("Atomic", targs, inferred, span);
                if self.concurrency_licensed("Atomic") {
                    // INSIDE the licensing branch: an unavailable type must not first get advice
                    // about its payload (the `unknown type 'Atomic'` error is the only useful one).
                    self.reject_eq_atomic_payload(&elem, span);
                    Some(Ty::atomic(elem))
                } else {
                    self.error(
                        span,
                        "unknown type 'Atomic' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "timer" => {
                // `timer(ms)` — a one-shot timeout channel: a `Channel[bool]` that delivers `true`
                // once, `ms` milliseconds after creation. The composable timeout primitive (recv it in
                // a `wait` arm). Takes an int; a `[T]` type arg is rejected upstream. NOT a global
                // builtin: requires `import std.time` (the arg is still checked on the unlicensed path
                // so a nested error surfaces; the name STAYS reserved). Returns `Channel[bool]` even on
                // the unlicensed path so a chained `.recv()` doesn't emit a confusing secondary error.
                // The arg/return types are single-sourced from the `native fn timer` decl in
                // `std/time.chz` (harvested to `time_timer_sig`); the fallback reproduces that exact shape
                // for the no-graph / unimported path where std.time was never harvested.
                let (params, ret) = self
                    .time_timer_sig
                    .as_ref()
                    .map(|s| (s.params.clone(), s.ret.clone()))
                    .unwrap_or_else(|| (vec![Ty::Int], Ty::channel(Ty::Bool)));
                self.check_args("timer", &params, args, span);
                if self.time_licensed("timer") {
                    Some(ret)
                } else {
                    self.error(
                        span,
                        "unknown function 'timer' (import it from std.time: `import std.time`)"
                            .to_string(),
                    );
                    Some(ret)
                }
            }
            "Executor" => {
                // `Executor()` — a fresh, empty, explicitly-owned work queue (C5 escape hatch).
                // Non-generic; zero arguments, or one `int` cap on the jobs running at once; a `[T]` type arg is rejected upstream. NOT a global
                // builtin: requires `import std.concurrency` (the name STAYS reserved).
                self.check_args_range("Executor", &[Ty::Int], 0, args, span);
                if self.concurrency_licensed("Executor") {
                    Some(Ty::Executor)
                } else {
                    self.error(
                        span,
                        "unknown type 'Executor' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "AtomicInt" => {
                // `AtomicInt(v)` — a fresh lock-free int atomic. Monomorphic (no `[T]`); the single arg
                // must be an int. NOT a global builtin: requires `import std.concurrency` (the name
                // STAYS reserved). The arg is checked even on the unlicensed path so a nested error
                // surfaces.
                self.check_args("AtomicInt", &[Ty::Int], args, span);
                if self.concurrency_licensed("AtomicInt") {
                    Some(Ty::AtomicInt)
                } else {
                    self.error(
                        span,
                        "unknown type 'AtomicInt' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            // Generic built-in constructors for Result / Option.
            // `Ok(x)`: success type known, error type open (unifies with the declared `E`).
            // Zero-arg `Ok()` is the spelling of `Result[nil, E]`'s success value (`## Decisions`,
            // TICKET-017) — `check_arity` is skipped so it never faults on 0 args. The matching
            // lowering is the `Op::Nil` + `Op::NewEnum` branch in `src/compiler/mod.rs`; keep both in
            // sync.
            "Ok" => Some(Ty::result_e(
                if args.is_empty() {
                    Ty::Nil
                } else {
                    let h = hint.and_then(|h| match h {
                        Ty::Result(t, _) => Some((**t).clone()),
                        _ => None,
                    });
                    self.one_arg_hinted(name, args, span, h.as_ref())
                },
                Ty::Unknown,
            )),
            "Some" => Some(Ty::option({
                let h = hint.and_then(|h| match h {
                    Ty::Option(t) => Some((**t).clone()),
                    _ => None,
                });
                self.one_arg_hinted(name, args, span, h.as_ref())
            })),
            // `Err(x)`: error type known (`typeof x`), success type open.
            "Err" => Some(Ty::result_e(Ty::Unknown, {
                let h = hint.and_then(|h| match h {
                    Ty::Result(_, e) => Some((**e).clone()),
                    _ => None,
                });
                self.one_arg_hinted(name, args, span, h.as_ref())
            })),
            _ => None,
        }
    }

    /// A bare struct constructor `S(args)` / `Alias(args)` the rule table answered
    /// `StructCtor(key)` for. Type arguments come from explicit call-site args (`S[int](…)`), an
    /// alias's pinned ones, else unifying the declared field types against the argument types.
    fn infer_struct_ctor_call(
        &mut self,
        name: &str,
        key: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let key = key.to_string();
        let pinned = self.alias_struct_head(name).map(|(_, p)| p);
        let Some((tps, fields, defaulted)) = self.structs.get(&key).map(|i| {
            (
                i.type_params.clone(),
                i.fields.clone(),
                i.defaulted_fields.clone(),
            )
        }) else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let Some(targs) = self.written_head_args(
            name,
            self.type_param_count(&key),
            pinned,
            targs.to_vec(),
            span,
        ) else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let targs = targs.as_slice();
        let slots = self.structs.get(&key).and_then(|i| i.field_slots.clone());
        let Some(bound) = self.bind_call(slots.as_deref(), name, args, targs.len(), span) else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        let field_tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
        if tps.is_empty() {
            // Struct ctor float fields are coerced per-field by the `NewStruct` site.
            self.check_args(name, &field_tys, args, span);
            return Ty::strukt(key);
        }
        // Generic struct: type arguments come from explicit call-site args (`S[int](…)`)
        // when given, else are inferred by unifying the declared field types (which
        // contain the struct's `Ty::Param`s) against the argument types.
        let hints = self.ctor_arg_hints(
            hint,
            &Ty::Struct(key.clone(), param_shape(&tps)),
            &tps,
            &field_tys,
            targs,
        );
        let arg_tys = self.infer_generic_arg_tys(args, &field_tys, &hints);
        self.check_ctor_arity(name, &tps, &fields, &defaulted, targs, args, span);
        let mut sub = self.seed_targs(name, &tps, targs, span);
        for (decl, actual) in field_tys.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: a `let`/return/param annotation (`Heap[int]`) seeds
        // any type param the args left FREE, BEFORE the deadlock probe — so the annotation
        // breaks the `Heap([], fn(a, b): a < b)` deadlock (it pins `T`, which in turn pins
        // the comparator's closure params via the per-arg checking-mode re-infer below).
        seed_from_hint(hint, &Ty::Struct(key.clone(), param_shape(&tps)), &mut sub);
        self.widen_targs_from_hint(
            hint,
            &Ty::Struct(key.clone(), param_shape(&tps)),
            &tps,
            &field_tys,
            &arg_tys,
            !targs.is_empty(),
            &mut sub,
            span,
        );
        // Detect the un-inferable closure-param deadlock (e.g. `Heap([], fn(a,b): a<b)`)
        // BEFORE the per-arg check, so it reports the cause instead of leaking a
        // "cannot compare T and T" from inside the lambda. Binds the params to Unknown.
        self.report_uninferable_closure_params(name, &tps, &field_tys, args, &mut sub, span);
        for (decl, (actual, arg)) in field_tys.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &tps, &sub, span);
        let targs = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Struct(key, targs)
    }

    /// A call of the module-level fn `name` the rule table answered `Fn` for, through its slot.
    fn infer_fn_call(
        &mut self,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let Some(sig) = self.functions.get(name).cloned() else {
            self.infer_all(args);
            return Ty::Unknown;
        };
        let diverges = self.resolution_diverges(&self.fn_resolution(name));
        // W7-42r: this call site's arity/defaults/arg types are now fixed against the
        // fn's signature, so a later module-scope `name := …` retypes the ONE slot it
        // dispatches through (see `fn_reads`).
        self.record_fn_read(name);
        // TICKET-186: keyword labels through a module slot bind only where
        // `labels_certain` says the slot holds this fn (K5).
        if let Some(named) = self
            .call_ctx
            .as_ref()
            .filter(|c| !c.named.is_empty())
            .map(|c| c.named.clone())
            && self.labels_certain(name).is_err()
        {
            self.consume_named();
            for a in args {
                self.infer_value(a);
            }
            for (_, v) in &named {
                self.infer_value(v);
            }
            self.error(span, super::globals::kw_ambiguous_msg(name));
            return sig.ret.clone();
        }
        let Some(bound) = self.bind_call(sig.slots.as_deref(), name, args, 0, span) else {
            return Ty::Unknown;
        };
        let args: &[Expr] = &bound;
        // A generic function: infer its type parameters from the arguments, enforce
        // bounds, and substitute into the return type.
        if !sig.type_params.is_empty() {
            // M24 — the witness key span is the CALLEE TOKEN (`name_span`), never the
            // call node: a pipe chain's links all carry the infix expression's span, so
            // keying on it aliased two witness calls onto one entry.
            return self.infer_generic_call(
                name,
                &sig,
                args,
                targs,
                name_span,
                span,
                hint,
                WitnessCallee::Free,
            );
        }
        // Float params are coerced at the callee's prologue (compile_fn / extern).
        // Honor an optional trailing tail (`min_params < params.len()`, e.g. a native
        // `from`-imported fn with an optional arg); for plain sigs `min_params ==
        // params.len()`, so this is identical to the old exact-arity check.
        if sig.c_variadic {
            self.check_c_variadic_args(name, &sig.params, args, span);
        } else {
            self.check_args_range(name, &sig.params, sig.min_params, args, span);
        }
        // TICKET-077: a `from`-imported diverging native fn (`exit`, under any bound
        // name) bottom-types like `panic`, so it type-checks in value position.
        if diverges {
            return Ty::Unknown;
        }
        sig.ret
    }

    /// Does the method `method` on receiver type `recv_ty` declare its OWN `[U]` type params? Only a
    /// user struct/enum method can; a builtin (`str`/`list`/…) member never does. Used to gate
    /// the member-level turbofish (`obj.method[A](x)`): a turbofish on anything else is an arity error.
    /// `None` when the receiver is a module with no member (and no type) `method`: a miss, which the
    /// module arm reports as one before any turbofish arity.
    pub(super) fn member_own_type_params(&self, recv_ty: &Ty, method: &str) -> Option<bool> {
        if let Ty::Module(mname) = recv_ty
            && let Some(sig) = self
                .imported_modules
                .get(mname)
                .and_then(|id| self.module_sigs.get(id))
            && sig.member(method).is_none()
            && !sig.types.contains(method)
        {
            return None;
        }
        Some(self.method_own_type_params(recv_ty, method))
    }

    fn method_own_type_params(&self, recv_ty: &Ty, method: &str) -> bool {
        match recv_ty {
            Ty::Struct(sname, _) => self
                .structs
                .get(sname)
                .and_then(|info| info.methods.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            Ty::Enum(ename, _) => self
                .enum_methods
                .get(ename)
                .and_then(|ms| ms.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            // A module-qualified fn (`geo.empty_list[int]()`): the module's own fns live in the
            // module sig, not in `structs`/`enums` — without this arm a generic module fn was told it
            // "declares no own type parameters" and its turbofish was an unreachable feature.
            Ty::Module(mname) => self
                .imported_modules
                .get(mname)
                .and_then(|id| self.module_sigs.get(id))
                .and_then(|s| s.certain_fn(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            // Reserved built-in receiver types: their harvested methods live in the re-seeded bare
            // `structs` tables (setup.rs phases 4c/5a), same as a user struct. Without these arms a
            // shipped generic method (`[1,2,3].map[int](...)`, `ex.submit_result[T](...)`) was told it
            // "declares no own type parameters" and its member turbofish was an arity error.
            Ty::List(_)
            | Ty::Map(_, _)
            | Ty::Set(_)
            | Ty::Shared(_)
            | Ty::RwShared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader => {
                // ARM-ONLY generics: the `RwShared` read-view `fold`/`fold_entries` are hand-built in
                // the `Ty::RwShared` dispatch arm (their `R` is not nameable in `RwShared[T]`'s
                // harvested surface), so they are absent from the `structs` table the lookup below
                // consults — which rejected their turbofish outright. They ARE genuinely generic and
                // already route through `infer_generic_method` WITH `type_args` (see `fold_sig`).
                // A wrong receiver element (`RwShared[int].fold[int]`) still rejects downstream, via
                // the native handle resolver's "no method".
                if matches!(recv_ty, Ty::RwShared(_)) && matches!(method, "fold" | "fold_entries") {
                    return true;
                }
                let bare = match recv_ty {
                    Ty::List(_) => "List",
                    Ty::Map(_, _) => "Map",
                    Ty::Set(_) => "Set",
                    Ty::Shared(_) => "Shared",
                    Ty::RwShared(_) => "RwShared",
                    Ty::Atomic(_) => "Atomic",
                    Ty::AtomicInt => "AtomicInt",
                    Ty::Executor => "Executor",
                    Ty::Socket => "Socket",
                    Ty::Listener => "Listener",
                    Ty::Writer => "Writer",
                    _ => "Reader",
                };
                self.structs
                    .get(bare)
                    .and_then(|info| info.methods.get(method))
                    .is_some_and(|sig| !sig.type_params.is_empty())
            }
            _ => false,
        }
    }

    /// Editor hover (probe-gated no-op): record a builtin method's CALL signature at the method-name
    /// token. The builtin `*_method_sig` helpers carry NO receiver param (unlike user methods), so the
    /// sig is recorded as-is — `str.upper()`'s `[]→str` renders "fn() -> str". Pure side effect: only
    /// runs under the hover probe and emits no error / changes no checking. Mirrors the user-struct
    /// method-arm probe block (which strips the receiver first).
    pub(super) fn record_method_hover(&mut self, name_span: Span, sig: &FnSig) {
        if self.hover_probe.is_some() {
            let fty = Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
                labels: crate::checker::FnLabels::default(),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
    }

    /// Record a DECL-site method-name hover (the `dbl` in `fn dbl(self) -> int:`). Mirrors the
    /// CALL-site method hover ([`infer_method_call`]): the receiver `self` is stripped for an
    /// instance method (so `fn dbl(self) -> int` displays "fn() -> int"), but a STATIC method (no
    /// receiver) keeps all its params. Probe-gated no-op like [`record_method_hover`].
    pub(super) fn record_method_decl_hover(&mut self, name_span: Span, sig: &FnSig) {
        if self.hover_probe.is_some() {
            let params: Vec<Ty> = if !sig.is_static && !sig.params.is_empty() {
                sig.params[1..].to_vec()
            } else {
                sig.params.clone()
            };
            let fty = Ty::Func {
                params,
                ret: Box::new(sig.ret.clone()),
                labels: FnLabels::default(),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
    }

    /// Shared dispatch prefix for the reserved native-handle method arms: lookup the harvested sig
    /// (`native_handle_method`), record the editor hover, and — if the sig carries its OWN `[U]` type
    /// params — PREPEND the concrete receiver and route through the generic solver (mirrors the `List`
    /// arm). Extracting this makes it structurally impossible for a handle arm to forget the generic
    /// branch (the forgettable hazard); each arm keeps its residual special case inline (Atomic numeric
    /// gate, Executor `submit` capture-floor, RwShared `read` R-recovery) by matching on the result.
    #[allow(clippy::too_many_arguments)]
    fn resolve_native_handle_method(
        &mut self,
        key: &str,
        method: &str,
        targs: &[Ty],
        name_span: Span,
        obj_ty: &Ty,
        type_args: &[Ty],
        args: &[Expr],
        span: Span,
        // Forwarded to the generic solver for the same reason as everywhere else on this path: a
        // harvested native method carrying its own `[U]` may have it only in the return type.
        hint: Option<&Ty>,
    ) -> NativeHandleMethod {
        let Some(sig) = self.native_method_sig(key, method, targs) else {
            return NativeHandleMethod::Miss;
        };
        self.record_method_hover(name_span, &sig);
        // A harvested method carrying its OWN `[U]` params needs the generic solver (the harvest
        // STRIPS `self`, so PREPEND the concrete receiver — mirrors the `List` arm).
        if !sig.type_params.is_empty() {
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            params.push(obj_ty.clone());
            params.extend(sig.params.iter().cloned());
            return NativeHandleMethod::Generic(self.infer_generic_method(
                method,
                &params,
                &[],
                &sig.ret,
                &sig.type_params,
                &[], // a native method never takes a witness (no user body to construct in)
                obj_ty,
                type_args,
                args,
                // The harvest strips `self` and the receiver is prepended just above, so the
                // declaration's own minimum shifts by one to match `params`.
                sig.min_params.saturating_add(1),
                name_span,
                span,
                hint,
            ));
        }
        NativeHandleMethod::Concrete(Box::new(sig))
    }

    /// The plain reserved-handle arm shared by `Socket`/`Listener`/`Writer`/`Reader`: no element type
    /// to substitute (`&[]`) and no per-arm residual — a Concrete sig is just `check_args_range` +
    /// `sig.ret`. Routed through `resolve_native_handle_method` so a future bodied/generic method on
    /// any of these handles auto-infers instead of silently missing dispatch.
    #[allow(clippy::too_many_arguments)]
    fn infer_fixed_native_handle_method(
        &mut self,
        key: &str,
        method: &str,
        name_span: Span,
        obj_ty: &Ty,
        type_args: &[Ty],
        args: &[Expr],
        span: Span,
    ) -> Ty {
        match self.resolve_native_handle_method(
            key,
            method,
            &[],
            name_span,
            obj_ty,
            type_args,
            args,
            span,
            // No enclosing annotation reaches this fixed-arity native path.
            None,
        ) {
            NativeHandleMethod::Generic(t) => t,
            NativeHandleMethod::Concrete(sig) => {
                self.check_args_range(method, &sig.params, sig.min_params, args, span);
                sig.ret
            }
            NativeHandleMethod::Miss => {
                self.infer_all(args);
                let names = self.method_names(key);
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
        }
    }

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
        // W8-3 — an in-place mutation (`xs.push(v)`, `m.remove(k)`, …) is a WRITE through the
        // receiver binding, so it taints inside a `spawn:` body and untaints in the parent, exactly
        // like `check_assign`'s lvalue arms. Recorded from `lookup` BEFORE `infer(obj)` runs, for the
        // same ordering reason: the receiver read below must not report a taint this very statement
        // supersedes. Simple-`Ident` receivers only here — the PROJECTED case (`xs[0].push(v)`,
        // `s.xs.push(v)`) is handled below, AFTER `infer(obj)` runs, because it needs `obj`'s own
        // stale read reported first (W19: a parent-side `xs[0].push(v)` observes the task's stale
        // write before it supersedes it). `mutates_receiver` is type-keyed so the handle types, whose
        // task-side writes ARE visible, never taint.
        if let ExprKind::Ident(name) = &obj.kind
            && let Some(rty) = self.lookup(name)
            && self.call_writes_receiver(&rty, method)
        {
            let name = name.clone();
            // W8-3 — a PARENT-side mutation reads the stale copy before it writes it, exactly like
            // the compound assign `n += 1` in `note_assign_root`: EVERY method in `mutates_receiver`
            // is a read-modify-write (`push`/`pop`/`sort`/`reverse`/`extend`/`insert`/`remove_at`/
            // `remove`/`update`/`add`), and the set contains no whole-container replacement at all —
            // `clear` does not exist in `std/prelude.chz` — so there is no member of it that could
            // legitimately supersede the task's write. Measured: task `xs.push("a")` then parent
            // `xs.push("b")` prints `1`, not `2`. Report BEFORE untainting; reporting consumes the
            // entry, so the receiver read in `infer(obj)` below still yields exactly one warning.
            self.note_task_write(&name, obj.span);
        }
        let obj_ty = self.infer(obj);
        // TICKET-165 — a mutator on a PROJECTED receiver (`xs[0].push(v)`, `s.xs.push(v)`,
        // `m[k].push(v)`) mutates the container IN PLACE through the root binding (measured: `ys :=
        // xs[0]; ys.push(2); print(xs)` prints `[[1, 2]]`), so it is a write through the root at the
        // projected path, just like a projected `=`/`+=` target. `infer(obj)` above already reported
        // any parent-side stale read of `obj` itself (W19), so this untaint runs strictly after it,
        // and it reuses the `obj_ty` `infer(obj)` just computed rather than re-inferring the receiver
        // (re-inferring would double-report — the same ceiling `note_projected_task_write`'s caller
        // must not cross).
        if self.call_writes_receiver(&obj_ty, method) {
            self.note_projected_task_write(obj);
        }
        // Refine-on-first-use: if `obj` is a simple variable whose type has an `Unknown` element/
        // key/value/type-arg slot (an empty literal / nullary variant / native `None`), and this is
        // a slot-supplying mutator (`push`/`add`/`insert`/`extend`), re-pin the binding to the
        // concrete shape the arg supplies — so a later conflicting op is a normal `check_args`
        // mismatch and the set-element Hashable ban runs at concrete-ification. Then re-read the
        // (possibly refined) receiver type from scope so dispatch sees the narrowed element.
        self.refine_receiver(obj, &obj_ty, method, args);
        let obj_ty = match &obj.kind {
            ExprKind::Ident(name) => self.lookup(name).unwrap_or(obj_ty),
            _ => obj_ty,
        };
        // Task 1 — a captured module-global aggregate mutated in a task (`xs.push(v)`, …) is no longer
        // a compile error: spawning deep-copies module globals per task, so the write hits the task's
        // OWN copy — invisible to the parent. The old frozen-module-global gate is deleted (`Shared`/`Channel` remain the escape
        // hatch for genuinely-shared cross-task state; they cross by shared Arc via `to_snap`).
        // A member-level turbofish (`obj.method[A, B](...)`) is only valid on a USER method that
        // declares its OWN `[U]` type params. On a builtin (`xs.len[int]()`, `xs.iter[int]()`) or a
        // non-generic user method it is an arity error — checked BEFORE the `.iter` fast-path below
        // so `xs.iter[int]()` is rejected like `xs.len[int]()` (it was silently swallowed otherwise).
        if !type_args.is_empty() && self.member_own_type_params(&obj_ty, method) == Some(false) {
            self.infer_all(args);
            self.error(
                span,
                format!("method '{method}' takes no type argument(s) (it declares no own type parameters)"),
            );
            return Ty::Unknown;
        }
        // `.iter()` — the formal `Iterable[T]` entry point. Returns a fresh cursor typed as the
        // existing `Iterator[T]` existential (no new `Ty`), `T = iter_elem`. Handled here, BEFORE the
        // per-type dispatch, for every built-in iterable AND for an `Iterator[T]` value (a generator
        // result or another cursor) where `iter()` is idempotent (returns self). A user STRUCT is
        // excluded so a struct that declares its own `iter` (the pure-`Iterable` producer) resolves
        // through the normal struct-method path below — its `iter` return type IS `Iterator[E]`.
        if method == "iter"
            && args.is_empty()
            && !matches!(&obj_ty, Ty::Struct(n, _) if n != "Iterator")
            && let Some(elem) = self.iter_elem(&obj_ty)
        {
            return Ty::Struct("Iterator".to_string(), vec![elem]);
        }
        // W7-53 I1′ — tell the type-blind backend which dispatch this `.eq(x)` takes. Recorded for
        // EVERY one-arg `eq` call (both verdicts), before the per-type dispatch below, so the record
        // site is one place and an aliased key is a hard error rather than a silent mis-lowering.
        if method == "eq" && args.len() == 1 {
            let proto = self.eq_is_protocol_dispatch(&obj_ty);
            self.record_proto_eq(name_span, proto, span);
        }
        match &obj_ty {
            // `module.fn(args)` is a plain call on the member — no `self`.
            Ty::Module(mname) => {
                // Captured before `sig`'s borrow, for the diverging-native check below (TICKET-077).
                let mod_id = self.imported_modules.get(mname).cloned();
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
                    // TICKET-077: a diverging native fn (`os.exit`) bottom-types like `panic`, so
                    // it type-checks in value position (e.g. a `match` arm).
                    if mod_id
                        .as_ref()
                        .is_some_and(|id| is_diverging_native(id, method))
                    {
                        return Ty::Unknown;
                    }
                    return fsig.ret;
                }
                // TICKET-196: a slot without `certain_fn` binds no labels, so a keyword call through
                // it is denied, as through a same-module value callee.
                if let Some(m) = member.as_ref().filter(|m| m.holds_fn())
                    && let Some(c) = &self.call_ctx
                    && !c.named.is_empty()
                    && let Err(deny) = m.labels()
                {
                    let named = c.named.clone();
                    self.consume_named();
                    self.infer_all(args);
                    for (_, v) in &named {
                        self.infer(v);
                    }
                    self.error(span, super::globals::kw_deny_msg(Some(method), deny));
                    return Ty::Unknown;
                }
                // W7-21 — a module GLOBAL that HOLDS a function value is callable through the module
                // (`m.G()`), like CPython's `m.G()` and Go's `pkg.G()`. Mirrors the fn-typed-FIELD
                // fallback in the struct arm. Editor hover, same as the `fsig` path above: the
                // member's own `Ty::Func` IS what `record_method_hover` would build from an `FnSig`,
                // so record it directly (no doc — a slot without `certain_fn` carries none).
                if self.hover_probe.is_some()
                    && let Some(t @ (Ty::Func { .. } | Ty::BuiltinFn { .. })) = &vty
                {
                    let t = t.clone();
                    self.hover_record_at(name_span, &t, HoverKind::Func, None);
                }
                match vty {
                    // STRICT — a module global holds a function VALUE, and a `Ty::Func` does not say
                    // which declaration it came from (a generic fn instantiated at float has an erased
                    // `T` param and coerces nothing). Same rule as the fn-value and fn-field call
                    // paths: no int→float widening through a function value.
                    Some(Ty::Func { params, ret, .. } | Ty::BuiltinFn { params, ret }) => {
                        self.check_args(method, &params, args, span);
                        return *ret;
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
                if method == "next" {
                    self.check_args(method, &[], args, span);
                    return Ty::option(targs[0].clone());
                }
                self.infer_all(args);
                self.error_help(
                    name_span,
                    format!(
                        "type {obj_ty} has no method '{method}' (an iterator only has `next()`)"
                    ),
                    suggest::did_you_mean(method, &["next".to_string()]),
                );
                Ty::Unknown
            }
            Ty::Struct(sname, targs) => {
                // Substitute the struct's type arguments into the method signature, so calling
                // `Stack[int].push(x)` checks `x` against `int`, not the parameter `T`.
                let resolved = self.struct_shape(sname).and_then(|info| {
                    info.methods.get(method).map(|sig| {
                        let map = struct_param_map(info, targs);
                        let inst = instantiate_method(sig, &map);
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
                )) = resolved
                {
                    // A STATIC method (no `self`) is NOT callable on a value — it is reached only as
                    // `Type.method(...)`. Reject the instance call with a pointer at the right form.
                    if is_static {
                        self.infer_all(args);
                        let disp = crate::compiler::bare_display(sname);
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
                            if self.hover_probe.is_some() {
                                let fty = Ty::Func {
                                    params: expected.to_vec(),
                                    ret: Box::new(ret.clone()),
                                    labels: crate::checker::FnLabels::default(),
                                };
                                self.hover_record_at(
                                    name_span,
                                    &fty,
                                    HoverKind::Func,
                                    mdoc.clone(),
                                );
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
                if let Some(Ty::Func { params, ret, .. }) = field_fn {
                    // STRICT — a fn-typed FIELD holds a function VALUE, and a `Ty::Func` does not say
                    // which declaration it came from (a generic fn instantiated at float has an erased
                    // `T` param and coerces nothing). Same rule as the positional/named fn-value call
                    // paths above: no int→float widening through a function value.
                    self.check_args(method, &params, args, span);
                    return *ret;
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
                        let disp = crate::compiler::bare_display(ename);
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
                self.error_help(
                    name_span,
                    format!("type str has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
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
                // no `remove` (it is `remove_at`, by index). NOT erased (W7-53): a free `elem` must
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
                            other => self.error(
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
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
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
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no method '{method}'"),
                    suggest::did_you_mean(method, &names),
                );
                Ty::Unknown
            }
            Ty::Channel(elem) => {
                // The sigs are harvested from `std/prelude.chz`'s `native struct Channel[T]` (re-seeded
                // by `seed_stdlib_structs`); the element type is substituted for `Ty::Param("T")` here.
                // `send(v)` moves `v` across the airlock; `check_args` enforces it matches the element
                // type `T`, which is itself sendable-checked at the channel's construction — so a
                // well-typed `send` is always sendable.
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
                // Zero-copy READ-view methods on a CONTAINER element of `RwShared[T]` — `List[E]`,
                // `Map[K,V]`, `Set[E]` (Tuple EXCLUDED: heterogeneous). They walk the stored
                // heap-independent `WireValue::List`/`Map`/`Set` under the read guard and `from_wire`
                // ONE entry/element at a time (O(1) memory), so a worker can scan/reduce a shared large
                // container without materializing the whole inner (what `get`/`read` do). E/K/V are NOT
                // nameable in `RwShared[T]`'s harvested `std/concurrency.chz` surface, so these sigs are
                // ARM-ONLY (hand-built here, element types ARM-RECOVERED by destructuring the concrete
                // container: `List[?E]`/`Map[?K,?V]`/`Set[?E]`). Dispatch branches on the container HEAD
                // first (method names overlap — `len` on all three, `for_each`/`fold` on List+Set), then
                // the method within that container's set; anything else (a scalar/tuple head, or a wrong
                // method for the head) falls through to the native handle resolver → a clean "no method"
                // reject (no check-OK-then-run-fault). The constructor-kind gate is the surface form of
                // the `where T: List/Map/Set` bound (see `container_bound_matches`). NOTE (runtime): the
                // walk re-acquires the read guard PER ENTRY and drops it before the closure/hash/eq
                // probe (never held across user code), so a nested read/write of the same box is
                // deadlock-free; see `rwshared_method` in `src/vm/netio.rs`.
                //
                // `fold`/`fold_entries` are GENUINELY generic (the `List.fold[U]` route): R PINS from
                // the concrete `init` accumulator, routed through `infer_generic_method` exactly as a
                // harvested generic method is. `for_each*`'s closure return is DISCARDED (`Ty::Unknown`
                // ret), so any return is accepted (a strict `-> nil` would reject `fn(x): acc.add(x)`).
                let fold_sig = |this: &mut Self, name: &str, elem_params: Vec<Ty>| -> Ty {
                    // `R` is the method's own param: rename it when the receiver mentions a caller's
                    // `R`, the same rule `instantiate_method` applies to a declared method.
                    let mut free = Vec::new();
                    ty_collect_params(&obj_ty, None, &mut free);
                    let r_name = if free.iter().any(|n| n == "R") {
                        fresh_param_name("R", &free.into_iter().collect())
                    } else {
                        "R".to_string()
                    };
                    let r = Ty::Param(r_name.clone());
                    let mut fn_params = vec![r.clone()];
                    fn_params.extend(elem_params);
                    let params = vec![
                        obj_ty.clone(),
                        r.clone(),
                        Ty::Func {
                            params: fn_params,
                            ret: Box::new(r.clone()),
                            labels: crate::checker::FnLabels::default(),
                        },
                    ];
                    let tps = vec![TyParam {
                        name: r_name,
                        name_span: Span::default(),
                        bounds: vec![],
                    }];
                    this.infer_generic_method(
                        name,
                        &params,
                        &[],
                        &r,
                        &tps,
                        &[],
                        &obj_ty,
                        type_args,
                        args,
                        usize::MAX, // synthesized builtin sig: exact arity
                        name_span,
                        span,
                        hint,
                    )
                };
                let func = |params: Vec<Ty>| Ty::Func {
                    params,
                    ret: Box::new(Ty::Unknown),
                    labels: crate::checker::FnLabels::default(),
                };
                match &elem {
                    Ty::List(e) => {
                        let e = (**e).clone();
                        match method {
                            "len" => {
                                self.check_args_range("len", &[], 0, args, span);
                                return Ty::Int;
                            }
                            // `at(i) -> Option[E]`: out of range is `None`, never a fault, matching the
                            // language's other named accessors (`get_key -> Option[V]` below,
                            // `std.json.at -> Option[Json]`). `RwShared` has no `[]` of its own (it
                            // does not satisfy `Index`), so this is its only read accessor.
                            "at" => {
                                self.check_args_range("at", &[Ty::Int], 1, args, span);
                                return Ty::option(e);
                            }
                            "slice" => {
                                self.check_args_range("slice", &[Ty::Int, Ty::Int], 2, args, span);
                                return Ty::list(e);
                            }
                            "for_each" => {
                                self.check_args_range("for_each", &[func(vec![e])], 1, args, span);
                                return Ty::Nil;
                            }
                            "fold" => return fold_sig(self, "fold", vec![e]),
                            _ => {}
                        }
                    }
                    Ty::Map(k, v) => {
                        let k = (**k).clone();
                        let v = (**v).clone();
                        match method {
                            "len" => {
                                self.check_args_range("len", &[], 0, args, span);
                                return Ty::Int;
                            }
                            "get_key" => {
                                self.check_args_range("get_key", &[k], 1, args, span);
                                return Ty::option(v);
                            }
                            "has" => {
                                self.check_args_range("has", &[k], 1, args, span);
                                return Ty::Bool;
                            }
                            // TICKET-192: the single-entry writers (update guard, O(1) expected).
                            "set_key" => {
                                self.check_args_range("set_key", &[k, v], 2, args, span);
                                return Ty::Nil;
                            }
                            "remove_key" => {
                                self.check_args_range("remove_key", &[k], 1, args, span);
                                return Ty::option(v);
                            }
                            "get_or_insert" => {
                                self.check_args_range(
                                    "get_or_insert",
                                    &[k, v.clone()],
                                    2,
                                    args,
                                    span,
                                );
                                return v;
                            }
                            "for_each_entry" => {
                                self.check_args_range(
                                    "for_each_entry",
                                    &[func(vec![k, v])],
                                    1,
                                    args,
                                    span,
                                );
                                return Ty::Nil;
                            }
                            "fold_entries" => return fold_sig(self, "fold_entries", vec![k, v]),
                            _ => {}
                        }
                    }
                    Ty::Set(e) => {
                        let e = (**e).clone();
                        match method {
                            "len" => {
                                self.check_args_range("len", &[], 0, args, span);
                                return Ty::Int;
                            }
                            "contains" => {
                                self.check_args_range("contains", &[e], 1, args, span);
                                return Ty::Bool;
                            }
                            "for_each" => {
                                self.check_args_range("for_each", &[func(vec![e])], 1, args, span);
                                return Ty::Nil;
                            }
                            "fold" => return fold_sig(self, "fold", vec![e]),
                            _ => {}
                        }
                    }
                    _ => {}
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
                            // R = the closure argument's actual return type (else `Unknown` on arity
                            // error). `check_args` already inferred the closure (emitting any body
                            // errors); this is a RECOVERY-ONLY re-inference, so snapshot + truncate to
                            // avoid double-reporting those same body errors.
                            let mark = self.diag_mark();
                            let recovered = args.first().map(|arg| self.infer_value(arg));
                            self.diag_rollback(mark);
                            if let Some(Ty::Func { ret, .. }) = recovered {
                                return *ret;
                            }
                            return Ty::Unknown;
                        }
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        let names = self.method_names("RwShared");
                        self.error_help(
                            name_span,
                            format!("type {obj_ty} has no method '{method}'"),
                            suggest::did_you_mean(method, &names),
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
                // TICKET-144 (W14-24): `cas` on a payload holding a `fn` value can never succeed —
                // each `load()` returns a fresh closure copy and closures compare by identity — so the
                // `cas(a.load(), new)` retry loop would spin forever. Go's `atomic.Value` panics
                // `comparing uncomparable type` there. `load`/`store`/`exchange` stay legal; a
                // `Param`/protocol payload hides the fn, so the runtime `cas` backstops it.
                if method == "cas"
                    && let Some(f) = self.reaches_func(&elem, &mut Vec::new())
                {
                    self.error(
                        name_span,
                        format!(
                            "Atomic[{elem}].cas cannot compare a payload that holds a function value ('{f}'): every load() returns a fresh copy, so the swap could never succeed; use store/exchange instead"
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
                // `submit(fn() -> _)->nil`, `shutdown()->nil`, `shutdown_now()->nil` (C5 escape hatch).
                // Non-generic — no element type to substitute (`&[]`). `submit`'s param is typed
                // `fn() -> ?` by `attach_native_module_metadata` so any-return closures are accepted.
                // A bodied generic method (`submit_result[T]`) routes through the helper's generic
                // solver (infer T from the closure return). `submit` itself is non-generic and keeps
                // the capture-floor path in the Concrete arm; the inner `self.submit(...)` that
                // `submit_result`'s body emits re-enters this arm on that non-generic path.
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
                        // A3b (B3.6): `submit`'s closure runs on a pool thread under `--parallel`, so
                        // its captures cross the airlock exactly like a `spawn` task's. Push a capture
                        // floor at the current scope depth around the argument check; the submitted
                        // closure opens its own scope at that depth, so its params/locals are
                        // task-local while any outer binding it reads is flagged by the `infer_ident`
                        // read gate (mirrors `spawn:`).
                        if method == "submit" {
                            self.capture_floors.push(self.scopes.len());
                            self.check_args_range(method, &sig.params, sig.min_params, args, span);
                            self.capture_floors.pop();
                            if let Some(task) = args.first() {
                                self.report_named_call_writes(task, &[], None, true);
                                let mut writes: Vec<String> = match &task.kind {
                                    ExprKind::Closure { body, .. } => self
                                        .closure_literal_writes
                                        .get(&(self.graph_module_idx, body.span))
                                        .cloned()
                                        .unwrap_or_else(|| {
                                            std::mem::take(&mut self.last_closure_writes)
                                                .into_iter()
                                                .collect()
                                        }),
                                    _ => self.value_writes(task),
                                };
                                writes.sort();
                                if let Some(written) = writes.first() {
                                    self.error(
                                        task.span,
                                        format!("'{written}' {}", crate::vm::COPY_WRITE_TAIL),
                                    );
                                }
                            }
                        } else {
                            self.check_args_range(method, &sig.params, sig.min_params, args, span);
                        }
                        sig.ret
                    }
                    NativeHandleMethod::Miss => {
                        self.infer_all(args);
                        let names = self.method_names("Executor");
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
                    let expected: Vec<Ty> = match msig.params.split_first() {
                        Some((_recv, rest)) => rest.iter().map(|t| subst(t, &map)).collect(),
                        None => Vec::new(),
                    };
                    // The receiver's label is dropped with its param.
                    let slots = callable_slots(
                        msig.labels.get(1..).unwrap_or(&[]),
                        expected.len(),
                        expected.len(),
                    );
                    let Some(bound) = self.bind_call(Some(&slots), method, args, 0, span) else {
                        return Ty::Unknown;
                    };
                    self.check_args_subst(method, &expected, expected.len(), &bound, span);
                    return self.bound_method_ret(&proto, method, &msig, &map);
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
            other => {
                self.infer_all(args);
                self.error(name_span, format!("type {other} has no method '{method}'"));
                Ty::Unknown
            }
        }
    }

    // ===== small helpers =====

    pub(super) fn one_arg(&mut self, name: &str, args: &[Expr], span: Span) -> Ty {
        self.check_arity(name, 1, args, span);
        args.first()
            .map(|a| self.infer_value(a))
            .unwrap_or(Ty::Unknown)
    }

    /// TICKET-124 (W13-13): like [`Checker::one_arg`], but reaches an expected type that is
    /// concrete up to in-scope type params (`ty_concrete_but` + `rigid_param`) into the single argument (`Some`/`Ok`/`Err`'s payload), so `Some([A()])` under an
    /// `Option[List[Named]]` hint infers the list literal's elements as `Named`, not `A`.
    pub(super) fn one_arg_hinted(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
        h: Option<&Ty>,
    ) -> Ty {
        self.check_arity(name, 1, args, span);
        let Some(a) = args.first() else {
            return Ty::Unknown;
        };
        match h {
            Some(t) if ty_concrete_but(t, &|n| self.rigid_param(n, &[])) => {
                self.infer_arg(a, Some(t))
            }
            _ => self.infer_value(a),
        }
    }

    pub(super) fn infer_all(&mut self, args: &[Expr]) {
        for a in args {
            self.infer_value(a);
        }
    }

    /// Check argument count and each argument's type against a known parameter list. STRICT — an int
    /// never widens into a `float` slot (D3, TICKET-138).
    /// TICKET-218 — reject a CONSTANT outside the C width a slot carries (`x: int8 = 1000`):
    /// `constant 1000 does not fit int8 (-128..127)`. Called beside the `assignable` of every
    /// value-into-slot site; a new site must call it too, or a constant there reaches C unchecked
    /// (and faults at run time). It descends through list/set/map/tuple literals, `Some`/`Ok`/`Err`,
    /// `if`/`match` branches and the `rhs` of `??`. Skipped while inferring a return (DEC-183) or in
    /// the generic-arg prepass; one report per span (`const_overflow_seen`).
    pub(super) fn check_const_fits(&mut self, slot: &Ty, value: &Expr) {
        if self.inferring_ret || self.generic_arg_prepass {
            return;
        }
        if let Some(w) = slot.width() {
            let shown = if *w == crate::native::cffi::CType::Float32 {
                crate::ast::const_float(value)
                    .filter(|f| !w.fits_f64(*f))
                    .map(|f| format!("{f:e}"))
            } else {
                crate::ast::const_int_scan(value, &mut self.const_scan_visits, &mut Vec::new())
                    .filter(|k| !w.fits_int(*k, true))
                    .map(|k| k.to_string())
            };
            if let Some(k) = shown
                && self.const_overflow_seen.insert(value.span)
            {
                let name = w.width_name().unwrap_or("?");
                let range = w.range_text();
                self.error(
                    value.span,
                    format!("constant {k} does not fit {name} {range}"),
                );
            }
        }
        match (slot, &value.kind) {
            (Ty::List(e) | Ty::Set(e), ExprKind::List(items, _) | ExprKind::Set(items)) => {
                for it in items {
                    self.check_const_fits(e, it);
                }
            }
            (Ty::Map(k, v), ExprKind::Map(es)) => {
                for (ke, ve) in es {
                    self.check_const_fits(k, ke);
                    self.check_const_fits(v, ve);
                }
            }
            (Ty::Tuple(ts), ExprKind::Tuple(xs)) => {
                for (t, x) in ts.iter().zip(xs) {
                    self.check_const_fits(t, x);
                }
            }
            (_, ExprKind::Call { callee, args, .. }) if args.len() == 1 => {
                let ExprKind::Ident(n) = &callee.kind else {
                    return;
                };
                match (n.as_str(), slot) {
                    ("Some", Ty::Option(e))
                    | ("Ok", Ty::Result(e, _))
                    | ("Err", Ty::Result(_, e)) => {
                        self.check_const_fits(e, &args[0]);
                    }
                    _ => {}
                }
            }
            (_, ExprKind::IfElse { then, els, .. }) => {
                self.check_const_fits(slot, then);
                self.check_const_fits(slot, els);
            }
            (_, ExprKind::Match { arms, .. }) => {
                for a in arms {
                    self.check_const_fits(slot, &a.body);
                }
            }
            (_, ExprKind::NullCoalesce { rhs, .. }) => self.check_const_fits(slot, rhs),
            _ => {}
        }
    }

    pub(super) fn check_args(&mut self, name: &str, params: &[Ty], args: &[Expr], span: Span) {
        self.check_args_range(name, params, params.len(), args, span);
    }

    /// D6c — `check_args` generalized to an optional trailing tail: the arg count must fall in
    /// `min_params..=params.len()`, and each supplied arg must match its positional param. Used for the
    /// net socket ops whose `timeout_ms` is optional. `min_params == params.len()` reproduces the
    /// exact-arity behavior of [`Checker::check_args`].
    pub(super) fn check_args_range(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_decl(name, params, min_params, args, span, false);
    }

    /// The arg check for a call to a C variadic `extern` fn (`fn printf(fmt: str, ...)`): the fixed
    /// params check as usual, and each surplus arg must be a C vararg (`c_vararg_ctype`).
    pub(super) fn check_c_variadic_args(
        &mut self,
        name: &str,
        params: &[Ty],
        args: &[Expr],
        span: Span,
    ) {
        if args.len() < params.len() {
            self.error(
                span,
                format!(
                    "'{name}' expects at least {} argument(s), got {}",
                    params.len(),
                    args.len()
                ),
            );
            self.infer_all(args);
            return;
        }
        let (fixed, extra) = args.split_at(params.len());
        self.check_args_range(name, params, params.len(), fixed, span);
        for (k, arg) in extra.iter().enumerate() {
            let ty = self.infer_arg(arg, None);
            if !matches!(ty, Ty::Unknown) && super::setup::c_vararg_ctype(&ty).is_none() {
                self.error(
                    arg.span,
                    format!(
                        "argument {} to extern fn '{name}' has type '{ty}', which cannot be passed \
                         to a C variadic parameter (C varargs take int, float, bool, str or ptr)",
                        params.len() + k + 1
                    ),
                );
            }
        }
    }

    /// Infer a single call argument in *checking mode*: if the argument is a closure literal and the
    /// expected slot type is a `fn(..)`, infer the closure WITH that expected type (source #1 — so its
    /// unannotated params bind to the expected param types and call sites are checked); otherwise it is
    /// the ordinary bottom-up [`Checker::infer_value`]. The single seam every `fn`-typed slot routes
    /// through, so closure-detection lives in one place.
    pub(super) fn infer_arg(&mut self, arg: &Expr, expected: Option<&Ty>) -> Ty {
        if let ExprKind::Closure { params, ret, body } = &arg.kind {
            if matches!(expected, Some(Ty::Func { .. })) {
                return self.infer_closure(params, ret.as_ref(), body, expected);
            }
            return self.infer_value(arg);
        }
        // Non-closure arg: thread the declared parameter type as an expected-type hint so a generic
        // ctor / generic fn-call passed directly as a call argument pre-seeds its type params —
        // `take(Heap([], fn(x, y): x < y))` with `fn take(h: Heap[int])` pins `T=int`. `infer_call`
        // consumes the hint; pair set+clear so a non-call arg never leaks it into a sibling arg.
        if let Some(e) = expected {
            self.expected_hint = Some(e.clone());
            let t = self.infer_value(arg);
            self.expected_hint = None;
            return t;
        }
        self.infer_value(arg)
    }

    /// First-pass bottom-up inference of a generic ctor/call's args (the pass that drives unification).
    /// A closure arg is inferred WITHOUT an expected type here (its params would be `Unknown`); its body
    /// errors — and the phase-5 "cannot infer parameter" error — are SUPPRESSED, because the per-arg
    /// check ([`Checker::check_generic_arg`]) re-infers each closure against its SUBSTITUTED expected
    /// type and reports cleanly there (mirrors the `RwShared.read` recovery-reinfer idiom). Every
    /// generic ctor/variant/fn/method path uses this pair so closure params are pinned by the field/
    /// param type, not left `Unknown`.
    pub(super) fn infer_generic_arg_tys(
        &mut self,
        args: &[Expr],
        declared: &[Ty],
        arg_hints: &[Option<Ty>],
    ) -> Vec<Ty> {
        // The "this read is re-pinned afterwards" licence ([`Checker::generic_fn_value_prepass`],
        // set by the two callers that DO re-pin) belongs to the IMMEDIATE bare-identifier arguments
        // only — they are the only shape `generic_fn_value_sig` can ever re-pin. Any other
        // argument is a whole SUBTREE whose own reads this call will never revisit, so the licence
        // must not leak into it: without this, `take2(Bx(ident), 5)` on a generic callee silenced the
        // nested ctor's wall too and check-cleanly built a `Bx[fn(T) -> T]` — a stored value whose
        // type nothing determines. Scoped here, in the ONE helper every generic-argument prepass goes
        // through, so it covers the method path's identical leak (`Holder(0).m(Bx(ident), 5)`, wrongly
        // accepted since the licence was introduced) in the same place.
        let repins = std::mem::take(&mut self.generic_fn_value_prepass);
        let tys = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                self.generic_fn_value_prepass = repins
                    && (matches!(a.kind, ExprKind::Ident(_))
                        || self.generic_fn_value_sig(a).is_some());
                if matches!(a.kind, ExprKind::Closure { .. }) {
                    let mark = self.diag_mark();
                    // Keep the closure's unannotated params `Unknown` in the unification prepass —
                    // the free-body scan (sources #2/#3) must not pin them here (see the field doc).
                    let saved = std::mem::replace(&mut self.generic_arg_prepass, true);
                    let t = self.infer_value(a);
                    self.generic_arg_prepass = saved;
                    self.diag_rollback(mark);
                    t
                } else if let Some(d) = declared.get(i)
                    && ty_fully_concrete(d)
                {
                    // TICKET-094 defect C — a CONCRETE declared slot (never the callee's own type
                    // variable) keeps the expected-type hint its non-generic twin already threads
                    // through `infer_arg`.
                    self.infer_arg(a, Some(d))
                } else if let Some(Some(h)) = arg_hints.get(i) {
                    // TICKET-124 (W13-13): the declared slot is a bare/under-determined type
                    // param, but the CTOR's own expected-type hint pinned this argument's type
                    // concretely — reach the hint into the nested argument instead of stopping at
                    // the outermost ctor.
                    self.infer_arg(a, Some(h))
                } else {
                    self.infer_value(a)
                }
            })
            .collect();
        // Hand the licence back exactly as it was found — the caller owns its own save/restore.
        self.generic_fn_value_prepass = repins;
        tys
    }

    /// Per-argument check for a generic ctor/call/method. `expected` is the arg's SUBSTITUTED declared
    /// type; `fallback` is its first-pass inferred type. A closure arg is re-inferred in checking-mode
    /// against `expected` (binding its unannotated params + reporting body errors here, since
    /// [`Checker::infer_generic_arg_tys`] suppressed them); other args keep `fallback`. Then assert
    /// assignability with the uniform "argument to '<name>'" diagnostic.
    /// Returns the arg's REFINED actual type: for a closure, its type re-inferred in checking-mode
    /// against `expected` (so `fn(x): x*2` against `fn(int) -> U` becomes the concrete `fn(int) -> int`
    /// — its body-return type is now known); for a non-closure, `fallback` unchanged. The generic-METHOD
    /// path ([`Checker::infer_generic_method`]) feeds these refined types back into a SECOND `unify`
    /// pass (the loop-back), so a return-position type param bound ONLY from a closure body (e.g. `map`'s
    /// `U`) resolves concretely instead of leaking a `Ty::Param`. The free-fn/ctor path
    /// ([`Checker::infer_generic_call`]) ignores the return (its behavior is unchanged).
    pub(super) fn check_generic_arg(
        &mut self,
        name: &str,
        expected: &Ty,
        fallback: &Ty,
        arg: &Expr,
    ) -> Ty {
        let refined = if matches!(arg.kind, ExprKind::Closure { .. }) {
            // Re-infer the closure in checking-mode against the substituted expected type: this binds
            // its unannotated params and re-reports its body errors (which `infer_generic_arg_tys`
            // suppressed). The assignability check below still uses the first-pass `fallback`
            // (Unknown-bearing) type — its params/return are leniently assignable, so a type param
            // bound ONLY from this closure's body (e.g. `Mapped`'s `U`, recovered from the closure's
            // return) doesn't spuriously fail against an unbound `Ty::Param`. The param binding + body
            // re-check is the real enforcement; the `fallback` check still catches an arity or
            // annotated-return mismatch. The re-inferred type (`fn(int) -> int`) is RETURNED for the
            // caller's loop-back unify.
            self.infer_arg(arg, Some(expected))
        } else {
            fallback.clone()
        };
        self.check_const_fits(expected, arg);
        if !self.assignable(expected, fallback) {
            let [fallback_s, expected_s] = Ty::render_distinct([fallback, expected]);
            self.error(
                arg.span,
                format!(
                    "argument to '{name}' has type {fallback_s}, expected {expected_s}{}",
                    float_fix_note_join(fallback, expected)
                ),
            );
        }
        refined
    }

    /// Trial-check the given closure args (by index into `args`/`decl_tys`) against their declared
    /// slot type substituted with `sub`, reporting whether ANY error was produced. Errors emitted
    /// during the probe are rolled back (`truncate`) so the trial never leaks diagnostics — it only
    /// answers "would these bodies error under this substitution?". Used by
    /// [`Checker::report_uninferable_closure_params`] to discriminate a genuine type-param deadlock
    /// from a harmless body. (Hover side effects are first-hit-wins and idempotent, so the eventual
    /// real per-arg check records the same result.)
    pub(super) fn trial_check_closure_args(
        &mut self,
        idxs: &[usize],
        decl_tys: &[Ty],
        args: &[Expr],
        sub: &HashMap<String, Ty>,
        prepass: bool,
    ) -> bool {
        // `prepass` routes an unannotated closure param whose substituted slot type is `Unknown`
        // through the silent-`Unknown` binding (see `infer_closure`) instead of the
        // free-scan/annotation-required path — so the "params bound to `Unknown`" trial actually
        // checks the body with `Unknown` params (no spurious "cannot infer parameter").
        let saved = std::mem::replace(&mut self.generic_arg_prepass, prepass);
        let mark = self.diag_mark();
        for &i in idxs {
            if let (Some(decl), Some(arg)) = (decl_tys.get(i), args.get(i)) {
                let expected = subst(decl, sub);
                self.infer_arg(arg, Some(&expected));
            }
        }
        let errored = self.errors.len() > mark.errors;
        self.diag_rollback(mark);
        self.generic_arg_prepass = saved;
        errored
    }

    /// Detect (and clearly report) the un-inferable type-parameter DEADLOCK that arises when a
    /// generic ctor/fn is given an **unannotated closure** for a closure-typed slot AND no other
    /// argument pins the type parameter that slot mentions (e.g. `Heap([], fn(a, b): a < b)` — the
    /// empty `[]` gives no element type and the bare comparator params give none either). Without
    /// this, the leftover `Ty::Param(T)` flows into the closure body and surfaces as a misleading
    /// "cannot compare T and T" inside the user's lambda.
    ///
    /// Fires ONLY on the genuine deadlock: a still-unbound param (`!sub.contains_key`) that is
    /// mentioned by an unannotated closure's PARAMETER slot AND whose body actually *constrains* it.
    /// The mention is a NECESSARY condition (forms that pin the param — turbofish, a concrete
    /// element, annotated closure params — leave `sub` populated, so the guard never reaches here);
    /// the body PROBE ([`Checker::trial_check_closure_args`], two trials) is the SUFFICIENT one: it
    /// fires only when leaving the param as the unbound `Ty::Param` errors the body but binding it to
    /// `Unknown` does not. That keeps a harmless body that never uses the param (`fn(x): print(x)`,
    /// `fn(x): 42`) type-checking — those ran on `main` and must not be newly rejected — and leaves an
    /// unrelated body error (errors under BOTH trials) for the normal per-arg check to report.
    /// On firing it binds the offending params to `Unknown` in `sub` so the downstream substituted
    /// closure check sees `Unknown` params (no cascade) — the resulting type is identical to the
    /// existing `unwrap_or(Ty::Unknown)` fallback, so this is behavior-preserving apart from the
    /// message. Returns `true` if it fired.
    pub(super) fn report_uninferable_closure_params(
        &mut self,
        name: &str,
        tps: &[TyParam],
        decl_tys: &[Ty],
        args: &[Expr],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) -> bool {
        let unbound: std::collections::HashSet<String> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .filter(|n| !sub.contains_key(n))
            .collect();
        if unbound.is_empty() {
            return false;
        }
        // Only an unbound param appearing in a closure's PARAMETER position is a deadlock: such a
        // param can't be recovered from the closure body (the params would have to be typed first).
        // A param that appears ONLY in the closure's RETURN slot (e.g. `Mapped`'s `f: fn(T) -> U`)
        // is still inferable from the body, so it must NOT trigger — scan declared param slots only,
        // aligned to the UNANNOTATED closure parameters.
        let mut mentioned: Vec<String> = Vec::new();
        let mut candidate_idxs: Vec<usize> = Vec::new();
        for (ai, (decl, arg)) in decl_tys.iter().zip(args).enumerate() {
            if let ExprKind::Closure {
                params: cparams, ..
            } = &arg.kind
                && let Ty::Func {
                    params: dparams, ..
                } = decl
            {
                let before = mentioned.len();
                for (i, cp) in cparams.iter().enumerate() {
                    if cp.ty.is_none()
                        && let Some(dp) = dparams.get(i)
                    {
                        ty_collect_params(dp, Some(&unbound), &mut mentioned);
                    }
                }
                // This closure arg has an unannotated param slot mentioning an unbound param — it is
                // a candidate whose BODY we must probe before deciding the deadlock actually bites.
                if mentioned.len() > before {
                    candidate_idxs.push(ai);
                }
            }
        }
        if mentioned.is_empty() {
            return false;
        }
        // Preserve declaration order of the type params in the message.
        let names: Vec<String> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .filter(|n| mentioned.contains(n))
            .collect();
        // PROBE the candidate closure bodies before firing. A still-unbound param in an unannotated
        // closure slot is a *potential* deadlock, but only a GENUINE one when the body actually
        // constrains that param (e.g. `a < b` — ordering on an unconstrained `Ty::Param` is
        // rejected). A harmless body (`print(x)`, a constant) imposes no constraint, type-checks on
        // every engine, and ran on `main`; firing there would reject previously-valid code. The
        // discriminator: check the candidate bodies twice — once leaving the params as the unbound
        // `Ty::Param` (the current `sub`), once with them bound to `Unknown`. Fire ONLY when the
        // unbound form errors AND the `Unknown` form is clean. That isolates a true "needs `T`"
        // body from a harmless one (clean under BOTH) and from an unrelated body error (errors under
        // BOTH — left for the normal per-arg check to report as itself).
        let errored_unbound =
            self.trial_check_closure_args(&candidate_idxs, decl_tys, args, sub, false);
        if !errored_unbound {
            return false;
        }
        let mut sub_unknown = sub.clone();
        for n in &names {
            sub_unknown.insert(n.clone(), Ty::Unknown);
        }
        let errored_unknown =
            self.trial_check_closure_args(&candidate_idxs, decl_tys, args, &sub_unknown, true);
        if errored_unknown {
            // The body errors even with the params known (Unknown) — the failure is unrelated to
            // `T`. Don't mask it; let the normal per-arg check surface the real diagnostic.
            return false;
        }
        let list = names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let turbo = names.join(", ");
        let word = if names.len() == 1 {
            "type parameter"
        } else {
            "type parameters"
        };
        self.error(
            span,
            format!(
                "cannot infer {word} {list} of `{name}`; annotate `{name}[{turbo}](…)` or the closure parameters"
            ),
        );
        for n in names {
            sub.insert(n, Ty::Unknown);
        }
        true
    }

    /// [`Checker::check_args_range`] for a List/Set COLLECTION mutator receiver — the only path that
    /// may show the element-pin annotation hint on a `push`/`add`/`insert` mismatch (handle methods
    /// like `Atomic.add` route through `check_args_range` and never see it).
    pub(super) fn check_args_range_coll(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_decl(name, params, min_params, args, span, true);
    }

    /// [`Checker::check_args_range`] for a SUBSTITUTED parameter list (a method of a generic type,
    /// whose `T`s are already replaced by the receiver's type args).
    pub(super) fn check_args_subst(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_decl(
            name,
            params,
            min_params.min(params.len()),
            args,
            span,
            false,
        );
    }

    /// PART A — passing a bare empty-collection binding (`b := []`) into a CONCRETE collection
    /// parameter (`f(xs: List[int])`) CONSTRAINS its element type: the requirement is dropped and
    /// the element pinned in one operation, the third refine-on-first-use site beside
    /// [`Self::refine_receiver`] (`push`/`add`) and [`Self::refine_index_receiver`] (`m[k]=v`).
    /// Passing into a concrete slot IS a use, so it pins exactly like the first `push`.
    ///
    /// Gated on the parameter being fully concrete so a generic (`fn ident[T](xs: List[T])`) or
    /// un-inferred slot pins nothing, and on the argument actually fitting so a mismatch keeps its
    /// ordinary diagnostic. Lives here rather than inline in `check_args_range_decl` because the
    /// GENERIC call paths never reach that function — `infer_generic_call` /
    /// `infer_generic_method` match arguments with `unify` directly — so a parameter made concrete
    /// by a SIBLING argument pinned nothing: measured, `fn move_first[T](a: List[T], b: List[T])`
    /// called `move_first(["x"], xs)` then `xs.push(1)` printed `['x', 1]`, check-clean at rc=0.
    pub(super) fn constrain_empty_arg(&mut self, arg: &Expr, pt: &Ty) {
        let ExprKind::Ident(name) = &arg.kind else {
            return;
        };
        // FULLY concrete — no `Ty::Unknown` AND no `Ty::Param`, nested too. The weaker
        // `!contains_unknown_in_slot` was enough while only `check_args_range_decl` called this (a
        // non-generic callee's params carry no `Ty::Param`), but the generic paths hand over a
        // SUBSTITUTED slot that can still be `List[T]` with `T` free — and pinning to that made the
        // binding rigidly `List[T]`: measured, `fn ident[T](xs: List[T])` called `ident(xs)` then
        // `xs.push(1)` reported *expected T, found int* on a program that must pin nothing.
        if !ty_fully_concrete(pt) {
            return;
        }
        let Some(bt) = self.lookup(name) else {
            return;
        };
        if !self.assignable(pt, &bt) {
            return;
        }
        self.drop_empty_site(name, Some(pt));
    }

    fn check_args_range_decl(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
        is_collection: bool,
    ) {
        if !(min_params..=params.len()).contains(&args.len()) {
            let want = if min_params == params.len() {
                format!("{}", params.len())
            } else {
                format!("{min_params}–{}", params.len())
            };
            self.error(
                span,
                format!("'{name}' expects {want} argument(s), got {}", args.len()),
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let at = self.infer_arg(arg, params.get(i));
            // PART A: passing a bare empty-collection binding (`b := []`) into a CONCRETE collection
            // parameter (`f(xs: List[int])`) constrains its element type — clear the pending annotation
            // requirement (the spec's typed-parameter false-positive guard, one binding away from the
            // direct-literal `f([])` form). Gated on the param being a fully-concrete type so an
            // un-inferred / generic slot does not spuriously satisfy the requirement.
            if let Some(pt) = params.get(i) {
                self.constrain_empty_arg(arg, pt);
                self.check_const_fits(pt, arg);
            }
            if let Some(pt) = params.get(i)
                && !self.assignable(pt, &at)
            {
                let [expected, actual] = Ty::render_distinct([pt, &at]);
                // Annotation hint for a collection mutator whose element slot was PINNED by an
                // earlier push/add/insert (refine-on-first-use). An un-annotated `xs := []` reads as
                // `list[<first element>]`; a later element of a different (e.g. protocol-sibling) type
                // is a real mismatch — point the user at the explicit annotation that makes a
                // mixed/protocol collection legal.
                // The element-pin narrative is only valid for a List/Set collection receiver. The
                // method name `add` also names `Atomic.add` (a handle), whose float mismatch must NOT
                // show the collection hint — gate on the receiver actually being a collection.
                let pnote = self.protocol_note(pt, &at);
                // An int at a `float` slot is not a stale element pin — the narrative below is
                // false there (the type was DECLARED, not learned from an earlier use), so
                // `float_fix_note` names the fix instead.
                let coll_mismatch_is_float_widen = matches!((pt, &at), (Ty::Float, Ty::Int));
                let hint = if !pnote.is_empty() {
                    pnote
                } else if is_collection
                    && i == 0
                    && matches!(name, "push" | "add" | "insert")
                    && !coll_mismatch_is_float_widen
                {
                    // Only an UN-BOUND/leaked type param (not in scope here) means "un-inferred": a
                    // return-only `T` from `empty[T]()` called with nothing to bind it from. A
                    // `Ty::Param` that IS in scope (`self.type_params`) is a legitimately-bound
                    // generic param genuinely pinned by an earlier push — keep the original
                    // narrative for it (and for every concrete element type).
                    if matches!(pt, Ty::Param(p) if !self.type_params.contains_key(p)) {
                        // The expected element type is an un-inferred type parameter (e.g. a
                        // return-only `T` from `empty[T]()` with nothing to bind it from), NOT a
                        // type pinned by an earlier push. The "earlier push" narrative is wrong
                        // here (this may be the FIRST push) and `List[<protocol>] = []` would not
                        // help — the fix is to bind the parameter at the construction site.
                        format!(
                            " (the collection's element type is the un-inferred type parameter {expected}; bind it at the construction site with a turbofish or annotation, e.g. `empty[int]()` or `xs: List[int] = ...`)"
                        )
                    } else {
                        // TICKET-124 (W13-15): the old "already pinned … by an earlier use" wording
                        // was false whenever the slot was DECLARED (`l: List[float] = [1.5]`), not
                        // learned from an earlier push — say what's actually true of both cases.
                        format!(
                            " (the collection's element type is {expected}, fixed by its annotation or an earlier use; annotate the binding, e.g. `List[<protocol>] = []`, for a mixed/protocol collection)"
                        )
                    }
                } else {
                    float_fix_note(pt, &at).to_string()
                };
                self.error(
                    arg.span,
                    format!(
                        "argument {} of '{name}': expected {expected}, found {actual}{hint}",
                        i + 1
                    ),
                );
            }
        }
    }

    pub(super) fn check_arity(&mut self, name: &str, n: usize, args: &[Expr], span: Span) {
        if args.len() != n {
            self.error(
                span,
                format!("{name}() expects {n} argument(s), got {}", args.len()),
            );
        }
    }

    /// A generic struct ctor's arity check (W8-47). `desugar` already declined to splice a
    /// `GenericProvider`-defaulted field when the call carries no turbofish (or a mismatched one),
    /// so `args` is genuinely short here. An UNBOUNDED owner type parameter with every omitted
    /// trailing field defaulted is a binder-inference problem, not a missing-argument one — report
    /// that instead of the plain arity message, naming the FIRST omitted field and the first type
    /// parameter its type mentions. Anything else falls through to `check_arity`.
    #[allow(clippy::too_many_arguments)] // ctor shape (tps/fields/defaulted) + call args + span
    pub(super) fn check_ctor_arity(
        &mut self,
        name: &str,
        tps: &[TyParam],
        fields: &[(String, Ty)],
        defaulted: &[String],
        targs: &[Ty],
        args: &[Expr],
        span: Span,
    ) {
        if args.len() == fields.len() {
            return;
        }
        if targs.is_empty()
            && args.len() < fields.len()
            && !tps.is_empty()
            && tps.iter().all(|tp| tp.bounds.is_empty())
        {
            let wanted: std::collections::HashSet<String> =
                tps.iter().map(|tp| tp.name.clone()).collect();
            let all_defaulted = fields[args.len()..]
                .iter()
                .all(|(fname, _)| defaulted.iter().any(|d| d == fname));
            if all_defaulted {
                for (fname, fty) in &fields[args.len()..] {
                    let mut got = Vec::new();
                    ty_collect_params(fty, Some(&wanted), &mut got);
                    if let Some(tp) = got.first() {
                        self.error(
                            span,
                            format!(
                                "cannot infer type parameter {tp} for '{name}'; the default for field '{fname}' can only be filled with explicit type arguments, e.g. {name}[int](...)"
                            ),
                        );
                        return;
                    }
                }
            }
        }
        self.check_arity(name, fields.len(), args, span);
    }

    pub(super) fn expect_bool(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer_value(e);
        if t != Ty::Bool && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be bool, found {t}"));
        }
    }

    pub(super) fn expect_int(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer_value(e);
        if t != Ty::Int && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be int, found {t}"));
        }
    }

    pub(super) fn expect_int_val(&mut self, e: &Expr) {
        self.expect_int(e, "argument");
    }

    // ===== generics & protocols =====

    /// The `Self` type for a struct's own methods: `Struct(name, [Param(p) for each type param])`,
    /// so inside `struct Stack[T]` the receiver is `Stack[T]` and `self.items` is `list[T]`.
    pub(super) fn struct_self_ty(&self, name: &str) -> Ty {
        // Key by the struct's runtime key (bare unless a cross-module clash disambiguated it), exactly
        // like `enum_self_ty`. In the multi-module (`build_graph`) path the layout is
        // stored under `<module-key>::Name`, so a bare-`name` lookup here would miss — leaving `self`'s
        // type wrong during both return inference and pass-2 body checking.
        let key = self.bare_key(name);
        let args = self
            .structs
            .get(&key)
            .map(|i| {
                i.type_params
                    .iter()
                    .map(|tp| Ty::Param(tp.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ty::Struct(key, args)
    }

    /// The `Ty::Enum` of an enum's own `self`: keyed by its runtime key, parameterized by its own
    /// generic type params as `Ty::Param`s (so `fn get(self) -> T` inside `enum Box[T]` resolves).
    pub(super) fn enum_self_ty(&self, name: &str) -> Ty {
        let key = self.bare_key(name);
        let args = self
            .enum_type_params
            .get(&key)
            .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
            .unwrap_or_default();
        Ty::Enum(key, args)
    }

    /// Reject a reserved builtin type name used as a generic type-PARAMETER identifier (`struct
    /// Box[int]` / `[List]` / `[Result]`, a method's own `[U]`, a `protocol P[int]`). A reserved name
    /// as a param is a one-way-ratchet violation that otherwise type-checks clean and then shadows
    /// kind-dependently — a scalar param (`int`/`str`/…) is dead/unreferenceable (the scalar wins in
    /// `resolve_type`), while a container/enum-builtin param (`List`/`Result`/…) silently SHADOWS the
    /// builtin and acts as a real generic. Mirror the decl-NAME guards (`struct int` →
    /// `reserved (builtin)`) so the param form errors identically. Predicate = `is_reserved_type` +
    /// the fixed-width FFI integer names (`int32`/`int64`/…, reserved TYPE names via
    /// `native::ffi::is_width`). Deliberately NOT `is_reserved_protocol`: a param named after a
    /// prebuilt protocol (`fn id[Comparable]`) is a protocol-name shadow, not a builtin-TYPE shadow,
    /// and is kept legal by design (guarded by `protocol_bound_and_typeparam_named_protocol_still_ok`,
    /// commit b2aa8ac). A param BOUND `[T: Comparable]` is likewise untouched (the bound is a separate
    /// `Bound` list, not the param name). Called once per decl at the hoist sites (struct/enum/
    /// fn_sig/protocol), NOT inside `enter_type_params` (which is re-entered during body checking and
    /// would double-report).
    pub(super) fn reject_reserved_type_params(&mut self, tps: &[TypeParam]) {
        for tp in tps {
            if is_reserved_type(&tp.name) || crate::native::ffi::is_width(&tp.name) {
                self.error(
                    tp.name_span,
                    format!("type '{}' is reserved (builtin)", tp.name),
                );
            }
        }
    }

    /// Install `tps` as the in-scope generic type parameters, returning the previous map to restore.
    /// The bounds are resolved here, in the declaring scope, by the silent `resolve_bounds`
    /// (TICKET-202).
    pub(super) fn enter_type_params(&mut self, tps: &[TypeParam]) -> HashMap<String, Vec<TyBound>> {
        let resolved = self.resolve_bounds(tps, Span::default());
        let saved = self.type_params.clone();
        for (tp, rtp) in tps.iter().zip(resolved) {
            // Editor hover (decl-site): record the bound generic param `T` at its DECLARATION token
            // (`fn id[T]`, `struct Box[T]`, a method `[U]`). This is the single funnel for entering
            // type params, so every generic decl form is covered. The hover renders the bare param
            // name (`T`); a bound suffix (`T: Comparable`) is not representable through the `Ty`-only
            // hover channel. GUARD on the probe so the `Ty::Param(..clone())` argument is not built on
            // every generic check (enter_type_params is hot — runs for every generic fn/struct/method).
            if self.hover_probe.is_some() {
                self.hover_record_at(
                    tp.name_span,
                    &Ty::Param(tp.name.clone()),
                    HoverKind::Struct,
                    None,
                );
            }
            self.type_params.insert(tp.name.clone(), rtp.bounds);
        }
        saved
    }

    /// [`Self::enter_type_params`] for an already-resolved list (a stored signature's).
    pub(super) fn enter_ty_params(&mut self, tps: &[TyParam]) -> HashMap<String, Vec<TyBound>> {
        let saved = self.type_params.clone();
        for tp in tps {
            if self.hover_probe.is_some() {
                self.hover_record_at(
                    tp.name_span,
                    &Ty::Param(tp.name.clone()),
                    HoverKind::Struct,
                    None,
                );
            }
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        saved
    }

    pub(super) fn exit_type_params(&mut self, saved: HashMap<String, Vec<TyBound>>) {
        self.type_params = saved;
    }

    /// Validate the bounds declared on a type parameter: each names a known protocol, and the number
    /// of type args matches the protocol's arity (a parameterized `protocol Container[T]` requires
    /// one; a bare protocol requires none). `Iterator` additionally may appear at most once (its
    /// element recovery can't disambiguate two).
    pub(super) fn check_bounds(&mut self, bounds: &[Bound], param: &str, span: Span) {
        let mut seen_iterator = false;
        for b in bounds {
            let Some(arity) = self.protocol_shape(&b.name).map(|p| p.type_params.len()) else {
                // A module-qualified bound (TICKET-173) that misses: an unimported module reuses the
                // qualified annotation's text (`src/checker/sig.rs`, `Type::Qualified`). The module
                // is everything before the LAST dot, so a full path `pkg.deep.P` names `pkg.deep`.
                if !b.name.contains("::")
                    && let Some((module, member)) = b.name.rsplit_once('.')
                    && !self.imported_modules.contains_key(module)
                {
                    let msg = self.ambiguous_bind_msg(module).unwrap_or_else(|| {
                        format!("unknown module '{module}' (import it to use `{module}.{member}`)")
                    });
                    self.error(span, msg);
                    continue;
                }
                // A protocol alias that APPLIES TYPE ARGUMENTS (`type IntBag = Bag[int]`) cannot name
                // a bound: there is no substitution seam to carry `int` into `T`'s bound. Diagnose
                // this shape by name rather than falling through to the generic "unknown protocol".
                if let Some(Type::Generic(head, ..)) = self.aliases.get(&b.name).cloned()
                    && self.protocol_shape(&head).is_some()
                {
                    self.error(
                        span,
                        format!(
                            "type alias '{}' applies type arguments, so it cannot name a bound on '{param}' -- write the protocol and its arguments in the bound",
                            b.name
                        ),
                    );
                    continue;
                }
                // A `where T: <scalar>` equality bound (int/float/bool/str/…) — not a protocol, but a
                // valid constraint pinning `T` to exactly that scalar type. It takes no type args.
                if Self::scalar_bound_ty(&b.name).is_some() {
                    if !b.args.is_empty() {
                        self.error(span, format!("type '{}' takes no type arguments", b.name));
                    }
                    continue;
                }
                // A `where T: List/Map/Set` constructor-kind bound (no element binder — takes no type
                // args here; the element/key/value types are free). Mirrors the scalar accept above.
                if Self::container_bound_matches(&b.name, &Ty::Unknown).is_some() {
                    if !b.args.is_empty() {
                        self.error(span, format!("type '{}' takes no type arguments", b.name));
                    }
                    continue;
                }
                self.error(
                    span,
                    format!("unknown protocol '{}' in bound on '{param}'", b.name),
                );
                continue;
            };
            if b.args.len() != arity {
                let msg = if arity == 0 {
                    format!("protocol '{}' takes no type arguments", b.name)
                } else {
                    format!(
                        "protocol '{}' takes {arity} type argument(s), found {}",
                        b.name,
                        b.args.len()
                    )
                };
                self.error(span, msg);
            }
            if b.name == "Iterator" {
                if seen_iterator {
                    self.error(span, format!("'{param}' has more than one Iterator bound"));
                }
                seen_iterator = true;
            }
            // Resolve the bound's type args (with the surrounding params in scope) so an unknown type
            // inside a bound — e.g. `Container[Bogus]` — is reported rather than silently accepted.
            for a in &b.args {
                let _ = self.resolve_type(a, span);
            }
        }
    }
}

/// TICKET-187 — the one slot builder for a callee with no declaration to bind against: a protocol
/// requirement (it binds by the PROTOCOL's parameter names and declares no defaults, so `min` is
/// its arity) and a fn value whose type carries no declaration slots (its labels and callee-filled
/// tail from `min`). Slot `i` is named `names[i]` and callee-filled from `min` on.
pub(super) fn callable_slots(
    names: &[Option<String>],
    min: usize,
    len: usize,
) -> Vec<crate::desugar::SlotSpec> {
    (0..len)
        .map(|i| crate::desugar::SlotSpec {
            name: names.get(i).cloned().flatten(),
            default: (i >= min).then_some(crate::desugar::Dflt::CalleeFilled),
            is_variadic: false,
        })
        .collect()
}

/// The bare head NAME of a type-level turbofish, in either carrier the parser produces:
/// `Type[int]` and `Type[int, str]` both arrive as an `Index` over an `Ident`. Syntax only —
/// whether that name is a shadowing type parameter is [`Checker::shadowing_type_param`]'s single
/// answer. That excludes a LOCAL binding (so `arr[i].len()` is untouched) but deliberately NOT a
/// module GLOBAL: a type parameter is the inner scope, so it shadows a global of the same name for
/// the whole body, and `g[0]` under `fn h[g](…)` is the parameter — Go answers the same.
fn type_apply_param_head(obj: &Expr) -> Option<String> {
    match &obj.kind {
        ExprKind::Index { obj: tobj, .. } => match &tobj.kind {
            ExprKind::Ident(n) => Some(n.clone()),
            _ => None,
        },
        _ => None,
    }
}
