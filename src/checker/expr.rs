// checker::expr — split out of checker/mod.rs. `super::*` == the `checker` module.
// Expression & call inference, keyword calls, type application, indexing.

use super::fn_value::WrittenTypeArgs;
use super::resolve::PathPos;
use super::setup::{HeadBinding, TypeHead, TypeHeadKind};
use super::*;

/// Methods that were removed or renamed: (receiver type, method, what to write instead).
const REMOVED_METHODS: &[(&str, &str, &str)] = &[
    (
        "str",
        "reverse",
        "`str.reverse()` is removed; write `s.reversed()`",
    ),
    (
        "Map",
        "has",
        "`Map.has()` is renamed; write `m.contains(k)`",
    ),
    (
        "Set",
        "has",
        "`Set.has()` is renamed; write `s.contains(v)`",
    ),
];

/// The note a removed or renamed method's miss carries, keyed by receiver type name.
pub(super) fn removed_method_note(recv: &str, method: &str) -> Option<&'static str> {
    REMOVED_METHODS
        .iter()
        .find(|(r, m, _)| *r == recv && *m == method)
        .map(|(_, _, text)| *text)
}

/// Result of resolving a bodied/native method on one of the reserved native handles
/// (`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`Writer`/`Reader`). The shared
/// `resolve_native_handle_method` helper folds the byte-identical lookup + hover + generic branch;
/// each arm keeps ITS residual (numeric gate, submit capture-floor, `read` R-recovery) inline.
pub(super) enum NativeHandleMethod {
    /// A harvested method carrying its OWN `[U]` params — already routed through the generic
    /// solver; the returned `Ty` is the arm's result.
    Generic(Ty),
    /// A non-generic sig; the caller runs its own residual (`check_args_range` + any special case).
    Concrete(Box<FnSig>),
    /// Lookup miss; the caller runs `infer_all(args)` + "no method" error.
    Miss,
}

/// TICKET-239 — how [`Checker::infer_generic_arg_tys`] inferred one generic-call argument. The one
/// answer to "was this argument prepassed, and is its prepass return a guess": every later reader
/// (`check_generic_arg`, the return-mask sites) reads this, never the node kind — an owned-slot
/// closure is a closure node too, and its return type is real.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ArgPass {
    /// Inferred once, against its slot or bottom-up; its type and its diagnostics stand.
    Checked,
    /// A closure whose slot depends on a sibling argument: inferred in a rolled-back prepass, to
    /// be re-inferred against its substituted slot. `ret_inferred` — its return was not written.
    Prepass { ret_inferred: bool },
}

impl ArgPass {
    /// Whether the argument must be re-inferred against its substituted slot.
    pub(super) fn deferred(self) -> bool {
        matches!(self, ArgPass::Prepass { .. })
    }
    /// Whether the argument's prepass return type is a guess that unification must not read.
    pub(super) fn masks_ret(self) -> bool {
        matches!(self, ArgPass::Prepass { ret_inferred: true })
    }
}

/// The container primitives of an `RwShared` box (TICKET-243), one row per element head:
/// `(head, the std.concurrency.collection type that owns the surface, the primitive names)`.
/// The `Ty::RwShared` arm types exactly these names, and only inside std (`std_internal_hidden`);
/// a Miss on one names the collection.
///
/// A walk is not a primitive (TICKET-246): the collection writes it as `inner.get()` plus a
/// loop. [`RWSHARED_WALKS`] lists the walk names per head only so a Miss on one still names the
/// collection.
pub(super) const RWSHARED_WALKS: &[(&str, &[&str])] = &[
    ("List", &["for_each", "fold"]),
    ("Map", &["for_each_entry", "fold_entries"]),
    ("Set", &["for_each", "fold"]),
];

pub(super) const RWSHARED_VIEWS: &[(&str, &str, &[&str])] = &[
    (
        "List",
        "ConcurrentList",
        &["_len", "_at", "_slice", "_push"],
    ),
    (
        "Map",
        "ConcurrentMap",
        &[
            "_len",
            "_get_key",
            "_has",
            "_set_key",
            "_remove_key",
            "_get_or_insert",
        ],
    ),
    (
        "Set",
        "ConcurrentSet",
        &["_len", "_contains", "_add", "_remove"],
    ),
];

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
        self.report_named_call_writes(callee, &slots, false);
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
            match callee.kind.as_ident() {
                Some(name) => {
                    if let Ok(key) = self.labels_certain(name) {
                        let pending = (key, super::globals::KwUse::GenStamp(call, crossing, span));
                        if !self.kw_pending.contains(&pending) {
                            self.kw_pending.push(pending);
                        }
                    }
                }
                None => crate::checker::record_call_table_entry(
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
                format!("'{written}' {}", crate::base::words::COPY_WRITE_TAIL),
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
            let sig = super::infer_access::json_decode_sig();
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
                // A bare imported variant (`Som(5)` after `import Som from Opt1`, or the prelude's
                // `Some` / `Ok` / `Err`): the variant constructor of its enum. A NULLARY variant is a
                // value, so calling it (`None(1)`) is the value call's `is not callable` below
                // (Rust: E0618).
                Some(Resolution::VariantFn {
                    enum_key, variant, ..
                }) => {
                    if self.reject_item_targs(name, args, &targs, span) {
                        return Ty::Unknown;
                    }
                    if let Some(v) = self.variants.get(&(enum_key.clone(), variant.clone())) {
                        let v = v.clone();
                        let pinned = self
                            .imported_variants
                            .get(name)
                            .and_then(|iv| iv.head.pinned.clone())
                            .unwrap_or_default();
                        for (_, a) in named {
                            self.infer_value(a);
                        }
                        return self.infer_variant_call(
                            &v,
                            name,
                            args,
                            &pinned,
                            callee.span,
                            span,
                            expected,
                        );
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
            let label = match callee.kind.as_ident() {
                Some(n) => format!("'{n}'"),
                None => "this expression".to_string(),
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
                TypeHeadKind::Enum => Ty::enum_ty(th.key.clone(), Vec::new()),
                TypeHeadKind::Struct | TypeHeadKind::Protocol => {
                    Ty::Struct(th.key.clone(), Vec::new())
                }
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
        // A builtin takes no named arguments; a user fn of the same name (it shadows the builtin)
        // does: the callee's resolution decides.
        if !named.is_empty()
            && matches!(
                self.classify_path(callee, super::resolve::PathPos::Callee),
                Some(Resolution::Builtin(_))
            )
        {
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
            Resolution::Local
            | Resolution::Global { .. }
            | Resolution::Fn { .. }
            | Resolution::Provider
            | Resolution::ParamMethodFn { .. }
            | Resolution::IndexCall
            | Resolution::Variant { .. }
            | Resolution::MethodFn { .. }
            | Resolution::VariantFn { .. }
            | Resolution::WitnessStatic(_)
            | Resolution::ModuleMember { .. }
            | Resolution::Module(_)
            | Resolution::Member
            | Resolution::PatBinding
            | Resolution::PatStruct(_)
            | Resolution::Decode(_) => {
                Some(self.infer_fn_call(name, args, targs, callee.span, span, expected))
            }
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
                let key = match callee.kind.as_ident() {
                    Some(n) => self.labels_certain(n),
                    None => Err(super::globals::KwDeny::NotOneFn),
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
                        let name = callee.kind.as_ident().map(|n| n.as_str());
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
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
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
            | other @ Ty::Var(_) => {
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
    pub(super) fn method_slots(
        &self,
        recv: &Ty,
        method: &str,
    ) -> Option<Vec<crate::desugar::SlotSpec>> {
        match recv {
            Ty::Struct(k, _) => self.struct_shape(k)?.methods.get(method)?.slots.clone(),
            Ty::Enum(k, _) => self.enum_methods_of(k)?.get(method)?.slots.clone(),
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
            | Ty::BuiltinFn { .. }
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
            | Ty::BuiltinFn { .. }
            | Ty::Tuple(_)
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
            | Ty::Unknown
            | Ty::Var(_) => None,
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
            | ExprKind::ElseGuard { .. } => false,
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
        ExprKind::Index { obj: tobj, .. } => tobj.kind.as_ident().cloned(),
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
    /// Check the arguments of the native handle method `owner.method`. When the pair is a row of
    /// [`crate::base::crossing::CROSSING_CLOSURE_METHODS`] its closure argument crosses into another heap
    /// (an `Executor` job, a `Shared` / `RwShared` box closure), exactly like a `spawn` task's:
    ///
    /// 1. a capture floor at the current scope depth surrounds the argument check, so the closure's
    ///    own params and locals are task-local and an outer binding it reads is a capture;
    /// 2. a direct write to a captured local is the D4 error (`COPY_WRITE_TAIL`), for a literal
    ///    closure and for a closure value alike.
    ///
    /// Known ceiling (measured, TICKET-246): a write through a callee is not seen here; it is the
    /// run-time D4 fault.
    pub(super) fn check_crossing_closure_call(
        &mut self,
        owner: &str,
        method: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        if !crate::base::crossing::CROSSING_CLOSURE_METHODS.contains(&(owner, method)) {
            self.check_args_range(method, params, min_params, args, span);
            return;
        }
        self.capture_floors.push(self.scopes.len());
        self.check_args_range(method, params, min_params, args, span);
        self.capture_floors.pop();
        let Some(task) = args.first() else {
            return;
        };
        self.report_named_call_writes(task, &[], true);
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
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => self.value_writes(task),
        };
        writes.sort();
        if let Some(written) = writes.first() {
            self.error(
                task.span,
                format!("'{written}' {}", crate::base::words::COPY_WRITE_TAIL),
            );
        }
    }
}

/// The one type argument of a `Shared[T](..)`-style turbofish, if exactly one was written.
pub(super) fn single_targ(targs: &[Ty]) -> Option<&Ty> {
    match targs {
        [t] => Some(t),
        _ => None,
    }
}
