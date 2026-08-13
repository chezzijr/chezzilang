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
//! not reference another parameter/field — `validate_defaults` enforces this (the default is cloned
//! into the caller's scope at the omitting call site, where parameters/fields are not bound).
//!
//! The pass is **scope-aware**: a local binding may shadow a top-level function name, so a call is
//! only rewritten when its callee resolves to a registered callable and is *not* shadowed by a local
//! (mirroring the checker, which treats a call as a named function only when the name is not a local).

use crate::ast::{
    Block, Chunk, DeferTarget, Expr, ExprKind, Import, MatchExprArm, Module, OptCall, Param,
    Pattern, Span, SpawnTarget, Stmt, StmtKind, Type, WaitArmKind, WaitTarget,
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
                local_struct: Vec::new(),
                first_pass: pass == 0,
                depth: 0,
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
            local_struct: Vec::new(),
            first_pass: pass == 0,
            depth: 0,
        };
        walker.walk_block(&mut module.stmts)?;
    }
    Ok(())
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
            // A native struct's BODIED methods compile like struct methods, so a caller using
            // named/default args on one needs its param-spec registered here too.
            StmtKind::NativeStruct { bodied_methods, .. } => bodied_methods,
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
                    is_variadic: p.is_variadic,
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
                            is_variadic: p.is_variadic,
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
            StmtKind::NativeStruct {
                name,
                bodied_methods,
                ..
            } => (name, bodied_methods),
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
                    is_variadic: p.is_variadic,
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
        // A fragment identifier IS a reference (`"{a}"` reads `a`), so descend. Only reachable
        // after `desugar` has rewritten the literal — `validate_defaults` runs before that and sees
        // the raw `Str` above, where a fragment reference is caught later by the checker instead.
        ExprKind::Interp(chunks) => chunks.iter().for_each(|c| {
            if let Chunk::Expr(e, _) = c {
                walk_idents(e, f)
            }
        }),
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
                            is_variadic: p.is_variadic,
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
    /// Variadic-collapse runs on the FIRST pass only. The module pass walks the tree twice (to lower
    /// spliced defaults); the collapse is NOT idempotent (re-running it on pass 2 would wrap the
    /// synthesized `List` in another `List`), so it must fire exactly once.
    first_pass: bool,
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
                    self.bind_param(p);
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
    /// **KNOWN RESIDUAL, and the reason this doc says "the tree this walk reaches" and not "every
    /// tree".** There is one way an *un-converted but well-formed* `Str` survives to those callers: a
    /// default argument spliced in during **pass 2**. `regs` is built once, before both passes, so a
    /// spliced default is raw declaration AST; `normalize_call` splices it in the TAIL of
    /// `walk_expr_inner`, after this node's children were walked; and there is no pass 3. A
    /// default-of-a-default is exactly that shape. Measured on a working-tree release binary,
    /// `fn g(a: int = "{ 1+1×15990 }".len())` / `fn h(b: int = g())` / `x := h()+1×15990`:
    /// `check` rc 0, this counter peaked at **15 995**, and the tree the checker and compiler
    /// actually descend is **~31 986** nodes — ~1.03× the measured ~33 100-node cliff. Latent, not
    /// live (no abort reachable: the decl-site walk caps the spliced default at ~16 000, and a
    /// three-deep default chain hits an unrelated pre-existing arity error), and **pre-existing** —
    /// the same fixture is accepted on `e1137096`. Not closed here because the fix belongs in the
    /// two-pass driver / `normalize_call`, which W7-51 is rewriting; closing it there means walking a
    /// pass-2 splice, or building `regs` from already-lowered ASTs. `docs/gaps.md` W7-50 tracks it.
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
    ///
    /// Idempotent on pass 2 for the same reason `normalize_call` is: the variadic collapse inside it
    /// is `first_pass`-gated, and an already-bound call has empty `named` and full arity.
    fn normalize_opt_call(&self, expr: &mut Expr) -> Result<(), ResolveError> {
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

        // A VARIADIC callable (`fn f(pre..., ...xs: T, kwonly...)`) collapses the surplus trailing
        // positionals into a synthesized `List` literal at the variadic slot, and binds the
        // keyword-only tail (everything after the variadic) from named args / defaults. After this the
        // call is an ordinary fully-positional call, so the checker AND both engines need zero
        // variadic-specific logic (parity by construction). This must run BEFORE the fixed-arity gates
        // below (a `f(1,2,3)` into a single `List` slot is "too many" by those rules).
        // Collapse on the FIRST pass only: after pass 1 the call is already in final collapsed form
        // (`args = [pre..., List, kwonly...]`, `named` empty), so re-running it on pass 2 would wrap
        // the synthesized `List` in another `List`. On pass 2 the collapsed call falls through to the
        // fixed-arity gate below, which is a no-op for it.
        if self.first_pass
            && let Some(v) = params.iter().position(|p| p.is_variadic)
        {
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
                        Some(d) => out.push(d.clone()),
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
                kind: ExprKind::List(elems),
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
                    out.push(d.clone());
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

/// Lower an `OptChain` / `NullCoalesce` carrier (in place) to an expression-position `match` —
/// the **Option** lowering:
///   `a ?? b`     → `match a: Some(__optN): __optN; None: b`
///   `x?.field`   → `match x: Some(__optN): Some(__optN.field); None: None`
///   `x?.m(args)` → `match x: Some(__optN): Some(__optN.m(args)); None: None`
/// The scrutinee is evaluated once by `match`; the payload binds to `__opt{tmp}` (the caller owns the
/// counter, so temps stay unique within one expression). The arm bodies and field/method access use
/// only nodes the checker + both engines already handle.
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
    fn carrier_in_default_survives_with_a_normalized_child() {
        // W7-43 inverted this: a `??` carrier spliced from a default SURVIVES desugar. What must
        // still hold is that the two-pass splice normalized its children — here `h()`'s own omitted
        // default is filled inside the spliced carrier's lhs.
        let s = desugar_ok(
            "fn h(k: int = 3) -> int?:\n    return Some(k)\nfn f(x: int = h() ?? 0):\n    print(x)\nr := f()\n",
        );
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else {
            panic!("let")
        };
        let ExprKind::Call { args, .. } = &value.kind else {
            panic!("call f")
        };
        let ExprKind::NullCoalesce { lhs, .. } = &args[0].kind else {
            panic!("carrier survives, got {:?}", args[0].kind)
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
        let ExprKind::List(es) = k else {
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
