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
    Block, DeferTarget, Expr, ExprKind, Import, MatchExprArm, Module, OptCall, Param, Pattern, Span,
    SpawnTarget, Stmt, StmtKind, Type,
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
}

/// Built-in / core methods on `str`/`list`/`map`/`set` (kept in sync with the checker's
/// `*_method_sig` tables + the HOF/`sort` handling in `infer_method_call`). A method call whose name
/// is one of these is never rewritten by the method path — its receiver may be a builtin type whose
/// shape we cannot see here. A user struct that happens to reuse one of these names therefore does
/// not get default/named support on that method (a documented, narrow limitation).
const BUILTIN_METHODS: &[&str] = &[
    "len", "upper", "lower", "trim", "message", "split", "chars", "join", "starts_with", "contains",
    "push", "pop", "reverse", "index_of", "sum", "sort", "map", "filter", "fold", "sort_by",
    "sort_by_key", "has", "get", "keys", "values", "remove", "add", "union", "intersection",
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
    for _pass in 0..2 {
    for mi in 0..graph.modules.len() {
        // Build this module's resolution context: own id + bare from-imports + module aliases.
        let own_id = graph.modules[mi].id.clone();
        let mut bare_from: HashMap<String, ModuleId> = HashMap::new();
        let mut aliases: HashMap<String, ModuleId> = HashMap::new();
        for imp in &graph.modules[mi].imports {
            match &imp.import {
                Import::Module { path, alias } => {
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
            fn_fields: &fn_fields,
        };
        let mut walker = Walker {
            ctx,
            scopes: Vec::new(),
            next_tmp: 0,
            skip_normalize: false,
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
    let mut fn_fields = HashSet::new();
    collect_fn_fields_into(&module.stmts, &mut fn_fields);
    let bare_from = HashMap::new();
    let aliases = HashMap::new();
    // Two passes — see the comment in [`run`] (spliced defaults are lowered on the second pass).
    for _pass in 0..2 {
        let ctx = Ctx {
            regs: &regs,
            own_id: &id,
            bare_from: &bare_from,
            aliases: &aliases,
            methods: &methods,
            fn_fields: &fn_fields,
        };
        let mut walker = Walker { ctx, scopes: Vec::new(), next_tmp: 0, skip_normalize: false };
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
    let fn_fields: HashSet<String> = HashSet::new();
    let ctx = Ctx {
        regs: &regs,
        own_id: &own_id,
        bare_from: &bare_from,
        aliases: &aliases,
        methods: &methods,
        fn_fields: &fn_fields,
    };
    let mut walker = Walker { ctx, scopes: Vec::new(), next_tmp: 0, skip_normalize: true };
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
        if let StmtKind::Struct { methods, .. } = &stmt.kind {
            for method in methods {
                let spec: Vec<PSpec> = method
                    .params
                    .iter()
                    .skip(1)
                    .map(|p| PSpec { name: p.name.clone(), default: p.default.clone() })
                    .collect();
                map.entry(method.name.clone()).or_default().push(spec);
            }
        }
    }
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
            StmtKind::Struct { fields, methods, .. } => {
                let fnames: HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
                for fld in fields {
                    if let Some(d) = &fld.default
                        && let Some(n) = default_referenced_name(d, &fnames)
                    {
                        return Err(err(
                            d.span,
                            format!("default value cannot reference field '{n}' (defaults are evaluated at the call site, where fields are not in scope)"),
                        ));
                    }
                }
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
                format!("default value cannot reference parameter '{n}' (defaults are evaluated at the call site, where parameters are not in scope)"),
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
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Bool(_) => {}
        ExprKind::List(xs) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().for_each(|x| walk_idents(x, f))
        }
        ExprKind::Map(ps) => ps.iter().for_each(|(k, v)| {
            walk_idents(k, f);
            walk_idents(v, f);
        }),
        ExprKind::Comprehension { key, elem, iter, guard, .. } => {
            if let Some(k) = key {
                walk_idents(k, f);
            }
            walk_idents(elem, f);
            walk_idents(iter, f);
            if let Some(g) = guard {
                walk_idents(g, f);
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
        ExprKind::Call { callee, args, named, .. } => {
            walk_idents(callee, f);
            args.iter().for_each(|a| walk_idents(a, f));
            named.iter().for_each(|(_, a)| walk_idents(a, f));
        }
        ExprKind::Field { obj, .. } => walk_idents(obj, f),
        ExprKind::Index { obj, index } => {
            walk_idents(obj, f);
            walk_idents(index, f);
        }
        ExprKind::Slice { obj, start, end } => {
            walk_idents(obj, f);
            walk_idents(start, f);
            walk_idents(end, f);
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
                        .map(|p| PSpec { name: p.name.clone(), default: p.default.clone() })
                        .collect(),
                );
            }
            StmtKind::Struct { name, fields, .. } => {
                reg.structs.insert(
                    name.clone(),
                    fields
                        .iter()
                        .map(|f| PSpec { name: f.name.clone(), default: f.default.clone() })
                        .collect(),
                );
            }
            _ => {}
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
    /// Counter for fresh temp names minted when lowering `?.`/`??` to `match` (`__opt0`, `__opt1`, …).
    /// `__`-prefixed names can't be written by user code, so they never collide with a real binding.
    next_tmp: usize,
    /// When set, skip `normalize_call` (named/default-arg resolution) — used for string-interpolation
    /// fragments, which are re-parsed after the module-wide pass and need only carrier lowering
    /// (their call-normalization was already skipped before this pass existed; kept identical).
    skip_normalize: bool,
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
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
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
            StmtKind::Let { names, value, .. } => {
                self.walk_expr(value)?;
                for n in names.iter() {
                    self.bind(n);
                }
            }
            StmtKind::Assign { target, value, .. } => {
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
                    self.bind(&p.name);
                }
                self.walk_block(&mut decl.body)?;
                self.pop_scope();
            }
            StmtKind::Struct { fields, methods, .. } => {
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
                    }
                    self.walk_block(&mut m.body)?;
                    self.pop_scope();
                }
            }
            StmtKind::If { branches, else_block } => {
                for (cond, body) in branches.iter_mut() {
                    self.walk_expr(cond)?;
                    self.walk_block(body)?;
                }
                if let Some(b) = else_block {
                    self.walk_block(b)?;
                }
            }
            StmtKind::For { vars, iter, body } => {
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
            StmtKind::Defer(target) => match target {
                DeferTarget::Call(e) => self.walk_expr(e)?,
                DeferTarget::Block(body) => self.walk_block(body)?,
            },
            StmtKind::Expr(e) => self.walk_expr(e)?,
            StmtKind::Parallel { body } => self.walk_block(body)?,
            StmtKind::Spawn(target) => match target {
                SpawnTarget::Call(e) => self.walk_expr(e)?,
                SpawnTarget::Block(body) => self.walk_block(body)?,
            },
            // No nested expressions / bindings to rewrite.
            StmtKind::Return(None)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Import(_)
            | StmtKind::Protocol { .. }
            | StmtKind::Enum { .. }
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
            ExprKind::Slice { obj, start, end } => {
                self.walk_expr(obj)?;
                self.walk_expr(start)?;
                self.walk_expr(end)?;
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
            ExprKind::Comprehension { key, elem, vars, iter, guard, .. } => {
                // `iter` is evaluated in the outer scope; `vars` are bound only for the element,
                // key, and guard expressions.
                self.walk_expr(iter)?;
                self.push_scope();
                for v in vars.iter() {
                    self.bind(v);
                }
                if let Some(g) = guard {
                    self.walk_expr(g)?;
                }
                if let Some(k) = key {
                    self.walk_expr(k)?;
                }
                self.walk_expr(elem)?;
                self.pop_scope();
            }
            ExprKind::Call { callee, args, named, .. } => {
                self.walk_expr(callee)?;
                for a in args.iter_mut() {
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
            // Leaves.
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bool(_)
            | ExprKind::Ident(_) => {}
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
                            pattern: variant_pat("Some", vec![Pattern::Ident(c.clone())]),
                            guard: None,
                            body: ident_expr(&c, span),
                        },
                        MatchExprArm { pattern: variant_pat("None", vec![]), guard: None, body: *rhs },
                    ],
                }
            }
            ExprKind::OptChain { obj, name, call } => {
                let c = self.fresh_opt_name();
                let field = Expr {
                    kind: ExprKind::Field { obj: Box::new(ident_expr(&c, span)), name },
                    span,
                };
                // `__c.field` or `__c.method(args)`, then wrapped in `Some(...)`.
                let access = match call {
                    None => field,
                    Some(OptCall { args, named, type_args }) => Expr {
                        kind: ExprKind::Call { callee: Box::new(field), args, named, type_args },
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
                            pattern: variant_pat("Some", vec![Pattern::Ident(c)]),
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

    /// Resolve `expr` (a `Call`) to a callable and rewrite named/omitted args into positional. Leaves
    /// the call untouched when the callee is not a registered callable (unless it carries named args,
    /// which is then an error).
    fn normalize_call(&self, expr: &mut Expr) -> Result<(), ResolveError> {
        let span = expr.span;
        let ExprKind::Call { callee, args, named, .. } = &expr.kind else {
            return Ok(());
        };

        // Resolve a free function / struct ctor / module-qualified callee (clone the spec so we can
        // then mutate `expr`).
        let module_spec: Option<Vec<PSpec>> = match &callee.kind {
            ExprKind::Ident(name) if !self.is_local(name) => self.ctx.resolve_bare(name).cloned(),
            ExprKind::Field { obj, name } => match &obj.kind {
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
            (None, ExprKind::Field { name, .. })
                if !is_builtin_method(name) && !self.ctx.fn_fields.contains(name) =>
            {
                match self.ctx.methods.get(name.as_str()) {
                    Some(cands) if !cands.is_empty() => {
                        if cands.iter().all(|c| *c == cands[0]) {
                            Some(cands[0].clone())
                        } else if !named.is_empty() {
                            return Err(err(
                                span,
                                format!("cannot bind named arguments for method '{name}': multiple structs define it with different parameters — pass arguments positionally"),
                            ));
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        let Some(params) = module_spec.or(method_spec) else {
            // Not a registered callable. Named args here are unsupported (closures / builtin methods).
            if !named.is_empty() {
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
                format!("too many arguments: expected at most {}, got {}", params.len(), args.len()),
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
                        ))
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
        Pattern::Ident(n) => f(n.clone()),
        Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) | Pattern::Or(bindings) => {
            for b in bindings {
                bind_pattern(b, f);
            }
        }
        Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
    }
}

fn err(span: crate::lexer::Span, message: String) -> ResolveError {
    ResolveError { message, span, module: None }
}

/// A nullary-or-payload variant pattern (`Some(__c)` / `None`) for desugared opt-chain `match` arms.
fn variant_pat(name: &str, bindings: Vec<Pattern>) -> Pattern {
    Pattern::Variant { name: name.to_string(), bindings }
}

/// A bare identifier expression at `span`.
fn ident_expr(name: &str, span: Span) -> Expr {
    Expr { kind: ExprKind::Ident(name.to_string()), span }
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
            modules: vec![LoadedModule { id, dotted: vec![], ast, imports: vec![], native: None }],
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
        let s = desugar_ok(
            "fn f(x: int, y: int = 2, z: int = 3):\n    print(x)\nr := f(1, z=9)\n",
        );
        assert_eq!(call_arg_ints(&s), vec![1, 2, 9]);
    }

    #[test]
    fn struct_ctor_named_and_default() {
        let s = desugar_ok(
            "struct P:\n    x: int\n    y: int = 0\nr := P(x=5)\n",
        );
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
        assert!(desugar_err("fn f(x: int):\n    print(x)\nr := f(z=1)\n")
            .message
            .contains("unknown named argument 'z'"));
    }

    #[test]
    fn duplicate_positional_and_named_errors() {
        assert!(desugar_err("fn f(x: int, y: int):\n    print(x)\nr := f(1, x=2)\n")
            .message
            .contains("both positionally and by name"));
    }

    #[test]
    fn missing_required_with_named_errors() {
        assert!(desugar_err("fn f(x: int, y: int):\n    print(x)\nr := f(y=2)\n")
            .message
            .contains("missing required argument 'x'"));
    }

    #[test]
    fn named_on_non_callable_errors() {
        // a local closure called with a named arg is unsupported
        assert!(desugar_err("g := fn(x: int): x\nr := g(x=1)\n")
            .message
            .contains("only supported on functions, struct constructors, and struct methods"));
    }

    #[test]
    fn local_shadows_function_not_rewritten() {
        // `f` is shadowed by a local binding; the call must NOT pull the top-level fn's default.
        let s = desugar_ok(
            "fn f(x: int, y: int = 9):\n    print(x)\nfn main():\n    f := fn(a: int): a\n    r := f(1)\nmain()\n",
        );
        // find the inner call: in main's body, `r := f(1)` stays a single positional arg.
        let StmtKind::Fn(decl) = &s[1].kind else { panic!("expected main fn") };
        let StmtKind::Let { value, .. } = &decl.body[1].kind else { panic!("expected r := f(1)") };
        let ExprKind::Call { args, .. } = &value.kind else { panic!("expected call") };
        assert_eq!(args.len(), 1, "shadowed local call must keep its single arg");
    }

    /// Pull positional arg ints out of a method call `recv.m(...)` in the last statement.
    fn method_call_arg_ints(stmts: &[Stmt]) -> Vec<i64> {
        let last = stmts.last().expect("a statement");
        let expr = match &last.kind {
            StmtKind::Let { value, .. } => value,
            StmtKind::Expr(e) => e,
            other => panic!("expected let/expr, got {other:?}"),
        };
        let ExprKind::Call { args, named, callee, .. } = &expr.kind else {
            panic!("expected a Call, got {:?}", expr.kind)
        };
        assert!(matches!(callee.kind, ExprKind::Field { .. }), "expected a method call");
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
    fn nested_call_normalized() {
        // a defaulted call nested as an argument is also filled
        let s = desugar_ok(
            "fn g(a: int, b: int = 7):\n    print(a)\nfn f(x: int):\n    print(x)\nr := f(g(1))\n",
        );
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else { panic!() };
        let ExprKind::Call { args, .. } = &value.kind else { panic!() };
        let ExprKind::Call { args: inner, .. } = &args[0].kind else { panic!("inner call") };
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
                assert!(matches!(&arms[0].pattern, Pattern::Variant { name, .. } if name == "Some"));
                assert!(matches!(&arms[1].pattern, Pattern::Variant { name, bindings } if name == "None" && bindings.is_empty()));
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
            let ExprKind::Match { arms, .. } = &e.kind else { panic!("expected Match") };
            let Pattern::Variant { bindings, .. } = &arms[0].pattern else { panic!("variant") };
            let Pattern::Ident(n) = &bindings[0] else { panic!("ident binding") };
            n.clone()
        };
        assert_ne!(name_of(&lhs), name_of(&rhs), "temps must be unique");
    }

    // ===== non-constant default expressions =====

    #[test]
    fn non_const_default_filled() {
        // A call expression as a default is cloned into the call site (left as a Call to evaluate).
        let s = desugar_ok("fn g() -> int:\n    return 9\nfn f(x: int = g() + 1):\n    print(x)\nr := f()\n");
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else { panic!("let") };
        let ExprKind::Call { args, .. } = &value.kind else { panic!("call") };
        assert_eq!(args.len(), 1, "the omitted default was filled");
        assert!(matches!(args[0].kind, ExprKind::Binary { .. }), "default is the `g() + 1` expr");
    }

    #[test]
    fn param_referencing_default_rejected() {
        let e = desugar_err("fn f(x: int, y: int = x + 1):\n    print(y)\n");
        assert!(e.to_string().contains("cannot reference parameter 'x'"), "got: {e}");
    }

    #[test]
    fn field_referencing_default_rejected() {
        let e = desugar_err("struct S:\n    a: int = 1\n    b: int = a\n");
        assert!(e.to_string().contains("cannot reference field 'a'"), "got: {e}");
    }

    #[test]
    fn method_param_referencing_default_rejected() {
        let e = desugar_err("struct S:\n    n: int\n    fn go(self, x: int, y: int = x):\n        return y\n");
        assert!(e.to_string().contains("cannot reference parameter 'x'"), "got: {e}");
    }

    #[test]
    fn defaulted_fn_call_in_default_is_normalized() {
        // `f(x = g())` where `g(a = 7)`: the spliced default `g()` must itself be normalized to
        // `g(7)` (second pass), not left under-arity.
        let s = desugar_ok("fn g(a: int = 7) -> int:\n    return a\nfn f(x: int = g()):\n    print(x)\nr := f()\n");
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else { panic!("let") };
        let ExprKind::Call { args, .. } = &value.kind else { panic!("call f") };
        // f's single arg is the spliced default `g(7)` — a Call with one positional arg.
        let ExprKind::Call { args: ginner, .. } = &args[0].kind else { panic!("inner call g") };
        assert_eq!(ginner.len(), 1, "g()'s own default was filled in the spliced default");
    }

    #[test]
    fn carrier_in_default_is_lowered() {
        // A `??` carrier inside a default must be lowered to a `match` (else the checker/VM panics).
        let s = desugar_ok("fn h() -> int?:\n    return Some(5)\nfn f(x: int = h() ?? 0):\n    print(x)\nr := f()\n");
        let last = s.last().unwrap();
        let StmtKind::Let { value, .. } = &last.kind else { panic!("let") };
        let ExprKind::Call { args, .. } = &value.kind else { panic!("call f") };
        // The spliced default must be a lowered `match` (NullCoalesce carrier is gone).
        assert!(matches!(args[0].kind, ExprKind::Match { .. }), "carrier lowered to match, got {:?}", args[0].kind);
    }
}
