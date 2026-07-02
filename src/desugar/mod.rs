//! Call-argument desugaring: normalize **named arguments** (`f(x=1)`) and **default arguments**
//! (`fn f(x: int, y: int = 10)`) into a plain positional `args` list.
//!
//! This pass runs inside [`crate::resolver::build_graph`], so the checker and **both** engines
//! (tree-walk interpreter + bytecode VM) consume the already-normalized AST — they only ever see
//! `Call.named` empty and a fully positional `Call.args`. That keeps the two engines in lockstep by
//! construction: there is no per-engine call-binding logic for defaults/named args.
//!
//! Scope: free functions (own module + `from`-imported + module-qualified `alias.f(...)`) and struct
//! constructors. Enum-variant constructors are excluded (payloads are unnamed) and methods are
//! deferred (resolving a receiver type needs the checker). A default may be any expression that does
//! not reference another parameter/field — `validate_defaults` enforces this (the default is cloned
//! into the caller's scope at the omitting call site, where parameters/fields are not bound).
//!
//! The pass is **scope-aware**: a local binding may shadow a top-level function name, so a call is
//! only rewritten when its callee resolves to a registered callable and is *not* shadowed by a local
//! (mirroring the checker, which treats a call as a named function only when the name is not a local).

use crate::ast::{
    Block, DeferTarget, Expr, ExprKind, Import, MatchExprArm, Module, OptCall, Param, Pattern,
    Span, SpawnTarget, Stmt, StmtKind, Type, WaitTarget,
};
use crate::resolver::{ModuleGraph, ModuleId, ResolveError};
use std::collections::{HashMap, HashSet};

/// A callable's parameter (or struct field), in declaration order, with its optional constant
/// default. Cloned out of the AST so the per-module registry is independent of the graph we mutate.
/// `PartialEq` lets us decide whether several same-named struct methods share one binding shape.
#[derive(Clone, PartialEq)]
struct PSpec {
    name: String,
    default: Option<Expr>,
    /// True for a `ref T` parameter — the call-arg lowering passes the box (alias) instead of
    /// auto-dereferencing to a `.get()` copy. See [`Walker::walk_expr`]'s `Call` arm.
    is_ref: bool,
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
    let regs = build_registries(graph);
    let methods = collect_methods(graph);
    let methods_by_struct = collect_methods_by_struct(graph);
    let fn_fields = collect_fn_fields(graph);
    // Index modules by id so we can resolve each module's imports against the others' registries.
    let mut module_index: HashMap<ModuleId, usize> = HashMap::new();
    for (i, m) in graph.modules.iter().enumerate() {
        module_index.insert(m.id.clone(), i);
    }

    // Two passes. Pass 1 lowers bodies + declaration-site defaults and splices each omitted default
    // into its call site — but the spliced copy comes from the registry (raw, un-lowered), so a
    // default that contains a `?.`/`??` carrier or a call to a defaulted function is still raw. Pass 2
    // re-walks, lowering those spliced default expressions in place. Already-lowered nodes and
    // already-filled calls are no-ops on the second pass, so this is idempotent.
    for pass in 0..2 {
        for mi in 0..graph.modules.len() {
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
                bare_from: &bare_from,
                aliases: &aliases,
                methods: &methods,
                methods_by_struct: &methods_by_struct,
                fn_fields: &fn_fields,
            };
            let mut walker = Walker {
                ctx,
                scopes: Vec::new(),
                ref_names: Vec::new(),
                local_fn: Vec::new(),
                local_struct: Vec::new(),
                next_tmp: 0,
                skip_normalize: false,
                lower_refs: pass == 0,
            };
            // Borrow the module's AST mutably; everything `walker` reads lives in `regs`/the maps above.
            let ast: &mut Module = &mut graph.modules[mi].ast;
            walker.walk_block(&mut ast.stmts)?;
        }
    }
    Ok(())
}

/// Desugar a single standalone module (no imports) in place. Used by the test/standalone runners,
/// which bypass [`build_graph`](crate::resolver::build_graph) and so must apply this pass themselves
/// to stay consistent with the file-backed graph path.
#[cfg(test)]
pub fn run_standalone(module: &mut Module) -> Result<(), ResolveError> {
    validate_defaults(&module.stmts)?;
    let id = ModuleId(std::path::PathBuf::from("<main>"));
    let mut regs = HashMap::new();
    regs.insert(id.clone(), collect_module_reg(&module.stmts));
    let mut methods = HashMap::new();
    collect_methods_into(&module.stmts, &mut methods);
    let methods_by_struct = collect_methods_by_struct_into_standalone(&module.stmts);
    let mut fn_fields = HashSet::new();
    collect_fn_fields_into(&module.stmts, &mut fn_fields);
    let bare_from = HashMap::new();
    let aliases = HashMap::new();
    // Two passes — see the comment in [`run`] (spliced defaults are lowered on the second pass).
    for pass in 0..2 {
        let ctx = Ctx {
            regs: &regs,
            own_id: &id,
            bare_from: &bare_from,
            aliases: &aliases,
            methods: &methods,
            methods_by_struct: &methods_by_struct,
            fn_fields: &fn_fields,
        };
        let mut walker = Walker {
            ctx,
            scopes: Vec::new(),
            ref_names: Vec::new(),
            local_fn: Vec::new(),
            local_struct: Vec::new(),
            next_tmp: 0,
            skip_normalize: false,
            lower_refs: pass == 0,
        };
        walker.walk_block(&mut module.stmts)?;
    }
    Ok(())
}

/// Lower `?.`/`??` carrier nodes to `match` in a single standalone expression. String-interpolation
/// fragments (`"{ … }"`) are re-parsed AFTER the module-wide [`run`] pass — by each engine, at
/// compile/eval time — so their carriers would otherwise reach the checker-less interpolation path
/// (a hard VM `unreachable!` panic, a graceful interp error → a parity break). Calling this on every
/// fragment in BOTH engines keeps them in lockstep. Ctx-free: call normalization is skipped (it was
/// never applied to fragments), only the carriers are rewritten.
pub fn lower_carriers(expr: &mut Expr) {
    let regs: HashMap<ModuleId, ModReg> = HashMap::new();
    let own_id = ModuleId(std::path::PathBuf::from("<fragment>"));
    let bare_from: HashMap<String, ModuleId> = HashMap::new();
    let aliases: HashMap<String, ModuleId> = HashMap::new();
    let methods: HashMap<String, Vec<Vec<PSpec>>> = HashMap::new();
    let methods_by_struct: HashMap<(String, String), Vec<PSpec>> = HashMap::new();
    let fn_fields: HashSet<String> = HashSet::new();
    let ctx = Ctx {
        regs: &regs,
        own_id: &own_id,
        bare_from: &bare_from,
        aliases: &aliases,
        methods: &methods,
        methods_by_struct: &methods_by_struct,
        fn_fields: &fn_fields,
    };
    // Interpolation fragments never contain `ref` bindings (they are sub-expressions), so ref-
    // lowering is inert here; leave it off to keep the fragment path minimal.
    let mut walker = Walker {
        ctx,
        scopes: Vec::new(),
        ref_names: Vec::new(),
        local_fn: Vec::new(),
        local_struct: Vec::new(),
        next_tmp: 0,
        skip_normalize: true,
        lower_refs: false,
    };
    // Infallible: `skip_normalize` suppresses the only error path (`normalize_call`).
    let _ = walker.walk_expr(expr);
}

/// Snapshot each module's free functions and struct constructors into a registry keyed by module id.
fn build_registries(graph: &ModuleGraph) -> HashMap<ModuleId, ModReg> {
    let mut regs = HashMap::new();
    for m in &graph.modules {
        regs.insert(m.id.clone(), collect_module_reg(&m.ast.stmts));
    }
    regs
}

/// A program-wide registry of struct **methods**, keyed by method name. A method's receiver type is
/// unknown in this pre-type pass, so a method call is resolved by name; each entry holds one param
/// spec (the params *after* the receiver `self`) per struct that defines that name. Spans all modules
/// since a receiver may be an imported struct's value.
fn collect_methods(graph: &ModuleGraph) -> HashMap<String, Vec<Vec<PSpec>>> {
    let mut map: HashMap<String, Vec<Vec<PSpec>>> = HashMap::new();
    for m in &graph.modules {
        collect_methods_into(&m.ast.stmts, &mut map);
    }
    map
}

/// Add one module's struct methods to `map`. The receiver (`self`, params[0]) is dropped — a call's
/// explicit args correspond to params[1..].
fn collect_methods_into(stmts: &[Stmt], map: &mut HashMap<String, Vec<Vec<PSpec>>>) {
    for stmt in stmts {
        // Struct AND enum methods share one name-keyed registry (a method call is resolved by name
        // in this pre-type pass; the checker has already validated the receiver type).
        let methods = match &stmt.kind {
            StmtKind::Struct { methods, .. } => methods,
            StmtKind::Enum { methods, .. } => methods,
            StmtKind::NewType { methods, .. } => methods,
            _ => continue,
        };
        for method in methods {
            let spec: Vec<PSpec> = method
                .params
                .iter()
                .skip(1)
                .map(|p| PSpec {
                    name: p.name.clone(),
                    default: p.default.clone(),
                    is_ref: p.is_ref,
                })
                .collect();
            map.entry(method.name.clone()).or_default().push(spec);
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
            let (name, methods) = match &stmt.kind {
                StmtKind::Struct { name, methods, .. } => (name, methods),
                StmtKind::Enum { name, methods, .. } => (name, methods),
                StmtKind::NewType { name, methods, .. } => (name, methods),
                _ => continue,
            };
            {
                for method in methods {
                    let spec: Vec<PSpec> = method
                        .params
                        .iter()
                        .skip(1)
                        .map(|p| PSpec {
                            name: p.name.clone(),
                            default: p.default.clone(),
                            is_ref: p.is_ref,
                        })
                        .collect();
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
) -> HashMap<(String, String), Vec<PSpec>> {
    let mut map: HashMap<(String, String), Vec<PSpec>> = HashMap::new();
    for stmt in stmts {
        let (name, methods) = match &stmt.kind {
            StmtKind::Struct { name, methods, .. } => (name, methods),
            StmtKind::Enum { name, methods, .. } => (name, methods),
            StmtKind::NewType { name, methods, .. } => (name, methods),
            _ => continue,
        };
        for method in methods {
            let spec: Vec<PSpec> = method
                .params
                .iter()
                .skip(1)
                .map(|p| PSpec {
                    name: p.name.clone(),
                    default: p.default.clone(),
                    is_ref: p.is_ref,
                })
                .collect();
            map.insert((name.clone(), method.name.clone()), spec);
        }
    }
    map
}

/// Reject any parameter/field default that references another parameter/field in the same signature.
/// A default is **cloned into the caller's scope** at the omitting call site (see `normalize_call`),
/// where those parameters/fields are not bound — so a non-param-referencing expression (`compute()`,
/// `1 + 2`, `GLOBAL * 2`) is fine, but `y: int = x + 1` is not. Covers top-level functions and struct
/// methods/fields (the only places defaults are collected). Runs before the call-rewrite pass.
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
                                "default value cannot reference field '{n}' (defaults are evaluated at the call site, where fields are not in scope)"
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
                    "default value cannot reference parameter '{n}' (defaults are evaluated at the call site, where parameters are not in scope)"
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
    match &e.kind {
        ExprKind::Ident(n) => f(n),
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        // A type-application head names a TYPE (not a value reference); its args are `Type`s.
        | ExprKind::TypeApply { .. }
        | ExprKind::Bool(_) => {}
        ExprKind::List(xs) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().for_each(|x| walk_idents(x, f))
        }
        ExprKind::Map(ps) => ps.iter().for_each(|(k, v)| {
            walk_idents(k, f);
            walk_idents(v, f);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            if let Some(k) = key {
                walk_idents(k, f);
            }
            walk_idents(elem, f);
            for clause in clauses {
                walk_idents(&clause.iter, f);
                for g in &clause.guards {
                    walk_idents(g, f);
                }
            }
        }
        ExprKind::Unary { expr, .. } => walk_idents(expr, f),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_idents(lhs, f);
            walk_idents(rhs, f);
        }
        ExprKind::Range { start, end } => {
            walk_idents(start, f);
            walk_idents(end, f);
        }
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
            walk_idents(callee, f);
            args.iter().for_each(|a| walk_idents(a, f));
            named.iter().for_each(|(_, a)| walk_idents(a, f));
        }
        ExprKind::Field { obj, .. } => walk_idents(obj, f),
        ExprKind::Index { obj, index } => {
            walk_idents(obj, f);
            walk_idents(index, f);
        }
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            walk_idents(obj, f);
            for c in [start, end, step].iter().filter_map(|c| c.as_deref()) {
                walk_idents(c, f);
            }
        }
        ExprKind::Try(x) => walk_idents(x, f),
        ExprKind::OptChain { obj, call, .. } => {
            walk_idents(obj, f);
            if let Some(c) = call {
                c.args.iter().for_each(|a| walk_idents(a, f));
                c.named.iter().for_each(|(_, a)| walk_idents(a, f));
            }
        }
        ExprKind::NullCoalesce { lhs, rhs } => {
            walk_idents(lhs, f);
            walk_idents(rhs, f);
        }
        ExprKind::DecodeCall { obj, arg, .. } => {
            walk_idents(obj, f);
            walk_idents(arg, f);
        }
        ExprKind::Closure { body, .. } => walk_idents(body, f),
        ExprKind::Match { scrutinee, arms } => {
            walk_idents(scrutinee, f);
            arms.iter().for_each(|a| {
                if let Some(g) = &a.guard {
                    walk_idents(g, f);
                }
                walk_idents(&a.body, f);
            });
        }
        ExprKind::IfElse { cond, then, els } => {
            walk_idents(cond, f);
            walk_idents(then, f);
            walk_idents(els, f);
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
fn collect_module_reg(stmts: &[Stmt]) -> ModReg {
    let mut reg = ModReg::default();
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Fn(decl) => {
                reg.fns.insert(
                    decl.name.clone(),
                    decl.params
                        .iter()
                        .map(|p| PSpec {
                            name: p.name.clone(),
                            default: p.default.clone(),
                            is_ref: p.is_ref,
                        })
                        .collect(),
                );
            }
            StmtKind::Struct { name, fields, .. } => {
                reg.structs.insert(
                    name.clone(),
                    fields
                        .iter()
                        .map(|f| PSpec {
                            name: f.name.clone(),
                            default: f.default.clone(),
                            is_ref: false,
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
    /// Names bound as `ref T` in each lexical scope (parallel to `scopes`). A bare rvalue use of such
    /// a name auto-derefs to `<name>.get()`; an assignment target lowers to `<name>.set(v)`; a call
    /// arg destined for a `ref` param stays the bare box ident (alias). Plain (non-ref) locals are
    /// never in this set, so they keep today's by-value semantics.
    ref_names: Vec<HashSet<String>>,
    /// Per-scope map of a LOCAL fn-value name to its parameters' `is_ref` flags (parallel to
    /// `scopes`). Populated when a local is bound to a bare named-fn (`g := bump`) or a closure
    /// literal (`g := fn(x: ref int): ...`). Lets `callee_param_is_ref` resolve a call through an
    /// indirect callee — the type-directed coercion decision the syntactic name lookup cannot make.
    local_fn: Vec<HashMap<String, Vec<bool>>>,
    /// Per-scope map of a LOCAL name to the struct type it was constructed/annotated as (parallel to
    /// `scopes`). Populated by `x := StructName(...)` and `x: StructName = ...`. Lets a method call
    /// `recv.m(args)` resolve `m`'s `ref` flags against the receiver's *actual* struct (so a sibling
    /// struct's same-named method with different ref-ness does not derail the decision — charge 2).
    local_struct: Vec<HashMap<String, String>>,
    /// Counter for fresh temp names minted when lowering `?.`/`??` to `match` (`__opt0`, `__opt1`, …).
    /// `__`-prefixed names can't be written by user code, so they never collide with a real binding.
    next_tmp: usize,
    /// When set, skip `normalize_call` (named/default-arg resolution) — used for string-interpolation
    /// fragments, which are re-parsed after the module-wide pass and need only carrier lowering
    /// (their call-normalization was already skipped before this pass existed; kept identical).
    skip_normalize: bool,
    /// `ref T` read/write/init lowering runs on the FIRST pass only. The module pass walks the tree
    /// twice (to lower spliced defaults); ref-lowering is NOT idempotent (re-walking a synthesized
    /// `r.get()` would re-deref its box ident to `r.get().get()`, and re-wrap a `Ref(v)` init), so it
    /// must fire exactly once. On pass 2 the Walker treats `ref` bindings as ordinary names.
    lower_refs: bool,
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
        self.ref_names.push(HashSet::new());
        self.local_fn.push(HashMap::new());
        self.local_struct.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.ref_names.pop();
        self.local_fn.pop();
        self.local_struct.pop();
    }

    /// Record `name` as a `ref T` binding in the current (innermost) scope.
    fn bind_ref(&mut self, name: &str) {
        if let Some(top) = self.ref_names.last_mut() {
            top.insert(name.to_string());
        }
    }

    /// True if `name` resolves to a `ref T` binding — **shadowing-aware**: the INNERMOST scope that
    /// declares `name` decides. A plain (`:=`) inner binding that shadows an outer `ref` of the same
    /// name is therefore NOT a ref (its reads/writes are ordinary). `scopes` and `ref_names` are kept
    /// in lockstep (every `bind_ref` is preceded by a `bind`), so the first `scopes` frame holding the
    /// name is the binding site; we consult that same frame's `ref_names`.
    fn is_ref(&self, name: &str) -> bool {
        for (vars, refs) in self.scopes.iter().zip(self.ref_names.iter()).rev() {
            if vars.contains(name) {
                return refs.contains(name);
            }
        }
        false
    }

    /// True if `e` is a bare identifier naming an in-scope `ref` binding.
    fn is_ref_ident(&self, e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Ident(n) if self.is_ref(n))
    }

    /// Record a LOCAL fn-value binding `name` carrying the given per-param `is_ref` flags, in the
    /// innermost scope (lockstep with `bind`). Looked up by `callee_param_is_ref` for an indirect call.
    fn bind_local_fn(&mut self, name: &str, flags: Vec<bool>) {
        if let Some(top) = self.local_fn.last_mut() {
            top.insert(name.to_string(), flags);
        }
    }

    /// Record that LOCAL `name` holds a value of struct type `sname`, in the innermost scope.
    fn bind_local_struct(&mut self, name: &str, sname: &str) {
        if let Some(top) = self.local_struct.last_mut() {
            top.insert(name.to_string(), sname.to_string());
        }
    }

    /// The `is_ref` flags of a local fn-value `name` (innermost binding wins, shadowing-aware).
    fn local_fn_flags(&self, name: &str) -> Option<&Vec<bool>> {
        for (vars, fns) in self.scopes.iter().zip(self.local_fn.iter()).rev() {
            if vars.contains(name) {
                return fns.get(name);
            }
        }
        None
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

    /// If `value` is a bare named-fn / closure literal whose callee param `is_ref` flags are knowable
    /// pre-type, return them — so a `g := <callee>` binding can be resolved at an indirect call site.
    /// `None` for any RHS we cannot type locally (a method value, a returned fn, etc.).
    fn fn_value_flags(&self, value: &Expr) -> Option<Vec<bool>> {
        match &value.kind {
            // `g := fn(x: ref int): ...` — the closure's own param ref-flags.
            ExprKind::Closure { params, .. } => Some(params.iter().map(|p| p.is_ref).collect()),
            // `g := bump` (a bare free-fn / ctor name, or a fn-value local being re-aliased).
            ExprKind::Ident(n) if !self.is_local(n) => self
                .ctx
                .resolve_bare(n)
                .map(|s| s.iter().map(|p| p.is_ref).collect()),
            ExprKind::Ident(n) => self.local_fn_flags(n).cloned(),
            _ => None,
        }
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
                is_ref,
                doc: _,
            } => {
                if *is_ref && self.lower_refs {
                    // `r: ref T = RHS`. The parser guarantees a single name here. CREATE-vs-ALIAS is
                    // driven by the RHS: a bare in-scope `ref` ident aliases the same box (share),
                    // anything else creates a FRESH `Ref(RHS)`. This syntactic test is provably
                    // equivalent to the type-driven rule because NO expression can have type `ref T`
                    // except a ref-binding ident (ref is barred from return types/collections/fields).
                    if self.is_ref_ident(value) {
                        // ALIAS: leave the box ident untouched (do NOT auto-deref it to `.get()`).
                    } else {
                        // CREATE: lower the RHS's inner expressions, then wrap in a fresh `Ref(...)`.
                        self.walk_expr(value)?;
                        let inner = std::mem::replace(value, ident_expr("", value.span));
                        *value = ref_ctor(inner);
                    }
                    for n in names.iter() {
                        self.bind(n);
                        self.bind_ref(n);
                    }
                } else {
                    // Snapshot the RHS shape for indirect-callee resolution BEFORE walking (walking a
                    // bare `ref` ident would rewrite it to `.get()`). Only meaningful on the ref-
                    // lowering pass — that is the only pass that consults these maps for arg coercion.
                    let fn_flags = if self.lower_refs && names.len() == 1 {
                        self.fn_value_flags(value)
                    } else {
                        None
                    };
                    let struct_ty = if self.lower_refs && names.len() == 1 {
                        // A `x: StructName = ...` annotation, or a `x := StructName(...)` ctor call.
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
                    if let Some(flags) = fn_flags {
                        self.bind_local_fn(&names[0], flags);
                    }
                    if let Some(sname) = struct_ty {
                        self.bind_local_struct(&names[0], &sname);
                    }
                }
            }
            StmtKind::Assign { target, value, op } => {
                // A `ref` assignment target mutates the pointee (never rebinds): `r = v` -> `r.set(v)`,
                // `r += 1` -> `r.set(r.get() <op> 1)`. Lowered to a statement-expression set call.
                if self.is_ref_ident(target) {
                    let op = *op;
                    self.walk_expr(value)?;
                    let box_ident = target.clone(); // the bare `ref` box ident (un-derefed)
                    let new_val = match op.to_binop() {
                        // Plain `=`: the set argument is the walked RHS as-is.
                        None => std::mem::replace(value, ident_expr("", value.span)),
                        // Compound `r OP= rhs`: `r.set(r.get() OP rhs)`.
                        Some(binop) => {
                            let rhs = std::mem::replace(value, ident_expr("", value.span));
                            let get = method_call(box_ident.clone(), "get", vec![]);
                            Expr {
                                kind: ExprKind::Binary {
                                    op: binop,
                                    lhs: Box::new(get),
                                    rhs: Box::new(rhs),
                                },
                                span: target.span,
                            }
                        }
                    };
                    let set = method_call(box_ident, "set", vec![new_val]);
                    stmt.kind = StmtKind::Expr(set);
                } else {
                    self.walk_expr(target)?;
                    self.walk_expr(value)?;
                }
            }
            StmtKind::Fn(decl) => {
                // Param defaults are evaluated in the caller's scope (no params bound), so normalize
                // their inner calls + lower their `?.`/`??` carriers here, outside the param scope.
                // `validate_defaults` guarantees a default references no param, so this is sound.
                for p in decl.params.iter_mut() {
                    if let Some(d) = &mut p.default {
                        self.walk_expr(d)?;
                    }
                }
                // Nested/top-level function body: params are a fresh scope.
                self.push_scope();
                for p in &decl.params {
                    self.bind(&p.name);
                    if p.is_ref && self.lower_refs {
                        self.bind_ref(&p.name);
                    }
                }
                self.walk_block(&mut decl.body)?;
                self.pop_scope();
            }
            StmtKind::Struct {
                fields, methods, ..
            } => {
                // Field defaults are spliced into the constructor call site — normalize them like
                // param defaults (outside any scope; they reference no field, per `validate_defaults`).
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
                        self.bind(&p.name);
                        if p.is_ref && self.lower_refs {
                            self.bind_ref(&p.name);
                        }
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
                    self.walk_expr(&mut arm.chan)?;
                    if let WaitTarget::Assign(e) = &mut arm.target {
                        self.walk_expr(e)?;
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
                        self.bind(&p.name);
                        if p.is_ref && self.lower_refs {
                            self.bind_ref(&p.name);
                        }
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
                }
            }
            // No nested expressions / bindings to rewrite.
            StmtKind::Return(None)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import(_)
            | StmtKind::Protocol { .. }
            | StmtKind::Extern { .. }
            // A `native fn`/`native ctor` decl is a body-less signature — no nested exprs/bindings.
            | StmtKind::Native(_)
            | StmtKind::TypeAlias { .. } => {}
        }
        Ok(())
    }

    fn walk_expr(&mut self, expr: &mut Expr) -> Result<(), ResolveError> {
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
            ExprKind::List(xs) | ExprKind::Set(xs) | ExprKind::Tuple(xs) => {
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
                    // A closure `ref` param is a `Ref[T]` box just like a named-fn `ref` param, so
                    // its body reads/writes lower to `.get()`/`.set()` (charge 3). The checker's
                    // `infer_closure` validates the pointee type; here we only drive the lowering.
                    if p.is_ref && self.lower_refs {
                        self.bind_ref(&p.name);
                    }
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
                // A positional arg that is a bare `ref` ident is lowered per the callee's param kind:
                // into a `ref` param it stays the bare box ident (alias — caller's binding is mutated
                // through it); into a plain `T` param it auto-derefs to `<ident>.get()` (a copy). All
                // other args (and `ref` idents whose param is unknown / non-ref) walk normally, which
                // already derefs a bare `ref` ident to `.get()` via the `Ident` leaf below.
                // Ref call-arg lowering runs on the first pass only (alongside all other ref-lowering;
                // see `lower_refs`). On pass 2 the args are already in final form, so walk plainly.
                let param_ref = if self.lower_refs {
                    self.callee_param_is_ref(callee)
                } else {
                    None
                };
                for (i, a) in args.iter_mut().enumerate() {
                    let param_is_ref = param_ref
                        .as_ref()
                        .is_some_and(|f| f.get(i).copied().unwrap_or(false));
                    if param_is_ref {
                        if self.is_ref_ident(a) {
                            // Row 1: `ref T` arg into a `ref T` param — pass the box (alias). Leave
                            // the bare ident untouched (do NOT auto-deref to `.get()`).
                            continue;
                        }
                        // Rows 3 & 4: you cannot take a reference to a by-value local or a temporary.
                        // (Emitted here, not in the checker, because the param's ref-ness and the
                        // arg's syntactic shape are both already known at this point — co-located with
                        // the alias/deref decision that shares the identical `param_ref` info.)
                        let msg = if matches!(a.kind, ExprKind::Ident(_)) {
                            "cannot pass a by-value local to a by-reference `ref` parameter; declare the local `ref` to pass it by reference".to_string()
                        } else {
                            "cannot pass a literal or temporary to a by-reference `ref` parameter; literals are temporary — bind a `ref` local first".to_string()
                        };
                        return Err(err(a.span, msg));
                    }
                    // Row 2 (ref T -> T) and all non-ref params: walk normally, which auto-derefs a
                    // bare `ref` ident to `.get()` (a copy) via the `Ident` leaf.
                    self.walk_expr(a)?;
                }
                for (_, v) in named.iter_mut() {
                    self.walk_expr(v)?;
                }
            }
            // Optional chaining `?.` / null-coalescing `??`: lower the carrier to a `match` in place,
            // then re-walk the resulting `Match` so its scrutinee and arm bodies (the synthesized
            // field/method access) are normalized like any other expression.
            ExprKind::OptChain { .. } | ExprKind::NullCoalesce { .. } => {
                self.lower_carrier(expr);
                return self.walk_expr(expr);
            }
            // A bare rvalue use of a `ref` binding auto-derefs to `<name>.get()`. (Write targets,
            // alias-inits, and ref-param call args are handled by their callers *before* reaching
            // here, so this only fires for genuine value reads.) The synthesized `get`/`set` calls
            // built elsewhere hold the un-derefed box ident and are never routed back through here.
            ExprKind::Ident(n) => {
                if self.is_ref(n) {
                    let box_ident = std::mem::replace(expr, ident_expr("", expr.span));
                    *expr = method_call(box_ident, "get", vec![]);
                    return Ok(());
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

        // Now normalize this node if it is a resolvable call (skipped for interpolation fragments,
        // which carry no module context and only need carrier lowering).
        if !self.skip_normalize
            && let ExprKind::Call { .. } = &expr.kind
        {
            self.normalize_call(expr)?;
        }
        Ok(())
    }

    /// Mint a fresh, collision-proof temp name (`__opt0`, `__opt1`, …) for an opt-chain payload bind.
    fn fresh_opt_name(&mut self) -> String {
        let n = self.next_tmp;
        self.next_tmp += 1;
        format!("__opt{n}")
    }

    /// Lower an `OptChain` / `NullCoalesce` carrier (in place) to an expression-position `match`:
    ///   `a ?? b`     → `match a: Some(__c): __c; None: b`
    ///   `x?.field`   → `match x: Some(__c): Some(__c.field); None: None`
    ///   `x?.m(args)` → `match x: Some(__c): Some(__c.m(args)); None: None`
    /// The scrutinee is evaluated once by `match`; the payload binds to a fresh `__c`. The arm bodies
    /// and field/method access use only nodes the checker + both engines already handle.
    fn lower_carrier(&mut self, expr: &mut Expr) {
        let span = expr.span;
        let kind = std::mem::replace(&mut expr.kind, ExprKind::Bool(false));
        expr.kind = match kind {
            ExprKind::NullCoalesce { lhs, rhs } => {
                let c = self.fresh_opt_name();
                ExprKind::Match {
                    scrutinee: lhs,
                    arms: vec![
                        MatchExprArm {
                            pattern: variant_pat(
                                "Some",
                                vec![Pattern::Ident(c.clone(), Span::default())],
                            ),
                            guard: None,
                            body: ident_expr(&c, span),
                        },
                        MatchExprArm {
                            pattern: variant_pat("None", vec![]),
                            guard: None,
                            body: *rhs,
                        },
                    ],
                }
            }
            ExprKind::OptChain { obj, name, call } => {
                let c = self.fresh_opt_name();
                let field = Expr {
                    kind: ExprKind::Field {
                        obj: Box::new(ident_expr(&c, span)),
                        name,
                        name_span: span,
                    },
                    span,
                };
                // `__c.field` or `__c.method(args)`, then wrapped in `Some(...)`.
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

    /// The per-parameter `is_ref` flags of a call's callee, in declaration order, if it resolves to a
    /// registered free function / struct constructor / unambiguous struct method. Mirrors
    /// `normalize_call`'s resolution so the call-arg alias/deref decision uses the SAME rule as the
    /// checker's coercion check. `None` for closures, builtins, fn-fields, or unknown callees (no
    /// `ref` param info available — the args then walk normally / auto-deref).
    fn callee_param_is_ref(&self, callee: &Expr) -> Option<Vec<bool>> {
        match &callee.kind {
            // A bare callee: a registered free fn / ctor (non-local), OR a LOCAL fn-value bound to a
            // named fn / closure (charges 1 & 3). The local case is resolved through `local_fn`, the
            // type-directed link the syntactic name lookup alone cannot see.
            ExprKind::Ident(name) if !self.is_local(name) => self
                .ctx
                .resolve_bare(name)
                .map(|s| s.iter().map(|p| p.is_ref).collect()),
            ExprKind::Ident(name) => self.local_fn_flags(name).cloned(),
            ExprKind::Field { obj, name, .. } => match &obj.kind {
                // `module.f(...)` — a qualified free fn.
                ExprKind::Ident(alias)
                    if !self.is_local(alias) && self.ctx.aliases.contains_key(alias) =>
                {
                    self.ctx
                        .resolve_qualified(alias, name)
                        .map(|s| s.iter().map(|p| p.is_ref).collect())
                }
                _ if !self.ctx.fn_fields.contains(name) => {
                    // A method call `recv.m(...)`. If the receiver's struct type is knowable pre-type
                    // (a named local, an inline ctor call, or a struct-returning fn call — see
                    // `receiver_struct_ty`), resolve `m` against THAT struct (charge 2 — sibling
                    // structs may disagree on a param's ref-ness). This receiver-aware lookup binds
                    // the exact user method even when its name collides with a builtin.
                    if let Some(sname) = self.receiver_struct_ty(obj)
                        && let Some(spec) = self.ctx.methods_by_struct.get(&(sname, name.clone()))
                    {
                        return Some(spec.iter().map(|p| p.is_ref).collect());
                    }
                    // A builtin-named call with an unknowable/builtin receiver: no `ref` info (the
                    // receiver may be a list/str/map/set). Don't consult the name-keyed user-method
                    // table — it could mis-bind a genuine builtin call's args.
                    if is_builtin_method(name) {
                        return None;
                    }
                    // Otherwise fall back to the name-keyed table, using it only when every defining
                    // struct agrees (an unambiguous pre-type shape).
                    match self.ctx.methods.get(name.as_str()) {
                        Some(cands)
                            if !cands.is_empty() && cands.iter().all(|c| *c == cands[0]) =>
                        {
                            Some(cands[0].iter().map(|p| p.is_ref).collect())
                        }
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Resolve `expr` (a `Call`) to a callable and rewrite named/omitted args into positional. Leaves
    /// the call untouched when the callee is not a registered callable (unless it carries named args,
    /// which is then an error).
    fn normalize_call(&self, expr: &mut Expr) -> Result<(), ResolveError> {
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
                if is_builtin_method(name) {
                    // The method name collides with a builtin (`add`, `map`, `push`, …). The receiver
                    // might be a genuine builtin value (List/Set/Map/str) whose shape we cannot see in
                    // this pre-type pass, so we resolve ONLY when the receiver's struct type is
                    // statically knowable AND that exact struct defines the method (then it's a user
                    // method — full named/default support). Anything else (builtin receiver, or an
                    // unknowable receiver) stays None: NO name-keyed fallback that could mis-bind a
                    // builtin call.
                    self.receiver_struct_ty(obj).and_then(|sname| {
                        self.ctx
                            .methods_by_struct
                            .get(&(sname, name.clone()))
                            .cloned()
                    })
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
            // both engines can read them off the Call AST.
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

        let mut out = Vec::with_capacity(params.len());
        for (i, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(e) => out.push(e),
                None => match &params[i].default {
                    Some(d) => out.push(d.clone()),
                    None => {
                        return Err(err(
                            span,
                            format!("missing required argument '{}'", params[i].name),
                        ));
                    }
                },
            }
        }
        *args = out;
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

/// A bare identifier expression at `span`.
fn ident_expr(name: &str, span: Span) -> Expr {
    Expr {
        kind: ExprKind::Ident(name.to_string()),
        span,
    }
}

/// `<recv>.<method>(<args>)` — a no-default method call carrying the receiver's span. Used by the
/// `ref T` lowering to build `r.get()` (read) and `r.set(v)` (write). `named`/`type_args` are empty.
fn method_call(recv: Expr, method: &str, args: Vec<Expr>) -> Expr {
    let span = recv.span;
    let callee = Expr {
        kind: ExprKind::Field {
            obj: Box::new(recv),
            name: method.to_string(),
            name_span: span,
        },
        span,
    };
    Expr {
        kind: ExprKind::Call {
            callee: Box::new(callee),
            args,
            named: vec![],
            type_args: vec![],
        },
        span,
    }
}

/// `Ref(<value>)` — a fresh-box constructor call for a `ref T` create-init. `Ref` is a reserved
/// global (backs the `ref` keyword): `std/ref.chz` is always linked into the graph, so this resolves
/// import-free, like `Result`/`Option`. No `import std.ref` needed.
fn ref_ctor(value: Expr) -> Expr {
    let span = value.span;
    Expr {
        kind: ExprKind::Call {
            callee: Box::new(ident_expr("Ref", span)),
            args: vec![value],
            named: vec![],
            type_args: vec![],
        },
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

    #[test]
    fn ambiguous_method_named_errors() {
        // Two structs define `set` with different params; a named call can't be bound unambiguously.
        assert!(desugar_err(
            "struct A:\n    n: int\n    fn set(self, x: int) -> int:\n        return x\nstruct B:\n    n: int\n    fn set(self, y: int) -> int:\n        return y\na := A(0)\nr := a.set(x=1)\n",
        )
        .message
        .contains("multiple structs"));
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
    fn null_coalesce_desugars_to_match() {
        let stmts = desugar_ok("a := Some(1)\nx := a ?? 0\n");
        match last_let_value(&stmts).kind {
            ExprKind::Match { arms, .. } => {
                assert_eq!(arms.len(), 2);
                // Some(__optN): __optN ; None: 0
                assert!(
                    matches!(&arms[0].pattern, Pattern::Variant { name, .. } if name == "Some")
                );
                assert!(
                    matches!(&arms[1].pattern, Pattern::Variant { name, bindings, .. } if name == "None" && bindings.is_empty())
                );
            }
            other => panic!("expected a Match, got {other:?}"),
        }
    }

    #[test]
    fn opt_chain_field_desugars_to_match() {
        let stmts = desugar_ok("struct P:\n    x: int\na := Some(P(1))\nv := a?.x\n");
        match last_let_value(&stmts).kind {
            ExprKind::Match { arms, .. } => {
                // Some arm wraps the field access in `Some(...)`; None arm yields the `None` ident.
                assert!(matches!(&arms[0].body.kind, ExprKind::Call { callee, .. }
                    if matches!(&callee.kind, ExprKind::Ident(n) if n == "Some")));
                assert!(matches!(&arms[1].body.kind, ExprKind::Ident(n) if n == "None"));
            }
            other => panic!("expected a Match, got {other:?}"),
        }
    }

    #[test]
    fn two_coalesce_in_one_expr_get_unique_temps() {
        // `(a ?? 0) + (b ?? 0)` — each desugared match must bind a DISTINCT temp name.
        let stmts = desugar_ok("a := Some(1)\nb := Some(2)\nx := (a ?? 0) + (b ?? 0)\n");
        let ExprKind::Binary { lhs, rhs, .. } = last_let_value(&stmts).kind else {
            panic!("expected a Binary");
        };
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

    #[test]
    fn non_const_default_filled() {
        // A call expression as a default is cloned into the call site (left as a Call to evaluate).
        let s = desugar_ok(
            "fn g() -> int:\n    return 9\nfn f(x: int = g() + 1):\n    print(x)\nr := f()\n",
        );
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else {
            panic!("let")
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!("call")
        };
        assert_eq!(args.len(), 1, "the omitted default was filled");
        assert!(
            matches!(args[0].kind, ExprKind::Binary { .. }),
            "default is the `g() + 1` expr"
        );
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
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else {
            panic!("let")
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!("call f")
        };
        // f's single arg is the spliced default `g(7)` — a Call with one positional arg.
        let ExprKind::Call { args: ginner, .. } = &args[0].kind else {
            panic!("inner call g")
        };
        assert_eq!(
            ginner.len(),
            1,
            "g()'s own default was filled in the spliced default"
        );
    }

    #[test]
    fn carrier_in_default_is_lowered() {
        // A `??` carrier inside a default must be lowered to a `match` (else the checker/VM panics).
        let s = desugar_ok(
            "fn h() -> int?:\n    return Some(5)\nfn f(x: int = h() ?? 0):\n    print(x)\nr := f()\n",
        );
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else {
            panic!("let")
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!("call f")
        };
        // The spliced default must be a lowered `match` (NullCoalesce carrier is gone).
        assert!(
            matches!(args[0].kind, ExprKind::Match { .. }),
            "carrier lowered to match, got {:?}",
            args[0].kind
        );
    }

    // ===== `ref T` binding lowering =====

    /// True if `e` is `Ref(<arg>)` — a call of the bare `Ref` constructor with one positional arg.
    fn is_ref_create(e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Call { callee, args, .. }
            if matches!(&callee.kind, ExprKind::Ident(n) if n == "Ref") && args.len() == 1)
    }

    /// True if `e` is `<recv>.get()` — a no-arg method call named `get`.
    fn is_get_call(e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Call { callee, args, .. }
            if args.is_empty()
            && matches!(&callee.kind, ExprKind::Field { name, .. } if name == "get"))
    }

    /// True if `e` is `<recv>.set(<arg>)`.
    fn is_set_call(e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Call { callee, args, .. }
            if args.len() == 1
            && matches!(&callee.kind, ExprKind::Field { name, .. } if name == "set"))
    }

    #[test]
    fn lowers_ref_read_write() {
        let s = desugar_ok("r: ref int = 0\nprint(r)\nr = 5\nr += 1\n");
        // 1) `r: ref int = 0`  ->  `r := Ref(0)` (create a fresh box)
        let StmtKind::Let { value, .. } = &s[0].kind else {
            panic!("let")
        };
        assert!(
            is_ref_create(value),
            "init should be Ref(0), got {:?}",
            value.kind
        );
        // 2) `print(r)`  ->  `print(r.get())`  (rvalue read auto-derefs)
        let StmtKind::Expr(e) = &s[1].kind else {
            panic!("expr")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("print call")
        };
        assert!(
            is_get_call(&args[0]),
            "rvalue read should be r.get(), got {:?}",
            args[0].kind
        );
        // 3) `r = 5`  ->  `r.set(5)`  (assignment lowers to a statement-expr set call)
        let StmtKind::Expr(e) = &s[2].kind else {
            panic!("set stmt, got {:?}", s[2].kind)
        };
        assert!(
            is_set_call(e),
            "assign should lower to r.set(5), got {:?}",
            e.kind
        );
        // 4) `r += 1`  ->  `r.set(r.get() + 1)`
        let StmtKind::Expr(e) = &s[3].kind else {
            panic!("compound set stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("set call")
        };
        let ExprKind::Binary { lhs, .. } = &args[0].kind else {
            panic!("set arg should be a binary")
        };
        assert!(
            is_get_call(lhs),
            "compound lhs should be r.get(), got {:?}",
            lhs.kind
        );
    }

    #[test]
    fn aliases_ref_ident() {
        // `r2: ref int = r` (RHS is already a ref binding) -> ALIAS: keep `r2 := r`, NOT `Ref(r)`.
        let s = desugar_ok("r: ref int = 0\nr2: ref int = r\n");
        let StmtKind::Let { value, .. } = &s[1].kind else {
            panic!("let")
        };
        assert!(
            !is_ref_create(value),
            "alias must NOT wrap in Ref(), got {:?}",
            value.kind
        );
        assert!(
            matches!(&value.kind, ExprKind::Ident(n) if n == "r"),
            "alias keeps the box ident"
        );
    }

    #[test]
    fn lowers_ref_arg_by_param_kind() {
        // byref(r) passes the box (alias); byval(r) auto-derefs to r.get() (a copy).
        let src = "fn byref(x: ref int):\n    x = 1\nfn byval(x: int):\n    print(x)\nr: ref int = 0\nbyref(r)\nbyval(r)\n";
        let s = desugar_ok(src);
        // byref(r) — last-but-one stmt
        let StmtKind::Expr(e) = &s[s.len() - 2].kind else {
            panic!("byref call stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            matches!(&args[0].kind, ExprKind::Ident(n) if n == "r"),
            "ref param arg should stay the bare box ident, got {:?}",
            args[0].kind
        );
        // byval(r) — last stmt
        let StmtKind::Expr(e) = &s[s.len() - 1].kind else {
            panic!("byval call stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            is_get_call(&args[0]),
            "non-ref param arg should auto-deref to r.get(), got {:?}",
            args[0].kind
        );
    }

    #[test]
    fn lowers_ref_arg_through_local_fn_value() {
        // Charge 1: `g := bump; g(r)` resolves through the LOCAL fn-value to bump's `ref` param, so
        // the box aliases (the arg stays the bare ident, not `.get()`).
        let src = "fn bump(x: ref int):\n    x = 1\nr: ref int = 0\ng := bump\ng(r)\n";
        let s = desugar_ok(src);
        let StmtKind::Expr(e) = &s[s.len() - 1].kind else {
            panic!("g(r) call stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            matches!(&args[0].kind, ExprKind::Ident(n) if n == "r"),
            "indirect ref param arg should alias (bare ident), got {:?}",
            args[0].kind
        );
    }

    #[test]
    fn lowers_ref_arg_through_receiver_typed_method() {
        // Charge 2: two structs share `apply` with DIFFERENT ref-ness; `a.apply(r)` resolves to A's
        // (ref) signature via the receiver type, so the box aliases — and `b.apply(r)` resolves to
        // B's (by-value) signature, so the arg auto-derefs.
        let src = "struct A:\n    t: int\n    fn apply(self, x: ref int):\n        x = 1\nstruct B:\n    t: int\n    fn apply(self, x: int):\n        print(x)\nr: ref int = 0\na := A(0)\nb := B(0)\na.apply(r)\nb.apply(r)\n";
        let s = desugar_ok(src);
        let StmtKind::Expr(e) = &s[s.len() - 2].kind else {
            panic!("a.apply stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            matches!(&args[0].kind, ExprKind::Ident(n) if n == "r"),
            "A.apply (ref) arg should alias (bare ident), got {:?}",
            args[0].kind
        );
        let StmtKind::Expr(e) = &s[s.len() - 1].kind else {
            panic!("b.apply stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            is_get_call(&args[0]),
            "B.apply (by-value) arg should auto-deref to r.get(), got {:?}",
            args[0].kind
        );
    }

    #[test]
    fn lowers_ref_arg_through_ctor_receiver_typed_method() {
        // Charge 2 (expression receiver): two structs share `apply` with DIFFERENT ref-ness; an
        // INLINE ctor-call receiver `A(0).apply(r)` must resolve to A's (ref) signature via the
        // receiver's struct type (just like a named local), so the box aliases — and `B(0).apply(r)`
        // resolves to B's (by-value) signature, so the arg auto-derefs.
        let src = "struct A:\n    t: int\n    fn apply(self, x: ref int):\n        x = 1\nstruct B:\n    t: int\n    fn apply(self, x: int):\n        print(x)\nr: ref int = 0\nA(0).apply(r)\nB(0).apply(r)\n";
        let s = desugar_ok(src);
        let StmtKind::Expr(e) = &s[s.len() - 2].kind else {
            panic!("A(0).apply stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            matches!(&args[0].kind, ExprKind::Ident(n) if n == "r"),
            "A(0).apply (ref) arg should alias (bare ident), got {:?}",
            args[0].kind
        );
        let StmtKind::Expr(e) = &s[s.len() - 1].kind else {
            panic!("B(0).apply stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            is_get_call(&args[0]),
            "B(0).apply (by-value) arg should auto-deref to r.get(), got {:?}",
            args[0].kind
        );
    }

    #[test]
    fn lowers_ref_arg_through_fn_call_receiver_typed_method() {
        // Charge 2 (struct-returning free-fn receiver): `mk()` returns `A`, so `mk().apply(r)` must
        // resolve to A's (ref) signature via the return type and alias the box.
        let src = "struct A:\n    t: int\n    fn apply(self, x: ref int):\n        x = 1\nstruct B:\n    t: int\n    fn apply(self, x: int):\n        print(x)\nfn mk() -> A:\n    return A(0)\nr: ref int = 0\nmk().apply(r)\n";
        let s = desugar_ok(src);
        let StmtKind::Expr(e) = &s[s.len() - 1].kind else {
            panic!("mk().apply stmt")
        };
        let ExprKind::Call { args, .. } = &e.kind else {
            panic!("call")
        };
        assert!(
            matches!(&args[0].kind, ExprKind::Ident(n) if n == "r"),
            "mk().apply (ref) arg should alias (bare ident), got {:?}",
            args[0].kind
        );
    }

    #[test]
    fn closure_ref_param_lowers_body_read() {
        // Charge 3: a closure `ref` param drives body read/write lowering like a named-fn ref param.
        // The body `x + 1` lowers to `x.get() + 1`.
        let s = desugar_ok("g := fn(x: ref int) -> int: x + 1\n");
        let StmtKind::Let { value, .. } = &s[0].kind else {
            panic!("let")
        };
        let ExprKind::Closure { body, .. } = &value.kind else {
            panic!("closure")
        };
        let ExprKind::Binary { lhs, .. } = &body.kind else {
            panic!("binary body")
        };
        assert!(
            is_get_call(lhs),
            "closure ref read should lower to x.get(), got {:?}",
            lhs.kind
        );
    }

    #[test]
    fn closure_byval_arg_into_ref_param_errors() {
        // Charge 3: a by-value local into a closure `ref` param is the same row-3 error as a named fn.
        let e = desugar_err("g := fn(x: ref int) -> int: x + 1\nn := 5\ng(n)\n");
        assert!(
            e.message.contains("by-reference") && e.message.contains("declare"),
            "expected the by-value->ref error, got: {:?}",
            e.message
        );
    }
}
