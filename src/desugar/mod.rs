//! Syntactic desugaring, run inside [`crate::resolver::build_graph`] before the checker: synthesize
//! default providers ([`synthesize_providers`]), fold Python full module paths (`fold_full_path`, DEC-175), and bound
//! `fn` nesting ([`MAX_FN_NESTING`], DEC-109).
//!
//! **This pass does not bind call arguments.** Which declaration a call binds against, and which
//! slot each named, omitted or variadic argument fills, is the checker's decision alone
//! (`Checker::bind_call`, TICKET-182); the compiler lowers from the checker's `CallPlanTable`.
//!
//! **How an omitted argument is materialised (W7-51).** [`dflt_for`] is the one default classifier.
//! A self-contained literal (`= 10`, `= -1`, `= None`, `= []`) is filled from the declaration's own
//! node, compiled in the declaring module. Anything else is compiled ONCE, as a hidden zero-arg `fn`
//! appended to the module that DECLARES the parameter ([`synthesize_providers`]), and an omitting
//! call calls it through `Op::MakeFuncIn`. That is what makes a default resolve — and evaluate — in
//! the definer's namespace (as Python, Ruby and Kotlin all do) instead of the caller's, and what lets
//! default chains compose to any depth. See [`Dflt`] and [`SlotSpec`].

use crate::ast::{
    Block, Chunk, DeferTarget, Expr, ExprKind, Import, MatchExprArm, Module, OptCall, Pattern,
    Span, SpawnTarget, Stmt, StmtKind, Type, TypeParam, WaitArmKind,
};
use crate::base::module_id::ModuleId;
use crate::base::provider_name::{Slot, provider_label, provider_name};
use crate::resolver::{ModuleGraph, ResolveError};
use std::collections::{HashMap, HashSet};

/// How an omitted argument is materialised at a call site.
///
/// The split is the whole of W7-51: a **self-contained literal** is cheap and context-free, so it is
/// still cloned into the caller (`= 10`, `= -1`, `= "hi"`, `= None`, `= []`); **everything else** is
/// compiled ONCE, as a zero-arg function in its defining module, and the caller merely calls it. A
/// provider body therefore resolves — and evaluates — in the DEFINER's namespace (`Obj::Func` carries
/// its `home`), which is what Python, Ruby and Kotlin all do, and what a spliced clone could not do.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Dflt {
    /// Cloned inline at the call site (and re-walked there, so it still spends the depth budget).
    Inline(Expr),
    /// Call the zero-arg provider synthesized in the module named by `module`.
    Provider { name: String },
    /// **Left to the CALLEE.** The default cannot be hoisted into a free top-level provider `fn` — its
    /// type or expression names `Self` on a GENERIC host (`Q[T]`, whose `T` is unbound outside the
    /// signature) or an enclosing type parameter. Rather than clone it into the caller and resolve it
    /// there (the caller-scope hazard this whole design exists to delete), the call site simply omits
    /// the argument: the callee's own prologue fills it from the declaration, in the declaring module,
    /// where `Self` and `T` are both in scope (`crate::base::op::Op::JumpIfProvided`).
    ///
    /// Only expressible as a TRAILING omission — see `Checker::bind_call` for the one shape
    /// that cannot be (a keyword call supplying a LATER parameter), which is refused rather than
    /// silently cloned.
    CalleeFilled,
    /// A STRUCT FIELD default whose declared type mentions the struct's own (unbounded) type
    /// parameters. Unlike `CalleeFilled`, a ctor has no callee to fill it from: `Op::NewStruct`
    /// takes its field count from the call site. So the provider is made GENERIC in the struct's
    /// type parameters (`tps` is their count), and the call site must forward its own turbofish to
    /// it — a ctor call with no turbofish, or a partial one, keeps the field required instead.
    GenericProvider { name: String, tps: usize },
    /// A native decl's literal default, read from the table the build generates from the decl
    /// (`crate::base::builtin_method::NATIVE_DEFAULTS`). The call site pushes the literal.
    Literal(crate::base::builtin_method::DeclDefault),
}

/// Is `e` a **self-contained literal** — an expression that can be cloned into any number of call
/// sites, in any module, and mean exactly the same thing?
///
/// Deliberately an allow-list and deliberately narrower than "resolves to the same value": when in
/// doubt the default becomes a provider, which is always correct and one call slower. In particular
/// a literal with a hole is NOT inline: it is an `Interp` holding arbitrary sub-expressions, and a
/// `Str` is final text with no hole in it. Excluding every `Call`/`Field`/`Ident` is also what keeps W7-49's
/// span-keyed side tables injective: an inline default records no keyword/carrier/witness entry, so
/// two clones of it cannot resolve two ways under one key.
fn is_inline_default(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_) => true,
        // `None` is a keyword, so the caller's clone means the same value in every scope. Do not
        // move it to a provider: a callee-filled `b: T? = None` then breaks every call that fills
        // a later parameter.
        ExprKind::NoneLit => true,
        ExprKind::Str(_) => true,
        ExprKind::Unary { expr, .. } => is_inline_default(expr),
        ExprKind::Binary { lhs, rhs, .. } => is_inline_default(lhs) && is_inline_default(rhs),
        ExprKind::Range { start, end, .. } => [start, end]
            .into_iter()
            .flatten()
            .all(|b| is_inline_default(b)),
        ExprKind::List(xs, _) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().all(is_inline_default)
        }
        ExprKind::Map(ps) => ps
            .iter()
            .all(|(k, v)| is_inline_default(k) && is_inline_default(v)),
        ExprKind::Interp(_)
        | ExprKind::Pass
        | ExprKind::Ident(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Compare { .. }
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

/// Classify one declared default. THE single decision point: [`synthesize_providers`] emits a
/// provider `fn` exactly when this returns [`Dflt::Provider`], and every registry collector calls
/// this to learn the name of the provider that was (or was not) emitted.
///
/// Three shapes keep the historical inline clone even though they are not literals, because a
/// provider is a free top-level `fn` declared `-> <the parameter's type>` and none of them can be
/// spelled as one:
///   * an **un-annotated** parameter — already `parameter 'x' needs a type annotation`;
///   * a *type* mentioning an **enclosing type parameter** (`x: T = mk()`) or **`Self`**
///     (`other: Self = mkq()`) — neither is bound outside the owner's signature. For a type
///     parameter the decl-site check already rejects the shape (`default value for parameter 'x':
///     expected T, found int`); `Self` is the opposite case and is why this carve-out is not just
///     about diagnostics — `other: Self = mkq()`, `other: Self = Q(5)` and `xs: List[Self] = mkl()`
///     are all LEGAL and all ran on `b1307258` (`6`, `6`, `2`), while a provider declared
///     `-> Self` is `unknown type 'Self'` (`docs/reference/syntax/`: `Self` names the receiver type and is
///     not spellable in a free fn's signature). Struct and enum hosts alike.
///   * an *expression* mentioning either (`x: int = mk[T]().n`) — the type is spellable but the body
///     is not: a provider would be checked with `T` unbound and add `unknown type 'T'` plus a
///     witness error on top of the two the shape already gets. Measured on `b1307258`: 2 errors;
///     with a provider: 4; without: 2 again.
///
/// The type-parameter shapes are compile errors today and stay at exactly the errors they already
/// had; the `Self` shapes are working programs and stay working. Both keep the caller-scope
/// resolution an inline clone implies — the same known hazard the pre-TICKET-182 splice fallback
/// documents.
pub(crate) fn dflt_for(
    d: &Expr,
    ty: Option<&Type>,
    type_params: &[String],
    self_ty: Option<&str>,
    name: String,
    field_owner_tps: Option<&[crate::ast::TypeParam]>,
) -> Dflt {
    if is_inline_default(d) {
        return Dflt::Inline(d.clone());
    }
    let Some(ty) = ty else {
        return Dflt::Inline(d.clone());
    };
    // `Self` is an implicit type parameter of every method. On a **non-generic** host it names one
    // concrete type, which a free top-level `fn` CAN spell — so the caller hands us that name and we
    // substitute it into the provider's declared return type, and `Self` is no longer unbound. On a
    // generic host `Self` is `Q[T]`, whose `T` is still unbound in a free fn, so those callers pass
    // `None` and the historical carve-out stands.
    let subst;
    let ty = if self_ty.is_some() {
        subst = subst_self_ty(ty, self_ty);
        &subst
    } else {
        ty
    };
    let mut unbound: Vec<String> = type_params.to_vec();
    if self_ty.is_none() {
        unbound.push("Self".to_string());
    }
    if crate::checker::type_mentions_any(ty, &unbound) {
        if let Some(otps) = field_owner_tps {
            let self_name = "Self".to_string();
            let otp_names: Vec<String> = otps.iter().map(|t| t.name.clone()).collect();
            if !otps.is_empty()
                && otps.iter().all(|t| t.bounds.is_empty())
                && crate::checker::type_mentions_any(ty, &otp_names)
                && !crate::checker::type_mentions_any(ty, std::slice::from_ref(&self_name))
                && !expr_mentions_type_param(d, std::slice::from_ref(&self_name))
            {
                return Dflt::GenericProvider {
                    name,
                    tps: otps.len(),
                };
            }
        }
        return Dflt::CalleeFilled;
    }
    // The EXPRESSION channel keeps `Self` unbound either way: rewriting `Self` inside the provider's
    // BODY (`Self()`, `Self.mk()`) needs a mutating expression walker, which is deliberately not part
    // of this change — such a default keeps the inline carve-out for now.
    let mut expr_unbound: Vec<String> = type_params.to_vec();
    // TICKET-201 (S3): a field default never runs where `Self` is bound, so `Self` there is an
    // unknown name, not a callee-filled binder (a ctor has no callee, DEC-035).
    if field_owner_tps.is_none() {
        expr_unbound.push("Self".to_string());
    }
    if expr_mentions_type_param(d, &expr_unbound) {
        return Dflt::CalleeFilled;
    }
    Dflt::Provider { name }
}

/// Rewrite `Self` to the owner type's name throughout a declared type, so a method's default can be
/// hoisted into a free top-level provider `fn` declared `-> <that type>`. `None` (a free fn, or a
/// GENERIC host whose `Self` is `Q[T]`) clones unchanged. See [`dflt_for`].
fn subst_self_ty(t: &Type, self_ty: Option<&str>) -> Type {
    let Some(owner) = self_ty else {
        return t.clone();
    };
    match t {
        Type::Named { name, span } if name == "Self" => Type::Named {
            name: owner.to_string(),
            span: *span,
        },
        Type::Named { .. } | Type::Nil(_) => t.clone(),
        Type::Qualified { module, name, args } => Type::Qualified {
            module: module.clone(),
            name: name.clone(),
            args: args.iter().map(|a| subst_self_ty(a, self_ty)).collect(),
        },
        Type::Generic(head, args, span) => Type::Generic(
            if head == "Self" {
                owner.to_string()
            } else {
                head.clone()
            },
            args.iter().map(|a| subst_self_ty(a, self_ty)).collect(),
            *span,
        ),
        Type::Func {
            params,
            ret,
            labels,
            variadic,
        } => Type::Func {
            params: params.iter().map(|a| subst_self_ty(a, self_ty)).collect(),
            ret: Box::new(subst_self_ty(ret, self_ty)),
            labels: labels.clone(),
            variadic: *variadic,
        },
        Type::Tuple(ts) => Type::Tuple(ts.iter().map(|a| subst_self_ty(a, self_ty)).collect()),
    }
}

/// Does the default expression `d` mention one of the owner's `type_params` — either as a value
/// identifier (`T.default()`) or inside a turbofish type argument (`mk[T]()`)? See [`dflt_for`].
fn expr_mentions_type_param(d: &Expr, type_params: &[String]) -> bool {
    if type_params.is_empty() {
        return false;
    }
    let hit = std::cell::Cell::new(false);
    walk_idents_and_types(
        d,
        &mut |n| hit.set(hit.get() || type_params.iter().any(|t| t == n)),
        &mut |t| hit.set(hit.get() || crate::checker::type_mentions_any(t, type_params)),
    );
    hit.get()
}

/// The type-parameter names in scope for a signature (`fn f[T](…) where U: …`), used by
/// [`dflt_for`]'s unbound-`T` carve-out. `extra` carries an enclosing struct/enum's own params.
fn tp_names(decl: &crate::ast::FnDecl, extra: &[String]) -> Vec<String> {
    let mut v = extra.to_vec();
    v.extend(decl.type_params.iter().map(|t| t.name.clone()));
    v.extend(decl.where_bounds.iter().map(|t| t.name.clone()));
    v
}

/// One declaration slot a call binds against: a parameter (receiver dropped) or a struct field, in
/// declaration order. `name` is `None` only for an unlabelled slot of a function VALUE's type. Built
/// here, from the declaration, by [`param_slots`] / [`field_slots`], so every default is classified
/// by the one classifier [`dflt_for`] that [`synthesize_providers`] also calls; the checker's
/// `bind_call` is the one reader.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlotSpec {
    pub name: Option<String>,
    pub default: Option<Dflt>,
    /// A variadic parameter (`...xs: T`): it collects the surplus positionals; every later slot is
    /// keyword-only. At most one per declaration; struct fields are never variadic.
    pub is_variadic: bool,
}

/// The one slot builder: a [`SlotSpec`] per parameter of `params`, each default classified by
/// [`dflt_for`]. `owner` is the name [`synthesize_providers`] passed for the same declaration (`f`,
/// or `S.m` for a method), `self_ty` is [`self_ty_for`]'s answer for the host and `tps` are the
/// type parameter names in scope.
pub(crate) fn slots_of(
    params: &[crate::ast::Param],
    file: u32,
    owner: &str,
    self_ty: Option<&str>,
    tps: &[String],
) -> Vec<SlotSpec> {
    params
        .iter()
        .map(|p| SlotSpec {
            name: Some(p.name.clone()),
            default: p.default.as_ref().map(|d| {
                dflt_for(
                    d,
                    p.ty.as_ref(),
                    tps,
                    self_ty,
                    provider_name(file, Slot::Param, owner, &p.name),
                    None,
                )
            }),
            is_variadic: p.is_variadic,
        })
        .collect()
}

/// The [`SlotSpec`]s of `decl`'s explicit parameters (a leading `self` is dropped). `host_tps` are
/// the host's type parameter names (empty for a free fn).
pub(crate) fn param_slots(
    decl: &crate::ast::FnDecl,
    file: u32,
    owner: &str,
    self_ty: Option<&str>,
    host_tps: &[String],
) -> Vec<SlotSpec> {
    let skip = usize::from(decl.takes_self());
    slots_of(
        &decl.params[skip..],
        file,
        owner,
        self_ty,
        &tp_names(decl, host_tps),
    )
}

/// The [`SlotSpec`]s of struct `owner`'s fields, as [`synthesize_providers`] classifies them.
pub(crate) fn field_slots(
    fields: &[crate::ast::Field],
    file: u32,
    owner: &str,
    owner_tps: &[TypeParam],
) -> Vec<SlotSpec> {
    let tps: Vec<String> = owner_tps.iter().map(|t| t.name.clone()).collect();
    fields
        .iter()
        .map(|f| SlotSpec {
            name: Some(f.name.clone()),
            default: f.default.as_ref().map(|d| {
                dflt_for(
                    d,
                    Some(&f.ty),
                    &tps,
                    None,
                    provider_name(file, Slot::Field, owner, &f.name),
                    Some(owner_tps),
                )
            }),
            is_variadic: false,
        })
        .collect()
}

/// Desugar every module in place: synthesize default providers, fold full module paths, lower
/// carriers, bound fn nesting. Errors carry the offending node's span. Call arguments are bound by
/// the checker (`Checker::bind_call`), not here; default legality is the checker's too
/// (`Checker::check_default_scope`, TICKET-201).
pub fn run(graph: &mut ModuleGraph) -> Result<(), ResolveError> {
    // W7-51 — every non-inline default becomes a zero-arg `fn` in the module that DECLARES it.
    synthesize_providers(graph);
    for mi in 0..graph.modules.len() {
        let mut aliases: HashMap<String, ModuleId> = HashMap::new();
        for imp in &graph.modules[mi].imports {
            if let Import::Module { path, alias, .. } = &imp.import {
                let local = alias
                    .clone()
                    .or_else(|| path.last().cloned())
                    .unwrap_or_default();
                if !local.is_empty() {
                    aliases.insert(local, imp.target.clone());
                }
            }
        }
        let module_names =
            module_level_names(&graph.modules[mi].ast.stmts, &graph.modules[mi].imports);
        let ctx = Ctx {
            aliases: &aliases,
            module_names: &module_names,
        };
        let mut walker = Walker {
            ctx,
            scopes: Vec::new(),
            type_params: Vec::new(),
            depth: 0,
            fn_depth: 0,
            builds_range: false,
            counting_iterand: false,
            skipped_fns: graph.fn_skips(&graph.modules[mi]),
            skips_native_bodies: graph.skips_prelude_native_bodies(&graph.modules[mi]),
        };
        let ast: &mut Module = &mut graph.modules[mi].ast;
        walker.walk_block(&mut ast.stmts)?;
        if walker.builds_range && graph.modules[mi].dotted != ["std", "prelude"] {
            graph.prelude_used.mark(crate::resolver::RANGE_STRUCT);
        }
    }
    Ok(())
}

/// Desugar a single standalone module (no imports) in place. Used by the test/standalone runners,
/// which bypass [`build_graph`](crate::resolver::build_graph) and so must apply this pass themselves
/// to stay consistent with the file-backed graph path.
#[cfg(any(test, feature = "test-api"))]
pub fn run_standalone(module: &mut Module) -> Result<(), ResolveError> {
    // Mirror [`run`]: synthesize providers into the single module first. Its `file` id is whatever
    // the test's lexer stamped; there is only one module, so any value is unique by construction.
    let file = module.stmts.first().map_or(0, |s| s.span.file);
    synthesize_providers_into(&mut module.stmts, file);
    let aliases = HashMap::new();
    let module_names = HashSet::new();
    let ctx = Ctx {
        aliases: &aliases,
        module_names: &module_names,
    };
    let mut walker = Walker {
        ctx,
        scopes: Vec::new(),
        type_params: Vec::new(),
        depth: 0,
        fn_depth: 0,
        builds_range: false,
        counting_iterand: false,
        skipped_fns: crate::resolver::SkippedFns::default(),
        skips_native_bodies: false,
    };
    walker.walk_block(&mut module.stmts)?;
    Ok(())
}

/// One synthesized provider: `fn <name>() -> <ret>: return <default>`.
///
/// `ret` is the parameter's **declared** type, never `None` — `None` means *inferred*
/// (`checker::sig`), and inference errors out on a `None`-only / `[]`-only return.
/// `is_test: false` keeps providers out of `chezzi test` discovery. Every span is the default
/// expression's own, so a diagnostic inside the body points at the text the user actually wrote, in
/// the module they wrote it in.
fn provider_fn(
    name: String,
    type_params: Vec<crate::ast::TypeParam>,
    ret: Type,
    mut default: Expr,
) -> Stmt {
    // The declaration keeps its own default; the provider body is a second node.
    crate::ast::renumber_expr(&mut default);
    let span = default.span;
    Stmt {
        kind: StmtKind::Fn(crate::ast::FnDecl {
            name,
            name_span: span,
            type_params,
            where_bounds: Vec::new(),
            params: Vec::new(),
            ret: Some(ret),
            body: vec![Stmt {
                kind: StmtKind::Return(Some(default)),
                span,
            }],
            is_generator: false,
            is_test: false,
            inline_expr_body: false,
            doc: None,
        }),
        span,
    }
}

/// Append the provider `fn`s for one signature's non-inline defaults.
fn push_param_providers(
    out: &mut Vec<Stmt>,
    file: u32,
    owner: &str,
    decl: &crate::ast::FnDecl,
    extra_tps: &[String],
    self_ty: Option<&str>,
) {
    let tps = tp_names(decl, extra_tps);
    for p in &decl.params {
        let Some(d) = &p.default else { continue };
        // `dflt_for` already returns `Inline` when `p.ty` is `None`, so the `expect` is unreachable
        // by construction — a provider always has a declared return type to carry.
        if let Dflt::Provider { name, .. } = dflt_for(
            d,
            p.ty.as_ref(),
            &tps,
            self_ty,
            provider_name(file, Slot::Param, owner, &p.name),
            None,
        ) {
            let ty =
                p.ty.clone()
                    .expect("a provider default has a declared type");
            // The SAME substitution `dflt_for` classified against, so the emitted provider's declared
            // return type and the decision to emit one can never disagree.
            out.push(provider_fn(
                name,
                Vec::new(),
                subst_self_ty(&ty, self_ty),
                d.clone(),
            ));
        }
    }
}

/// The owner type name to substitute for `Self` in a method's provider, or `None` when there is
/// nothing spellable to substitute: a GENERIC host (`Self` is `Q[T]`, and `T` is unbound in the free
/// `fn` a provider is) keeps the historical inline carve-out. See [`dflt_for`].
fn self_ty_for<'a>(owner_type: &'a str, host_type_params: &[String]) -> Option<&'a str> {
    host_type_params.is_empty().then_some(owner_type)
}

/// **W7-51 — compile each non-inline default ONCE, in the module that declares it.**
///
/// For every parameter/field default that [`dflt_for`] classifies as a provider, append a hidden
/// zero-arg `fn` to the DECLARING module's top level whose body returns that default expression. A
/// call site that omits the argument then emits a call to this function instead of a clone of the
/// expression, which fixes two things at once:
///
///   * **scope** — `Obj::Func` carries its `home` module, so the body reads the definer's globals
///     and the definer's imports, not the caller's. Before this, `fn f(x: int = K)` in `g.chz`
///     resolved `K` in whatever module called `g.f()` — an `unknown name` at best and a *silently
///     different value* when the caller happened to declare its own `K`.
///   * **depth** — a nested default (`fn b(y = c())` called from `fn a(x = b())`) is an ordinary
///     call inside an ordinary function body, so chains compose to any depth. Before this the
///     splice happened in the tail of `walk_expr_inner`, after the node's children were walked, and
///     the driver's two passes bounded the chain at depth 2.
///
/// Appended, not inserted: top-level `fn`s are hoisted by both `compiler::collect_globals` and the
/// checker's signature pre-pass, so declaration position is irrelevant.
fn synthesize_providers(graph: &mut ModuleGraph) {
    for m in graph.modules.iter_mut() {
        synthesize_providers_into(&mut m.ast.stmts, m.file);
    }
}

/// [`synthesize_providers`] for one module's top-level statements.
fn synthesize_providers_into(stmts: &mut Vec<Stmt>, file: u32) {
    let mut new_fns: Vec<Stmt> = Vec::new();
    for stmt in stmts.iter() {
        match &stmt.kind {
            StmtKind::Fn(decl) => {
                push_param_providers(&mut new_fns, file, &decl.name, decl, &[], None);
            }
            StmtKind::Struct {
                name,
                type_params,
                fields,
                methods,
                ..
            } => {
                let stps: Vec<String> = type_params.iter().map(|t| t.name.clone()).collect();
                for f in fields {
                    let Some(d) = &f.default else { continue };
                    match dflt_for(
                        d,
                        Some(&f.ty),
                        &stps,
                        None,
                        provider_name(file, Slot::Field, name, &f.name),
                        Some(type_params),
                    ) {
                        Dflt::Provider { name: pn, .. } => {
                            new_fns.push(provider_fn(pn, Vec::new(), f.ty.clone(), d.clone()));
                        }
                        Dflt::GenericProvider { name: pn, .. } => {
                            new_fns.push(provider_fn(
                                pn,
                                type_params.clone(),
                                f.ty.clone(),
                                d.clone(),
                            ));
                        }
                        Dflt::Inline(_) | Dflt::CalleeFilled | Dflt::Literal(_) => {}
                    }
                }
                for (i, mth) in methods.iter().enumerate() {
                    let nth = methods[..i].iter().filter(|m| m.name == mth.name).count();
                    let owner = crate::base::provider_name::method_owner(name, &mth.name, nth);
                    push_param_providers(
                        &mut new_fns,
                        file,
                        &owner,
                        mth,
                        &stps,
                        self_ty_for(name, &stps),
                    );
                }
            }
            StmtKind::Enum {
                name,
                type_params,
                methods,
                ..
            } => {
                let stps: Vec<String> = type_params.iter().map(|t| t.name.clone()).collect();
                for (i, mth) in methods.iter().enumerate() {
                    let nth = methods[..i].iter().filter(|m| m.name == mth.name).count();
                    let owner = crate::base::provider_name::method_owner(name, &mth.name, nth);
                    push_param_providers(
                        &mut new_fns,
                        file,
                        &owner,
                        mth,
                        &stps,
                        self_ty_for(name, &stps),
                    );
                }
            }
            StmtKind::NativeStruct {
                name,
                type_params,
                bodied_methods,
                ..
            } => {
                let stps: Vec<String> = type_params.iter().map(|t| t.name.clone()).collect();
                for (i, mth) in bodied_methods.iter().enumerate() {
                    let nth = bodied_methods[..i]
                        .iter()
                        .filter(|m| m.name == mth.name)
                        .count();
                    let owner = crate::base::provider_name::method_owner(name, &mth.name, nth);
                    push_param_providers(
                        &mut new_fns,
                        file,
                        &owner,
                        mth,
                        &stps,
                        self_ty_for(name, &stps),
                    );
                }
            }
            StmtKind::Let { .. }
            | StmtKind::Assign { .. }
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
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. }
            | StmtKind::Assert { .. }
            | StmtKind::Expr(_) => {}
        }
    }
    stmts.extend(new_fns);
}

/// **Provider cycle check** — `fn f(x: int = f())` used to silently expand to a three-deep
/// `f(f(f()))` (the two-pass driver's fixed point), which the checker then rejected as an arity
/// cascade (2 × `'f' expects 1 argument(s), got 0`) rather than as the cycle it is; under providers
/// the expansion would instead be unbounded runtime recursion. Every provider body is scanned AFTER normalization, so a provider→provider edge is
/// literally a `$def$…` identifier in the body; a back edge among those is a compile error.
/// Cross-module edges cannot close a cycle (the splice only reaches a module in the caller's own
/// transitive import closure, and imports are acyclic), but the DFS spans the graph anyway rather
/// than relying on that.
///
/// **What this scan does NOT catch, deliberately:** a cycle that leaves the provider graph. It walks
/// provider→provider edges only, so `fn f(x: int = helper())` with `fn helper() -> int: return f()`
/// passes it and recurses at RUNTIME instead — a clean `maximum call depth (10000) exceeded`, rc 1,
/// the same shape as CPython's `RecursionError`. Following ordinary
/// call edges too would mean deciding recursion over the whole program's call graph, which is a
/// confident-wrong-answer risk the project declines to take (`docs/contributing/gaps.md` W7-12); the runtime
/// fault is the documented, accepted outcome (`docs/reference/syntax/` §5).
pub(crate) fn check_provider_cycles_in(
    edges: &HashMap<String, (Vec<String>, Span)>,
) -> Result<(), ResolveError> {
    // Iterative DFS with an explicit on-stack set: `state` is 1 = in progress, 2 = done.
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut order: Vec<&String> = edges.keys().collect();
    order.sort();
    for root in order {
        if state.get(root.as_str()).copied() == Some(2) {
            continue;
        }
        state.insert(root.as_str(), 1);
        let mut stack: Vec<(&str, usize)> = vec![(root.as_str(), 0)];
        while let Some((node, i)) = stack.pop() {
            let Some((outs, span)) = edges.get(node) else {
                state.insert(node, 2);
                continue;
            };
            if i >= outs.len() {
                state.insert(node, 2);
                continue;
            }
            stack.push((node, i + 1));
            let next = outs[i].as_str();
            match state.get(next).copied() {
                Some(1) => {
                    return Err(err(
                        *span,
                        format!(
                            "the default for {} is cyclic: evaluating it requires evaluating the default for {} again",
                            provider_label(node),
                            provider_label(next)
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    state.insert(next, 1);
                    stack.push((next, 0));
                }
            }
        }
    }
    Ok(())
}

/// Visit every identifier reference in an expression (a `Field`/`OptChain` member name is the member,
/// not a reference, so only the receiver is visited), plus every **type** it spells: a turbofish's
/// arguments (`mk[T]()`, `obj?.m[T]()`), a type-application head's, a `decode[T](…)` target, and a
/// closure's parameter and return annotations.
///
/// The `decode`/closure arms were added after the rest: without them `dflt_for`'s unbound-`T`
/// carve-out missed both shapes and gave them a provider whose body spells a type parameter that is
/// out of scope there, which is exactly the cascade the carve-out exists to prevent. Measured on
/// `fn g[T](x: int = json.decode[T](src()).is_ok().to_int())`: `b1307258` 1 error, before this arm
/// **3**, after it 1 again; on `fn h[T](x: int = apply(fn(a: T) -> int: 0))`: 1, **2**, 1. Both
/// shapes were, and stay, rejected — the arms buy the diagnostic, not the verdict.
fn walk_idents_and_types(e: &Expr, f: &mut impl FnMut(&str), tf: &mut impl FnMut(&Type)) {
    match &e.kind {
        ExprKind::Ident(n) => f(n),
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass => {}
        // A fragment identifier IS a reference (`"{a}"` reads `a`), so descend.
        ExprKind::Interp(chunks) => chunks.iter().for_each(|c| {
            if let Chunk::Expr(e, _, fields) = c {
                walk_idents_and_types(e, f, tf);
                fields.iter().for_each(|x| walk_idents_and_types(x, f, tf));
            }
        }),
        ExprKind::List(xs, _) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().for_each(|x| walk_idents_and_types(x, f, tf))
        }
        ExprKind::Map(ps) => ps.iter().for_each(|(k, v)| {
            walk_idents_and_types(k, f, tf);
            walk_idents_and_types(v, f, tf);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            if let Some(k) = key {
                walk_idents_and_types(k, f, tf);
            }
            walk_idents_and_types(elem, f, tf);
            for clause in clauses {
                walk_idents_and_types(&clause.iter, f, tf);
                for g in &clause.guards {
                    walk_idents_and_types(g, f, tf);
                }
            }
        }
        ExprKind::Unary { expr, .. } => walk_idents_and_types(expr, f, tf),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_idents_and_types(lhs, f, tf);
            walk_idents_and_types(rhs, f, tf);
        }
        ExprKind::Compare { operands, .. } => {
            for o in operands {
                walk_idents_and_types(o, f, tf);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(start) = start {
                walk_idents_and_types(start, f, tf);
            }
            if let Some(end) = end {
                walk_idents_and_types(end, f, tf);
            }
        }
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
            bracket,
        } => {
            walk_idents_and_types(callee, f, tf);
            args.iter().for_each(|a| walk_idents_and_types(a, f, tf));
            if let Some(b) = bracket {
                walk_idents_and_types(b, f, tf);
            }
            named
                .iter()
                .for_each(|(_, a)| walk_idents_and_types(a, f, tf));
            type_args.iter().for_each(&mut *tf);
        }
        ExprKind::Field { obj, .. } => walk_idents_and_types(obj, f, tf),
        // The bracket's head and index reading are expressions; its type reading holds `Type`s.
        ExprKind::Index { obj, index, types } => {
            walk_idents_and_types(obj, f, tf);
            if let Some(i) = index {
                walk_idents_and_types(i, f, tf);
            }
            types.iter().for_each(tf);
        }
        ExprKind::Try(x) => walk_idents_and_types(x, f, tf),
        ExprKind::OptChain { obj, call, .. } => {
            walk_idents_and_types(obj, f, tf);
            if let Some(c) = call {
                c.args.iter().for_each(|a| walk_idents_and_types(a, f, tf));
                c.bracket
                    .iter()
                    .for_each(|b| walk_idents_and_types(b, f, tf));
                c.named
                    .iter()
                    .for_each(|(_, a)| walk_idents_and_types(a, f, tf));
                c.type_args.iter().for_each(&mut *tf);
            }
        }
        ExprKind::NullCoalesce { lhs, rhs, .. } => {
            walk_idents_and_types(lhs, f, tf);
            walk_idents_and_types(rhs, f, tf);
        }
        // A closure's parameter NAMES are bindings, not references, so only their annotations and
        // the return annotation go down the type channel; `f` is untouched.
        ExprKind::Closure { params, ret, body } => {
            for p in params {
                if let Some(t) = &p.ty {
                    tf(t);
                }
            }
            if let Some(t) = ret {
                tf(t);
            }
            walk_idents_and_types(body, f, tf);
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_idents_and_types(scrutinee, f, tf);
            arms.iter().for_each(|a| {
                if let Some(g) = &a.guard {
                    walk_idents_and_types(g, f, tf);
                }
                walk_idents_and_types(&a.body, f, tf);
            });
        }
        ExprKind::IfElse { cond, then, els } => {
            walk_idents_and_types(cond, f, tf);
            walk_idents_and_types(then, f, tf);
            walk_idents_and_types(els, f, tf);
        }
        // A `recover:` block is never a realistic default expression; its block statements are not
        // walked (conservative under-detection only for this absurd case).
        ExprKind::Recover(..) => {}
        // The parser builds a guard only as a let's value, never inside a default expression.
        ExprKind::ElseGuard { value, .. } => walk_idents_and_types(value, f, tf),
    }
}

/// Per-module context for the full-path fold (all borrows outlive the mutable AST walk).
struct Ctx<'a> {
    /// Whole-module imports: bound name → target module.
    aliases: &'a HashMap<String, ModuleId>,
    /// This module's top-level names that hide a full module path's head (see
    /// [`module_level_names`]). Precomputed, never read from walk position: a hoisted decl binds
    /// before any statement runs.
    module_names: &'a HashSet<String>,
}

struct Walker<'a> {
    ctx: Ctx<'a>,
    scopes: Vec<HashSet<String>>,
    /// Per-scope type-parameter names, parallel to `scopes`; a type parameter shadows a module name.
    type_params: Vec<HashSet<String>>,
    /// Current [`Walker::walk_expr`] recursion depth — see that method. This counter is what turns
    /// [`crate::parser::MAX_AST_DEPTH`] into a **global** bound instead of a per-`Parser` one.
    depth: usize,
    /// TICKET-109 — how many `fn` bodies (a top-level or nested `fn`, a `test fn`, or a struct, enum
    /// or native-struct method) enclose the statement being walked. See [`MAX_FN_NESTING`].
    fn_depth: usize,
    /// TICKET-255 — true once a range literal that builds a `Range` value was walked (a counting-loop
    /// iterand builds none). Feeds [`ModuleGraph::prelude_used`].
    builds_range: bool,
    /// Set by the `For` arm just before it walks its iterand: that iterand is a counting loop
    /// ([`crate::ast::counting_range`]), so the `Range` arm must not record a use.
    counting_iterand: bool,
    /// TICKET-265 — this module's [`ModuleGraph::fn_skips`]. The checker and the compiler skip those
    /// bodies, so a range literal inside one records no `Range` use.
    skipped_fns: crate::resolver::SkippedFns,
    /// [`crate::resolver::ModuleGraph::skips_prelude_native_bodies`] for this module: the checker
    /// and the compiler skip those bodies, so the walk does too.
    skips_native_bodies: bool,
}

/// TICKET-109 / W12-12 — how deep `fn` declarations may nest. A top-level `fn`, a `test fn` or a
/// method is level 1; each `fn` declared in its body adds one. The bound is CPython's INDENTATION
/// bound, not a performance bound: `compile()` on a 100-deep `def` chain raises `IndentationError: too
/// many levels of indentation` (measured on CPython 3.14.7), so 100 is the ancestor's limit and
/// strictly more permissive than it. TICKET-157 memoized the checker's nested-fn return inference
/// (`Checker::ret_memo`), which had walked the body `2^(N+2) - 4` times and forced the earlier cap of
/// 16. The memoized walk is still superlinear at absurd depth (the per-`diag_mark` clone grows with
/// depth), so the constant stays; lowering it to a performance bound needs a new measurement.
pub const MAX_FN_NESTING: usize = 100;

impl Walker<'_> {
    /// Enter one `fn` body (TICKET-109): count it, and reject it past [`MAX_FN_NESTING`] at its name.
    /// Every caller decrements `fn_depth` after walking the body. An `Err` aborts the whole walk, so
    /// that path needs no decrement.
    fn enter_fn(&mut self, name: &str, name_span: crate::lexer::Span) -> Result<(), ResolveError> {
        self.fn_depth += 1;
        if self.fn_depth > MAX_FN_NESTING {
            return Err(err(
                name_span,
                format!(
                    "fn '{name}' is nested {} deep; fn declarations nest at most {MAX_FN_NESTING} deep (declare it at an outer level)",
                    self.fn_depth
                ),
            ));
        }
        Ok(())
    }

    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    /// Fold a Python full module path (TICKET-175): in `pkg.deep.Point.zero`, the LONGEST prefix
    /// whose dot-join is an imported module's bound name (`pkg.deep`, the resolver's synthetic
    /// full-path bind) becomes one `Ident("pkg.deep")`, so checker and compiler see the same
    /// `module.member` shape as `deep.Point.zero`. Runs at the OUTERMOST `Field` of a chain first
    /// (the caller walks parents before children), so the longest prefix wins. A head that a local,
    /// a parameter, a type parameter, or any module-level name shadows is left alone.
    fn fold_full_path(&self, expr: &mut Expr) {
        let mut segs: Vec<String> = Vec::new();
        let mut cur = &*expr;
        while let ExprKind::Field { obj, name, .. } = &cur.kind {
            segs.push(name.clone());
            cur = obj;
        }
        let ExprKind::Ident(head) = &cur.kind else {
            return;
        };
        if segs.is_empty()
            || head.contains('.')
            || self.is_local(head)
            || self.is_type_param(head)
            || self.ctx.module_names.contains(head)
        {
            return;
        }
        let head_span = cur.span;
        segs.push(head.clone());
        segs.reverse();
        // `segs` is now `[head, s1, s2, …]`; find the longest `head.s1…sk` that is a bound name.
        let Some(k) = (2..=segs.len())
            .rev()
            .find(|&k| self.ctx.aliases.contains_key(&segs[..k].join(".")))
        else {
            return;
        };
        let joined = segs[..k].join(".");
        let mut node = expr;
        for _ in 0..segs.len() - k {
            let ExprKind::Field { obj, .. } = &mut node.kind else {
                return;
            };
            node = obj;
        }
        *node = ident_expr(&joined, head_span);
    }

    fn bind(&mut self, name: &str) {
        if let Some(top) = self.scopes.last_mut() {
            top.insert(name.to_string());
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashSet::new());
        self.type_params.push(HashSet::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.type_params.pop();
    }

    /// Whether `name` is a type-parameter name in scope. A type parameter shadows a struct name.
    fn is_type_param(&self, name: &str) -> bool {
        self.type_params.iter().any(|s| s.contains(name))
    }

    /// Walk a block in its own lexical scope (sequential `let`s bind into this scope).
    fn walk_block(&mut self, stmts: &mut Block) -> Result<(), ResolveError> {
        self.push_scope();
        for stmt in stmts.iter_mut() {
            self.walk_stmt(stmt)?;
        }
        self.pop_scope();
        Ok(())
    }

    fn walk_stmt(&mut self, stmt: &mut Stmt) -> Result<(), ResolveError> {
        match &mut stmt.kind {
            StmtKind::Let {
                target,
                ty: _,
                value,
                // `const` is not lowered here (compile-time-only; the checker enforces it) — ignore.
                is_const: _,
                doc: _,
            } => {
                self.walk_expr(value)?;
                for (n, _) in target.binders() {
                    self.bind(n);
                }
            }
            StmtKind::Assign { target, value, op: _ } => {
                self.walk_expr(target)?;
                self.walk_expr(value)?;
            }
            StmtKind::Fn(decl) => {
                // The DECL-SITE copy of each default is normalized here, outside the param scope
                // (no param is bound where a default runs; the checker owns default legality,
                // `Checker::check_default_scope`). This copy is what the checker type-checks against the param's
                // declared type, and what `compile_suite_new_thunk` compiles for a test suite's
                // fields; the provider carries an independent copy of the same expression.
                for p in decl.params.iter_mut() {
                    if let Some(d) = &mut p.default {
                        self.walk_expr(d)?;
                    }
                }
                // Nested/top-level function body: params are a fresh scope.
                self.enter_fn(&decl.name, decl.name_span)?;
                self.push_scope();
                self.type_params
                    .last_mut()
                    .unwrap()
                    .extend(decl.type_params.iter().map(|t| t.name.clone()));
                for p in &decl.params {
                    self.bind(&p.name);
                }
                let skipped = self.fn_depth == 1
                    && self.skipped_fns.skips(&decl.name);
                let builds_range = self.builds_range;
                self.walk_block(&mut decl.body)?;
                if skipped {
                    self.builds_range = builds_range;
                }
                self.pop_scope();
                self.fn_depth -= 1;
            }
            StmtKind::Struct {
                type_params,
                fields,
                methods,
                ..
            } => {
                // Field defaults: normalize the decl-site copy like param defaults (outside any
                // scope; no field is bound where a default runs, and the checker owns default
                // legality).
                for f in fields.iter_mut() {
                    if let Some(d) = &mut f.default {
                        self.walk_expr(d)?;
                    }
                }
                for m in methods.iter_mut() {
                    for p in m.params.iter_mut() {
                        if let Some(d) = &mut p.default {
                            self.walk_expr(d)?;
                        }
                    }
                    self.enter_fn(&m.name, m.name_span)?;
                    self.push_scope();
                    self.type_params.last_mut().unwrap().extend(
                        type_params
                            .iter()
                            .chain(m.type_params.iter())
                            .map(|t| t.name.clone()),
                    );
                    for p in &m.params {
                        self.bind(&p.name);
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
                    self.fn_depth -= 1;
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches.iter_mut() {
                    self.walk_expr(cond)?;
                    self.walk_block(body)?;
                }
                if let Some(b) = else_block {
                    self.walk_block(b)?;
                }
            }
            StmtKind::For { target, iter, body } => {
                self.counting_iterand = crate::ast::counting_range(target, iter).is_some();
                self.walk_expr(iter)?;
                self.push_scope();
                for (v, _) in target.binders() {
                    self.bind(v);
                }
                for s in body.iter_mut() {
                    self.walk_stmt(s)?;
                }
                self.pop_scope();
            }
            StmtKind::While { cond, body } => {
                self.walk_expr(cond)?;
                self.walk_block(body)?;
            }
            StmtKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee)?;
                for arm in arms.iter_mut() {
                    self.push_scope();
                    bind_pattern(&arm.pattern, &mut |n| {
                        if let Some(top) = self.scopes.last_mut() {
                            top.insert(n);
                        }
                    });
                    if let Some(g) = &mut arm.guard {
                        self.walk_expr(g)?;
                    }
                    for s in arm.body.iter_mut() {
                        self.walk_stmt(s)?;
                    }
                    self.pop_scope();
                }
            }
            StmtKind::Return(Some(e)) => self.walk_expr(e)?,
            StmtKind::Yield(e) => self.walk_expr(e)?,
            StmtKind::Defer(target) => match target {
                DeferTarget::Call(e) => self.walk_expr(e)?,
                DeferTarget::Block(body) => self.walk_block(body)?,
            },
            StmtKind::Expr(e) => self.walk_expr(e)?,
            StmtKind::Assert { cond, msg } => {
                self.walk_expr(cond)?;
                if let Some(m) = msg {
                    self.walk_expr(m)?;
                }
            }
            StmtKind::Parallel { body } => self.walk_block(body)?,
            StmtKind::Spawn(target) => match target {
                SpawnTarget::Call(e) => self.walk_expr(e)?,
                SpawnTarget::Block(body) => self.walk_block(body)?,
            },
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    // The arm body is its own scope; a `v := ch.recv()` bind lives in it.
                    self.push_scope();
                    // The guard runs before the arm binds: it cannot see the arm's binding.
                    if let Some(g) = &mut arm.guard {
                        self.walk_expr(g)?;
                    }
                    match &mut arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            self.walk_expr(chan)?;
                            for (name, _) in target.binders() {
                                self.bind(name);
                            }
                        }
                        WaitArmKind::Send { call } => self.walk_expr(call)?,
                    }
                    for s in arm.body.iter_mut() {
                        self.walk_stmt(s)?;
                    }
                    self.pop_scope();
                }
                if let Some(b) = else_block {
                    self.walk_block(b)?;
                }
            }
            // Enum method bodies (and param defaults) are rewritten exactly like a struct's; an enum
            // has no fields to splice.
            StmtKind::Enum {
                type_params,
                methods,
                ..
            } => {
                for m in methods.iter_mut() {
                    for p in m.params.iter_mut() {
                        if let Some(d) = &mut p.default {
                            self.walk_expr(d)?;
                        }
                    }
                    self.enter_fn(&m.name, m.name_span)?;
                    self.push_scope();
                    self.type_params.last_mut().unwrap().extend(
                        type_params
                            .iter()
                            .chain(m.type_params.iter())
                            .map(|t| t.name.clone()),
                    );
                    for p in &m.params {
                        self.bind(&p.name);
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
                    self.fn_depth -= 1;
                }
            }
            // A `native struct`'s BODIED Chezzi methods ARE compiled to bytecode, so their bodies +
            // param defaults must be desugared exactly like an enum/struct method (default/named-arg
            // normalization, `ref` lowering). The bodyless `native fn` sigs alongside them have nothing.
            StmtKind::NativeStruct {
                type_params,
                bodied_methods,
                ..
            } => {
                let skip = self.skips_native_bodies;
                for m in bodied_methods.iter_mut().filter(|_| !skip) {
                    for p in m.params.iter_mut() {
                        if let Some(d) = &mut p.default {
                            self.walk_expr(d)?;
                        }
                    }
                    self.enter_fn(&m.name, m.name_span)?;
                    self.push_scope();
                    self.type_params.last_mut().unwrap().extend(
                        type_params
                            .iter()
                            .chain(m.type_params.iter())
                            .map(|t| t.name.clone()),
                    );
                    for p in &m.params {
                        self.bind(&p.name);
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
                    self.fn_depth -= 1;
                }
            }
            // No nested expressions / bindings to rewrite.
            StmtKind::Return(None)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Pass
            | StmtKind::Import(_)
            | StmtKind::Protocol { .. }
            | StmtKind::Extern { .. }
            // A `native fn`/`native ctor` decl is a body-less signature — no nested exprs/bindings.
            | StmtKind::Native(_)
            // A `native enum` decl carries only body-less variants/method sigs — nothing to desugar.
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. }
            | StmtKind::TypeAlias { .. } => {}
        }
        Ok(())
    }

    /// **THE GLOBAL AST-DEPTH BOUND** (W7-50). [`crate::parser::MAX_AST_DEPTH`] bounds the tree
    /// the recursive front-end walkers descend. A `{…}` hole is parsed by the SAME `Parser` as the
    /// code around it (`Parser::parse_hole`), so its `depth` and `fold_depth` carry on and the
    /// budgets cannot compose: `"{ <15 985 deep> }".len()` nested three times is refused at the
    /// parser, as one expression is. (Until TICKET-252 a hole was re-lexed and re-parsed by a
    /// second `Parser` that started at zero, and each level bought a fresh 16 000.) This counter
    /// stays as the backstop on the tree the checker and compiler descend: every front-end path
    /// routes through here, because `resolver::build_graph` ends in [`run`].
    ///
    /// Statement nesting cannot compose (a `{…}` hole holds an expression, never a block) and is
    /// bounded by `parser::MAX_DEPTH`.
    fn walk_expr(&mut self, expr: &mut Expr) -> Result<(), ResolveError> {
        if self.depth >= crate::parser::MAX_AST_DEPTH {
            return Err(err(
                expr.span,
                format!(
                    "expression nested too deeply (limit {}); this counts the whole expression \
                     after desugaring, and an interpolated `{{…}}` fragment or a spliced default \
                     argument nests INSIDE the expression around it and spends the same budget",
                    crate::parser::MAX_AST_DEPTH
                ),
            ));
        }
        self.depth += 1;
        let r = self.walk_expr_inner(expr);
        self.depth -= 1;
        r
    }

    fn walk_expr_inner(&mut self, expr: &mut Expr) -> Result<(), ResolveError> {
        self.fold_full_path(expr);
        // Recurse into children first, so nested calls are normalized regardless of this node.
        match &mut expr.kind {
            ExprKind::Unary { expr: inner, .. } => self.walk_expr(inner)?,
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs)?;
                self.walk_expr(rhs)?;
            }
            ExprKind::Compare { operands, .. } => {
                for o in operands {
                    self.walk_expr(o)?;
                }
            }
            ExprKind::Range { start, end, .. } => {
                if !std::mem::take(&mut self.counting_iterand) {
                    self.builds_range = true;
                }
                if let Some(start) = start {
                    self.walk_expr(start)?;
                }
                if let Some(end) = end {
                    self.walk_expr(end)?;
                }
            }
            ExprKind::List(xs, _) | ExprKind::Set(xs) | ExprKind::Tuple(xs) => {
                for x in xs.iter_mut() {
                    self.walk_expr(x)?;
                }
            }
            ExprKind::Map(pairs) => {
                for (k, v) in pairs.iter_mut() {
                    self.walk_expr(k)?;
                    self.walk_expr(v)?;
                }
            }
            ExprKind::Field { obj, .. } => self.walk_expr(obj)?,
            ExprKind::Index { obj, index, .. } => {
                self.walk_expr(obj)?;
                if let Some(index) = index {
                    self.walk_expr(index)?;
                }
            }
            ExprKind::Try(inner) => self.walk_expr(inner)?,
            ExprKind::Closure { params, body, .. } => {
                self.push_scope();
                for p in params.iter() {
                    self.bind(&p.name);
                }
                self.walk_expr(body)?;
                self.pop_scope();
            }
            ExprKind::Match { scrutinee, arms } => {
                self.walk_expr(scrutinee)?;
                for arm in arms.iter_mut() {
                    self.push_scope();
                    bind_pattern(&arm.pattern, &mut |n| {
                        if let Some(top) = self.scopes.last_mut() {
                            top.insert(n);
                        }
                    });
                    if let Some(g) = &mut arm.guard {
                        self.walk_expr(g)?;
                    }
                    self.walk_expr(&mut arm.body)?;
                    self.pop_scope();
                }
            }
            ExprKind::IfElse { cond, then, els } => {
                self.walk_expr(cond)?;
                self.walk_expr(then)?;
                self.walk_expr(els)?;
            }
            ExprKind::Recover(block, _) => self.walk_block(block)?,
            ExprKind::ElseGuard { value, body, .. } => {
                self.walk_expr(value)?;
                self.walk_block(body)?;
            }
            ExprKind::Comprehension {
                key, elem, clauses, ..
            } => {
                // Clauses nest (first outermost): each clause's `iter` is walked in the scope of the
                // earlier clauses' vars, then that clause's vars are bound for everything after it
                // (later clauses' iters/guards, this clause's guards, and the key/element). One
                // cumulative scope per clause; pop them all at the end.
                for clause in clauses.iter_mut() {
                    self.walk_expr(&mut clause.iter)?;
                    self.push_scope();
                    for (v, _) in clause.target.binders() {
                        self.bind(v);
                    }
                    for g in clause.guards.iter_mut() {
                        self.walk_expr(g)?;
                    }
                }
                if let Some(k) = key {
                    self.walk_expr(k)?;
                }
                self.walk_expr(elem)?;
                for _ in clauses.iter() {
                    self.pop_scope();
                }
            }
            ExprKind::Call {
                callee,
                args,
                named,
                bracket,
                ..
            } => {
                self.walk_expr(callee)?;
                for a in args.iter_mut() {
                    self.walk_expr(a)?;
                }
                if let Some(b) = bracket {
                    self.walk_expr(b)?;
                }
                for (_, v) in named.iter_mut() {
                    self.walk_expr(v)?;
                }
            }
            // W7-43 — optional chaining `?.` / null-coalescing `??` SURVIVE this pass: the choice
            // between the Option lowering and the Result (`?` then `.`) one needs the operand's
            // TYPE, which only the checker has. Walk the children like any other node, then
            // normalize the `?.` call part explicitly (the checker binds the `?.` call part — the carrier no
            // longer becomes a `Call` here, so `walk_expr`'s tail can't do it).
            ExprKind::NullCoalesce { lhs, rhs, .. } => {
                self.walk_expr(lhs)?;
                self.walk_expr(rhs)?;
            }
            ExprKind::OptChain { obj, call, .. } => {
                self.walk_expr(obj)?;
                if let Some(c) = call {
                    for a in c.args.iter_mut() {
                        self.walk_expr(a)?;
                    }
                    if let Some(b) = c.bracket.as_mut() {
                        self.walk_expr(b)?;
                    }
                    for (_, v) in c.named.iter_mut() {
                        self.walk_expr(v)?;
                    }
                }
            }
            ExprKind::Ident(_) => {}
            ExprKind::Interp(chunks) => {
                // A fragment is an ordinary child: the lexer gave its tokens their real source
                // positions, so its own span is the one the checker and compiler report too. The
                // parser built the chunks, so calls in a fragment are normalized like any other.
                for c in chunks.iter_mut() {
                    if let crate::ast::Chunk::Expr(e, _, fields) = c {
                        self.walk_expr(e)?;
                        for f in fields.iter_mut() {
                            self.walk_expr(f)?;
                        }
                    }
                }
            }
            // Leaves.
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass => {}
        }
        Ok(())
    }
}

/// Collect the binding names introduced by a `match` pattern.
fn bind_pattern(pat: &Pattern, f: &mut impl FnMut(String)) {
    match pat {
        Pattern::Ident(n, _, _) => f(n.clone()),
        Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) | Pattern::Or(bindings) => {
            for b in bindings {
                bind_pattern(b, f);
            }
        }
        Pattern::Carrier { inner, .. } => bind_pattern(inner, f),
        Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
    }
}

fn err(span: crate::lexer::Span, message: String) -> ResolveError {
    ResolveError {
        message,
        span,
        module: None,
        // No `Builder`/graph in scope here to attribute a path — `build_graph_impl` fills this in
        // from the graph it already has, by scanning for `span.file`, if still `None` when this
        // propagates out of `desugar::run`.
        path: None,
    }
}

/// A nullary-or-payload variant pattern (`Some(__c)` / `None`) for desugared opt-chain `match` arms.
fn variant_pat(id: crate::ast::NodeId, name: &str, bindings: Vec<Pattern>) -> Pattern {
    Pattern::Variant {
        id,
        name: name.to_string(),
        bindings,
        enum_name: None,
        module_name: None,
    }
}

/// Lower an `OptChain` / `NullCoalesce` carrier (in place) to an expression-position `match` —
/// the **Option** lowering:
///   `a ?? b`     → `match a: Some(__optN): __optN; None: b`
///   `x?.field`   → `match x: Some(__optN): Some(__optN.field); None: None`
///   `x?.m(args)` → `match x: Some(__optN): Some(__optN.m(args)); None: None`
/// The scrutinee is evaluated once by `match`; the payload binds to `__opt{tmp}` (the caller owns the
/// counter, so temps stay unique within one expression). The arm bodies and field/method access use
/// only nodes the checker + the compiler already handle.
///
/// Ctx-free and free-standing on purpose: every consumer that needs this lowering must call THIS
/// function, so the synthesized spans (and therefore the `WitnessKey`s derived from
/// them) cannot drift between consumers.
pub fn lower_carrier_option(expr: &mut Expr, tmp: usize) {
    lower_carrier_option_as(expr, tmp, false)
}

/// TICKET-239 — the `?.` lowering for a CALL that returns nothing (`CarrierMode::OptionVoid`):
///   `x?.m(args)` → `match x: Some(__optN): __optN.m(args); None: pass`
/// The call runs when the value is present and the expression has no value. Same builder as
/// [`lower_carrier_option`], so both forms give every shared node the same id.
pub fn lower_carrier_option_void(expr: &mut Expr, tmp: usize) {
    lower_carrier_option_as(expr, tmp, true)
}

/// The one builder behind [`lower_carrier_option`] and [`lower_carrier_option_void`]. Both forms
/// draw node ids in the SAME order: the checker infers the void form speculatively and then, on a
/// miss, the value form of the same carrier, and the resolve table is not rolled back between.
fn lower_carrier_option_as(expr: &mut Expr, tmp: usize, void: bool) {
    let span = expr.span;
    let (base, k) = (expr.id, std::cell::Cell::new(0));
    let nid = || {
        k.set(k.get() + 1);
        base.carrier_child(k.get())
    };
    let c = crate::ast::CarrierTemp::Payload.name(tmp);
    let kind = std::mem::replace(&mut expr.kind, ExprKind::Bool(false));
    expr.kind = match kind {
        ExprKind::NullCoalesce { lhs, rhs, .. } => {
            // Synthesized arms take the scrutinee span: a diagnostic on one stays where it was.
            let arm_span = lhs.span;
            ExprKind::Match {
                scrutinee: lhs,
                arms: vec![
                    MatchExprArm {
                        span: arm_span,
                        pattern: variant_pat(
                            nid(),
                            &crate::ast::private_name("Some"),
                            vec![Pattern::Ident(c.clone(), Span::default(), nid())],
                        ),
                        guard: None,
                        body: ident_expr_at(nid(), &c, span),
                    },
                    MatchExprArm {
                        span: arm_span,
                        pattern: variant_pat(nid(), crate::lexer::NONE, vec![]),
                        guard: None,
                        body: *rhs,
                    },
                ],
            }
        }
        ExprKind::OptChain {
            obj,
            name,
            name_span,
            call,
        } => {
            // The synthesized callee `Field` takes the carrier's REAL `name_span`, not `span`:
            // `span` is the primary's span, shared by every link of a chain, so two synthesized
            // method callees in one chain would collide on a single `WitnessKey`.
            let field = Expr {
                id: nid(),
                kind: ExprKind::Field {
                    obj: Box::new(ident_expr_at(nid(), &c, span)),
                    name,
                    name_span,
                },
                span,
            };
            // `__optN.field` or `__optN.method(args)`, then wrapped in `Some(...)`.
            let access = match call {
                None => field,
                Some(call) => {
                    let id = nid();
                    Expr {
                        id,
                        kind: opt_call_kind(field, call, span, nid),
                        span,
                    }
                }
            };
            // Drawn before the branch, so the void form skips no id.
            let (wrap_id, some_id) = (nid(), nid());
            let some_body = if void {
                access
            } else {
                Expr {
                    id: wrap_id,
                    kind: ExprKind::Call {
                        callee: Box::new(ident_expr_at(
                            some_id,
                            &crate::ast::private_name("Some"),
                            span,
                        )),
                        args: vec![access],
                        named: vec![],
                        type_args: vec![],
                        bracket: None,
                    },
                    span,
                }
            };
            let arm_span = obj.span;
            ExprKind::Match {
                scrutinee: obj,
                arms: vec![
                    MatchExprArm {
                        span: arm_span,
                        pattern: variant_pat(
                            nid(),
                            &crate::ast::private_name("Some"),
                            vec![Pattern::Ident(c, Span::default(), nid())],
                        ),
                        guard: None,
                        body: some_body,
                    },
                    MatchExprArm {
                        span: arm_span,
                        pattern: variant_pat(nid(), crate::lexer::NONE, vec![]),
                        guard: None,
                        body: Expr {
                            id: nid(),
                            kind: if void {
                                ExprKind::Pass
                            } else {
                                ExprKind::NoneLit
                            },
                            span,
                        },
                    },
                ],
            }
        }
        other @ ExprKind::Int(_)
        | other @ ExprKind::Float(_)
        | other @ ExprKind::Str(_)
        | other @ ExprKind::Interp(_)
        | other @ ExprKind::Bytes(_)
        | other @ ExprKind::RawStr(_)
        | other @ ExprKind::Bool(_)
        | other @ ExprKind::NoneLit
        | other @ ExprKind::Pass
        | other @ ExprKind::Ident(_)
        | other @ ExprKind::List(..)
        | other @ ExprKind::Tuple(_)
        | other @ ExprKind::Map(_)
        | other @ ExprKind::Set(_)
        | other @ ExprKind::Comprehension { .. }
        | other @ ExprKind::Unary { .. }
        | other @ ExprKind::Binary { .. }
        | other @ ExprKind::Compare { .. }
        | other @ ExprKind::Range { .. }
        | other @ ExprKind::Call { .. }
        | other @ ExprKind::Field { .. }
        | other @ ExprKind::Index { .. }
        | other @ ExprKind::Try(_)
        | other @ ExprKind::Closure { .. }
        | other @ ExprKind::Match { .. }
        | other @ ExprKind::IfElse { .. }
        | other @ ExprKind::Recover(..)
        | other @ ExprKind::ElseGuard { .. } => other, // unreachable: caller guards on the two carrier kinds
    };
}

/// Lower a `NullCoalesce` carrier (in place) to the **Result-discard** lowering:
///   `r ?? b` → `match r: Ok(__optN): __optN; Err(_): b`
/// The `Err` arm binds NOTHING — `Pattern::Wildcard` — so the error payload is discarded. This is
/// Rust's `Result::unwrap_or`, not `?`: it never propagates. Nothing is required of `E`, because
/// Chezzi is GC'd and has no destructor to run on the dropped value.
///
/// Distinct from [`lower_carrier_try`], which also applies to a `Result` operand but PROPAGATES the
/// error with `?`. Collapsing the two into one `CarrierMode` would let `?.`'s propagation reach a
/// `??`.
pub fn lower_carrier_result_coalesce(expr: &mut Expr, tmp: usize) {
    let span = expr.span;
    let (base, k) = (expr.id, std::cell::Cell::new(0));
    let nid = || {
        k.set(k.get() + 1);
        base.carrier_child(k.get())
    };
    let c = crate::ast::CarrierTemp::Payload.name(tmp);
    let kind = std::mem::replace(&mut expr.kind, ExprKind::Bool(false));
    let ExprKind::NullCoalesce { lhs, rhs, .. } = kind else {
        unreachable!("lower_carrier_result_coalesce applies to '??' only");
    };
    let arm_span = lhs.span;
    expr.kind = ExprKind::Match {
        scrutinee: lhs,
        arms: vec![
            MatchExprArm {
                span: arm_span,
                pattern: variant_pat(
                    nid(),
                    &crate::ast::private_name("Ok"),
                    vec![Pattern::Ident(c.clone(), Span::default(), nid())],
                ),
                guard: None,
                body: ident_expr_at(nid(), &c, span),
            },
            MatchExprArm {
                span: arm_span,
                pattern: variant_pat(
                    nid(),
                    &crate::ast::private_name("Err"),
                    vec![Pattern::Wildcard],
                ),
                guard: None,
                body: *rhs,
            },
        ],
    };
}

/// Lower an `OptChain` carrier (in place) to the **Result** lowering — `?` then `.`:
///   `x?.field`   → `x?.field`      i.e. `Field { obj: Try(x), … }`
///   `x?.m(args)` → `x?.m(args)`    i.e. `Call { callee: Field { obj: Try(x), … }, … }`
/// The output is EXACTLY what the parser builds for the spaced spelling `x? .field` /
/// `x? .m(args)`: `parse_postfix` reuses the primary's span for every postfix link, so `Try`,
/// `Field` and `Call` all carry `expr.span`, and the `Field`'s `name_span` is the name token's own
/// span — which is what the carrier already holds. That equality is the whole point: the two
/// spellings must produce byte-identical ASTs, diagnostics and bytecode.
///
/// `NullCoalesce` never reaches here: `?.` on a `Result` propagates with `?`, while `??` on a
/// `Result` discards via [`lower_carrier_result_coalesce`].
pub fn lower_carrier_try(expr: &mut Expr) {
    let span = expr.span;
    let (base, k) = (expr.id, std::cell::Cell::new(0));
    let nid = || {
        k.set(k.get() + 1);
        base.carrier_child(k.get())
    };
    let kind = std::mem::replace(&mut expr.kind, ExprKind::Bool(false));
    let ExprKind::OptChain {
        obj,
        name,
        name_span,
        call,
    } = kind
    else {
        unreachable!(
            "lower_carrier_try applies to '?.' only; '??' on a Result uses lower_carrier_result_coalesce"
        );
    };
    let field = Expr {
        id: nid(),
        kind: ExprKind::Field {
            obj: Box::new(Expr {
                id: nid(),
                kind: ExprKind::Try(obj),
                span,
            }),
            name,
            name_span,
        },
        span,
    };
    expr.kind = match call {
        None => field.kind,
        Some(call) => opt_call_kind(field, call, span, nid),
    };
}

/// The call of a `?.` method link on its synthesized callee `field`: `field(args)`, or, when
/// the bracket has no type reading (`h?.fs[0](10)`, `OptCall::bracket`), `field[i](args)`
/// as the parser builds it after `.`. Only the index form draws an id (`index_id`), so a plain
/// link's ids are unchanged.
fn opt_call_kind(
    field: Expr,
    call: OptCall,
    span: Span,
    index_id: impl FnOnce() -> crate::ast::NodeId,
) -> ExprKind {
    let OptCall {
        args,
        named,
        type_args,
        bracket,
    } = call;
    let (callee, bracket) = match bracket {
        Some(index) if type_args.is_empty() => (
            Expr {
                id: index_id(),
                kind: ExprKind::Index {
                    obj: Box::new(field),
                    index: Some(index),
                    types: Vec::new(),
                },
                span,
            },
            None,
        ),
        bracket => (field, bracket),
    };
    ExprKind::Call {
        callee: Box::new(callee),
        args,
        named,
        type_args,
        bracket,
    }
}

/// A module's top-level names that hide a full module path's head (TICKET-175): `let`, `fn` and
/// extern fn names, type declarations, every `from` bind, and every whole-module bind except two.
/// An un-aliased one-segment `import pkg` does not hide `pkg.deep` (it is the same package head),
/// and the resolver's dotted synthetic bind is the full path itself.
fn module_level_names(
    stmts: &[Stmt],
    imports: &[crate::base::module_id::ResolvedImport],
) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in stmts {
        match &s.kind {
            StmtKind::Let { target, .. } => out.extend(target.binder_names()),
            StmtKind::Fn(d) => {
                out.insert(d.name.clone());
            }
            StmtKind::Extern { fns, .. } => out.extend(fns.iter().map(|f| f.name.clone())),
            StmtKind::Struct { name, .. }
            | StmtKind::Enum { name, .. }
            | StmtKind::TypeAlias { name, .. }
            | StmtKind::Protocol { name, .. } => {
                out.insert(name.clone());
            }
            StmtKind::Import(Import::Variants { names, .. }) => {
                out.extend(
                    names
                        .iter()
                        .map(|(n, a)| a.clone().unwrap_or_else(|| n.clone())),
                );
            }
            StmtKind::Assign { .. }
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
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. }
            | StmtKind::Assert { .. }
            | StmtKind::Expr(_) => {}
        }
    }
    for imp in imports {
        match &imp.import {
            Import::Module { path, alias, .. } => {
                let Some(bound) = alias.clone().or_else(|| path.last().cloned()) else {
                    continue;
                };
                if crate::ast::is_full_path_bind(&bound) || (alias.is_none() && path.len() == 1) {
                    continue;
                }
                out.insert(bound);
            }
            // The resolver never resolves a variant import; the `stmts` loop above names it.
            Import::Variants { .. } => {}
            Import::From { names, .. } => {
                out.extend(
                    names
                        .iter()
                        .map(|(n, a)| a.clone().unwrap_or_else(|| n.clone())),
                );
            }
        }
    }
    out
}

/// A bare identifier expression at `span`.
fn ident_expr(name: &str, span: Span) -> Expr {
    ident_expr_at(crate::ast::NodeId::fresh(), name, span)
}

fn ident_expr_at(id: crate::ast::NodeId, name: &str, span: Span) -> Expr {
    Expr {
        id,
        kind: ExprKind::Ident(name.to_string()),
        span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::ExprKind;
    use crate::lexer;
    use crate::resolver::LoadedModule;
    use std::path::PathBuf;

    /// Parse `src` into a single-module graph (no imports), run desugar, return the module's stmts.
    fn desugar_ok(src: &str) -> Vec<Stmt> {
        let ast = crate::parser::parse(lexer::tokenize(src).unwrap()).expect("parse");
        let id = ModuleId(PathBuf::from("<test>"));
        let mut graph = ModuleGraph {
            entry: id.clone(),
            prelude_used: crate::resolver::PreludeUse::ALL,
            skipped_fns: std::collections::HashMap::new(),
            modules: vec![LoadedModule {
                id,
                dotted: vec![],
                ast,
                file: 0,
                imports: vec![],
                native: None,
            }],
        };
        run(&mut graph).expect("desugar");
        graph.modules.remove(0).ast.stmts
    }

    fn desugar_err(src: &str) -> ResolveError {
        let ast = crate::parser::parse(lexer::tokenize(src).unwrap()).expect("parse");
        let id = ModuleId(PathBuf::from("<test>"));
        let mut graph = ModuleGraph {
            entry: id.clone(),
            prelude_used: crate::resolver::PreludeUse::ALL,
            skipped_fns: std::collections::HashMap::new(),
            modules: vec![LoadedModule {
                id,
                dotted: vec![],
                ast,
                file: 0,
                imports: vec![],
                native: None,
            }],
        };
        run(&mut graph).expect_err("expected a desugar error")
    }

    /// TICKET-109 — `n` nested `fn {prefix}{i}():` declarations, the outermost indented `indent`
    /// levels, the innermost body `pass`.
    fn fn_chain(prefix: &str, n: usize, indent: usize) -> String {
        let mut src = String::new();
        for i in 0..n {
            src.push_str(&"    ".repeat(indent + i));
            src.push_str(&format!("fn {prefix}{i}():\n"));
        }
        src.push_str(&"    ".repeat(indent + n));
        src.push_str("pass\n");
        src
    }

    /// TICKET-109 / W12-12 — `fn` declarations nest 100 deep (`MAX_FN_NESTING`) and no deeper; a
    /// top-level `fn` or a method is level 1. A sibling chain starts again at level 1, so the second
    /// 100-deep chain here fails if a body's walk forgets to decrement the depth.
    #[test]
    fn fn_nesting_one_hundred_deep_is_accepted() {
        // The production front-end stack: a 100-deep parse and walk overflows a 2 MiB test thread.
        crate::on_frontend_stack_scoped(|| {
            desugar_ok(&(fn_chain("f", 100, 0) + &fn_chain("g", 100, 0)));
            desugar_ok(&format!(
                "struct S:\n    x: int\n    fn m(self):\n{}",
                fn_chain("f", 99, 2)
            ));
        });
    }

    /// TICKET-109 / W12-12 — the 101st level is one resolve error at that fn's name, whether the
    /// chain starts at a top-level `fn` or at a method.
    #[test]
    fn fn_nesting_one_hundred_and_one_deep_is_rejected_at_the_fn_name() {
        let e = crate::on_frontend_stack_scoped(|| desugar_err(&fn_chain("f", 101, 0)));
        assert_eq!(
            e.message,
            "fn 'f100' is nested 101 deep; fn declarations nest at most 100 deep (declare it at an outer level)"
        );
        assert_eq!((e.span.line, e.span.col), (101, 404));
        let e = crate::on_frontend_stack_scoped(|| {
            desugar_err(&format!(
                "struct S:\n    x: int\n    fn m(self):\n{}",
                fn_chain("f", 100, 2)
            ))
        });
        assert_eq!(
            e.message,
            "fn 'f99' is nested 101 deep; fn declarations nest at most 100 deep (declare it at an outer level)"
        );
        assert_eq!((e.span.line, e.span.col), (103, 408));
    }

    /// Pull the positional arg ints out of the call inside the last statement (`x := CALL` or `CALL`).
    fn call_arg_ints(stmts: &[Stmt]) -> Vec<i64> {
        let last = stmts.last().expect("a statement");
        let expr = match &last.kind {
            StmtKind::Let { value, .. } => value,
            StmtKind::Expr(e) => e,
            other => panic!("expected let/expr, got {other:?}"),
        };
        let ExprKind::Call { args, named, .. } = &expr.kind else {
            panic!("expected a Call, got {:?}", expr.kind)
        };
        assert!(named.is_empty(), "named must be cleared after desugar");
        args.iter()
            .map(|a| match a.kind {
                ExprKind::Int(n) => n,
                ref other => panic!("expected an int arg, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn plain_full_arity_unchanged() {
        let s = desugar_ok("fn f(x: int, y: int):\n    print(x)\nr := f(1, 2)\n");
        assert_eq!(call_arg_ints(&s), vec![1, 2]);
    }

    #[test]
    fn under_arity_no_default_left_for_checker() {
        // No default on `y`: desugar leaves it (checker will report the arity error).
        let s = desugar_ok("fn f(x: int, y: int):\n    print(x)\nr := f(1)\n");
        // unchanged: a single positional arg, no named
        assert_eq!(call_arg_ints(&s), vec![1]);
    }

    // TICKET-066 W10-17: a named argument for a parameter that PRECEDES a variadic is falsely
    // rejected as "missing required argument", even though it IS supplied. Named args work with no
    // variadic present (`missing_required_with_named_errors` above passes), and a post-variadic
    // keyword param works too; only a PRE-variadic named param is broken.
    #[test]
    fn named_arg_before_variadic_is_not_missing() {
        desugar_ok("fn f(a: int, ...rest: int) -> int:\n    return a + rest.len()\nr := f(a=1)\n");
        desugar_ok(
            "fn g(a: int, b: int, ...r: int) -> int:\n    return a + b + r.len()\nr1 := g(1, b=2)\nr2 := g(b=2, a=1)\n",
        );
    }

    /// Pull the named-arg keys off the call inside the last statement.
    fn call_named_keys(stmts: &[Stmt]) -> Vec<String> {
        let last = stmts.last().expect("a statement");
        let expr = match &last.kind {
            StmtKind::Let { value, .. } => value,
            StmtKind::Expr(e) => e,
            other => panic!("expected let/expr, got {other:?}"),
        };
        let ExprKind::Call { named, .. } = &expr.kind else {
            panic!("expected a Call, got {:?}", expr.kind)
        };
        named.iter().map(|(k, _)| k.clone()).collect()
    }

    #[test]
    fn print_end_kwarg_is_kept_in_named() {
        // `print` is special-cased: its `sep`/`end` named args survive desugar (not rewritten to
        // positional), so the checker and engines can read them off the Call.
        let s = desugar_ok("print(\"a\", end=\"\")\n");
        assert_eq!(call_named_keys(&s), vec!["end".to_string()]);
    }

    #[test]
    fn print_sep_and_end_kwargs_kept() {
        let s = desugar_ok("print(\"a\", \"b\", sep=\"-\", end=\"!\")\n");
        assert_eq!(
            call_named_keys(&s),
            vec!["sep".to_string(), "end".to_string()]
        );
    }

    #[test]
    fn local_shadows_function_not_rewritten() {
        // `f` is shadowed by a local binding; the call must NOT pull the top-level fn's default.
        let s = desugar_ok(
            "fn f(x: int, y: int = 9):\n    print(x)\nfn main():\n    f := fn(a: int): a\n    r := f(1)\nmain()\n",
        );
        // find the inner call: in main's body, `r := f(1)` stays a single positional arg.
        let StmtKind::Fn(decl) = &s[1].kind else {
            panic!("expected main fn")
        };
        let StmtKind::Let { value, .. } = &decl.body[1].kind else {
            panic!("expected r := f(1)")
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!("expected call")
        };
        assert_eq!(
            args.len(),
            1,
            "shadowed local call must keep its single arg"
        );
    }

    /// Pull positional arg ints out of a method call `recv.m(...)` in the last statement.
    fn method_call_arg_ints(stmts: &[Stmt]) -> Vec<i64> {
        let last = stmts.last().expect("a statement");
        let expr = match &last.kind {
            StmtKind::Let { value, .. } => value,
            StmtKind::Expr(e) => e,
            other => panic!("expected let/expr, got {other:?}"),
        };
        let ExprKind::Call {
            args,
            named,
            callee,
            ..
        } = &expr.kind
        else {
            panic!("expected a Call, got {:?}", expr.kind)
        };
        assert!(
            matches!(callee.kind, ExprKind::Field { .. }),
            "expected a method call"
        );
        assert!(named.is_empty(), "named must be cleared after desugar");
        args.iter()
            .map(|a| match a.kind {
                ExprKind::Int(n) => n,
                ref other => panic!("expected an int arg, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn builtin_method_name_not_normalized() {
        // `push` is a builtin list method; a 0-arg call must NOT be rewritten even if a struct
        // happens to define a `push` with a default.
        let s = desugar_ok(
            "struct Q:\n    n: int\n    fn push(self, x: int = 9):\n        print(x)\nxs := [1, 2]\nxs.push(3)\n",
        );
        // xs.push(3) stays one positional arg (the builtin), not rewritten to the struct spec.
        assert_eq!(method_call_arg_ints(&s), vec![3]);
    }

    #[test]
    fn real_builtin_set_insert_untouched() {
        // A genuine builtin-type receiver: `s.insert(3)` on a Set must NOT be rewritten, even though a
        // struct also defines `insert` with a default. Desugar binds no calls (`Checker::bind_call` does).
        let s = desugar_ok(
            "struct Counter:\n    n: int\n    fn insert(self, amount: int = 1) -> int:\n        return self.n + amount\ns := Set([1, 2])\ns.insert(3)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![3]);
    }

    /// TICKET-180 — a default spliced into two calls is two nodes, not one node placed twice.
    #[test]
    fn a_spliced_default_gets_fresh_node_ids() {
        let stmts = desugar_ok("fn f(x: int = 5) -> int:\n    return x\nprint(f())\nprint(f())\n");
        let m = crate::ast::Module { stmts };
        assert_eq!(crate::ast::duplicate_ids(&m), vec![]);
    }

    /// The value expr of the last `name := <expr>` statement.
    fn last_let_value(stmts: &[Stmt]) -> Expr {
        match &stmts.last().expect("a statement").kind {
            StmtKind::Let { value, .. } => value.clone(),
            other => panic!("expected a let, got {other:?}"),
        }
    }

    #[test]
    fn opt_chain_field_survives_desugar() {
        let stmts = desugar_ok("struct P:\n    x: int\na := ?P(1)\nv := a?.x\n");
        match last_let_value(&stmts).kind {
            ExprKind::OptChain { name, call, .. } => {
                assert_eq!(name, "x");
                assert!(call.is_none(), "a field access carries no call part");
            }
            other => panic!("expected an OptChain, got {other:?}"),
        }
    }

    /// Parse `src` WITHOUT desugaring and return the last `name := <expr>` value — carriers survive.
    fn raw_last_let_value(src: &str) -> Expr {
        let ast = crate::parser::parse(lexer::tokenize(src).unwrap()).expect("parse");
        last_let_value(&ast.stmts)
    }

    #[test]
    fn lower_carrier_try_matches_the_spaced_spelling_exactly() {
        // THE load-bearing equivalence: `a?.f` lowered by `lower_carrier_try` must be the very AST
        // the parser builds for `a? .f` — spans included. The two sources are column-aligned (`a`
        // at col 6, `f` at col 10 in both) precisely so span equality is a real assertion.
        for (carrier_src, spaced_src) in [
            ("x := a ?.f\n", "x := a? .f\n"),
            ("x := a ?.f(1, k=2)\n", "x := a? .f(1, k=2)\n"),
        ] {
            let mut lowered = raw_last_let_value(carrier_src);
            assert!(
                matches!(lowered.kind, ExprKind::OptChain { .. }),
                "the carrier must survive parsing"
            );
            lower_carrier_try(&mut lowered);
            let spaced = raw_last_let_value(spaced_src);
            assert_eq!(lowered, spaced, "{carrier_src:?} vs {spaced_src:?}");
        }
    }

    #[test]
    fn lower_carrier_option_uses_the_carriers_own_name_span() {
        // Each link of `a?.m(c)?.n(c)` must give its synthesized callee `Field` a DISTINCT
        // `name_span` — they share `span` (the primary's), so `span` would collide two witness keys.
        let name_spans = |src: &str| -> (Span, Span) {
            let mut outer = raw_last_let_value(src);
            let ExprKind::OptChain { ref mut obj, .. } = outer.kind else {
                panic!("outer carrier")
            };
            lower_carrier_option(obj, 0);
            lower_carrier_option(&mut outer, 1);
            // `match <inner>: Some(__opt1): Some(__opt1.n(c)) …`
            let callee_name_span = |e: &Expr| -> Span {
                let ExprKind::Match { arms, .. } = &e.kind else {
                    panic!("match")
                };
                let ExprKind::Call { args, .. } = &arms[0].body.kind else {
                    panic!("?... wrapper")
                };
                let ExprKind::Call { callee, .. } = &args[0].kind else {
                    panic!("method call")
                };
                let ExprKind::Field { name_span, .. } = &callee.kind else {
                    panic!("callee field")
                };
                *name_span
            };
            let ExprKind::Match { scrutinee, .. } = &outer.kind else {
                panic!("match")
            };
            (callee_name_span(scrutinee), callee_name_span(&outer))
        };
        let (inner, outer) = name_spans("x := a?.m(c)?.n(c)\n");
        assert_ne!(inner, outer, "two witness calls must not share one key");
        assert_eq!(
            inner,
            Span {
                line: 1,
                col: 9,
                file: 0
            }
        );
        assert_eq!(
            outer,
            Span {
                line: 1,
                col: 15,
                file: 0
            }
        );
    }

    #[test]
    fn lower_carrier_result_coalesce_builds_ok_err_arms() {
        let stmts = desugar_ok("fn g() -> int!str:\n    return ?1\nx := g() ?? 0\n");
        let mut e = last_let_value(&stmts);
        lower_carrier_result_coalesce(&mut e, 0);
        let ExprKind::Match { arms, .. } = &e.kind else {
            panic!("expected a Match, got {:?}", e.kind)
        };
        assert_eq!(arms.len(), 2);
        let Pattern::Variant { name, bindings, .. } = &arms[0].pattern else {
            panic!("expected a variant pattern, got {:?}", arms[0].pattern)
        };
        assert_eq!(*name, crate::ast::private_name("Ok"));
        assert_eq!(bindings.len(), 1);
        assert!(
            matches!(&bindings[0], Pattern::Ident(n, _, _) if *n == crate::ast::CarrierTemp::Payload.name(0))
        );
        let Pattern::Variant { name, bindings, .. } = &arms[1].pattern else {
            panic!("expected a variant pattern, got {:?}", arms[1].pattern)
        };
        assert_eq!(*name, crate::ast::private_name("Err"));
        assert_eq!(bindings.len(), 1);
        assert!(matches!(&bindings[0], Pattern::Wildcard));
    }

    #[test]
    fn two_coalesce_in_one_expr_get_unique_temps() {
        // `(a ?? 0) + (b ?? 0)` — both carriers now survive desugar, and the temp names are minted
        // by whoever lowers them. Assert the property at that point instead: two lowerings with
        // distinct counter values bind DISTINCT temps.
        let stmts = desugar_ok("a := ?1\nb := ?2\nx := (a ?? 0) + (b ?? 0)\n");
        let ExprKind::Binary {
            mut lhs, mut rhs, ..
        } = last_let_value(&stmts).kind
        else {
            panic!("expected a Binary");
        };
        assert!(matches!(lhs.kind, ExprKind::NullCoalesce { .. }));
        assert!(matches!(rhs.kind, ExprKind::NullCoalesce { .. }));
        lower_carrier_option(&mut lhs, 0);
        lower_carrier_option(&mut rhs, 1);
        let name_of = |e: &Expr| -> String {
            let ExprKind::Match { arms, .. } = &e.kind else {
                panic!("expected Match")
            };
            let Pattern::Variant { bindings, .. } = &arms[0].pattern else {
                panic!("variant")
            };
            let Pattern::Ident(n, _, _) = &bindings[0] else {
                panic!("ident binding")
            };
            n.clone()
        };
        assert_ne!(name_of(&lhs), name_of(&rhs), "temps must be unique");
    }

    // ===== non-constant default expressions =====

    #[test]
    fn a_literal_default_is_still_cloned_inline() {
        // The inline class is not provided: `= 1 + 2` is filled from the declaration itself.
        let s = desugar_ok("fn f(x: int = 1 + 2):\n    print(x)\nr := f()\n");
        assert!(
            !s.iter().any(
                |st| matches!(&st.kind, StmtKind::Fn(d) if d.name.starts_with(crate::base::provider_name::PROVIDER_PREFIX))
            ),
            "no provider is synthesized for a self-contained literal"
        );
    }

    // ===== variadic collapse =====
}
