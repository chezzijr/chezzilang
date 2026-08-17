//! Call-argument desugaring: normalize **named arguments** (`f(x=1)`) and **default arguments**
//! (`fn f(x: int, y: int = 10)`) into a plain positional `args` list.
//!
//! This pass runs inside [`crate::resolver::build_graph`], so the checker and the VM
//! consume the already-normalized AST — they only ever see
//! `Call.named` empty and a fully positional `Call.args`. That keeps the front-end and VM in lockstep by
//! construction: there is no per-phase call-binding logic for defaults/named args.
//!
//! Scope: free functions (own module + `from`-imported + module-qualified `alias.f(...)`) and struct
//! constructors. Enum-variant constructors are excluded (payloads are unnamed) and methods are
//! deferred (resolving a receiver type needs the checker). A default may be any expression that does
//! not reference another parameter/field — `validate_defaults` enforces this (no parameter/field is
//! bound where a default is evaluated).
//!
//! **How an omitted argument is materialised (W7-51).** A self-contained literal (`= 10`, `= -1`,
//! `= None`, `= []`) is cloned into the call site. Anything else is compiled ONCE, as a hidden
//! zero-arg `fn` appended to the module that DECLARES the parameter ([`synthesize_providers`]), and
//! the call site gets a call to it. That is what makes a default resolve — and evaluate — in the
//! definer's namespace (as Python, Ruby and Kotlin all do) instead of the caller's, and what lets
//! default chains compose to any depth. See [`Dflt`] and [`Walker::splice_default`].
//!
//! The pass is **scope-aware**: a local binding may shadow a top-level function name, so a call is
//! only rewritten when its callee resolves to a registered callable and is *not* shadowed by a local
//! (mirroring the checker, which treats a call as a named function only when the name is not a local).

use crate::ast::{
    Block, Chunk, DeferTarget, Expr, ExprKind, Import, MatchExprArm, Module, OptCall, Param,
    Pattern, Span, SpawnTarget, Stmt, StmtKind, Type, TypeParam, WaitArmKind, WaitTarget,
};
use crate::resolver::{ModuleGraph, ModuleId, ResolveError};
use std::collections::{HashMap, HashSet};

/// Name prefix of a synthesized **default-argument provider** — the hidden zero-arg function that
/// evaluates one parameter/field default in the module that DECLARES it (W7-51). `$` is unspellable
/// in Chezzi source, so a provider name can never collide with a user global or an import bind.
pub const PROVIDER_PREFIX: &str = "$def$";

/// Which kind of slot a provider was synthesized for. The name embeds it because `owner.param` alone
/// is NOT injective: a struct field (`struct S: m: int = g()`, owner `S`, param `m`) and a same-named
/// free function's parameter (`fn S(m: int = g())`, owner `S`, param `m`) produced the SAME name, and
/// both declarations are legal today — measured on `b1307258` the pair type-checked clean, and with
/// one name for both providers the checker reported `function '$def$2$S.m$' is already defined`
/// (leaking an internal symbol into `--errors=json`, i.e. the editor squiggle). A method's owner
/// carries a `.` (`S.m`) and a struct/fn name never can, so within one kind the name is injective.
#[derive(Clone, Copy, PartialEq)]
enum Slot {
    /// A parameter of a free fn or a method.
    Param,
    /// A struct field.
    Field,
}

impl Slot {
    /// One character, so the name stays short and stays unspellable.
    fn tag(self) -> char {
        match self {
            Slot::Param => 'p',
            Slot::Field => 'f',
        }
    }
}

/// The provider function's name for one parameter/field default. `file` is the DECLARING module's
/// [`crate::resolver::LoadedModule::file`] id (already unique per module, and the same coordinate
/// the checker→compiler side-table keys use), `slot` says parameter vs struct field (see [`Slot`]),
/// `owner` names the declaring callable (`f`, `S` for a struct field, `S.m` for a method). ONE
/// function, called by the synthesizer, every registry collector and the checker's decl-site `?`
/// gate (each passing the result straight into [`dflt_for`]), so they can never drift into naming a
/// provider that does not exist.
fn provider_name(file: u32, slot: Slot, owner: &str, param: &str) -> String {
    format!("{PROVIDER_PREFIX}{file}${}${owner}.{param}$", slot.tag())
}

/// [`provider_name`] for a **parameter** slot, for the checker's decl-site `?` gate: it asks whether
/// the default it is about to infer will be judged again inside a provider body, and the honest way
/// to answer is to look for the function [`synthesize_providers`] would have emitted.
pub(crate) fn param_provider_name(file: u32, owner: &str, param: &str) -> String {
    provider_name(file, Slot::Param, owner, param)
}

/// Render a compiled function's name for a user-visible message — a stack-trace frame, chiefly.
/// A synthesized provider's internal name is unspellable ON PURPOSE (`$def$2$f.x$`), which also
/// makes it unreadable, so a frame for one is shown as what it is. Every other name passes through
/// borrowed, so this is free on the ordinary path.
pub fn display_fn_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.starts_with(PROVIDER_PREFIX) {
        std::borrow::Cow::Owned(format!("<default for {}>", provider_label(name)))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Decode a provider name back into a human phrase for a diagnostic (`'x' of 'f'`), which every
/// caller prefixes with its own "the default for …". Deliberately noun-free: the name does not
/// record whether the slot is a **parameter** or a struct **field**, and calling a field a parameter
/// was measurable (`struct S: n: int = S().n` reported `the default value for parameter 'n' of 'S'
/// is cyclic`). Total: an unparseable name (impossible for one [`provider_name`] built) degrades to
/// itself.
fn provider_label(name: &str) -> String {
    let Some(rest) = name
        .strip_prefix(PROVIDER_PREFIX)
        .and_then(|r| r.strip_suffix('$'))
    else {
        return format!("'{name}'");
    };
    // `<file>$<slot>$<owner>.<param>` — the owner may itself contain a `.` (`S.m`), the param never
    // does; `<file>` and `<slot>` are both internal coordinates and neither is shown.
    let Some((_, rest)) = rest.split_once('$') else {
        return format!("'{name}'");
    };
    let Some((_, owner_param)) = rest.split_once('$') else {
        return format!("'{name}'");
    };
    match owner_param.rsplit_once('.') {
        Some((owner, param)) => format!("'{param}' of '{owner}'"),
        None => format!("'{owner_param}'"),
    }
}

/// How an omitted argument is materialised at a call site.
///
/// The split is the whole of W7-51: a **self-contained literal** is cheap and context-free, so it is
/// still cloned into the caller (`= 10`, `= -1`, `= "hi"`, `= None`, `= []`); **everything else** is
/// compiled ONCE, as a zero-arg function in its defining module, and the caller merely calls it. A
/// provider body therefore resolves — and evaluates — in the DEFINER's namespace (`Obj::Func` carries
/// its `home`), which is what Python, Ruby and Kotlin all do, and what a spliced clone could not do.
#[derive(Clone, PartialEq)]
enum Dflt {
    /// Cloned inline at the call site (and re-walked there, so it still spends the depth budget).
    Inline(Expr),
    /// Call the zero-arg provider synthesized in the module named by `module`.
    Provider { module: ModuleId, name: String },
    /// **Left to the CALLEE.** The default cannot be hoisted into a free top-level provider `fn` — its
    /// type or expression names `Self` on a GENERIC host (`Q[T]`, whose `T` is unbound outside the
    /// signature) or an enclosing type parameter. Rather than clone it into the caller and resolve it
    /// there (the caller-scope hazard this whole design exists to delete), the call site simply omits
    /// the argument: the callee's own prologue fills it from the declaration, in the declaring module,
    /// where `Self` and `T` are both in scope (`crate::vm::op::Op::JumpIfProvided`).
    ///
    /// Only expressible as a TRAILING omission — see [`Walker::normalize_call`] for the one shape
    /// that cannot be (a keyword call supplying a LATER parameter), which is refused rather than
    /// silently cloned.
    CalleeFilled,
}

/// Is `e` a **self-contained literal** — an expression that can be cloned into any number of call
/// sites, in any module, and mean exactly the same thing?
///
/// Deliberately an allow-list and deliberately narrower than "resolves to the same value": when in
/// doubt the default becomes a provider, which is always correct and one call slower. In particular
/// a `Str` carrying `{`/`}` is NOT inline — this same pass turns it into an `Interp` holding
/// arbitrary sub-expressions. Excluding every `Call`/`Field`/`Ident` is also what keeps W7-49's
/// span-keyed side tables injective: an inline default records no keyword/carrier/witness entry, so
/// two clones of it cannot resolve two ways under one key.
fn is_inline_default(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_) => true,
        ExprKind::Str(s) => !s.contains('{') && !s.contains('}'),
        // The only identifiers that are self-contained VALUES rather than references to a namespace:
        // the nullary builtin variant and `nil`. Both are keywords to the LEXER, which is what makes
        // the clone safe in practice — NOT a guarantee that the name means the same thing in the
        // caller. A local really can shadow `None`: `fn f(x: int? = None)` called from a body
        // containing `None := 5` reports `argument 1 of 'f': expected Option[int], found int` at the
        // DECLARATION, i.e. the caller's local reached the clone. Identical on `b1307258`, so this
        // is a pre-existing corner of the inline class and not something W7-51 introduced.
        ExprKind::Ident(n) => n == "None" || n == "nil",
        ExprKind::Unary { expr, .. } => is_inline_default(expr),
        ExprKind::Binary { lhs, rhs, .. } => is_inline_default(lhs) && is_inline_default(rhs),
        ExprKind::Range { start, end } => is_inline_default(start) && is_inline_default(end),
        ExprKind::List(xs, _) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().all(is_inline_default)
        }
        ExprKind::Map(ps) => ps
            .iter()
            .all(|(k, v)| is_inline_default(k) && is_inline_default(v)),
        _ => false,
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
///     `-> Self` is `unknown type 'Self'` (`docs/syntax.md`: `Self` names the receiver type and is
///     not spellable in a free fn's signature). Struct and enum hosts alike.
///   * an *expression* mentioning either (`x: int = mk[T]().n`) — the type is spellable but the body
///     is not: a provider would be checked with `T` unbound and add `unknown type 'T'` plus a
///     witness error on top of the two the shape already gets. Measured on `b1307258`: 2 errors;
///     with a provider: 4; without: 2 again.
///
/// The type-parameter shapes are compile errors today and stay at exactly the errors they already
/// had; the `Self` shapes are working programs and stay working. Both keep the caller-scope
/// resolution an inline clone implies — the same known hazard [`Walker::splice_default`]'s fallback
/// documents.
fn dflt_for(
    d: &Expr,
    ty: Option<&Type>,
    type_params: &[String],
    self_ty: Option<&str>,
    module: &ModuleId,
    name: String,
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
        return Dflt::CalleeFilled;
    }
    // The EXPRESSION channel keeps `Self` unbound either way: rewriting `Self` inside the provider's
    // BODY (`Self()`, `Self.mk()`) needs a mutating expression walker, which is deliberately not part
    // of this change — such a default keeps the inline carve-out for now.
    let mut expr_unbound: Vec<String> = type_params.to_vec();
    expr_unbound.push("Self".to_string());
    if expr_mentions_type_param(d, &expr_unbound) {
        return Dflt::CalleeFilled;
    }
    Dflt::Provider {
        module: module.clone(),
        name,
    }
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
        Type::Named { .. } => t.clone(),
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
        } => Type::Func {
            params: params.iter().map(|a| subst_self_ty(a, self_ty)).collect(),
            ret: Box::new(subst_self_ty(ret, self_ty)),
            labels: labels.clone(),
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

/// A callable's parameter (or struct field), in declaration order, with its optional
/// default. Cloned out of the AST so the per-module registry is independent of the graph we mutate.
/// `PartialEq` lets us decide whether several same-named struct methods share one binding shape.
#[derive(Clone, PartialEq)]
struct PSpec {
    name: String,
    default: Option<Dflt>,
    /// True for a variadic parameter (`...xs: T`). `normalize_call` sweeps all surplus trailing
    /// positional args into a synthesized `List` literal at this slot; everything after it is
    /// keyword-only. At most one per spec. Struct fields are never variadic.
    is_variadic: bool,
}

/// Built-in / core methods on `str`/`list`/`map`/`set` (kept in sync with the checker's
/// `*_method_sig` tables + the HOF/`sort` handling in `infer_method_call`). The receiver of a call
/// whose name is one of these MIGHT be a builtin type whose shape we cannot see here, so the
/// name-keyed method path skips it. A user struct/enum that reuses one of these names DOES still get
/// default/named support — but only when the receiver's struct type is statically knowable pre-type
/// (a typed local, an inline ctor call, or a struct-returning fn call: see `receiver_struct_ty`),
/// resolved through `methods_by_struct`. A genuine builtin receiver (List/Set/Map/str) — or a
/// receiver whose type is not statically knowable (e.g. an unannotated param, an inferred enum
/// value) — is left untouched; a named-arg call there is an accurate error, not the misleading
/// "only supported on … struct methods".
const BUILTIN_METHODS: &[&str] = &[
    "len",
    "upper",
    "lower",
    "trim",
    "message",
    "split",
    "chars",
    "join",
    "starts_with",
    "contains",
    "push",
    "pop",
    "reverse",
    "index_of",
    "sum",
    "sort",
    "map",
    "filter",
    "fold",
    "sort_by",
    "sort_by_key",
    "has",
    "get",
    "keys",
    "values",
    "remove",
    "add",
    "union",
    "intersection",
    "difference",
];

fn is_builtin_method(name: &str) -> bool {
    BUILTIN_METHODS.contains(&name)
}

/// Free functions and struct constructors declared by one module.
#[derive(Default)]
struct ModReg {
    fns: HashMap<String, Vec<PSpec>>,
    structs: HashMap<String, Vec<PSpec>>,
    /// For a free fn whose declared return type is a struct of THIS module, the bare struct name.
    /// Lets a struct-returning-fn-call receiver (`mk().apply(r)`) resolve its method by receiver
    /// type pre-type, exactly like a named-local or ctor-call receiver.
    fn_ret_struct: HashMap<String, String>,
}

impl ModReg {
    /// Look up a name as either a function or a struct constructor (functions take precedence; a
    /// well-formed module never declares both with the same name).
    fn callable(&self, name: &str) -> Option<&Vec<PSpec>> {
        self.fns.get(name).or_else(|| self.structs.get(name))
    }
}

/// Desugar every module's calls in place. Errors carry the offending call's span.
pub fn run(graph: &mut ModuleGraph) -> Result<(), ResolveError> {
    for m in &graph.modules {
        validate_defaults(&m.ast.stmts)?;
    }
    // W7-51 — every non-inline default becomes a zero-arg `fn` in the module that DECLARES it,
    // BEFORE the registries are snapshotted (so a collector's `Dflt::Provider` always names a
    // function that exists) and before the walk (so a provider body is normalized like any other).
    synthesize_providers(graph);
    let regs = build_registries(graph);
    let methods = collect_methods(graph);
    let methods_by_struct = collect_methods_by_struct(graph);
    let fn_fields = collect_fn_fields(graph);
    // Index modules by id so we can resolve each module's imports against the others' registries.
    let mut module_index: HashMap<ModuleId, usize> = HashMap::new();
    for (i, m) in graph.modules.iter().enumerate() {
        module_index.insert(m.id.clone(), i);
    }
    let closures = import_closures(graph, &module_index);
    // ONE pass (W7-51). The old driver ran twice because a default was spliced RAW into the tail of
    // `walk_expr_inner`, after that node's children had already been walked — so a carrier or a
    // nested defaulted call inside a default needed a second sweep, and a chain three deep needed a
    // third that never came. Now a non-inline default is never spliced at all (the call site gets a
    // complete zero-arg call to its provider, which needs no further rewriting) and an inline one is
    // walked by `splice_default` at the moment it is cloned. Nothing is ever pushed into a subtree
    // the pass has already walked past, so depth is structural rather than pass-bounded.
    for (mi, deps) in closures.iter().enumerate() {
        // Build this module's resolution context: own id + bare from-imports + module aliases.
        let own_id = graph.modules[mi].id.clone();
        let mut bare_from: HashMap<String, ModuleId> = HashMap::new();
        let mut aliases: HashMap<String, ModuleId> = HashMap::new();
        for imp in &graph.modules[mi].imports {
            match &imp.import {
                Import::Module { path, alias, .. } => {
                    let local = alias
                        .clone()
                        .or_else(|| path.last().cloned())
                        .unwrap_or_default();
                    if !local.is_empty() {
                        aliases.insert(local, imp.target.clone());
                    }
                }
                Import::From { names, .. } => {
                    for (name, alias) in names {
                        let local = alias.clone().unwrap_or_else(|| name.clone());
                        bare_from.insert(local, imp.target.clone());
                    }
                }
            }
        }

        let ctx = Ctx {
            regs: &regs,
            own_id: &own_id,
            deps,
            bare_from: &bare_from,
            aliases: &aliases,
            methods: &methods,
            methods_by_struct: &methods_by_struct,
            fn_fields: &fn_fields,
        };
        let mut walker = Walker {
            ctx,
            scopes: Vec::new(),
            local_struct: Vec::new(),
            needed: std::collections::BTreeMap::new(),
            depth: 0,
        };
        {
            // Borrow the module's AST mutably; everything `walker` reads lives in `regs`/the maps above.
            let ast: &mut Module = &mut graph.modules[mi].ast;
            walker.walk_block(&mut ast.stmts)?;
        }
        // Give this module an import edge for every OTHER module's provider it now calls. The
        // provider then binds like any ordinary `from`-imported function — no new AST node, no new
        // opcode: the checker (`Checker::bind_import`), the compiler (`collect_globals`) and the VM
        // (`Vm::bind_import`) all read `LoadedModule.imports`, and `desugar::run` is called from
        // `resolver::build_graph` before every one of them. Drained in NAME order (a `BTreeMap`)
        // because import order feeds `ModuleProto::global_slots`, which must be deterministic.
        for (name, (target, span)) in std::mem::take(&mut walker.needed) {
            let dotted = graph.modules[module_index[&target]].dotted.clone();
            graph.modules[mi]
                .imports
                .push(crate::resolver::ResolvedImport {
                    target,
                    import: Import::From {
                        path: dotted,
                        names: vec![(name, None)],
                        name_spans: vec![span],
                    },
                    span,
                });
        }
    }
    check_provider_cycles(graph)
}

/// Each module's **transitive import closure**, parallel to `graph.modules`: every module reachable
/// from it by following `import` edges, at any depth, excluding itself.
///
/// This is the predicate [`Walker::splice_default`] refuses on, and it is a *dependency* rule, not a
/// load-order one — the same three files must compile the same way however the entry happens to
/// order its `import` lines. Load order is only a consequence, and relying on it made a cosmetic
/// reorder in a third module flip a compile error (measured: `import z` / `import a` refused,
/// `import a` / `import z` accepted, same files).
///
/// One forward sweep suffices, and that is also where the load-order invariant the VM needs is
/// checked: `resolver::Builder::visit` recurses into a module's imports BEFORE pushing the module
/// itself and rejects cycles, so `graph.modules` is a topological order — a dependency always sits
/// at a strictly lower index, and `out[t]` below is therefore already complete when read. That is
/// exactly the property `Vm::bind_import` needs (it indexes `module_objs[target_idx]`, pushed as
/// each module RUNS, and panics on a target that has not run yet), so a synthetic edge to a
/// transitive dependency can never outrun its target. The `debug_assert` is the standing check that
/// the resolver has not stopped producing that order.
fn import_closures(
    graph: &ModuleGraph,
    index: &HashMap<ModuleId, usize>,
) -> Vec<HashSet<ModuleId>> {
    let mut out: Vec<HashSet<ModuleId>> = Vec::with_capacity(graph.modules.len());
    for (i, m) in graph.modules.iter().enumerate() {
        let mut set: HashSet<ModuleId> = HashSet::new();
        for imp in &m.imports {
            let Some(&t) = index.get(&imp.target) else {
                continue;
            };
            debug_assert!(
                t < i,
                "graph.modules must be in dependency order (deps first): module {i} imports {t}"
            );
            // Both statements sit INSIDE the guard, so the same event degrades the same way: if the
            // resolver ever stopped producing dependency order, this target is simply absent from
            // the closure and `splice_default` refuses the default, instead of admitting an edge
            // whose closure was never read — which `Vm::bind_import` would meet as an index panic on
            // `module_objs[target_idx]` (`src/vm/exec.rs`) in a RELEASE build, where the
            // `debug_assert` above is compiled out.
            if t < i {
                set.extend(out[t].iter().cloned());
                set.insert(imp.target.clone());
            }
        }
        out.push(set);
    }
    out
}

/// Desugar a single standalone module (no imports) in place. Used by the test/standalone runners,
/// which bypass [`build_graph`](crate::resolver::build_graph) and so must apply this pass themselves
/// to stay consistent with the file-backed graph path.
#[cfg(test)]
pub fn run_standalone(module: &mut Module) -> Result<(), ResolveError> {
    validate_defaults(&module.stmts)?;
    let id = ModuleId(std::path::PathBuf::from("<main>"));
    // Mirror [`run`]: synthesize providers into the single module first. Its `file` id is whatever
    // the test's lexer stamped; there is only one module, so any value is unique by construction.
    let file = module.stmts.first().map_or(0, |s| s.span.file);
    synthesize_providers_into(&mut module.stmts, &id, file);
    let mut regs = HashMap::new();
    regs.insert(id.clone(), collect_module_reg(&module.stmts, &id, file));
    let mut methods = HashMap::new();
    collect_methods_into(&module.stmts, &mut methods, &id, file);
    let methods_by_struct = collect_methods_by_struct_into_standalone(&module.stmts, &id, file);
    let mut fn_fields = HashSet::new();
    collect_fn_fields_into(&module.stmts, &mut fn_fields);
    let bare_from = HashMap::new();
    let aliases = HashMap::new();
    let deps = HashSet::new();
    // ONE pass — see the comment in [`run`]. A standalone module has no imports, so every provider
    // it calls is its own and no synthetic import edge can be needed.
    let ctx = Ctx {
        regs: &regs,
        own_id: &id,
        deps: &deps,
        bare_from: &bare_from,
        aliases: &aliases,
        methods: &methods,
        methods_by_struct: &methods_by_struct,
        fn_fields: &fn_fields,
    };
    let mut walker = Walker {
        ctx,
        scopes: Vec::new(),
        local_struct: Vec::new(),
        needed: std::collections::BTreeMap::new(),
        depth: 0,
    };
    walker.walk_block(&mut module.stmts)?;
    debug_assert!(walker.needed.is_empty(), "standalone module has no imports");
    let mut edges = HashMap::new();
    collect_provider_edges(&module.stmts, &mut edges);
    check_provider_cycles_in(&edges)
}

/// Snapshot each module's free functions and struct constructors into a registry keyed by module id.
fn build_registries(graph: &ModuleGraph) -> HashMap<ModuleId, ModReg> {
    let mut regs = HashMap::new();
    for m in &graph.modules {
        regs.insert(
            m.id.clone(),
            collect_module_reg(&m.ast.stmts, &m.id, m.file),
        );
    }
    regs
}

/// One synthesized provider: `fn <name>() -> <ret>: return <default>`.
///
/// `ret` is the parameter's **declared** type, never `None` — `None` means *inferred*
/// (`checker::sig`), and inference errors out on a `None`-only / `[]`-only return.
/// `is_test: false` keeps providers out of `chezzi test` discovery. Every span is the default
/// expression's own, so a diagnostic inside the body points at the text the user actually wrote, in
/// the module they wrote it in.
fn provider_fn(name: String, ret: Type, default: Expr) -> Stmt {
    let span = default.span;
    Stmt {
        kind: StmtKind::Fn(crate::ast::FnDecl {
            name,
            name_span: span,
            type_params: Vec::new(),
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
    id: &ModuleId,
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
            id,
            provider_name(file, Slot::Param, owner, &p.name),
        ) {
            let ty =
                p.ty.clone()
                    .expect("a provider default has a declared type");
            // The SAME substitution `dflt_for` classified against, so the emitted provider's declared
            // return type and the decision to emit one can never disagree.
            out.push(provider_fn(name, subst_self_ty(&ty, self_ty), d.clone()));
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
        let (id, file) = (m.id.clone(), m.file);
        synthesize_providers_into(&mut m.ast.stmts, &id, file);
    }
}

/// [`synthesize_providers`] for one module's top-level statements.
fn synthesize_providers_into(stmts: &mut Vec<Stmt>, id: &ModuleId, file: u32) {
    let mut new_fns: Vec<Stmt> = Vec::new();
    for stmt in stmts.iter() {
        match &stmt.kind {
            StmtKind::Fn(decl) => {
                push_param_providers(&mut new_fns, id, file, &decl.name, decl, &[], None);
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
                    if let Dflt::Provider { name: pn, .. } = dflt_for(
                        d,
                        Some(&f.ty),
                        &stps,
                        None,
                        id,
                        provider_name(file, Slot::Field, name, &f.name),
                    ) {
                        new_fns.push(provider_fn(pn, f.ty.clone(), d.clone()));
                    }
                }
                for mth in methods {
                    let owner = format!("{name}.{}", mth.name);
                    push_param_providers(
                        &mut new_fns,
                        id,
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
            }
            | StmtKind::NewType {
                name,
                type_params,
                methods,
                ..
            } => {
                let stps: Vec<String> = type_params.iter().map(|t| t.name.clone()).collect();
                for mth in methods {
                    let owner = format!("{name}.{}", mth.name);
                    push_param_providers(
                        &mut new_fns,
                        id,
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
                for mth in bodied_methods {
                    let owner = format!("{name}.{}", mth.name);
                    push_param_providers(
                        &mut new_fns,
                        id,
                        file,
                        &owner,
                        mth,
                        &stps,
                        self_ty_for(name, &stps),
                    );
                }
            }
            _ => {}
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
/// confident-wrong-answer risk the project declines to take (`docs/gaps.md` W7-12); the runtime
/// fault is the documented, accepted outcome (`docs/syntax.md` §5).
fn check_provider_cycles(graph: &ModuleGraph) -> Result<(), ResolveError> {
    let mut edges: HashMap<String, (Vec<String>, Span)> = HashMap::new();
    for m in &graph.modules {
        collect_provider_edges(&m.ast.stmts, &mut edges);
    }
    check_provider_cycles_in(&edges)
}

/// One module's provider→provider edges, keyed by provider name (globally unique — the name embeds
/// the declaring module's `file` id).
fn collect_provider_edges(stmts: &[Stmt], edges: &mut HashMap<String, (Vec<String>, Span)>) {
    for stmt in stmts {
        if let StmtKind::Fn(decl) = &stmt.kind
            && decl.name.starts_with(PROVIDER_PREFIX)
        {
            let mut outs: Vec<String> = Vec::new();
            for s in &decl.body {
                if let StmtKind::Return(Some(e)) = &s.kind {
                    walk_idents(e, &mut |n| {
                        if n.starts_with(PROVIDER_PREFIX) {
                            outs.push(n.to_string());
                        }
                    });
                }
            }
            edges.insert(decl.name.clone(), (outs, stmt.span));
        }
    }
}

fn check_provider_cycles_in(
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

/// A program-wide registry of struct **methods**, keyed by method name. A method's receiver type is
/// unknown in this pre-type pass, so a method call is resolved by name; each entry holds one param
/// spec (the params *after* the receiver `self`) per struct that defines that name. Spans all modules
/// since a receiver may be an imported struct's value.
fn collect_methods(graph: &ModuleGraph) -> HashMap<String, Vec<Vec<PSpec>>> {
    let mut map: HashMap<String, Vec<Vec<PSpec>>> = HashMap::new();
    for m in &graph.modules {
        collect_methods_into(&m.ast.stmts, &mut map, &m.id, m.file);
    }
    map
}

/// The `[PSpec]` for one method's explicit parameters. The receiver (`self`, params[0]) is dropped —
/// a call's explicit args correspond to params[1..]. `owner` is `<Type>.<method>`, matching what
/// [`synthesize_providers`] passed for the same declaration.
fn method_spec(
    method: &crate::ast::FnDecl,
    owner_type: &str,
    type_params: &[TypeParam],
    id: &ModuleId,
    file: u32,
) -> Vec<PSpec> {
    let owner = format!("{owner_type}.{}", method.name);
    let stps: Vec<String> = type_params.iter().map(|t| t.name.clone()).collect();
    let tps = tp_names(method, &stps);
    // Drop the RECEIVER slot only when there is one. A STATIC method (the "no `self` ⇒ static" rule
    // the checker classifies by, `FnSig::is_static`) has no receiver, so its explicit arguments start
    // at param 0; skipping unconditionally dropped its FIRST real parameter, which silently deleted
    // that parameter's default from the spec — `struct S: fn mk(a: int = 5)` called as `S.mk()` was
    // `'mk' expects 1 argument(s), got 0` on `0104d57b`, i.e. a default that could never be filled.
    let skip = usize::from(method.params.first().is_some_and(|p| p.name == "self"));
    method
        .params
        .iter()
        .skip(skip)
        .map(|p| PSpec {
            name: p.name.clone(),
            default: p.default.as_ref().map(|d| {
                dflt_for(
                    d,
                    p.ty.as_ref(),
                    &tps,
                    self_ty_for(owner_type, &stps),
                    id,
                    provider_name(file, Slot::Param, &owner, &p.name),
                )
            }),
            is_variadic: p.is_variadic,
        })
        .collect()
}

/// Add one module's struct methods to `map`.
fn collect_methods_into(
    stmts: &[Stmt],
    map: &mut HashMap<String, Vec<Vec<PSpec>>>,
    id: &ModuleId,
    file: u32,
) {
    for stmt in stmts {
        // Struct AND enum methods share one name-keyed registry (a method call is resolved by name
        // in this pre-type pass; the checker has already validated the receiver type).
        let (owner_type, type_params, methods) = match &stmt.kind {
            StmtKind::Struct {
                name,
                type_params,
                methods,
                ..
            }
            | StmtKind::Enum {
                name,
                type_params,
                methods,
                ..
            }
            | StmtKind::NewType {
                name,
                type_params,
                methods,
                ..
            } => (name, type_params, methods),
            // A native struct's BODIED methods compile like struct methods, so a caller using
            // named/default args on one needs its param-spec registered here too.
            StmtKind::NativeStruct {
                name,
                type_params,
                bodied_methods,
                ..
            } => (name, type_params, bodied_methods),
            _ => continue,
        };
        for method in methods {
            map.entry(method.name.clone())
                .or_default()
                .push(method_spec(method, owner_type, type_params, id, file));
        }
    }
}

/// Program-wide struct-method specs keyed by `(struct_name, method_name)` — the receiver-type-aware
/// sibling of [`collect_methods`]. Used to resolve a method call's `ref` param flags when the
/// receiver's struct type is known locally (so `a.apply(r)` picks `A`'s `apply`, not a sibling
/// struct's same-named method). The receiver (`self`, params[0]) is dropped, like `collect_methods`.
fn collect_methods_by_struct(graph: &ModuleGraph) -> HashMap<(String, String), Vec<PSpec>> {
    // Value `None` marks a key whose per-module specs DISAGREE (a struct-name collision): dropped at
    // the end so the conflicting entry never drives a coercion decision.
    let mut map: HashMap<(String, String), Option<Vec<PSpec>>> = HashMap::new();
    for m in &graph.modules {
        for stmt in &m.ast.stmts {
            let (name, type_params, methods) = match &stmt.kind {
                StmtKind::Struct {
                    name,
                    type_params,
                    methods,
                    ..
                }
                | StmtKind::Enum {
                    name,
                    type_params,
                    methods,
                    ..
                }
                | StmtKind::NewType {
                    name,
                    type_params,
                    methods,
                    ..
                } => (name, type_params, methods),
                _ => continue,
            };
            {
                for method in methods {
                    let spec: Vec<PSpec> = method_spec(method, name, type_params, &m.id, m.file);
                    let key = (name.clone(), method.name.clone());
                    // Struct names are program-global (a reused name is a hard collision error in the
                    // checker), but two modules CAN parse a same-named struct. If their specs for the
                    // same method disagree we must NOT pick one by collection order — null the entry so
                    // resolution falls back to the name-keyed agreement check (which won't mis-coerce).
                    match map.entry(key) {
                        std::collections::hash_map::Entry::Vacant(v) => {
                            v.insert(Some(spec));
                        }
                        std::collections::hash_map::Entry::Occupied(mut o) => {
                            if o.get().as_ref() != Some(&spec) {
                                o.insert(None);
                            }
                        }
                    }
                }
            }
        }
    }
    map.into_iter()
        .filter_map(|(k, v)| v.map(|spec| (k, spec)))
        .collect()
}

/// Single-module [`collect_methods_by_struct`] for the standalone (test/compiler/interp) path.
#[cfg(test)]
fn collect_methods_by_struct_into_standalone(
    stmts: &[Stmt],
    id: &ModuleId,
    file: u32,
) -> HashMap<(String, String), Vec<PSpec>> {
    let mut map: HashMap<(String, String), Vec<PSpec>> = HashMap::new();
    for stmt in stmts {
        let (name, type_params, methods) = match &stmt.kind {
            StmtKind::Struct {
                name,
                type_params,
                methods,
                ..
            }
            | StmtKind::Enum {
                name,
                type_params,
                methods,
                ..
            }
            | StmtKind::NewType {
                name,
                type_params,
                methods,
                ..
            } => (name, type_params, methods),
            StmtKind::NativeStruct {
                name,
                type_params,
                bodied_methods,
                ..
            } => (name, type_params, bodied_methods),
            _ => continue,
        };
        for method in methods {
            let spec: Vec<PSpec> = method_spec(method, name, type_params, id, file);
            map.insert((name.clone(), method.name.clone()), spec);
        }
    }
    map
}

/// Reject any parameter/field default that references another parameter/field in the same signature.
/// A default is evaluated with NO parameter/field bound — in its own provider function, or as a
/// literal clone at the call site (see [`Dflt`]) — so a non-param-referencing expression
/// (`compute()`, `1 + 2`, `GLOBAL * 2`) is fine, but `y: int = x + 1` is not. Covers top-level
/// functions and struct methods/fields (the only places defaults are collected). Runs before
/// provider synthesis and the call-rewrite pass.
fn validate_defaults(stmts: &[Stmt]) -> Result<(), ResolveError> {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Fn(decl) => check_param_defaults(&decl.params)?,
            StmtKind::Struct {
                fields, methods, ..
            } => {
                let fnames: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                for fld in fields {
                    if let Some(d) = &fld.default
                        && let Some(n) = default_referenced_name(d, &fnames)
                    {
                        return Err(err(
                            d.span,
                            format!(
                                "default value cannot reference field '{n}' (a default is evaluated on its own, where fields are not in scope)"
                            ),
                        ));
                    }
                }
                for m in methods {
                    check_param_defaults(&m.params)?;
                }
            }
            StmtKind::Enum { methods, .. } | StmtKind::NewType { methods, .. } => {
                for m in methods {
                    check_param_defaults(&m.params)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reject a param default that references any parameter in the same list.
fn check_param_defaults(params: &[Param]) -> Result<(), ResolveError> {
    let names: HashSet<&str> = params.iter().map(|p| p.name.as_str()).collect();
    for p in params {
        if let Some(d) = &p.default
            && let Some(n) = default_referenced_name(d, &names)
        {
            return Err(err(
                d.span,
                format!(
                    "default value cannot reference parameter '{n}' (a default is evaluated on its own, where parameters are not in scope)"
                ),
            ));
        }
    }
    Ok(())
}

/// The first name in `names` referenced as an identifier anywhere in `e`, if any. A `Field`'s member
/// name and a `Closure`'s own params are not treated specially (conservative: a default reusing a
/// param name as a closure binding is rejected — a non-issue in practice).
fn default_referenced_name(e: &Expr, names: &HashSet<&str>) -> Option<String> {
    let mut found: Option<String> = None;
    walk_idents(e, &mut |n| {
        if found.is_none() && names.contains(n) {
            found = Some(n.to_string());
        }
    });
    found
}

/// Visit every identifier reference in an expression (a `Field`/`OptChain` member name is the member,
/// not a reference, so only the receiver is visited).
fn walk_idents(e: &Expr, f: &mut impl FnMut(&str)) {
    walk_idents_and_types(e, f, &mut |_| {});
}

/// [`walk_idents`] plus every **type** an expression spells: a turbofish's arguments
/// (`mk[T]()`, `obj?.m[T]()`), a type-application head's, a `decode[T](…)` target, and a closure's
/// parameter and return annotations. Only [`expr_mentions_type_param`] passes a non-empty `tf`;
/// every other caller goes through [`walk_idents`], whose `tf` is a no-op, so no existing name-set
/// check widened when the type channel was added.
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
        // A STILL-RAW interpolated literal. `validate_defaults` runs BEFORE the `Str -> Interp`
        // rewrite, so the only way to see the references a fragment makes is to parse it here. This
        // used to be skipped, with a comment claiming the checker caught such a reference later — it
        // does not: the decl-site copy is inferred with the parameters in scope, so
        // `fn f(n: int, x: str = "n={n}")` type-checked clean. That left the default meaning two
        // different things (the provider resolves `n` in MODULE scope, so a direct call printed the
        // global while a call through a function value printed the parameter), and, where no such
        // global existed at all, reached the backend as `compiler: global 'n' has no slot` — a host
        // panic on a check-clean program. A parse failure is ignored: the real parse reports it.
        ExprKind::Str(raw) => {
            if raw.contains('{')
                && let Ok(chunks) = crate::interpolation::parse_interpolation(raw, e.span)
            {
                for c in &chunks {
                    if let Chunk::Expr(inner, _) = c {
                        walk_idents_and_types(inner, f, tf);
                    }
                }
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_) => {}
        // A type-application head names a TYPE (not a value reference); its args are `Type`s.
        ExprKind::TypeApply { args, .. } => args.iter().for_each(tf),
        // A fragment identifier IS a reference (`"{a}"` reads `a`), so descend. Reached once
        // `desugar` has rewritten the literal; before that the raw-`Str` arm above parses it.
        ExprKind::Interp(chunks) => chunks.iter().for_each(|c| {
            if let Chunk::Expr(e, _) = c {
                walk_idents_and_types(e, f, tf)
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
        ExprKind::Range { start, end } => {
            walk_idents_and_types(start, f, tf);
            walk_idents_and_types(end, f, tf);
        }
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
        } => {
            walk_idents_and_types(callee, f, tf);
            args.iter().for_each(|a| walk_idents_and_types(a, f, tf));
            named
                .iter()
                .for_each(|(_, a)| walk_idents_and_types(a, f, tf));
            type_args.iter().for_each(&mut *tf);
        }
        ExprKind::Field { obj, .. } => walk_idents_and_types(obj, f, tf),
        ExprKind::Index { obj, index } => {
            walk_idents_and_types(obj, f, tf);
            walk_idents_and_types(index, f, tf);
        }
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            walk_idents_and_types(obj, f, tf);
            for c in [start, end, step].iter().filter_map(|c| c.as_deref()) {
                walk_idents_and_types(c, f, tf);
            }
        }
        ExprKind::Try(x) => walk_idents_and_types(x, f, tf),
        ExprKind::OptChain { obj, call, .. } => {
            walk_idents_and_types(obj, f, tf);
            if let Some(c) = call {
                c.args.iter().for_each(|a| walk_idents_and_types(a, f, tf));
                c.named
                    .iter()
                    .for_each(|(_, a)| walk_idents_and_types(a, f, tf));
                c.type_args.iter().for_each(&mut *tf);
            }
        }
        ExprKind::NullCoalesce { lhs, rhs } => {
            walk_idents_and_types(lhs, f, tf);
            walk_idents_and_types(rhs, f, tf);
        }
        ExprKind::DecodeCall { obj, ty, arg } => {
            walk_idents_and_types(obj, f, tf);
            tf(ty);
            walk_idents_and_types(arg, f, tf);
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
        ExprKind::Recover(_) => {}
    }
}

/// Program-wide set of struct **field** names whose declared type is a function (`f: fn(T) -> U`).
/// A `recv.f(args)` call on such a field parses identically to a method call; we use this set to keep
/// `normalize_call` from injecting a same-named *method*'s defaults into a fn-field call (the field
/// is field-access-then-call, resolved by the checker + engines, not a method). Spans all modules
/// since the receiver may be an imported struct's value.
fn collect_fn_fields(graph: &ModuleGraph) -> HashSet<String> {
    let mut set = HashSet::new();
    for m in &graph.modules {
        collect_fn_fields_into(&m.ast.stmts, &mut set);
    }
    set
}

fn collect_fn_fields_into(stmts: &[Stmt], set: &mut HashSet<String>) {
    for stmt in stmts {
        if let StmtKind::Struct { fields, .. } = &stmt.kind {
            for f in fields {
                if matches!(f.ty, Type::Func { .. }) {
                    set.insert(f.name.clone());
                }
            }
        }
    }
}

/// Build the callable registry (free functions + struct constructors) for one module's top level.
fn collect_module_reg(stmts: &[Stmt], id: &ModuleId, file: u32) -> ModReg {
    let mut reg = ModReg::default();
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Fn(decl) => {
                let tps = tp_names(decl, &[]);
                reg.fns.insert(
                    decl.name.clone(),
                    decl.params
                        .iter()
                        .map(|p| PSpec {
                            name: p.name.clone(),
                            default: p.default.as_ref().map(|d| {
                                dflt_for(
                                    d,
                                    p.ty.as_ref(),
                                    &tps,
                                    None,
                                    id,
                                    provider_name(file, Slot::Param, &decl.name, &p.name),
                                )
                            }),
                            is_variadic: p.is_variadic,
                        })
                        .collect(),
                );
            }
            StmtKind::Struct {
                name,
                type_params,
                fields,
                ..
            } => {
                let tps: Vec<String> = type_params.iter().map(|t| t.name.clone()).collect();
                reg.structs.insert(
                    name.clone(),
                    fields
                        .iter()
                        .map(|f| PSpec {
                            name: f.name.clone(),
                            default: f.default.as_ref().map(|d| {
                                dflt_for(
                                    d,
                                    Some(&f.ty),
                                    &tps,
                                    None,
                                    id,
                                    provider_name(file, Slot::Field, name, &f.name),
                                )
                            }),
                            is_variadic: false,
                        })
                        .collect(),
                );
            }
            _ => {}
        }
    }
    // Second pass: a free fn whose declared return type names a struct of THIS module records the
    // bare struct head (so `mk().m(r)` resolves `m` by the receiver's struct type pre-type). Done
    // after both maps are filled so a fn declared before its return struct still resolves.
    for stmt in stmts {
        if let StmtKind::Fn(decl) = &stmt.kind {
            let head = match &decl.ret {
                Some(Type::Named { name: n, .. }) | Some(Type::Generic(n, ..)) => Some(n.clone()),
                _ => None,
            };
            if let Some(h) = head
                && reg.structs.contains_key(&h)
            {
                reg.fn_ret_struct.insert(decl.name.clone(), h);
            }
        }
    }
    reg
}

/// Per-module resolution context (all borrows outlive the mutable AST walk).
struct Ctx<'a> {
    regs: &'a HashMap<ModuleId, ModReg>,
    own_id: &'a ModuleId,
    /// This module's **transitive import closure** ([`import_closures`]). A synthetic provider
    /// import is only legal to a module in here; see [`Walker::splice_default`].
    deps: &'a HashSet<ModuleId>,
    bare_from: &'a HashMap<String, ModuleId>,
    aliases: &'a HashMap<String, ModuleId>,
    /// Program-wide struct-method specs (see [`collect_methods`]).
    methods: &'a HashMap<String, Vec<Vec<PSpec>>>,
    /// Receiver-type-keyed struct-method specs (see [`collect_methods_by_struct`]). Lets a method
    /// call resolve its `ref` param flags from the receiver's struct type when that type is known
    /// locally — the precise sibling of `methods` (which is keyed by name only).
    methods_by_struct: &'a HashMap<(String, String), Vec<PSpec>>,
    /// Program-wide function-typed field names (see [`collect_fn_fields`]).
    fn_fields: &'a HashSet<String>,
}

impl Ctx<'_> {
    /// Resolve a bare name (`f(...)`) to a callable's param spec: own module first, then a
    /// `from`-imported name. Returns `None` for builtins, native-module members, or unknown names.
    fn resolve_bare(&self, name: &str) -> Option<&Vec<PSpec>> {
        if let Some(spec) = self.regs.get(self.own_id).and_then(|r| r.callable(name)) {
            return Some(spec);
        }
        let target = self.bare_from.get(name)?;
        self.regs.get(target).and_then(|r| r.callable(name))
    }

    /// Resolve a module-qualified name (`alias.f(...)`).
    fn resolve_qualified(&self, alias: &str, name: &str) -> Option<&Vec<PSpec>> {
        let target = self.aliases.get(alias)?;
        self.regs.get(target).and_then(|r| r.callable(name))
    }
}

struct Walker<'a> {
    ctx: Ctx<'a>,
    scopes: Vec<HashSet<String>>,
    /// Per-scope map of a LOCAL name to the struct type it was constructed/annotated as (parallel to
    /// `scopes`). Populated by `x := StructName(...)` and `x: StructName = ...`. Lets a method call
    /// `recv.m(args)` resolve `m`'s param defaults/variadic against the receiver's *actual* struct (so
    /// a sibling struct's same-named method does not derail the decision).
    local_struct: Vec<HashMap<String, String>>,
    /// Providers in OTHER modules this module's call sites now call, `name → (declaring module,
    /// first call site)`. Drained into synthetic `from` imports after the walk. A `BTreeMap` so the
    /// drain order is the (globally unique) provider name — import order feeds the compiler's
    /// `global_slots`, which must not depend on hash iteration order.
    needed: std::collections::BTreeMap<String, (ModuleId, Span)>,
    /// Current [`Walker::walk_expr`] recursion depth — see that method. This counter is what turns
    /// [`crate::parser::MAX_AST_DEPTH`] into a **global** bound instead of a per-`Parser` one.
    depth: usize,
}

impl Walker<'_> {
    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    fn bind(&mut self, name: &str) {
        if let Some(top) = self.scopes.last_mut() {
            top.insert(name.to_string());
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashSet::new());
        self.local_struct.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.local_struct.pop();
    }

    /// Record that LOCAL `name` holds a value of struct type `sname`, in the innermost scope.
    fn bind_local_struct(&mut self, name: &str, sname: &str) {
        if let Some(top) = self.local_struct.last_mut() {
            top.insert(name.to_string(), sname.to_string());
        }
    }

    /// If `ty` names a struct known to this module (a bare `Type::Named` or a `Type::Generic` head
    /// that resolves to a declared struct), return that struct name — so a typed parameter (`x: A`)
    /// records its receiver struct type just like a `x := A()` let binding. Mirrors the struct check
    /// in [`Self::struct_value_ty`].
    fn annot_struct_ty(&self, ty: &Type) -> Option<String> {
        let name = match ty {
            Type::Named { name, .. } => name,
            Type::Generic(name, ..) => name,
            _ => return None,
        };
        self.ctx
            .regs
            .get(self.ctx.own_id)
            .filter(|r| r.structs.contains_key(name))
            .map(|_| name.clone())
    }

    /// Bind a function/method parameter into the current scope, additionally recording its receiver
    /// struct type when the annotation names a known struct — so a typed-parameter receiver
    /// (`fn f(x: A): x.m(...)`) resolves through [`Self::receiver_struct_ty`] like a let-bound local.
    fn bind_param(&mut self, p: &Param) {
        self.bind(&p.name);
        if let Some(ty) = &p.ty
            && let Some(sname) = self.annot_struct_ty(ty)
        {
            self.bind_local_struct(&p.name, &sname);
        }
    }

    /// The struct type a local receiver `name` was constructed/annotated as (innermost wins).
    fn local_struct_ty(&self, name: &str) -> Option<&String> {
        for (vars, sts) in self.scopes.iter().zip(self.local_struct.iter()).rev() {
            if vars.contains(name) {
                return sts.get(name);
            }
        }
        None
    }

    /// If `value` is a bare struct-constructor call (`StructName(...)`), the struct's name — so a
    /// `x := StructName(...)` binding can later resolve a method call on `x` by receiver type.
    fn struct_value_ty(&self, value: &Expr) -> Option<String> {
        if let ExprKind::Call { callee, .. } = &value.kind
            && let ExprKind::Ident(n) = &callee.kind
            && !self.is_local(n)
            && self
                .ctx
                .regs
                .get(self.ctx.own_id)
                .is_some_and(|r| r.structs.contains_key(n))
        {
            return Some(n.clone());
        }
        None
    }

    /// The struct name of a method-call receiver `obj`, when knowable pre-type — so a shared method
    /// name (siblings disagreeing on a param's ref-ness) resolves to the RIGHT sibling regardless of
    /// the receiver's syntactic shape. Covers: (i) a named local, (ii) an inline ctor call
    /// `StructName(...)`, (iii) a free-fn call `mk()` whose declared return type is a struct. Returns
    /// `None` for any receiver whose struct type cannot be determined syntactically (the caller then
    /// falls back to the agreement-gated name-keyed table).
    fn receiver_struct_ty(&self, obj: &Expr) -> Option<String> {
        match &obj.kind {
            // (i) a named local receiver: its constructed/annotated struct type.
            ExprKind::Ident(recv) if self.is_local(recv) => self.local_struct_ty(recv).cloned(),
            // (ii) inline ctor call `StructName(...)` — struct head is syntactic.
            ExprKind::Call { .. } if self.struct_value_ty(obj).is_some() => {
                self.struct_value_ty(obj)
            }
            // (iii) struct-returning free fn `mk()` — resolved through the SAME module the callee
            // resolves in (own module first, then a `from`-import), mirroring `resolve_bare`.
            ExprKind::Call { callee, .. } => {
                let ExprKind::Ident(n) = &callee.kind else {
                    return None;
                };
                if self.is_local(n) {
                    return None;
                }
                if let Some(s) = self
                    .ctx
                    .regs
                    .get(self.ctx.own_id)
                    .and_then(|r| r.fn_ret_struct.get(n))
                {
                    return Some(s.clone());
                }
                let target = self.ctx.bare_from.get(n)?;
                self.ctx
                    .regs
                    .get(target)
                    .and_then(|r| r.fn_ret_struct.get(n))
                    .cloned()
            }
            _ => None,
        }
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
                names,
                name_spans: _,
                ty,
                value,
                // `const` is not lowered here (compile-time-only; the checker enforces it) — ignore.
                is_const: _,
                doc: _,
            } => {
                // Record the RHS's struct type (a `x: StructName = ...` annotation or a
                // `x := StructName(...)` ctor call) so a later method call on `x` resolves its
                // param defaults/variadic against the receiver's actual struct.
                let struct_ty = if names.len() == 1 {
                    match ty {
                        Some(Type::Named { name: n, .. }) => Some(n.clone()),
                        _ => self.struct_value_ty(value),
                    }
                } else {
                    None
                };
                self.walk_expr(value)?;
                for n in names.iter() {
                    self.bind(n);
                }
                if let Some(sname) = struct_ty {
                    self.bind_local_struct(&names[0], &sname);
                }
            }
            StmtKind::Assign { target, value, op: _ } => {
                self.walk_expr(target)?;
                self.walk_expr(value)?;
            }
            StmtKind::Fn(decl) => {
                // The DECL-SITE copy of each default is normalized here, outside the param scope
                // (no param is bound where a default runs; `validate_defaults` guarantees it
                // references none). This copy is what the checker type-checks against the param's
                // declared type, and what `compile_suite_new_thunk` compiles for a test suite's
                // fields; the provider carries an independent copy of the same expression.
                for p in decl.params.iter_mut() {
                    if let Some(d) = &mut p.default {
                        self.walk_expr(d)?;
                    }
                }
                // Nested/top-level function body: params are a fresh scope.
                self.push_scope();
                for p in &decl.params {
                    self.bind_param(p);
                }
                self.walk_block(&mut decl.body)?;
                self.pop_scope();
            }
            StmtKind::Struct {
                fields, methods, ..
            } => {
                // Field defaults: normalize the decl-site copy like param defaults (outside any
                // scope; they reference no field, per `validate_defaults`).
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
                    self.push_scope();
                    for p in &m.params {
                        self.bind_param(p);
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
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
            StmtKind::For {
                vars, iter, body, ..
            } => {
                self.walk_expr(iter)?;
                self.push_scope();
                for v in vars.iter() {
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
                    match &mut arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            self.walk_expr(chan)?;
                            if let WaitTarget::Assign(e) = target {
                                self.walk_expr(e)?;
                            }
                        }
                        WaitArmKind::Send { call } => self.walk_expr(call)?,
                    }
                    self.walk_block(&mut arm.body)?;
                }
                if let Some(b) = else_block {
                    self.walk_block(b)?;
                }
            }
            // Enum AND newtype method bodies (and param defaults) are rewritten exactly like a
            // struct's; neither has fields to splice.
            StmtKind::Enum { methods, .. } | StmtKind::NewType { methods, .. } => {
                for m in methods.iter_mut() {
                    for p in m.params.iter_mut() {
                        if let Some(d) = &mut p.default {
                            self.walk_expr(d)?;
                        }
                    }
                    self.push_scope();
                    for p in &m.params {
                        self.bind_param(p);
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
                }
            }
            // A `native struct`'s BODIED Chezzi methods ARE compiled to bytecode, so their bodies +
            // param defaults must be desugared exactly like an enum/struct method (default/named-arg
            // normalization, `ref` lowering). The bodyless `native fn` sigs alongside them have nothing.
            StmtKind::NativeStruct { bodied_methods, .. } => {
                for m in bodied_methods.iter_mut() {
                    for p in m.params.iter_mut() {
                        if let Some(d) = &mut p.default {
                            self.walk_expr(d)?;
                        }
                    }
                    self.push_scope();
                    for p in &m.params {
                        self.bind_param(p);
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
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
            | StmtKind::TypeAlias { .. } => {}
        }
        Ok(())
    }

    /// **THE GLOBAL AST-DEPTH BOUND** (W7-50). [`crate::parser::MAX_AST_DEPTH`] is enforced by the
    /// `Parser` that builds a tree, and an interpolated `{…}` fragment is built by a *different*
    /// `Parser` — `interpolation::parse_expr_str` re-lexes the fragment text and calls
    /// [`crate::parser::parse_expr`], whose `depth`/`fold_depth` start at zero. So before this guard
    /// the budgets **composed**: each nesting level of `"{ <15 985 deep> }".len()` bought a fresh
    /// 16 000, and three levels type-checked clean at ~46 000 AST nodes — past the measured ~33 100
    /// node cliff of the binding walker, i.e. an uncatchable SIGABRT on a well-typed program
    /// (`chezzi run`, debug, on the 384 MiB [`crate::vm::VM_STACK_BYTES`] worker).
    ///
    /// [`Self::walk_expr_inner`] is the seam where that composition physically happens: its
    /// `ExprKind::Str` arm calls `parse_interpolation` and then **re-enters this walk on the
    /// fragment's subtree**, so one `Walker` descends the whole composed tree. Measured on the
    /// three-level fixture, pre-guard: peak `walk_expr` depth 15 000 / 30 000 / 45 000 for one, two
    /// and three levels — exactly the sum. That makes this counter the depth of the tree the checker
    /// and the compiler descend afterwards, not a per-parse estimate of it, which is why the bound
    /// lives here rather than as a remaining-budget parameter threaded through the re-parse: there is
    /// one number, and no caller can forget to pass it. Measured after: total accepted depth is
    /// ~16 000 at one, two, three and four nesting levels alike, where it used to be L × 16 000.
    ///
    /// **Every front-end path routes through here.** `resolver::build_graph` ends in
    /// [`run`], and `chezzi check` / `run` / `test` and the LSP all go through `build_graph` — for
    /// `chezzi run` on the VM thread too, *before* the compile walk. (`chezzi ast` and the LSP's
    /// `semantic_overlay` parse without the resolver, but both treat `ExprKind::Str` as a LEAF, so
    /// they never descend a fragment at all.) The three other `parse_interpolation` callers —
    /// `checker::check_interpolation`, `checker::scan_expr_for_pin`, `compiler::compile_str` /
    /// `interp_exprs` — fire only on an `ExprKind::Str` this walk did not convert.
    ///
    /// **The W7-50 residual is CLOSED by W7-51 — measured, not argued.** Until then there was one
    /// way an *un-converted but well-formed* `Str` could survive to those callers: a default
    /// argument spliced in on the driver's **second pass**, after this walk had gone past it, with
    /// no third pass to catch it. There is now no such splice. A non-literal default is never
    /// cloned at all (the call site gets a call to its provider, whose body is walked as an
    /// ordinary top-level `fn`), and the literal class that IS cloned excludes any `Str` carrying
    /// `{`/`}` *and* is re-walked by [`Self::splice_default`] anyway.
    ///
    /// Measured on the same fixture the residual was recorded with —
    /// `fn g(a: int = "{ 1+1×15990 }".len())` / `fn h(b: int = g())` / `x := h()+1×15990` — with a
    /// temporary probe on `checker::check_interpolation`'s success arm (which fires exactly when a
    /// well-formed `Str` reached the checker un-converted): **`925dd0f7`: 1 hit**, peak walk depth
    /// 15 995, i.e. the ~31 986-node composed tree. **Here: 0 hits**, peak walk depth 15 994, and
    /// `chezzi run` prints `15995`. The counter is therefore now an upper bound on the tree the
    /// checker and compiler descend, which is what the bound was for.
    ///
    /// **Non-interpolated programs are unaffected**, bisected before and after: double fold *k* = 16,
    /// flat fold 15 997, postfix 15 996, composed `f(g(…)+1×99)` 127, parens 254 — all identical. An
    /// interpolated literal is now charged for the nodes it hangs beneath (`.len()`, the `Interp`
    /// itself), so a fragment within ~4 nodes of the ceiling is refused where the parser alone
    /// accepted it; that is the bound doing its job, not slack. Statement nesting cannot compose (a
    /// `{…}` fragment holds an expression, never a block) and is bounded by `parser::MAX_DEPTH`.
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
        // Recurse into children first, so nested calls are normalized regardless of this node.
        match &mut expr.kind {
            ExprKind::Unary { expr: inner, .. } => self.walk_expr(inner)?,
            ExprKind::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs)?;
                self.walk_expr(rhs)?;
            }
            ExprKind::Range { start, end } => {
                self.walk_expr(start)?;
                self.walk_expr(end)?;
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
            ExprKind::Index { obj, index } => {
                self.walk_expr(obj)?;
                self.walk_expr(index)?;
            }
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => {
                self.walk_expr(obj)?;
                for c in [start, end, step].into_iter().flatten() {
                    self.walk_expr(c)?;
                }
            }
            ExprKind::Try(inner) => self.walk_expr(inner)?,
            ExprKind::DecodeCall { obj, arg, .. } => {
                self.walk_expr(obj)?;
                self.walk_expr(arg)?;
            }
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
            ExprKind::Recover(block) => self.walk_block(block)?,
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
                    for v in clause.vars.iter() {
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
                ..
            } => {
                self.walk_expr(callee)?;
                for a in args.iter_mut() {
                    self.walk_expr(a)?;
                }
                for (_, v) in named.iter_mut() {
                    self.walk_expr(v)?;
                }
            }
            // W7-43 — optional chaining `?.` / null-coalescing `??` SURVIVE this pass: the choice
            // between the Option lowering and the Result (`?` then `.`) one needs the operand's
            // TYPE, which only the checker has. Walk the children like any other node, then
            // normalize the `?.` call part explicitly (see `normalize_opt_call` — the carrier no
            // longer becomes a `Call` here, so `walk_expr`'s tail can't do it).
            ExprKind::NullCoalesce { lhs, rhs } => {
                self.walk_expr(lhs)?;
                self.walk_expr(rhs)?;
            }
            ExprKind::OptChain { obj, call, .. } => {
                self.walk_expr(obj)?;
                if let Some(c) = call {
                    for a in c.args.iter_mut() {
                        self.walk_expr(a)?;
                    }
                    for (_, v) in c.named.iter_mut() {
                        self.walk_expr(v)?;
                    }
                }
                self.normalize_opt_call(expr)?;
            }
            ExprKind::Ident(_) => {}
            // A string literal carrying `{…}` is PARSED HERE, once, into `ExprKind::Interp` — before
            // the normalization below runs. That is the whole point: a fragment call gets named
            // args / defaults / variadic sweeping exactly like any other call, in THIS scope (so a
            // local shadowing a fn name still wins), instead of being re-parsed after the pass by
            // each consumer. A malformed interpolation stays an `ExprKind::Str`, so the checker and
            // compiler still report it with their existing message and span.
            ExprKind::Str(raw) if raw.contains('{') || raw.contains('}') => {
                if let Ok(chunks) = crate::interpolation::parse_interpolation(raw, expr.span) {
                    expr.kind = ExprKind::Interp(chunks);
                    // `walk_expr_inner`, NOT `walk_expr`: this is a re-entry on the SAME node, which
                    // occupies one AST level, not two. Going back through the depth guard charged an
                    // extra level per interpolation and measurably over-rejected — a lone
                    // `x := "{ 1+1×15997 }".len()` at the parser's own flat ceiling stopped building.
                    return self.walk_expr_inner(expr);
                }
            }
            ExprKind::Interp(chunks) => {
                // No re-anchoring to the string literal: a fragment is re-lexed against the
                // literal's `PosMap`, so its own span is the real physical source position (and the
                // one the checker and compiler report too). See `interpolation::parse_interpolation`.
                for c in chunks.iter_mut() {
                    if let crate::ast::Chunk::Expr(e, _) = c {
                        self.walk_expr(e)?;
                    }
                }
            }
            // Leaves.
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            // A type-application head holds only `Type`s — nothing to walk; the checker consumes it.
            | ExprKind::TypeApply { .. }
            | ExprKind::Bool(_) => {}
        }

        // Now normalize this node if it is a resolvable call.
        if let ExprKind::Call { .. } = &expr.kind {
            self.normalize_call(expr)?;
        }
        Ok(())
    }

    /// Materialise one omitted argument into `out` — the single replacement for the three
    /// `default.clone()` sites this pass used to have.
    ///
    /// [`Dflt::Inline`] still clones, but is WALKED here, in the caller's own walk: that charges the
    /// composed tree's [`crate::parser::MAX_AST_DEPTH`] budget for the clone (a spliced default nests
    /// inside the expression around it) and means the single pass never has to assume a literal needs
    /// no lowering. [`Dflt::Provider`] emits a complete zero-arg call — nothing left to rewrite.
    ///
    /// **The cross-module edge is a DEPENDENCY rule, and where it does not hold the default falls
    /// back to the caller-scope clone.** A synthetic import to the definer is emitted only when the
    /// definer is in this module's transitive import closure ([`import_closures`]); otherwise the
    /// call site gets [`Dflt::Inline`]'s clone of the same expression, which is exactly what
    /// `b1307258` did for every default. The rule is a dependency one and not a load-order one
    /// because the predicate must not read import ORDER, or a cosmetic reorder in a third file flips
    /// the behaviour of an unrelated call. Measured, three files, only `main`'s two import lines
    /// swapped (`main` imports `z` and `a`; `a` declares `struct S: fn mprobe(self, x: int = av())`
    /// and `fn av() -> int: return 11`; `z` declares its own `av() -> 500` and calls `p.mprobe()`
    /// through a protocol-typed param): with a load-order predicate, `import z` / `import a` was a
    /// compile error while `import a` / `import z` printed `11`. Under the closure rule both orders
    /// behave the same.
    ///
    /// **The fallback is not safe; it is the lesser of two evils, chosen deliberately.** In the
    /// program above the clone resolves `av` in `z`, so the call prints `500` where the definer
    /// wrote `11` — a silent wrong value, the very defect W7-51 exists to fix. It is still the right
    /// call here, because the alternative refuses a shape with no workaround: the path that reaches
    /// this is the name-keyed METHOD path, which resolves `recv.m()` by method NAME across every
    /// module in the graph, so the definer need not be related to the caller at all. That is the
    /// ordinary protocol/implementation split — `z` declares `protocol P` and takes a `P`, `a`
    /// declares the struct that satisfies it — and it cannot know the receiver's module. The remedy
    /// a refusal would suggest (make `z` import `a`) is an import cycle whenever `a` imports `z` for
    /// the protocol, so refusing made a defaulted method argument unusable through a protocol at
    /// all: measured, that program printed `12` on `b1307258` and on CPython, and was refused with
    /// `cannot use the default … does not import` between `e2d9bd4e` and `dfdc7a1b`. Refusing a
    /// working, ancestor-agreeing program is the larger harm; the caller-scope corner is narrow (a
    /// method default whose free names resolve to something DIFFERENT in the caller) and is
    /// documented in `docs/syntax.md` §5 and `docs/gaps.md` W7-51.
    ///
    /// Where the definer IS reachable — every same-module call, and every cross-module call that
    /// imports the definer, which is the common case — the provider is used and the default resolves
    /// in its own module.
    ///
    /// **Load-order safety.** `Vm::bind_import` indexes `self.module_objs[target_idx]`, a `Vec`
    /// pushed as each module RUNS, so an edge to a module that has not run yet panics. A transitive
    /// dependency always loads first — see the topological-order argument and its `debug_assert` in
    /// [`import_closures`] — so a synthetic edge can never outrun its target.
    fn splice_default(
        &mut self,
        d: &Dflt,
        site: Span,
        out: &mut Vec<Expr>,
    ) -> Result<(), ResolveError> {
        match d {
            // Never reached: `normalize_call` handles this by NOT calling here (the argument is
            // simply omitted and the callee fills it). Kept exhaustive so a future producer of this
            // variant cannot silently fall into the clone path.
            Dflt::CalleeFilled => {
                debug_assert!(
                    false,
                    "a callee-filled default must not be spliced at the call site"
                );
            }
            Dflt::Inline(e) => {
                let mut e = e.clone();
                self.walk_expr(&mut e)?;
                out.push(e);
            }
            Dflt::Provider { module, name } => {
                // A cross-module provider is reached one of two ways, and BOTH resolve in the
                // definer's namespace — the difference is only how the caller names it.
                //
                //   * **In this module's transitive import closure** — synthesize a `from` import
                //     and call the bound name. The checker types the call fully.
                //   * **Out of the closure** — no import may be synthesized (`Vm::bind_import`
                //     resolves its target when the CALLER's module loads, and a non-dependency can
                //     load later), so nothing is recorded here: the compiler lowers the bare
                //     provider ident to a direct, call-time reference to the definer's proto
                //     (`Op::MakeFuncIn`), and the checker types the call from the parameter slot it
                //     fills. This is the path the name-keyed METHOD lookup reaches — the ordinary
                //     protocol/implementation split, where the definer need not be related to the
                //     caller at all.
                if module != self.ctx.own_id && self.ctx.deps.contains(module) {
                    self.needed
                        .entry(name.clone())
                        .or_insert_with(|| (module.clone(), site));
                }
                out.push(Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(ident_expr(name, site)),
                        args: Vec::new(),
                        named: Vec::new(),
                        type_args: Vec::new(),
                    },
                    span: site,
                });
            }
        }
        Ok(())
    }

    /// W7-43 — normalize the CALL PART of an optional-chained method call (`obj?.m(args)`): named →
    /// positional binding, omitted defaults, variadic collapse.
    ///
    /// Before W7-43 the carrier was lowered here and its arm body — a real `Call { callee: Field }` —
    /// fell through [`Self::walk_expr`]'s tail into [`Self::normalize_call`]. The carrier now survives
    /// the pass, so nothing would otherwise normalize it and `u?.greet(greeting="hi")` would reach the
    /// checker with `named` un-rewritten. Runs `normalize_call` on the exact `Call { callee: Field }`
    /// shape both `lower_carrier_*` build, then moves the (possibly rewritten) args back.
    ///
    /// The synthetic receiver is the REAL `obj`, not a placeholder: `receiver_struct_ty` yields `None`
    /// for an `Option`/`Result`-typed receiver in every reachable case (an `Option` annotation is
    /// never `Type::Named`, and `Some(...)`/`Ok(...)` is not a registered struct ctor), so this
    /// reproduces the old `__optN` receiver's behaviour exactly — while not being WRONG if
    /// `local_struct` ever learns to track more receivers.
    fn normalize_opt_call(&mut self, expr: &mut Expr) -> Result<(), ResolveError> {
        let span = expr.span;
        let ExprKind::OptChain {
            obj,
            name,
            name_span,
            call: Some(c),
        } = &mut expr.kind
        else {
            return Ok(());
        };
        let mut tmp = Expr {
            kind: ExprKind::Call {
                callee: Box::new(Expr {
                    kind: ExprKind::Field {
                        obj: obj.clone(),
                        name: name.clone(),
                        name_span: *name_span,
                    },
                    span,
                }),
                args: std::mem::take(&mut c.args),
                named: std::mem::take(&mut c.named),
                type_args: c.type_args.clone(),
            },
            span,
        };
        let res = self.normalize_call(&mut tmp);
        if let ExprKind::Call { args, named, .. } = tmp.kind {
            c.args = args;
            c.named = named;
        }
        res
    }

    /// Resolve `expr` (a `Call`) to a callable and rewrite named/omitted args into positional. Leaves
    /// the call untouched when the callee is not a registered callable (unless it carries named args,
    /// which is then an error).
    fn normalize_call(&mut self, expr: &mut Expr) -> Result<(), ResolveError> {
        let span = expr.span;
        let ExprKind::Call {
            callee,
            args,
            named,
            ..
        } = &expr.kind
        else {
            return Ok(());
        };
        // The ORIGIN stamp for a synthesized variadic pack (see `ExprKind::List`). Captured here, in
        // the immutable borrow, because the collapse below re-borrows `expr.kind` mutably. It is the
        // callee's own token — the one component that stays distinct per link of a pipe/postfix chain,
        // where the CALL span does not.
        let pack_origin = crate::checker::witness_key_span(callee, span);

        // Resolve a free function / struct ctor / module-qualified callee (clone the spec so we can
        // then mutate `expr`).
        let module_spec: Option<Vec<PSpec>> = match &callee.kind {
            ExprKind::Ident(name) if !self.is_local(name) => self.ctx.resolve_bare(name).cloned(),
            ExprKind::Field { obj, name, .. } => match &obj.kind {
                ExprKind::Ident(alias) if !self.is_local(alias) => {
                    self.ctx.resolve_qualified(alias, name).cloned()
                }
                _ => None,
            },
            _ => None,
        };

        // Otherwise, a method call `recv.m(...)`: resolve `m`'s params by name across user structs
        // (the receiver type is unknown in this pre-type pass). Builtin/core method names are skipped
        // — their receiver may be a list/str/map/set. When several structs define `m` with *different*
        // params, a named call can't be bound unambiguously, so that is an error; a plain (no-named)
        // call is left untouched for the checker rather than guessing a default fill.
        // Field-aware: a `recv.f(...)` call where `f` is a function-typed *field* also parses as a
        // `Field` callee but is field-access-then-call (resolved by the checker + engines), not a
        // method. Skip method-default normalization for such names so a same-named method's default
        // can't be injected into a fn-field call.
        let method_spec: Option<Vec<PSpec>> = match (&module_spec, &callee.kind) {
            (None, ExprKind::Field { obj, name, .. }) if !self.ctx.fn_fields.contains(name) => {
                // Receiver-aware FIRST: when the receiver's struct type is statically knowable
                // (a typed local/param, an inline ctor call, or a struct-returning fn — see
                // `receiver_struct_ty`), bind `m` against THAT exact struct's spec. This is the ONLY
                // path that resolves a call when several structs define `m` with DIFFERENT parameter
                // lists (a variadic method next to a fixed-arity sibling, or two variadics differing
                // only in the variadic param's NAME): the name-keyed `methods` table below bails on
                // any disagreement, so without this a valid variadic method call would reach the
                // checker uncollapsed and be rejected against its single `List[T]` slot.
                let recv_spec = self.receiver_struct_ty(obj).and_then(|sname| {
                    self.ctx
                        .methods_by_struct
                        .get(&(sname, name.clone()))
                        .cloned()
                });
                if recv_spec.is_some() {
                    recv_spec
                } else if is_builtin_method(name) {
                    // A builtin-named method (`add`, `map`, `push`, …) with an unknowable receiver:
                    // the receiver might be a genuine builtin value (List/Set/Map/str), so there is
                    // NO name-keyed fallback that could mis-bind a builtin call.
                    None
                } else {
                    match self.ctx.methods.get(name.as_str()) {
                        Some(cands) if !cands.is_empty() => {
                            if cands.iter().all(|c| *c == cands[0]) {
                                Some(cands[0].clone())
                            } else if !named.is_empty() {
                                return Err(err(
                                    span,
                                    format!(
                                        "cannot bind named arguments for method '{name}': multiple structs define it with different parameters — pass arguments positionally"
                                    ),
                                ));
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                }
            }
            _ => None,
        };

        let Some(params) = module_spec.or(method_spec) else {
            // Not a registered callable. Named args here are unsupported (closures / builtin methods)
            // — EXCEPT the `print` builtin, which accepts `sep=`/`end=` (str expressions). For print
            // we validate the keys here and LEAVE them in `named` (un-rewritten) so the checker and
            // the compiler can read them off the Call AST.
            if !named.is_empty() {
                if let ExprKind::Ident(n) = &callee.kind
                    && n == "print"
                    && !self.is_local(n)
                {
                    let mut seen_sep = false;
                    let mut seen_end = false;
                    for (k, _) in named.iter() {
                        let dup = match k.as_str() {
                            "sep" => std::mem::replace(&mut seen_sep, true),
                            "end" => std::mem::replace(&mut seen_end, true),
                            _ => {
                                return Err(err(
                                    span,
                                    "print() only accepts the named arguments 'sep' and 'end'"
                                        .to_string(),
                                ));
                            }
                        };
                        if dup {
                            return Err(err(
                                span,
                                "print() only accepts the named arguments 'sep' and 'end'"
                                    .to_string(),
                            ));
                        }
                    }
                    // Keys are valid (subset of {sep,end}, no dups): keep `named` intact.
                    return Ok(());
                }
                // A method call whose name collides with a builtin, where the receiver's struct type
                // is NOT statically knowable (an unannotated param, an inferred enum value, or a
                // genuine builtin receiver). Named/default support needs a known receiver — say so,
                // instead of the misleading "only supported on … struct methods" (it IS a method).
                if let ExprKind::Field { name, .. } = &callee.kind
                    && is_builtin_method(name)
                    && !self.ctx.fn_fields.contains(name)
                {
                    return Err(err(
                        span,
                        format!(
                            "method '{name}' reuses a built-in method name, so named/default arguments can't be bound here unless the receiver's struct type is statically known — if it's a user-struct method, bind the receiver to a typed local or inline constructor; a built-in method takes no named arguments"
                        ),
                    ));
                }
                // A genuine call through a first-class function VALUE reached by an Ident (a local /
                // param bound to a fn) or an arbitrary expression, carrying keyword arguments
                // (`g(name="Bob")`, Swift-style). LEAVE the named args intact so the checker can
                // resolve each label against the value's labelled function type and record the
                // positional permutation for the backends. A METHOD-syntax callee (`recv.f(name=…)`)
                // still routes to the method path, which does not resolve value keywords — keep the
                // historical error for it (no silent drop of a keyword).
                if !matches!(&callee.kind, ExprKind::Field { .. }) {
                    return Ok(());
                }
                return Err(err(
                    span,
                    "named arguments are only supported on functions, struct constructors, and struct methods"
                        .to_string(),
                ));
            }
            return Ok(());
        };

        // A VARIADIC callable (`fn f(pre..., ...xs: T, kwonly...)`) collapses the surplus trailing
        // positionals into a synthesized `List` literal at the variadic slot, and binds the
        // keyword-only tail (everything after the variadic) from named args / defaults. After this the
        // call is an ordinary fully-positional call, so the checker AND the compiler need zero
        // variadic-specific logic (correct by construction). This must run BEFORE the fixed-arity gates
        // below (a `f(1,2,3)` into a single `List` slot is "too many" by those rules).
        // Un-gated since W7-51: the driver walks each module ONCE, so the collapse (which is not
        // idempotent — a second run would wrap the synthesized `List` in another `List`) fires
        // exactly once by construction.
        if let Some(v) = params.iter().position(|p| p.is_variadic) {
            let ExprKind::Call { args, named, .. } = &mut expr.kind else {
                return Ok(());
            };
            let mut positional: Vec<Option<Expr>> =
                std::mem::take(args).into_iter().map(Some).collect();
            let named_list = std::mem::take(named);
            let mut out: Vec<Expr> = Vec::with_capacity(params.len());
            // Pre-variadic slots (indices 0..v): one positional each, else its default, else error.
            for (i, pspec) in params.iter().enumerate().take(v) {
                let supplied = positional.get_mut(i).and_then(Option::take);
                match supplied {
                    Some(e) => out.push(e),
                    None => match &pspec.default {
                        Some(d) => self.splice_default(d, span, &mut out)?,
                        None => {
                            return Err(err(
                                span,
                                format!("missing required argument '{}'", pspec.name),
                            ));
                        }
                    },
                }
            }
            // The variadic slot sweeps EVERY remaining positional (index >= v) into a `List` literal —
            // so a positional can never land in a keyword-only slot.
            let elems: Vec<Expr> = positional.into_iter().skip(v).flatten().collect();
            out.push(Expr {
                // `Some(..)` marks this as the synthesized pack, NOT a list the user wrote: `span` is
                // the CALL's, which a pipe shares with the LHS primary, so the pack and a piped list
                // literal would otherwise key the same `ListWidenTable` slot. See `ExprKind::List`.
                kind: ExprKind::List(elems, Some(pack_origin)),
                span,
            });
            // Keyword-only tail (indices v+1..): named args may name ONLY these slots. Naming the
            // variadic itself or a pre-variadic slot is an error (they are positional).
            let mut kw: HashMap<String, Expr> = HashMap::new();
            for (n, e) in named_list {
                match params.iter().position(|p| p.name == n) {
                    None => return Err(err(span, format!("unknown named argument '{n}'"))),
                    Some(idx) if idx <= v => {
                        return Err(err(
                            span,
                            format!(
                                "argument '{n}' is positional (it is at or before the variadic parameter) and cannot be passed by name"
                            ),
                        ));
                    }
                    Some(_) => {
                        if kw.insert(n.clone(), e).is_some() {
                            return Err(err(span, format!("duplicate named argument '{n}'")));
                        }
                    }
                }
            }
            for pspec in params.iter().skip(v + 1) {
                if let Some(e) = kw.remove(&pspec.name) {
                    out.push(e);
                } else if let Some(d) = &pspec.default {
                    self.splice_default(d, span, &mut out)?;
                } else {
                    return Err(err(
                        span,
                        format!("missing required keyword argument '{}'", pspec.name),
                    ));
                }
            }
            *args = out;
            return Ok(());
        }

        // Decide whether this call needs rewriting. Plain positional calls whose arity is wrong (too
        // many, or too few without defaults to fill) are left untouched so the type checker reports
        // its usual arity error. We only rewrite when there are named args, or when every omitted
        // trailing slot has a default to fill.
        let under_arity_fillable = args.len() < params.len()
            && (args.len()..params.len()).all(|i| params[i].default.is_some());
        if named.is_empty() && !under_arity_fillable {
            return Ok(());
        }
        // Named args present alongside too many positional ones: a clear error.
        if args.len() > params.len() {
            return Err(err(
                span,
                format!(
                    "too many arguments: expected at most {}, got {}",
                    params.len(),
                    args.len()
                ),
            ));
        }

        // Re-borrow mutably to take ownership of the existing arg lists.
        let ExprKind::Call { args, named, .. } = &mut expr.kind else {
            return Ok(());
        };
        let positional = std::mem::take(args);
        let named_list = std::mem::take(named);
        let np = positional.len();

        let mut slots: Vec<Option<Expr>> = (0..params.len()).map(|_| None).collect();
        for (i, a) in positional.into_iter().enumerate() {
            slots[i] = Some(a);
        }
        for (n, e) in named_list {
            let Some(idx) = params.iter().position(|p| p.name == n) else {
                return Err(err(span, format!("unknown named argument '{n}'")));
            };
            if idx < np {
                return Err(err(
                    span,
                    format!("argument '{n}' specified both positionally and by name"),
                ));
            }
            if slots[idx].is_some() {
                return Err(err(span, format!("duplicate named argument '{n}'")));
            }
            slots[idx] = Some(e);
        }

        // Build the positional list. A `Dflt::CalleeFilled` slot contributes NOTHING: the callee's
        // own prologue fills it, so the call is simply short by that many trailing arguments.
        let mut out: Vec<Option<Expr>> = Vec::with_capacity(params.len());
        for (i, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(e) => out.push(Some(e)),
                None => match &params[i].default {
                    Some(Dflt::CalleeFilled) => out.push(None),
                    Some(d) => {
                        let mut one = Vec::new();
                        self.splice_default(d, span, &mut one)?;
                        out.extend(one.into_iter().map(Some));
                    }
                    None => {
                        return Err(err(
                            span,
                            format!("missing required argument '{}'", params[i].name),
                        ));
                    }
                },
            }
        }
        // Drop the trailing run of callee-filled slots — that is exactly what a short call encodes.
        while matches!(out.last(), Some(None)) {
            out.pop();
        }
        // Anything still unfilled now sits BEFORE a supplied argument, which a short call cannot
        // express (it pushes fewer values; it cannot leave a gap). Refuse instead of falling back to
        // the caller-scope clone: that clone resolving in the caller is the defect this design
        // removes, and it must not survive in the one corner nobody looks at. Reachable only by a
        // KEYWORD call that supplies a later parameter while omitting a `Self`-on-a-generic-host
        // default — the parser already forbids a required parameter after a defaulted one, so
        // positional calls can never produce this shape.
        if let Some(i) = out.iter().position(Option::is_none) {
            let later = params
                .iter()
                .enumerate()
                .skip(i + 1)
                .find(|(j, _)| out.get(*j).is_some_and(Option::is_some))
                .map(|(_, p)| p.name.clone())
                .unwrap_or_default();
            return Err(err(
                span,
                format!(
                    "the default for '{}' is filled by the callee and can only be omitted from the END of a call, but '{later}' is supplied after it — pass '{}' explicitly",
                    params[i].name, params[i].name
                ),
            ));
        }
        *args = out
            .into_iter()
            .map(|e| e.expect("no holes remain"))
            .collect();
        Ok(())
    }
}

/// Collect the binding names introduced by a `match` pattern.
fn bind_pattern(pat: &Pattern, f: &mut impl FnMut(String)) {
    match pat {
        Pattern::Ident(n, _) => f(n.clone()),
        Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) | Pattern::Or(bindings) => {
            for b in bindings {
                bind_pattern(b, f);
            }
        }
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
fn variant_pat(name: &str, bindings: Vec<Pattern>) -> Pattern {
    Pattern::Variant {
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
/// function, so the synthesized spans (and therefore the `KeywordKey`/`WitnessKey`s derived from
/// them) cannot drift between consumers.
pub fn lower_carrier_option(expr: &mut Expr, tmp: usize) {
    let span = expr.span;
    let c = format!("__opt{tmp}");
    let kind = std::mem::replace(&mut expr.kind, ExprKind::Bool(false));
    expr.kind = match kind {
        ExprKind::NullCoalesce { lhs, rhs } => ExprKind::Match {
            scrutinee: lhs,
            arms: vec![
                MatchExprArm {
                    pattern: variant_pat("Some", vec![Pattern::Ident(c.clone(), Span::default())]),
                    guard: None,
                    body: ident_expr(&c, span),
                },
                MatchExprArm {
                    pattern: variant_pat("None", vec![]),
                    guard: None,
                    body: *rhs,
                },
            ],
        },
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
                kind: ExprKind::Field {
                    obj: Box::new(ident_expr(&c, span)),
                    name,
                    name_span,
                },
                span,
            };
            // `__optN.field` or `__optN.method(args)`, then wrapped in `Some(...)`.
            let access = match call {
                None => field,
                Some(OptCall {
                    args,
                    named,
                    type_args,
                }) => Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(field),
                        args,
                        named,
                        type_args,
                    },
                    span,
                },
            };
            let some_body = Expr {
                kind: ExprKind::Call {
                    callee: Box::new(ident_expr("Some", span)),
                    args: vec![access],
                    named: vec![],
                    type_args: vec![],
                },
                span,
            };
            ExprKind::Match {
                scrutinee: obj,
                arms: vec![
                    MatchExprArm {
                        pattern: variant_pat("Some", vec![Pattern::Ident(c, Span::default())]),
                        guard: None,
                        body: some_body,
                    },
                    MatchExprArm {
                        pattern: variant_pat("None", vec![]),
                        guard: None,
                        body: ident_expr("None", span),
                    },
                ],
            }
        }
        other => other, // unreachable: caller guards on the two carrier kinds
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
/// `NullCoalesce` never reaches here — `??` stays Option-only.
pub fn lower_carrier_try(expr: &mut Expr) {
    let span = expr.span;
    let kind = std::mem::replace(&mut expr.kind, ExprKind::Bool(false));
    let ExprKind::OptChain {
        obj,
        name,
        name_span,
        call,
    } = kind
    else {
        unreachable!("lower_carrier_try applies to `?.` only; `??` is Option-only");
    };
    let field = Expr {
        kind: ExprKind::Field {
            obj: Box::new(Expr {
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
        Some(OptCall {
            args,
            named,
            type_args,
        }) => ExprKind::Call {
            callee: Box::new(field),
            args,
            named,
            type_args,
        },
    };
}

/// A bare identifier expression at `span`.
fn ident_expr(name: &str, span: Span) -> Expr {
    Expr {
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
    fn fills_trailing_default() {
        let s = desugar_ok("fn f(x: int, y: int = 10):\n    print(x)\nr := f(1)\n");
        assert_eq!(call_arg_ints(&s), vec![1, 10]);
    }

    /// A BODIED method on a `native struct` must go through desugar exactly like a struct/enum method.
    /// Before the fix the `NativeStruct` desugar arm was a no-op, so a bodied method's body was never
    /// walked and its call sites never got default-arg splicing. Here `compute`'s body calls
    /// `helper(1)`; after desugar it must be `helper(1, 9)` — proving the body is now desugared.
    #[test]
    fn native_struct_bodied_method_body_is_desugared() {
        let stmts = desugar_ok(
            "fn helper(a: int, b: int = 9) -> int:\n    return a + b\n\
             native struct R:\n    native fn read_line(self) -> str\n    \
             fn compute(self) -> int:\n        return helper(1)\n",
        );
        let bodied = stmts
            .iter()
            .find_map(|s| match &s.kind {
                StmtKind::NativeStruct { bodied_methods, .. } => bodied_methods.first(),
                _ => None,
            })
            .expect("a native struct with a bodied method");
        let ret = match &bodied.body.last().expect("a body statement").kind {
            StmtKind::Return(Some(e)) => e,
            other => panic!("expected a return, got {other:?}"),
        };
        let ExprKind::Call { args, .. } = &ret.kind else {
            panic!("expected a call, got {:?}", ret.kind)
        };
        let ints: Vec<i64> = args
            .iter()
            .map(|a| match a.kind {
                ExprKind::Int(n) => n,
                ref other => panic!("expected an int arg, got {other:?}"),
            })
            .collect();
        assert_eq!(ints, vec![1, 9], "the bodied method body was not desugared");
    }

    #[test]
    fn fills_multiple_defaults() {
        let s = desugar_ok("fn f(x: int, y: int = 2, z: int = 3):\n    print(x)\nr := f(1)\n");
        assert_eq!(call_arg_ints(&s), vec![1, 2, 3]);
    }

    #[test]
    fn reorders_named() {
        let s = desugar_ok("fn f(x: int, y: int):\n    print(x)\nr := f(y=2, x=1)\n");
        assert_eq!(call_arg_ints(&s), vec![1, 2]);
    }

    #[test]
    fn positional_plus_named() {
        let s = desugar_ok("fn f(x: int, y: int):\n    print(x)\nr := f(1, y=2)\n");
        assert_eq!(call_arg_ints(&s), vec![1, 2]);
    }

    #[test]
    fn named_fills_remaining_default() {
        let s = desugar_ok("fn f(x: int, y: int = 2, z: int = 3):\n    print(x)\nr := f(1, z=9)\n");
        assert_eq!(call_arg_ints(&s), vec![1, 2, 9]);
    }

    #[test]
    fn struct_ctor_named_and_default() {
        let s = desugar_ok("struct P:\n    x: int\n    y: int = 0\nr := P(x=5)\n");
        assert_eq!(call_arg_ints(&s), vec![5, 0]);
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

    #[test]
    fn unknown_named_errors() {
        assert!(
            desugar_err("fn f(x: int):\n    print(x)\nr := f(z=1)\n")
                .message
                .contains("unknown named argument 'z'")
        );
    }

    #[test]
    fn duplicate_positional_and_named_errors() {
        assert!(
            desugar_err("fn f(x: int, y: int):\n    print(x)\nr := f(1, x=2)\n")
                .message
                .contains("both positionally and by name")
        );
    }

    #[test]
    fn missing_required_with_named_errors() {
        assert!(
            desugar_err("fn f(x: int, y: int):\n    print(x)\nr := f(y=2)\n")
                .message
                .contains("missing required argument 'x'")
        );
    }

    #[test]
    fn named_on_value_call_left_intact_for_checker() {
        // Swift-style keyword args through a function VALUE: a value call (Ident/expr callee) carrying
        // named args is LEFT INTACT by desugar (named preserved) so the checker resolves it against the
        // value's labels — no longer a desugar error.
        let stmts = desugar_ok("g := fn(x: int): x\nr := g(x=1)\n");
        assert_eq!(call_named_keys(&stmts), vec!["x".to_string()]);
        // A METHOD-syntax callee (`recv.f(name=…)`) still routes to the method path and keeps the
        // historical error (it does not resolve value keywords — no silent keyword drop).
        assert!(
            desugar_err("struct S:\n    v: int\nfn go(s: S):\n    s.missing(name=1)\n")
                .message
                .contains("only supported on functions, struct constructors, and struct methods")
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
    fn print_unknown_kwarg_errors() {
        assert!(
            desugar_err("print(\"a\", foo=\"x\")\n")
                .message
                .contains("only accepts the named arguments 'sep' and 'end'")
        );
    }

    #[test]
    fn print_duplicate_kwarg_errors() {
        assert!(
            desugar_err("print(\"a\", sep=\"-\", sep=\".\")\n")
                .message
                .contains("only accepts the named arguments 'sep' and 'end'")
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
    fn method_fills_trailing_default() {
        let s = desugar_ok(
            "struct P:\n    n: int\n    fn bump(self, x: int = 5) -> int:\n        return self.n + x\np := P(1)\nr := p.bump()\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![5]);
    }

    #[test]
    fn method_reorders_named() {
        let s = desugar_ok(
            "struct P:\n    n: int\n    fn span(self, a: int, b: int) -> int:\n        return a + b\np := P(1)\nr := p.span(b=2, a=1)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![1, 2]);
    }

    #[test]
    fn method_positional_plus_named() {
        let s = desugar_ok(
            "struct P:\n    n: int\n    fn span(self, a: int, b: int) -> int:\n        return a + b\np := P(1)\nr := p.span(1, b=2)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![1, 2]);
    }

    #[test]
    fn method_unknown_named_errors() {
        assert!(desugar_err(
            "struct P:\n    n: int\n    fn bump(self, x: int) -> int:\n        return x\np := P(1)\nr := p.bump(z=2)\n",
        )
        .message
        .contains("unknown named argument 'z'"));
    }

    /// A default may not reference a parameter — **including through an interpolated fragment**.
    ///
    /// `validate_defaults` runs BEFORE the `Str -> Interp` rewrite, so the name walk saw only a raw
    /// literal and this slipped through. It was not caught later either: the decl-site copy is
    /// inferred with the parameters in scope, so `fn f(n: int, x: str = "n={n}")` type-checked clean
    /// and then meant two different things — the provider resolves `n` in MODULE scope, so a direct
    /// `f(3)` printed the module's `n` while `g := f; g(3)` printed the PARAMETER. Where no such
    /// global existed at all it reached the backend as `compiler: global 'n' has no slot`, a host
    /// panic on a check-clean program.
    #[test]
    fn a_default_cannot_reference_a_parameter_through_an_interpolated_fragment() {
        assert!(
            desugar_err("n := 100\nfn f(n: int, x: str = \"n={n}\") -> str:\n    return x\n")
                .message
                .contains("cannot reference parameter 'n'")
        );
        // A fragment naming something that is NOT a parameter stays legal.
        assert!(
            crate::desugar::run_standalone(
                &mut crate::parser::parse(
                    crate::lexer::tokenize(
                        "g := 1\nfn f(n: int, x: str = \"g={g}\") -> str:\n    return x\n"
                    )
                    .unwrap()
                )
                .unwrap()
            )
            .is_ok()
        );
    }

    /// The ONE shape a callee-filled default cannot cover, refused rather than silently cloned.
    ///
    /// A `Self`-typed default on a GENERIC host cannot become a free provider `fn`, so the CALLEE
    /// fills it — which a short call encodes by pushing fewer values, and which therefore cannot
    /// leave a HOLE before a supplied argument. The parser already forbids a required parameter
    /// after a defaulted one, so only a KEYWORD call supplying a later parameter can produce this.
    /// Falling back to the caller-scope clone here would keep, in the one corner nobody looks at,
    /// exactly the defect this design removes.
    #[test]
    fn a_callee_filled_default_cannot_be_omitted_before_a_supplied_argument() {
        assert!(
            desugar_err(
                "struct G[T]:\n    v: T\n    fn m(self, xs: List[Self] = mkl(), k: int = 9) -> int:\n        return xs.len() + k\nfn mkl[T]() -> List[G[T]]:\n    return []\nfn main():\n    print(G(1).m(k=3))\n",
            )
            .message
            .contains("can only be omitted from the END of a call")
        );
    }

    #[test]
    fn ambiguous_method_named_errors() {
        // Two structs define `set` with different params; a named call on an UNRESOLVABLE receiver
        // (an unannotated closure param — no static struct type) can't be bound unambiguously.
        // (A named call on a KNOWN receiver — `a := A(0); a.set(x=1)` — now resolves receiver-aware
        // to A.set; see `ambiguous_method_named_resolves_on_known_receiver`.)
        assert!(desugar_err(
            "struct A:\n    n: int\n    fn set(self, x: int) -> int:\n        return x\nstruct B:\n    n: int\n    fn set(self, y: int) -> int:\n        return y\ng := fn(a): a.set(x=1)\n",
        )
        .message
        .contains("multiple structs"));
    }

    #[test]
    fn ambiguous_method_named_resolves_on_known_receiver() {
        // With a statically-known receiver (`a := A(0)`), a named call to a name-colliding method
        // binds receiver-aware to the RIGHT struct's spec — no "multiple structs" error (mirrors
        // `builtin_named_method_known_receiver_normalized`, now extended to plain method names).
        let s = desugar_ok(
            "struct A:\n    n: int\n    fn set(self, x: int) -> int:\n        return x\nstruct B:\n    n: int\n    fn set(self, y: int) -> int:\n        return y\na := A(0)\nr := a.set(x=1)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![1]);
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
    fn builtin_named_method_known_receiver_normalized() {
        // A user struct method whose name collides with a builtin (`add`) DOES get named/default
        // support when the receiver's struct type is statically known (a named local).
        let s = desugar_ok(
            "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\nc := Counter(0)\nr := c.add(amount=5)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![5]);
    }

    #[test]
    fn builtin_named_method_inline_ctor() {
        // Inline ctor receiver `Counter(0).add(amount=5)` — struct type knowable syntactically.
        let s = desugar_ok(
            "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\nr := Counter(0).add(amount=5)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![5]);
    }

    #[test]
    fn builtin_default_filled_positional() {
        // A 0-arg builtin-named user-method call on a known receiver fills the default.
        let s = desugar_ok(
            "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\nr := Counter(0).add()\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![1]);
    }

    #[test]
    fn builtin_named_method_struct_returning_fn() {
        // Struct-returning free fn receiver `mk().add(amount=5)` — return type names a struct.
        let s = desugar_ok(
            "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\nfn mk() -> Counter:\n    return Counter(0)\nr := mk().add(amount=5)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![5]);
    }

    #[test]
    fn enum_builtin_named_method_annotated_receiver() {
        // An enum method reusing a builtin name (`map`) resolves on a type-annotated local receiver.
        let s = desugar_ok(
            "enum E:\n    A\n    B\n    fn map(self, n: int = 2) -> int:\n        return n\nm: E = E.A\nr := m.map(n=5)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![5]);
    }

    #[test]
    fn real_builtin_set_add_untouched() {
        // A genuine builtin-type receiver: `s.add(3)` on a Set must NOT be rewritten, even though a
        // struct also defines `add` with a default. receiver_struct_ty is None for a Set local.
        let s = desugar_ok(
            "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\ns := Set([1, 2])\ns.add(3)\n",
        );
        assert_eq!(method_call_arg_ints(&s), vec![3]);
    }

    #[test]
    fn builtin_named_unknowable_receiver_accurate_error() {
        // Named args on a builtin-colliding name whose receiver type is NOT statically known: the
        // diagnostic must be accurate (mentions the builtin-name clash), not the misleading
        // "only supported on functions, struct constructors, and struct methods".
        let e = desugar_err(
            "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\ns := Set([1, 2])\ns.add(x=3)\n",
        );
        assert!(
            e.message.contains("reuses a built-in method name"),
            "got: {}",
            e.message
        );
        assert!(
            !e.message.contains("only supported on"),
            "must not use the misleading message; got: {}",
            e.message
        );
    }

    #[test]
    fn builtin_named_no_struct_defines_no_panic() {
        // A builtin-named named-arg call where NO user struct defines it: clean error, no panic.
        let e = desugar_err("s := Set([1, 2])\ns.add(x=3)\n");
        assert!(
            e.message.contains("reuses a built-in method name"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn builtin_named_fn_field_not_mislabeled_as_method() {
        // A function-typed struct FIELD whose name collides with a builtin (`map`), called with a
        // named arg, is field-access-then-call — NOT a method. It must fall through to the generic
        // unsupported-named-args error, never the "reuses a built-in method name" method diagnostic
        // (which would wrongly imply a typed-local would help). Guards the fn_fields omission.
        let e = desugar_err(
            "struct S:\n    map: fn(int) -> int\ns := S(fn(x: int) -> int: x)\ns.map(arg=1)\n",
        );
        assert!(
            !e.message.contains("reuses a built-in method name"),
            "fn-field call must not get the builtin-method-name diagnostic; got: {}",
            e.message
        );
    }

    #[test]
    fn nested_call_normalized() {
        // a defaulted call nested as an argument is also filled
        let s = desugar_ok(
            "fn g(a: int, b: int = 7):\n    print(a)\nfn f(x: int):\n    print(x)\nr := f(g(1))\n",
        );
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else {
            panic!()
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!()
        };
        let ExprKind::Call { args: inner, .. } = &args[0].kind else {
            panic!("inner call")
        };
        assert_eq!(inner.len(), 2, "nested g(1) should fill default -> g(1, 7)");
    }

    /// The value expr of the last `name := <expr>` statement.
    fn last_let_value(stmts: &[Stmt]) -> Expr {
        match &stmts.last().expect("a statement").kind {
            StmtKind::Let { value, .. } => value.clone(),
            other => panic!("expected a let, got {other:?}"),
        }
    }

    #[test]
    fn null_coalesce_survives_desugar() {
        // W7-43 inverted this: the carrier is NOT lowered here any more (the choice needs the
        // operand's type). What desugar still owes it is a NORMALIZED child on each side.
        let stmts = desugar_ok(
            "fn g(x: int, y: int = 7) -> int?:\n    return Some(x)\nx := g(1) ?? g(2, y=9)\n",
        );
        match last_let_value(&stmts).kind {
            ExprKind::NullCoalesce { lhs, rhs } => {
                let args_of = |e: &Expr| -> usize {
                    let ExprKind::Call { args, named, .. } = &e.kind else {
                        panic!("expected a Call, got {:?}", e.kind)
                    };
                    assert!(named.is_empty(), "named must be bound to positional slots");
                    args.len()
                };
                assert_eq!(args_of(&lhs), 2, "omitted default filled on the lhs");
                assert_eq!(args_of(&rhs), 2, "named arg bound to a slot on the rhs");
            }
            other => panic!("expected a NullCoalesce, got {other:?}"),
        }
    }

    #[test]
    fn opt_chain_survives_desugar_with_a_normalized_call() {
        // The carrier survives; `normalize_opt_call` still binds its named args and fills defaults
        // (before W7-43 the lowered arm body got this from `walk_expr`'s ordinary `Call` tail).
        let stmts = desugar_ok(
            "struct P:\n    x: int\n    fn tag(self, a: int, b: int = 4) -> int:\n        return a\na := Some(P(1))\nv := a?.tag(1, b=9)\n",
        );
        match last_let_value(&stmts).kind {
            ExprKind::OptChain { name, call, .. } => {
                assert_eq!(name, "tag");
                let c = call.expect("a method call");
                assert!(
                    c.named.is_empty(),
                    "named must be bound to positional slots"
                );
                assert_eq!(c.args.len(), 2);
                assert!(matches!(c.args[1].kind, ExprKind::Int(9)));
            }
            other => panic!("expected an OptChain, got {other:?}"),
        }
    }

    #[test]
    fn opt_chain_field_survives_desugar() {
        let stmts = desugar_ok("struct P:\n    x: int\na := Some(P(1))\nv := a?.x\n");
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
                    panic!("Some(...) wrapper")
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
    fn two_coalesce_in_one_expr_get_unique_temps() {
        // `(a ?? 0) + (b ?? 0)` — both carriers now survive desugar, and the temp names are minted
        // by whoever lowers them. Assert the property at that point instead: two lowerings with
        // distinct counter values bind DISTINCT temps.
        let stmts = desugar_ok("a := Some(1)\nb := Some(2)\nx := (a ?? 0) + (b ?? 0)\n");
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
            let Pattern::Ident(n, _) = &bindings[0] else {
                panic!("ident binding")
            };
            n.clone()
        };
        assert_ne!(name_of(&lhs), name_of(&rhs), "temps must be unique");
    }

    // ===== non-constant default expressions =====

    /// The last `let` in a module — NOT `stmts.last()`, since W7-51 APPENDS the synthesized
    /// providers after the user's own statements (top-level `fn`s are hoisted, so position is
    /// irrelevant to behavior, but it moves the tail).
    fn last_let(stmts: &[Stmt]) -> &Expr {
        stmts
            .iter()
            .rev()
            .find_map(|st| match &st.kind {
                StmtKind::Let { value, .. } => Some(value),
                _ => None,
            })
            .expect("a let statement")
    }

    /// The single zero-arg argument an omitting call site now carries, and the body of the provider
    /// it names. Panics with a readable message if the call was not rewritten into a provider call.
    fn provider_arg<'a>(stmts: &'a [Stmt], call: &Expr) -> &'a Expr {
        let ExprKind::Call { args, .. } = &call.kind else {
            panic!("expected a Call, got {:?}", call.kind)
        };
        assert_eq!(args.len(), 1, "the omitted default was filled");
        let ExprKind::Call {
            callee,
            args: pargs,
            ..
        } = &args[0].kind
        else {
            panic!(
                "the filled slot must be a provider CALL, got {:?}",
                args[0].kind
            )
        };
        assert!(pargs.is_empty(), "a provider takes no arguments");
        let ExprKind::Ident(name) = &callee.kind else {
            panic!("provider callee must be a bare Ident")
        };
        assert!(
            name.starts_with(PROVIDER_PREFIX),
            "expected a `$def$…` provider, got '{name}'"
        );
        for st in stmts {
            if let StmtKind::Fn(decl) = &st.kind
                && &decl.name == name
            {
                assert!(
                    !decl.is_test,
                    "a provider must not be discovered by `chezzi test`"
                );
                assert!(decl.ret.is_some(), "a provider declares its return type");
                let [
                    Stmt {
                        kind: StmtKind::Return(Some(e)),
                        ..
                    },
                ] = decl.body.as_slice()
                else {
                    panic!("a provider body is exactly `return <default>`")
                };
                return e;
            }
        }
        panic!("no provider named '{name}' was synthesized into the module")
    }

    #[test]
    fn non_const_default_filled() {
        // W7-51 — a non-literal default is no longer cloned into the caller: the call site gets a
        // zero-arg call to a provider synthesized in the DECLARING module, whose body is the
        // default expression.
        let s = desugar_ok(
            "fn g() -> int:\n    return 9\nfn f(x: int = g() + 1):\n    print(x)\nr := f()\n",
        );
        let body = provider_arg(&s, last_let(&s));
        assert!(
            matches!(body.kind, ExprKind::Binary { .. }),
            "the provider returns the `g() + 1` expr, got {:?}",
            body.kind
        );
    }

    #[test]
    fn a_literal_default_is_still_cloned_inline() {
        // The inline class costs no call and records no side-table key — `= 1 + 2` is spliced as
        // the literal expression itself, exactly as before W7-51.
        let s = desugar_ok("fn f(x: int = 1 + 2):\n    print(x)\nr := f()\n");
        let ExprKind::Call { args, .. } = &last_let(&s).kind else {
            panic!("call")
        };
        assert!(
            matches!(args[0].kind, ExprKind::Binary { .. }),
            "an inline literal default is cloned, not provided — got {:?}",
            args[0].kind
        );
        assert!(
            !s.iter().any(
                |st| matches!(&st.kind, StmtKind::Fn(d) if d.name.starts_with(PROVIDER_PREFIX))
            ),
            "no provider is synthesized for a self-contained literal"
        );
    }

    #[test]
    fn a_self_referencing_default_is_a_cycle_error() {
        // Before W7-51 this silently expanded to a three-deep `f(f(f()))` (the two-pass fixed
        // point) and was then rejected as an arity cascade, not as a cycle; as a provider it would
        // be unbounded recursion, so it is refused here, naming the parameter.
        let e = desugar_err("fn f(x: int = f()) -> int:\n    return x\nr := f()\n");
        assert!(
            e.to_string().contains("is cyclic") && e.to_string().contains("'x' of 'f'"),
            "got: {e}"
        );
    }

    #[test]
    fn a_self_referencing_field_default_is_a_cycle_error_without_calling_it_a_parameter() {
        // A provider name records the slot but NOT whether it is a parameter or a struct field, so
        // the label is noun-free. Measured before that change: `the default value for parameter 'n'
        // of 'S' is cyclic` — a field called a parameter.
        let e = desugar_err("struct S:\n    n: int = S().n\nr := S().n\n");
        let s = e.to_string();
        assert!(
            s.contains("is cyclic") && s.contains("'n' of 'S'"),
            "got: {e}"
        );
        assert!(!s.contains("parameter"), "a field is not a parameter: {e}");
    }

    #[test]
    fn param_referencing_default_rejected() {
        let e = desugar_err("fn f(x: int, y: int = x + 1):\n    print(y)\n");
        assert!(
            e.to_string().contains("cannot reference parameter 'x'"),
            "got: {e}"
        );
    }

    #[test]
    fn field_referencing_default_rejected() {
        let e = desugar_err("struct S:\n    a: int = 1\n    b: int = a\n");
        assert!(
            e.to_string().contains("cannot reference field 'a'"),
            "got: {e}"
        );
    }

    #[test]
    fn method_param_referencing_default_rejected() {
        let e = desugar_err(
            "struct S:\n    n: int\n    fn go(self, x: int, y: int = x):\n        return y\n",
        );
        assert!(
            e.to_string().contains("cannot reference parameter 'x'"),
            "got: {e}"
        );
    }

    #[test]
    fn defaulted_fn_call_in_default_is_normalized() {
        // `f(x = g())` where `g(a = 7)`: the spliced default `g()` must itself be normalized to
        // `g(7)` (second pass), not left under-arity.
        let s = desugar_ok(
            "fn g(a: int = 7) -> int:\n    return a\nfn f(x: int = g()):\n    print(x)\nr := f()\n",
        );
        // f's provider body is `g(7)` — `g`'s own omitted default was filled by the same one pass.
        let body = provider_arg(&s, last_let(&s));
        let ExprKind::Call { args: ginner, .. } = &body.kind else {
            panic!("inner call g, got {:?}", body.kind)
        };
        assert_eq!(
            ginner.len(),
            1,
            "g()'s own default was filled inside the provider body"
        );
    }

    #[test]
    fn carrier_in_default_survives_with_a_normalized_child() {
        // W7-43 inverted this: a `??` carrier in a default SURVIVES desugar. What must still hold is
        // that the walk normalized its children — here `h()`'s own omitted default is filled inside
        // the carrier's lhs, now in the provider body rather than a spliced clone.
        let s = desugar_ok(
            "fn h(k: int = 3) -> int?:\n    return Some(k)\nfn f(x: int = h() ?? 0):\n    print(x)\nr := f()\n",
        );
        let body = provider_arg(&s, last_let(&s));
        let ExprKind::NullCoalesce { lhs, .. } = &body.kind else {
            panic!("carrier survives, got {:?}", body.kind)
        };
        let ExprKind::Call { args: hargs, .. } = &lhs.kind else {
            panic!("call h")
        };
        assert_eq!(
            hargs.len(),
            1,
            "h()'s own default was filled inside the carrier"
        );
    }

    // ===== variadic collapse =====

    /// Pull the last call's positional args as an ExprKind slice.
    fn last_call_args(stmts: &[Stmt]) -> Vec<ExprKind> {
        let last = stmts.last().expect("a statement");
        let expr = match &last.kind {
            StmtKind::Let { value, .. } => value,
            StmtKind::Expr(e) => e,
            other => panic!("expected let/expr, got {other:?}"),
        };
        let ExprKind::Call { args, named, .. } = &expr.kind else {
            panic!("expected a Call, got {:?}", expr.kind)
        };
        assert!(
            named.is_empty(),
            "named must be cleared after variadic collapse"
        );
        args.iter().map(|a| a.kind.clone()).collect()
    }

    fn list_ints(k: &ExprKind) -> Vec<i64> {
        let ExprKind::List(es, _) = k else {
            panic!("expected a List literal, got {k:?}");
        };
        es.iter()
            .map(|e| match e.kind {
                ExprKind::Int(n) => n,
                ref o => panic!("expected int, got {o:?}"),
            })
            .collect()
    }

    #[test]
    fn variadic_sweeps_positionals_into_list() {
        let s = desugar_ok("fn f(...xs: int):\n    return\nr := f(1, 2, 3)\n");
        let args = last_call_args(&s);
        assert_eq!(args.len(), 1);
        assert_eq!(list_ints(&args[0]), vec![1, 2, 3]);
    }

    #[test]
    fn variadic_zero_args_is_empty_list() {
        let s = desugar_ok("fn f(...xs: int):\n    return\nr := f()\n");
        let args = last_call_args(&s);
        assert_eq!(args.len(), 1);
        assert_eq!(list_ints(&args[0]), Vec::<i64>::new());
    }

    #[test]
    fn variadic_keyword_only_tail_bound_by_name() {
        let s = desugar_ok("fn g(...xs: int, flag: bool):\n    return\nr := g(1, flag=true)\n");
        let args = last_call_args(&s);
        assert_eq!(args.len(), 2);
        assert_eq!(list_ints(&args[0]), vec![1]);
        assert!(matches!(args[1], ExprKind::Bool(true)));
    }

    #[test]
    fn variadic_missing_required_keyword_errors() {
        let e = desugar_err("fn g(...xs: int, flag: bool):\n    return\nr := g(1)\n");
        assert!(
            e.message
                .contains("missing required keyword argument 'flag'"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn variadic_stray_positional_swept_not_placed_in_kwonly() {
        // `true` is swept into xs (a positional can never occupy the keyword-only slot); flag then
        // has no value → missing required keyword arg.
        let e = desugar_err("fn g(...xs: int, flag: bool):\n    return\nr := g(1, 2, true)\n");
        assert!(
            e.message
                .contains("missing required keyword argument 'flag'"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn variadic_naming_the_variadic_errors() {
        let e = desugar_err("fn f(...xs: int):\n    return\nr := f(xs=1)\n");
        assert!(
            e.message.contains("positional") && e.message.contains("xs"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn variadic_keyword_only_default_filled() {
        let s = desugar_ok("fn g(...xs: int, flag: bool = false):\n    return\nr := g(1, 2)\n");
        let args = last_call_args(&s);
        assert_eq!(args.len(), 2);
        assert_eq!(list_ints(&args[0]), vec![1, 2]);
        assert!(matches!(args[1], ExprKind::Bool(false)));
    }

    #[test]
    fn variadic_with_leading_positional() {
        let s = desugar_ok("fn f(a: str, ...xs: int):\n    return\nr := f(\"h\", 1, 2)\n");
        let args = last_call_args(&s);
        assert_eq!(args.len(), 2);
        assert!(matches!(args[0], ExprKind::Str(ref s) if s == "h"));
        assert_eq!(list_ints(&args[1]), vec![1, 2]);
    }
}
