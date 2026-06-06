//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching the interpreter's hoist); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Expr, ExprKind, FnDecl, Import, LitPattern, Param, Pattern, Span,
    Stmt, StmtKind, Type, UnaryOp,
};
use crate::resolver::{ModuleGraph, ModuleId, ResolvedImport};
use std::collections::HashMap;
use std::fmt;

pub use ty::Ty;
use ty::compatible;

/// What a `match` scrutinee is being matched against, threaded through the match-checking helpers.
enum MatchKind {
    /// Enum/Result/Option scrutinee — arms are variant patterns.
    Variants { label: String, variants: HashMap<String, Vec<Ty>> },
    /// int/str/bool scrutinee — arms are literal patterns (+ a required `_` wildcard).
    Literal(Ty),
    /// Un-inferable (`Ty::Unknown`) scrutinee — skip exhaustiveness, accept any pattern shape.
    Skip,
}

/// A type error, with the source span it occurred at. Mirrors `ParseError` / `RuntimeError`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "type error ({}): {}", self.span, self.message)
    }
}

/// The dotted path an import targets, for error messages (`core.db`).
fn module_label(import: &Import) -> String {
    let path = match import {
        Import::Module { path, .. } | Import::From { path, .. } => path,
    };
    path.join(".")
}

/// `Result`/`Option` are builtin generic types — the `?` operator and the top-level-error logic in
/// both engines key on these literal names, so a user type that shadows them would collide. Reject
/// the redefinition at declaration.
fn is_reserved_type(name: &str) -> bool {
    name == "Result" || name == "Option"
}

/// A function (or method) signature: parameter types and return type.
#[derive(Clone)]
struct FnSig {
    params: Vec<Ty>,
    ret: Ty,
}

/// A struct's shape: ordered `(field, type)` pairs and its methods by name.
struct StructInfo {
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
}

/// A user enum variant: which enum it belongs to and its payload field types.
#[derive(Clone)]
struct VariantInfo {
    enum_name: String,
    payload: Vec<Ty>,
}

/// A module's public surface, computed when it's checked, consumed by its importers: its top-level
/// functions, top-level values (`:=` / typed lets), and the type names it declares.
#[derive(Clone, Default)]
struct ModuleSig {
    functions: HashMap<String, FnSig>,
    values: HashMap<String, Ty>,
    types: std::collections::HashSet<String>,
}

/// Type-check a single parsed module (no imports). Retained as the unit-test entry point; the CLI
/// drives [`check_graph`] so single- and multi-file programs share one path.
#[cfg(test)]
pub fn check(module: &crate::ast::Module) -> Result<(), Vec<CheckError>> {
    let mut c = Checker::new();
    c.check_module(&module.stmts, None, &[]);
    if c.errors.is_empty() {
        Ok(())
    } else {
        Err(c.errors)
    }
}

/// Entry point for a multi-file program: type-check every module in the graph (dependencies
/// before dependents), accumulating all errors across all modules (Go-style). Type names are
/// program-global in M4.5, so a name reused across modules is a collision.
pub fn check_graph(graph: &ModuleGraph) -> Result<(), Vec<CheckError>> {
    let mut c = Checker::new();
    for lm in &graph.modules {
        // A native std module (std.math/io/os) has no AST: its public surface is a static table.
        if let Some(name) = lm.native {
            c.module_sigs.insert(lm.id.clone(), native_module_sig(name));
            continue;
        }
        let label = if lm.id == graph.entry { None } else { Some(lm.label()) };
        c.begin_module(label);
        let sig = c.check_module(&lm.ast.stmts, Some(&lm.id), &lm.imports);
        c.module_sigs.insert(lm.id.clone(), sig);
    }
    if c.errors.is_empty() {
        Ok(())
    } else {
        Err(c.errors)
    }
}

/// The static type signatures of a native std module's members (M6c). This is the **third**
/// lockstep table: it must agree with the runtime members in `src/native/<module>.rs` and the
/// per-engine value lowering. `std.math` params are `float` (the language has no implicit int→float,
/// so callers pass floats); `pi`/`e` are float constants.
fn native_module_sig(name: &str) -> ModuleSig {
    let mut sig = ModuleSig::default();
    let mut func = |n: &str, params: Vec<Ty>, ret: Ty| {
        sig.functions.insert(n.to_string(), FnSig { params, ret });
    };
    match name {
        "std.math" => {
            func("abs", vec![Ty::Float], Ty::Float);
            func("min", vec![Ty::Float, Ty::Float], Ty::Float);
            func("max", vec![Ty::Float, Ty::Float], Ty::Float);
            func("floor", vec![Ty::Float], Ty::Float);
            func("ceil", vec![Ty::Float], Ty::Float);
            func("round", vec![Ty::Float], Ty::Float);
            func("pow", vec![Ty::Float, Ty::Float], Ty::Float);
            func("sqrt", vec![Ty::Float], Ty::Float);
            sig.values.insert("pi".into(), Ty::Float);
            sig.values.insert("e".into(), Ty::Float);
        }
        "std.io" => {
            func("print", vec![Ty::Str], Ty::Nil);
            func("eprint", vec![Ty::Str], Ty::Nil);
            func("read_line", vec![], Ty::option(Ty::Str));
            func("read_file", vec![Ty::Str], Ty::result(Ty::Str));
            func("write_file", vec![Ty::Str, Ty::Str], Ty::result(Ty::Nil));
        }
        "std.os" => {
            func("args", vec![], Ty::list(Ty::Str));
            func("env", vec![Ty::Str], Ty::option(Ty::Str));
            func("getcwd", vec![], Ty::result(Ty::Str));
        }
        _ => {}
    }
    sig
}

struct Checker {
    errors: Vec<CheckError>,
    scopes: Vec<HashMap<String, Ty>>,
    functions: HashMap<String, FnSig>,
    structs: HashMap<String, StructInfo>,
    /// enum name → its variant names, in declaration order (for exhaustiveness).
    enums: HashMap<String, Vec<String>>,
    variants: HashMap<String, VariantInfo>,
    struct_names: std::collections::HashSet<String>,
    enum_names: std::collections::HashSet<String>,
    /// Declared return type of the function body currently being checked (`Nil` at top level).
    current_ret: Ty,
    /// True while pass-1 is inferring a function's return type: `check_return` records each
    /// return's type into `collected_rets` instead of diagnosing against `current_ret`.
    inferring_ret: bool,
    /// Return types gathered from the body during return-type inference (see `infer_fn_ret`).
    collected_rets: Vec<Ty>,
    /// Public surfaces of already-checked modules (multi-file programs), keyed by module id.
    module_sigs: HashMap<ModuleId, ModuleSig>,
    /// Names bound to an imported module in the *current* module → which module they refer to.
    imported_modules: HashMap<String, ModuleId>,
    /// Label of the module currently being checked (`None` = entry); prefixes its error messages.
    current_module_label: Option<String>,
    /// How many enclosing `for`/`while` loops we're inside *within the current function body*.
    /// Reset to 0 when descending into a (nested) function or closure body so an inner `break`
    /// can't escape into an outer loop. `> 0` ⇒ `break`/`continue` are legal here.
    loop_depth: usize,
}

impl Checker {
    fn new() -> Self {
        Checker {
            errors: Vec::new(),
            scopes: Vec::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variants: HashMap::new(),
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            current_ret: Ty::Nil,
            inferring_ret: false,
            collected_rets: Vec::new(),
            module_sigs: HashMap::new(),
            imported_modules: HashMap::new(),
            current_module_label: None,
            loop_depth: 0,
        }
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        let message = match &self.current_module_label {
            Some(label) => format!("in module '{label}': {}", message.into()),
            None => message.into(),
        };
        self.errors.push(CheckError { message, span });
    }

    /// Reset per-module state (functions, scopes, imports, current fn) before checking the next
    /// module of a multi-file program. Program-global tables (structs/enums/variants/their names,
    /// `module_sigs`) and accumulated `errors` are kept.
    fn begin_module(&mut self, label: Option<String>) {
        self.scopes.clear();
        self.functions.clear();
        self.imported_modules.clear();
        self.current_ret = Ty::Nil;
        self.inferring_ret = false;
        self.collected_rets.clear();
        self.current_module_label = label;
    }

    /// Check one module's statements with its imports bound first; returns its public signature.
    /// `id` is `Some` for a graph module (enables import binding), `None` for a lone `check`.
    fn check_module(
        &mut self,
        stmts: &[Stmt],
        _id: Option<&ModuleId>,
        imports: &[ResolvedImport],
    ) -> ModuleSig {
        self.push_scope();
        for imp in imports {
            self.bind_import(imp);
        }
        self.collect_names(stmts);
        self.hoist(stmts);
        self.infer_returns(stmts);
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        let sig = self.capture_sig(stmts);
        self.pop_scope();
        sig
    }

    /// Bind an import into the current module: a whole-module import becomes a `Ty::Module` name;
    /// a `from` import injects each member (function/value) into scope, validating it exists.
    fn bind_import(&mut self, imp: &ResolvedImport) {
        match &imp.import {
            Import::Module { path, alias } => {
                let name = alias.clone().unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.imported_modules.insert(name.clone(), imp.target.clone());
                self.declare(&name, Ty::Module(name.clone()));
            }
            Import::From { path: _, names } => {
                let sig = self.module_sigs.get(&imp.target).cloned().unwrap_or_default();
                for (member, alias) in names {
                    let bind = alias.as_ref().unwrap_or(member);
                    if let Some(fsig) = sig.functions.get(member) {
                        self.functions.insert(bind.clone(), fsig.clone());
                    } else if let Some(vty) = sig.values.get(member) {
                        self.declare(bind, vty.clone());
                    } else if sig.types.contains(member) {
                        // A type name is program-global already; nothing to inject.
                    } else {
                        self.error(
                            imp.span,
                            format!("module '{}' has no member '{member}'", module_label(&imp.import)),
                        );
                    }
                }
            }
        }
    }

    /// Capture this module's public surface (own top-level fns/values/types) after checking.
    fn capture_sig(&self, stmts: &[Stmt]) -> ModuleSig {
        let mut sig = ModuleSig::default();
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    if let Some(fsig) = self.functions.get(&decl.name) {
                        sig.functions.insert(decl.name.clone(), fsig.clone());
                    }
                }
                StmtKind::Let { names, .. } => {
                    for name in names {
                        if let Some(ty) = self.lookup(name) {
                            sig.values.insert(name.clone(), ty);
                        }
                    }
                }
                StmtKind::Struct { name, .. } | StmtKind::Enum { name, .. } => {
                    sig.types.insert(name.clone());
                }
                _ => {}
            }
        }
        sig
    }

    // ===== scopes =====

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    fn declare(&mut self, name: &str, ty: Ty) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
    }
    fn lookup(&self, name: &str) -> Option<Ty> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }

    // ===== pass 1: hoist declarations =====

    /// First sub-pass: learn every struct/enum *name* so `resolve_type` can recognize them even
    /// when used before their definition (or inside each other).
    fn collect_names(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Struct { name, .. } => {
                    self.struct_names.insert(name.clone());
                }
                StmtKind::Enum { name, .. } => {
                    self.enum_names.insert(name.clone());
                }
                _ => {}
            }
        }
    }

    /// Second sub-pass: resolve and register signatures, fields, and variants. Redeclarations
    /// (a name defined twice) are reported here — otherwise "last write wins" would silently
    /// mis-type or, for struct methods, panic in pass 2 on a key that no longer exists.
    fn hoist(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    if self.functions.contains_key(&decl.name) {
                        self.error(s.span, format!("function '{}' is already defined", decl.name));
                    }
                    let sig = self.fn_sig(decl, s.span);
                    self.functions.insert(decl.name.clone(), sig);
                }
                StmtKind::Struct { name, fields, methods } => {
                    if is_reserved_type(name) {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    if self.structs.contains_key(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    let fields: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| (f.name.clone(), self.resolve_type(&f.ty, s.span)))
                        .collect();
                    let methods = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.structs.insert(name.clone(), StructInfo { fields, methods });
                }
                StmtKind::Enum { name, variants } => {
                    if is_reserved_type(name) {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    if self.enums.contains_key(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    let mut names = Vec::new();
                    for v in variants {
                        // Variants are keyed globally by bare name; a name shared across two enums
                        // would otherwise collapse and mis-type. Reject the collision outright.
                        if self.variants.contains_key(&v.name) {
                            self.error(s.span, format!("variant '{}' is already defined", v.name));
                        }
                        names.push(v.name.clone());
                        let payload =
                            v.payload.iter().map(|t| self.resolve_type(t, s.span)).collect();
                        self.variants.insert(
                            v.name.clone(),
                            VariantInfo { enum_name: name.clone(), payload },
                        );
                    }
                    self.enums.insert(name.clone(), names);
                }
                _ => {}
            }
        }
    }

    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type.
    fn fn_sig(&mut self, decl: &FnDecl, span: Span) -> FnSig {
        let params = decl
            .params
            .iter()
            .map(|p| match &p.ty {
                Some(t) => self.resolve_type(t, span),
                None if p.name == "self" => Ty::Unknown, // bound in check_fn_body
                None => {
                    self.error(span, format!("parameter '{}' needs a type annotation", p.name));
                    Ty::Unknown
                }
            })
            .collect();
        // No `-> T`: leave the return as `Unknown` for now — `infer_returns` (run after `hoist`)
        // walks the body and replaces it with the inferred type. `Unknown` is the safe placeholder
        // any *other* function's inference sees in the meantime (forward refs degrade silently
        // rather than to a confidently-wrong `Nil`).
        let ret = decl.ret.as_ref().map(|t| self.resolve_type(t, span)).unwrap_or(Ty::Unknown);
        FnSig { params, ret }
    }

    /// Pass-1.5: for every function/method that omitted `-> T`, infer its return type from the
    /// body and overwrite the provisional `Unknown` left by `fn_sig`. Runs after `hoist`, so all
    /// type names, variants, and (provisional) function sigs are already visible to the inference.
    fn infer_returns(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() => {
                    let Some(sig) = self.functions.get(&decl.name).cloned() else { continue };
                    let ret = self.infer_fn_ret(decl, None, &sig.params);
                    if let Some(sig) = self.functions.get_mut(&decl.name) {
                        sig.ret = ret;
                    }
                }
                StmtKind::Struct { name, methods, .. } => {
                    let self_ty = Ty::Struct(name.clone());
                    for m in methods {
                        if m.ret.is_some() {
                            continue;
                        }
                        let Some(sig) =
                            self.structs.get(name).and_then(|s| s.methods.get(&m.name)).cloned()
                        else {
                            continue;
                        };
                        let ret = self.infer_fn_ret(m, Some(self_ty.clone()), &sig.params);
                        if let Some(ms) =
                            self.structs.get_mut(name).and_then(|s| s.methods.get_mut(&m.name))
                        {
                            ms.ret = ret;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Infer one function's return type by walking its body in inference mode: every `return`'s
    /// type is collected by `check_return` (with errors suppressed — pass 2 re-reports for real).
    /// The pick rule, in order:
    /// - first concrete non-`nil` return wins (pass 2 then validates the rest against it);
    /// - else, if any value-return was uncertain (`Unknown` — a forward ref to a not-yet-inferred
    ///   function, or a self-recursive call) → `Unknown`, so the function stays permissive instead
    ///   of producing spurious errors (forward refs degrade *silently*, as the design promises);
    /// - else (only bare `return`s / no returns at all) → `nil` (void preserved).
    ///
    /// Inference is single-pass in source order with no fixpoint: a call to a *later* un-annotated
    /// function infers `Unknown` (permissive) rather than its precise type — define callees first,
    /// or annotate, for a precise inferred type.
    fn infer_fn_ret(&mut self, decl: &FnDecl, self_ty: Option<Ty>, params: &[Ty]) -> Ty {
        let mark = self.errors.len();
        let saved_ret = std::mem::replace(&mut self.current_ret, Ty::Unknown);
        let saved_flag = std::mem::replace(&mut self.inferring_ret, true);
        let saved_rets = std::mem::take(&mut self.collected_rets);
        self.push_scope();
        for (i, param) in decl.params.iter().enumerate() {
            let ty = if param.name == "self" {
                self_ty.clone().unwrap_or(Ty::Unknown)
            } else {
                params.get(i).cloned().unwrap_or(Ty::Unknown)
            };
            self.declare(&param.name, ty);
        }
        for stmt in &decl.body {
            self.check_stmt(stmt);
        }
        self.pop_scope();
        let found = std::mem::replace(&mut self.collected_rets, saved_rets);
        self.inferring_ret = saved_flag;
        self.current_ret = saved_ret;
        self.errors.truncate(mark); // discard inference-time errors; pass 2 re-reports them for real
        if let Some(t) = found.iter().find(|t| !t.is_unknown() && **t != Ty::Nil) {
            t.clone()
        } else if found.iter().any(|t| t.is_unknown()) {
            // A value-return we couldn't pin (forward ref / recursion): stay permissive, not `nil`.
            Ty::Unknown
        } else {
            Ty::Nil
        }
    }

    /// Resolve an AST `Type` annotation into a checker `Ty`, reporting unknown type names.
    fn resolve_type(&mut self, t: &Type, span: Span) -> Ty {
        match t {
            Type::Named(n) => match n.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "nil" => Ty::Nil,
                _ if self.struct_names.contains(n) => Ty::Struct(n.clone()),
                _ if self.enum_names.contains(n) => Ty::Enum(n.clone()),
                _ => {
                    self.error(span, format!("unknown type '{n}'"));
                    Ty::Unknown
                }
            },
            Type::Func { params, ret } => Ty::Func {
                params: params.iter().map(|p| self.resolve_type(p, span)).collect(),
                ret: Box::new(self.resolve_type(ret, span)),
            },
            Type::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve_type(t, span)).collect()),
            Type::Generic(n, args) => match (n.as_str(), args.as_slice()) {
                ("list", [inner]) => Ty::list(self.resolve_type(inner, span)),
                ("Result", [inner]) => Ty::result(self.resolve_type(inner, span)),
                ("Option", [inner]) => Ty::option(self.resolve_type(inner, span)),
                ("map", [k, v]) => {
                    let key = self.resolve_type(k, span);
                    let value = self.resolve_type(v, span);
                    if !is_hashable_key(&key) {
                        self.error(
                            span,
                            format!("map key type must be a hashable scalar (int, str, or bool), found {key}"),
                        );
                    }
                    Ty::map(key, value)
                }
                _ => {
                    self.error(span, format!("unknown generic type '{n}'"));
                    Ty::Unknown
                }
            },
        }
    }

    // ===== pass 2: check statements =====

    fn check_block(&mut self, block: &Block) {
        self.push_scope();
        for stmt in block {
            self.check_stmt(stmt);
        }
        self.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        let span = stmt.span;
        match &stmt.kind {
            StmtKind::Let { names, ty, value } => {
                let val_ty = self.infer(value);
                if names.len() > 1 {
                    // destructuring let `a, b := expr` — `expr` must be a tuple of matching arity.
                    self.check_destructure(names, &val_ty, value.span);
                    return;
                }
                let name = &names[0];
                let declared = match ty {
                    Some(t) => {
                        let expected = self.resolve_type(t, span);
                        if !compatible(&expected, &val_ty) {
                            self.error(
                                value.span,
                                format!("cannot assign {val_ty} to variable of type {expected}"),
                            );
                        }
                        expected
                    }
                    None => val_ty,
                };
                self.declare(name, declared);
            }
            StmtKind::Assign { target, op, value } => {
                let val_ty = self.infer(value);
                self.check_assign(target, *op, val_ty, span);
            }
            StmtKind::Fn(decl) => {
                // `.get` (not index) is panic-safe even when a redeclaration left a different sig.
                if let Some(sig) = self.functions.get(&decl.name).cloned() {
                    self.check_fn_body(decl, None, sig);
                }
            }
            StmtKind::Struct { name, methods, .. } => {
                let self_ty = Ty::Struct(name.clone());
                for m in methods {
                    // Panic-safe: a redeclared struct name means `structs[name]` is a *different*
                    // struct whose method table may not contain `m.name`.
                    if let Some(sig) =
                        self.structs.get(name).and_then(|s| s.methods.get(&m.name)).cloned()
                    {
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
            }
            StmtKind::Enum { .. } | StmtKind::Import(_) => {} // nothing to check in pass 2
            StmtKind::If { branches, else_block } => {
                for (cond, body) in branches {
                    self.expect_bool(cond, "if condition");
                    self.check_block(body);
                }
                if let Some(body) = else_block {
                    self.check_block(body);
                }
            }
            StmtKind::For { var, iter, body } => {
                let elem = self.iter_elem(iter);
                self.push_scope();
                self.declare(var, elem);
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
            StmtKind::Expr(e) => {
                self.infer(e);
            }
        }
    }

    /// Check a destructuring let `a, b, … := value`. The value's type must be a tuple whose arity
    /// matches the binding count; each name is then declared with its element type. An `Unknown`
    /// value (an already-reported error) declares all names `Unknown` so no cascade follows.
    fn check_destructure(&mut self, names: &[String], val_ty: &Ty, span: Span) {
        match val_ty {
            Ty::Unknown => {
                for name in names {
                    self.declare(name, Ty::Unknown);
                }
            }
            Ty::Tuple(elems) if elems.len() == names.len() => {
                for (name, ty) in names.iter().zip(elems) {
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
                self.error(span, format!("cannot destructure non-tuple value of type {other}"));
                for name in names {
                    self.declare(name, Ty::Unknown);
                }
            }
        }
    }

    fn check_assign(&mut self, target: &Expr, op: AssignOp, val_ty: Ty, span: Span) {
        match &target.kind {
            ExprKind::Ident(name) => {
                let Some(var_ty) = self.lookup(name) else {
                    self.error(span, format!("cannot assign to undeclared variable '{name}'"));
                    return;
                };
                self.check_assign_value(&var_ty, op, &val_ty, target.span);
            }
            // `xs[i] = v` — only lists are mutable by index. Strings are immutable; other types
            // aren't indexable. (`infer_index` would green-light a str index — handle it here.)
            ExprKind::Index { obj, index } => {
                match self.infer(obj) {
                    Ty::Map(k, v) => {
                        let idx_ty = self.infer(index);
                        if !compatible(&k, &idx_ty) {
                            self.error(index.span, format!("map key must be {k}, found {idx_ty}"));
                        }
                        self.check_assign_value(&v, op, &val_ty, target.span);
                    }
                    Ty::List(elem) => {
                        self.expect_int(index, "index");
                        self.check_assign_value(&elem, op, &val_ty, target.span);
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
                    other => {
                        self.expect_int(index, "index");
                        self.error(target.span, format!("cannot index-assign into {other}"));
                    }
                }
            }
            // `p.x = v` — only data fields of a struct are assignable (not methods, not module
            // members). `infer_field` would accept those, so check the field kind here.
            ExprKind::Field { obj, name } => {
                let obj_ty = self.infer(obj);
                match &obj_ty {
                    Ty::Struct(sname) => {
                        let field_ty = self
                            .structs
                            .get(sname)
                            .and_then(|info| info.fields.iter().find(|(f, _)| f == name))
                            .map(|(_, ty)| ty.clone());
                        match field_ty {
                            Some(ty) => self.check_assign_value(&ty, op, &val_ty, target.span),
                            None => self.error(
                                target.span,
                                format!("cannot assign to '{name}': type {obj_ty} has no field '{name}'"),
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
            _ => self.error(target.span, "invalid assignment target (only variables can be assigned)"),
        }
    }

    fn check_assign_value(&mut self, target_ty: &Ty, op: AssignOp, val_ty: &Ty, span: Span) {
        match op {
            AssignOp::Eq => {
                if !compatible(target_ty, val_ty) {
                    self.error(span, format!("cannot assign {val_ty} to {target_ty}"));
                }
            }
            AssignOp::PlusEq | AssignOp::MinusEq => {
                // `+=` mirrors `+` (numeric, or str+str for `+=`); `-=` is numeric only.
                // No implicit widening: `int <op> float` yields a float, which can't flow back
                // into a concrete int slot — reject it (gap #9), mirroring strict `=` (`x = 1.5`).
                let str_ok = op == AssignOp::PlusEq && *target_ty == Ty::Str && *val_ty == Ty::Str;
                let widens = *target_ty == Ty::Int && *val_ty == Ty::Float;
                let num_ok = target_ty.is_numeric() && val_ty.is_numeric() && !widens;
                let known = !target_ty.is_unknown() && !val_ty.is_unknown();
                if known && !str_ok && !num_ok {
                    let sym = if op == AssignOp::PlusEq { "+=" } else { "-=" };
                    self.error(span, format!("cannot apply {sym} to {target_ty} and {val_ty}"));
                }
            }
        }
    }

    fn check_return(&mut self, value: Option<&Expr>, span: Span) {
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
        let ret = self.current_ret.clone();
        match value {
            Some(e) => {
                let ty = self.infer(e);
                if ret == Ty::Nil {
                    self.error(e.span, "function returns nothing, cannot return a value");
                } else if !compatible(&ret, &ty) {
                    self.error(e.span, format!("expected return type {ret}, found {ty}"));
                }
            }
            None => {
                if ret != Ty::Nil {
                    self.error(span, format!("expected a return value of type {ret}"));
                }
            }
        }
    }

    fn check_fn_body(&mut self, decl: &FnDecl, self_ty: Option<Ty>, sig: FnSig) {
        let saved_ret = std::mem::replace(&mut self.current_ret, sig.ret.clone());
        // A nested function checked while pass-1 is inferring an *outer* function's return must not
        // feed the outer `collected_rets` — this body's `return`s are diagnosed, not collected.
        let saved_inferring = std::mem::replace(&mut self.inferring_ret, false);
        // A function body opens a fresh loop context: a loop enclosing this fn's *definition* must
        // not make a `break`/`continue` in the body legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        self.push_scope();
        for (i, param) in decl.params.iter().enumerate() {
            let ty = if param.name == "self" {
                self_ty.clone().unwrap_or(Ty::Unknown)
            } else {
                sig.params.get(i).cloned().unwrap_or(Ty::Unknown)
            };
            self.declare(&param.name, ty);
        }
        for stmt in &decl.body {
            self.check_stmt(stmt);
        }
        self.pop_scope();
        self.current_ret = saved_ret;
        self.inferring_ret = saved_inferring;
        self.loop_depth = saved_loop_depth;
    }

    /// The element type produced by iterating `iter` in a `for` loop.
    fn iter_elem(&mut self, iter: &Expr) -> Ty {
        if let ExprKind::Range { start, end } = &iter.kind {
            self.expect_int(start, "range bound");
            self.expect_int(end, "range bound");
            return Ty::Int;
        }
        match self.infer(iter) {
            Ty::List(inner) => *inner,
            Ty::Str => Ty::Str,
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(iter.span, format!("cannot iterate over {other}"));
                Ty::Unknown
            }
        }
    }

    /// How a `match` is being checked, derived from the scrutinee's type.
    fn match_kind(&mut self, scrutinee: &Expr) -> MatchKind {
        let sty = self.infer(scrutinee);
        match &sty {
            Ty::Enum(name) => {
                let variants = self
                    .enums
                    .get(name)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| (v.clone(), self.variants[&v].payload.clone()))
                    .collect();
                MatchKind::Variants { label: name.clone(), variants }
            }
            Ty::Result(inner) => MatchKind::Variants {
                label: "Result".into(),
                variants: HashMap::from([
                    ("Ok".into(), vec![(**inner).clone()]),
                    ("Err".into(), vec![Ty::Unknown]),
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
            Ty::Unknown => MatchKind::Skip, // un-inferable: skip exhaustiveness
            other => {
                self.error(scrutinee.span, format!("cannot match on non-enum type {other}"));
                MatchKind::Skip
            }
        }
    }

    /// Push a scope and bind one arm's pattern payload, recording coverage + diagnostics. Returns
    /// `true` if this arm was a `_` wildcard. The caller must `pop_scope` after the arm body.
    fn bind_match_arm(
        &mut self,
        pattern: &Pattern,
        kind: &MatchKind,
        span: Span,
        covered: &mut std::collections::HashSet<String>,
    ) -> bool {
        // A wildcard binds nothing and is valid in every mode.
        if let Pattern::Wildcard = pattern {
            self.push_scope();
            return true;
        }
        match kind {
            MatchKind::Skip => {
                // Un-inferable scrutinee: accept the pattern shape permissively (it may be a
                // variant or a literal). Still scope so the caller can `pop_scope` uniformly.
                if let Pattern::Variant { name, bindings } = pattern {
                    covered.insert(name.clone());
                    self.push_scope();
                    for b in bindings {
                        self.declare(b, Ty::Unknown);
                    }
                } else {
                    self.push_scope();
                }
            }
            MatchKind::Variants { label, variants } => match pattern {
                Pattern::Variant { name, bindings } => {
                    let payload = match variants.get(name) {
                        Some(p) => Some(p.clone()),
                        None => {
                            self.error(span, format!("'{name}' is not a variant of {label}"));
                            None
                        }
                    };
                    if !covered.insert(name.clone()) {
                        self.error(span, format!("duplicate match arm '{name}'"));
                    }
                    self.push_scope();
                    if let Some(payload) = &payload {
                        if payload.len() != bindings.len() {
                            self.error(
                                span,
                                format!(
                                    "variant '{name}' binds {} value(s), but {} given",
                                    payload.len(),
                                    bindings.len()
                                ),
                            );
                        }
                        for (b, t) in bindings.iter().zip(payload.iter()) {
                            self.declare(b, t.clone());
                        }
                    } else {
                        for b in bindings {
                            self.declare(b, Ty::Unknown);
                        }
                    }
                }
                Pattern::Literal(_) => {
                    self.error(span, format!("cannot match a literal against {label}"));
                    self.push_scope();
                }
                Pattern::Wildcard => unreachable!("wildcard handled above"),
            },
            MatchKind::Literal(ty) => {
                match pattern {
                    Pattern::Literal(lit) => {
                        let lit_ty = match lit {
                            LitPattern::Int(_) => Ty::Int,
                            LitPattern::Str(_) => Ty::Str,
                            LitPattern::Bool(_) => Ty::Bool,
                        };
                        if &lit_ty != ty {
                            self.error(
                                span,
                                format!("literal of type {lit_ty} cannot match scrutinee of type {ty}"),
                            );
                        }
                    }
                    Pattern::Variant { .. } => {
                        self.error(span, format!("cannot match a variant against {ty}"));
                    }
                    Pattern::Wildcard => unreachable!("wildcard handled above"),
                }
                self.push_scope();
            }
        }
        false
    }

    /// Report a non-exhaustive match.
    /// - Variants mode: missing variants, unless a `_` wildcard was seen.
    /// - Literal mode: int/str/bool literal domains are open, so a `_` wildcard is *required*
    ///   (we do NOT special-case `true`+`false` closing the bool domain — keeping one rule).
    /// - Skip mode: un-inferable scrutinee, no exhaustiveness check.
    fn check_exhaustive(
        &mut self,
        kind: &MatchKind,
        covered: &std::collections::HashSet<String>,
        has_wildcard: bool,
        span: Span,
    ) {
        if has_wildcard {
            return;
        }
        match kind {
            MatchKind::Skip => {}
            MatchKind::Variants { label, variants } => {
                let mut missing: Vec<String> =
                    variants.keys().filter(|v| !covered.contains(*v)).cloned().collect();
                if !missing.is_empty() {
                    missing.sort();
                    self.error(
                        span,
                        format!("non-exhaustive match on {label}: missing {}", missing.join(", ")),
                    );
                }
            }
            MatchKind::Literal(_) => {
                self.error(span, "non-exhaustive match: add a `_` arm".to_string());
            }
        }
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchArm]) {
        let kind = self.match_kind(scrutinee);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        for arm in arms {
            has_wildcard |=
                self.bind_match_arm(&arm.pattern, &kind, scrutinee.span, &mut covered);
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        self.check_exhaustive(&kind, &covered, has_wildcard, scrutinee.span);
    }

    /// Infer an expression-position `match`: bind each arm, infer its value, and unify the arm
    /// types into one result. Exhaustiveness is still enforced.
    fn infer_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchExprArm]) -> Ty {
        let kind = self.match_kind(scrutinee);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut result: Option<Ty> = None;
        for arm in arms {
            has_wildcard |=
                self.bind_match_arm(&arm.pattern, &kind, scrutinee.span, &mut covered);
            let t = self.infer(&arm.body);
            self.pop_scope();
            result = Some(self.unify_branch(result, t, arm.body.span));
        }
        self.check_exhaustive(&kind, &covered, has_wildcard, scrutinee.span);
        result.unwrap_or(Ty::Unknown)
    }

    /// Infer an expression-position `if c: a else: b`: condition is bool, the two branches unify.
    fn infer_if_else(&mut self, cond: &Expr, then: &Expr, els: &Expr) -> Ty {
        self.expect_bool(cond, "if condition");
        let t_then = self.infer(then);
        let t_els = self.infer(els);
        let acc = self.unify_branch(None, t_then, then.span);
        self.unify_branch(Some(acc), t_els, els.span)
    }

    /// Fold one branch's type into a match/if expression's running result type. The first concrete
    /// branch sets the type; a later incompatible branch is a real error (and yields `Unknown` to
    /// suppress cascades). `Unknown` branches never override a concrete result.
    fn unify_branch(&mut self, acc: Option<Ty>, t: Ty, span: Span) -> Ty {
        match acc {
            None => t,
            Some(prev) => {
                if compatible(&prev, &t) {
                    if prev.is_unknown() {
                        t
                    } else {
                        prev
                    }
                } else {
                    self.error(span, format!("branches have incompatible types: {prev} and {t}"));
                    Ty::Unknown
                }
            }
        }
    }

    // ===== expression inference =====

    fn infer(&mut self, expr: &Expr) -> Ty {
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(_) => Ty::Str, // opaque; interpolation contents are not checked (M2 defer)
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Ident(name) => self.infer_ident(name, expr.span),
            ExprKind::List(items) => self.infer_list(items),
            ExprKind::Tuple(items) => Ty::Tuple(items.iter().map(|e| self.infer(e)).collect()),
            ExprKind::Map(entries) => self.infer_map(entries),
            ExprKind::Unary { op, expr: inner } => self.infer_unary(*op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs),
            ExprKind::Range { start, end } => {
                self.expect_int(start, "range bound");
                self.expect_int(end, "range bound");
                Ty::list(Ty::Int)
            }
            ExprKind::Call { callee, args } => self.infer_call(callee, args, expr.span),
            ExprKind::Field { obj, name } => self.infer_field(obj, name),
            ExprKind::Index { obj, index } => self.infer_index(obj, index),
            ExprKind::Try(inner) => self.infer_try(inner, expr.span),
            ExprKind::Closure { params, ret, body } => self.infer_closure(params, ret.as_ref(), body),
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms),
            ExprKind::IfElse { cond, then, els } => self.infer_if_else(cond, then, els),
        }
    }

    fn infer_ident(&mut self, name: &str, span: Span) -> Ty {
        if let Some(ty) = self.lookup(name) {
            return ty;
        }
        if let Some(sig) = self.functions.get(name) {
            return Ty::Func { params: sig.params.clone(), ret: Box::new(sig.ret.clone()) };
        }
        if name == "None" {
            return Ty::option(Ty::Unknown);
        }
        // A nullary user variant used as a value (e.g. `Red`).
        if let Some(v) = self.variants.get(name)
            && v.payload.is_empty()
        {
            return Ty::Enum(v.enum_name.clone());
        }
        self.error(span, format!("unknown name '{name}'"));
        Ty::Unknown
    }

    fn infer_list(&mut self, items: &[Expr]) -> Ty {
        let mut elem = Ty::Unknown;
        for item in items {
            let t = self.infer(item);
            if elem.is_unknown() {
                elem = t;
            } else if !t.is_unknown() && !compatible(&elem, &t) {
                self.error(item.span, format!("list elements differ: {elem} vs {t}"));
            }
        }
        Ty::list(elem)
    }

    /// Infer the type of a map literal `{k: v, …}`. Keys must share one (hashable) type, values
    /// another; heterogeneity and non-hashable keys are errors. Empty `{}` → `map[?, ?]`.
    fn infer_map(&mut self, entries: &[(Expr, Expr)]) -> Ty {
        let mut key = Ty::Unknown;
        let mut value = Ty::Unknown;
        for (k_expr, v_expr) in entries {
            let kt = self.infer(k_expr);
            let vt = self.infer(v_expr);
            if !kt.is_unknown() && !is_hashable_key(&kt) {
                self.error(
                    k_expr.span,
                    format!("map key type must be a hashable scalar (int, str, or bool), found {kt}"),
                );
            }
            if key.is_unknown() {
                key = kt;
            } else if !kt.is_unknown() && !compatible(&key, &kt) {
                self.error(k_expr.span, format!("map keys differ: {key} vs {kt}"));
            }
            if value.is_unknown() {
                value = vt;
            } else if !vt.is_unknown() && !compatible(&value, &vt) {
                self.error(v_expr.span, format!("map values differ: {value} vs {vt}"));
            }
        }
        Ty::map(key, value)
    }

    fn infer_unary(&mut self, op: UnaryOp, inner: &Expr) -> Ty {
        let t = self.infer(inner);
        match op {
            UnaryOp::Neg => {
                if !t.is_numeric() && !t.is_unknown() {
                    self.error(inner.span, format!("cannot negate {t}"));
                    return Ty::Unknown;
                }
                t
            }
            UnaryOp::Not => {
                if t != Ty::Bool && !t.is_unknown() {
                    self.error(inner.span, format!("'not' expects bool, found {t}"));
                }
                Ty::Bool
            }
        }
    }

    fn infer_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Ty {
        use BinaryOp::*;
        let l = self.infer(lhs);
        let r = self.infer(rhs);
        let either_unknown = l.is_unknown() || r.is_unknown();
        match op {
            And | Or => {
                if l != Ty::Bool && !l.is_unknown() {
                    self.error(lhs.span, format!("logical operator expects bool, found {l}"));
                }
                if r != Ty::Bool && !r.is_unknown() {
                    self.error(rhs.span, format!("logical operator expects bool, found {r}"));
                }
                Ty::Bool
            }
            Add => {
                if l == Ty::Str && r == Ty::Str {
                    Ty::Str
                } else if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply + to {l} and {r}"));
                    Ty::Unknown
                }
            }
            Sub | Mul | Div | Mod => {
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply {} to {l} and {r}", op_sym(op)));
                    Ty::Unknown
                }
            }
            Lt | LtEq | Gt | GtEq => {
                let ok = (l.is_numeric() && r.is_numeric()) || (l == Ty::Str && r == Ty::Str);
                if !ok && !either_unknown {
                    self.error(lhs.span, format!("cannot compare {l} and {r}"));
                }
                Ty::Bool
            }
            Eq | NotEq => Ty::Bool, // equality is permissive (matches the interpreter)
        }
    }

    fn infer_field(&mut self, obj: &Expr, name: &str) -> Ty {
        let obj_ty = self.infer(obj);
        match &obj_ty {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index as a
            // decimal string; out-of-range or non-numeric is an error.
            Ty::Tuple(elems) => match name.parse::<usize>() {
                Ok(i) if i < elems.len() => elems[i].clone(),
                _ => {
                    self.error(obj.span, format!("tuple {obj_ty} has no element '.{name}'"));
                    Ty::Unknown
                }
            },
            Ty::Struct(sname) => {
                if let Some(info) = self.structs.get(sname) {
                    if let Some((_, ty)) = info.fields.iter().find(|(f, _)| f == name) {
                        return ty.clone();
                    }
                    if info.methods.contains_key(name) {
                        let sig = &info.methods[name];
                        return Ty::Func {
                            params: sig.params.clone(),
                            ret: Box::new(sig.ret.clone()),
                        };
                    }
                }
                self.error(obj.span, format!("type {obj_ty} has no field '{name}'"));
                Ty::Unknown
            }
            Ty::Module(mname) => {
                let member = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id))
                    .map(|sig| {
                        if let Some(fsig) = sig.functions.get(name) {
                            Some(Ty::Func {
                                params: fsig.params.clone(),
                                ret: Box::new(fsig.ret.clone()),
                            })
                        } else {
                            sig.values.get(name).cloned()
                        }
                    });
                match member {
                    Some(Some(ty)) => ty,
                    _ => {
                        self.error(obj.span, format!("module '{mname}' has no member '{name}'"));
                        Ty::Unknown
                    }
                }
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(obj.span, format!("type {other} has no field '{name}'"));
                Ty::Unknown
            }
        }
    }

    fn infer_index(&mut self, obj: &Expr, index: &Expr) -> Ty {
        // Map keys are NOT int — infer the object first and check the index against the key type.
        match self.infer(obj) {
            Ty::Map(k, v) => {
                let idx_ty = self.infer(index);
                if !compatible(&k, &idx_ty) {
                    self.error(index.span, format!("map key must be {k}, found {idx_ty}"));
                }
                *v
            }
            Ty::List(inner) => {
                self.expect_int(index, "index");
                *inner
            }
            Ty::Str => {
                self.expect_int(index, "index");
                Ty::Str
            }
            Ty::Unknown => {
                self.expect_int(index, "index");
                Ty::Unknown
            }
            other => {
                self.expect_int(index, "index");
                self.error(obj.span, format!("cannot index into {other}"));
                Ty::Unknown
            }
        }
    }

    fn infer_try(&mut self, inner: &Expr, span: Span) -> Ty {
        let t = self.infer(inner);
        // The enclosing function must be able to early-return the Err/None. We allow Result/Option
        // (propagate) and Nil (top-level / `fn main()` — the interpreter unwinds it at the boundary).
        match &self.current_ret {
            Ty::Result(_) | Ty::Option(_) | Ty::Nil => {}
            other => self.error(
                span,
                format!("'?' used in a function that returns {other}, not Result or Option"),
            ),
        }
        match t {
            Ty::Result(inner) | Ty::Option(inner) => *inner,
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(span, format!("'?' expects Result or Option, found {other}"));
                Ty::Unknown
            }
        }
    }

    fn infer_closure(&mut self, params: &[Param], ret: Option<&Type>, body: &Expr) -> Ty {
        // A closure body opens a fresh loop context (same rule as `check_fn_body`): a loop around
        // the closure's definition must not make a `break`/`continue` inside it legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        self.push_scope();
        let param_tys: Vec<Ty> = params
            .iter()
            .map(|p| {
                let ty = p.ty.as_ref().map(|t| self.resolve_type(t, body.span)).unwrap_or(Ty::Unknown);
                self.declare(&p.name, ty.clone());
                ty
            })
            .collect();
        let body_ty = self.infer(body);
        self.pop_scope();
        self.loop_depth = saved_loop_depth;
        let ret_ty = match ret {
            Some(t) => {
                let declared = self.resolve_type(t, body.span);
                if !compatible(&declared, &body_ty) {
                    self.error(
                        body.span,
                        format!("closure body has type {body_ty}, but its return type is {declared}"),
                    );
                }
                declared
            }
            None => body_ty,
        };
        Ty::Func { params: param_tys, ret: Box::new(ret_ty) }
    }

    // ===== calls =====

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Ty {
        // Method call: `obj.method(args)`.
        if let ExprKind::Field { obj, name } = &callee.kind {
            return self.infer_method_call(obj, name, args, span);
        }
        if let ExprKind::Ident(name) = &callee.kind {
            // Shadowing local (e.g. a closure bound to a variable) wins over a global of the same name.
            if self.lookup(name).is_none()
                && let Some(ty) = self.infer_named_call(name, args, span)
            {
                return ty;
            }
        }
        // Fall back: the callee is an arbitrary expression; it must evaluate to a function.
        let callee_ty = self.infer(callee);
        match callee_ty {
            Ty::Func { params, ret } => {
                self.check_args("closure", &params, args, span);
                *ret
            }
            Ty::Unknown => {
                for a in args {
                    self.infer(a);
                }
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

    /// Resolve a by-name call (builtin / constructor / variant / global fn). Returns `None` if
    /// `name` is none of those, so the caller can treat it as a value-call.
    fn infer_named_call(&mut self, name: &str, args: &[Expr], span: Span) -> Option<Ty> {
        match name {
            "print" => {
                for a in args {
                    self.infer(a);
                }
                Some(Ty::Nil)
            }
            "len" => {
                self.check_arity("len", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer(a) {
                        Ty::List(_) | Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("len() expects a list or str, got {other}")),
                    }
                }
                Some(Ty::Int)
            }
            "range" => {
                for a in args {
                    self.expect_int_val(a);
                }
                if args.is_empty() || args.len() > 2 {
                    self.error(span, "range() expects range(end) or range(start, end)");
                }
                Some(Ty::list(Ty::Int))
            }
            "int" => {
                self.check_arity("int", 1, args, span);
                self.infer_all(args);
                Some(Ty::Int)
            }
            "float" => {
                self.check_arity("float", 1, args, span);
                self.infer_all(args);
                Some(Ty::Float)
            }
            "str" => {
                self.check_arity("str", 1, args, span);
                self.infer_all(args);
                Some(Ty::Str)
            }
            // Generic built-in constructors for Result / Option.
            "Ok" => Some(Ty::result(self.one_arg(name, args, span))),
            "Some" => Some(Ty::option(self.one_arg(name, args, span))),
            "Err" => {
                let _ = self.one_arg(name, args, span);
                Some(Ty::result(Ty::Unknown))
            }
            _ => {
                // Struct constructor?
                if let Some(info) = self.structs.get(name) {
                    let params: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
                    self.check_args(name, &params, args, span);
                    return Some(Ty::Struct(name.to_string()));
                }
                // User enum variant constructor?
                if let Some(v) = self.variants.get(name).cloned() {
                    self.check_args(name, &v.payload, args, span);
                    return Some(Ty::Enum(v.enum_name));
                }
                // Global function?
                if let Some(sig) = self.functions.get(name).cloned() {
                    self.check_args(name, &sig.params, args, span);
                    return Some(sig.ret);
                }
                None
            }
        }
    }

    fn infer_method_call(&mut self, obj: &Expr, method: &str, args: &[Expr], span: Span) -> Ty {
        let obj_ty = self.infer(obj);
        match &obj_ty {
            // `module.fn(args)` is a plain call on the member — no `self`.
            Ty::Module(mname) => {
                let fsig = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id))
                    .and_then(|sig| sig.functions.get(method).cloned());
                if let Some(fsig) = fsig {
                    self.check_args(method, &fsig.params, args, span);
                    return fsig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("module '{mname}' has no member '{method}'"));
                Ty::Unknown
            }
            Ty::Struct(sname) => {
                let sig = self.structs.get(sname).and_then(|i| i.methods.get(method).cloned());
                if let Some(sig) = sig {
                    // The first param is the receiver (bound implicitly from `obj`), so the call's
                    // explicit args correspond to params[1..]. A method with NO params has no
                    // receiver slot — both engines prepend the receiver and would error at runtime,
                    // so reject the call here instead.
                    match sig.params.split_first() {
                        Some((_receiver, expected)) => self.check_args(method, expected, args, span),
                        None => {
                            self.error(
                                span,
                                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
                            );
                            self.infer_all(args);
                        }
                    }
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            // Core-type methods (M6): built-in methods on `str` and `list[T]`.
            Ty::Str => {
                if let Some(sig) = str_method_sig(method) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type str has no method '{method}'"));
                Ty::Unknown
            }
            Ty::List(elem) => {
                // Higher-order methods whose result/param types depend on the closure's
                // `Ty::Func` can't be expressed by the fixed `list_method_sig` table, so handle
                // them here. `Ty::Unknown` arguments are tolerated permissively (no cascade).
                if matches!(method, "map" | "filter" | "fold" | "sort_by") {
                    let elem = (**elem).clone();
                    return self.infer_list_hof(method, &elem, args, span);
                }
                if let Some(sig) = list_method_sig(method, elem) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                if method == "sum" {
                    self.error(span, format!("sum() requires a numeric list, found list[{elem}]"));
                } else if method == "sort" {
                    self.error(span, format!("sort() requires a list of int, float, or str, found list[{elem}]"));
                } else {
                    self.error(span, format!("type {obj_ty} has no method '{method}'"));
                }
                Ty::Unknown
            }
            Ty::Map(k, v) => {
                if let Some(sig) = map_method_sig(method, k, v) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Unknown => {
                self.infer_all(args);
                Ty::Unknown
            }
            other => {
                self.infer_all(args);
                self.error(span, format!("type {other} has no method '{method}'"));
                Ty::Unknown
            }
        }
    }

    /// Type-check the higher-order list methods `map` / `filter` / `fold`, whose signatures
    /// depend on the closure argument's `Ty::Func` and so can't live in `list_method_sig`.
    /// `elem` is the list's element type. Returns the method's result type (`Ty::Unknown` on
    /// error so a single mismatch doesn't cascade).
    fn infer_list_hof(&mut self, method: &str, elem: &Ty, args: &[Expr], span: Span) -> Ty {
        match method {
            // map(f: fn(T) -> U) -> list[U]
            "map" => {
                if args.len() != 1 {
                    self.error(span, format!("'map' expects 1 argument(s), got {}", args.len()));
                    self.infer_all(args);
                    return Ty::Unknown;
                }
                let ft = self.infer(&args[0]);
                match ft {
                    Ty::Unknown => Ty::Unknown,
                    Ty::Func { params, ret } if params.len() == 1 && compatible(&params[0], elem) => {
                        Ty::list(*ret)
                    }
                    other => {
                        self.error(
                            args[0].span,
                            format!("map expects a function fn({elem}) -> U, found {other}"),
                        );
                        Ty::Unknown
                    }
                }
            }
            // filter(p: fn(T) -> bool) -> list[T]
            "filter" => {
                if args.len() != 1 {
                    self.error(span, format!("'filter' expects 1 argument(s), got {}", args.len()));
                    self.infer_all(args);
                    return Ty::Unknown;
                }
                let pt = self.infer(&args[0]);
                match pt {
                    Ty::Unknown => Ty::list(elem.clone()),
                    Ty::Func { params, ret }
                        if params.len() == 1
                            && compatible(&params[0], elem)
                            && compatible(&ret, &Ty::Bool) =>
                    {
                        Ty::list(elem.clone())
                    }
                    other => {
                        self.error(
                            args[0].span,
                            format!("filter expects a predicate fn({elem}) -> bool, found {other}"),
                        );
                        Ty::Unknown
                    }
                }
            }
            // fold(init: U, f: fn(U, T) -> U) -> U
            "fold" => {
                if args.len() != 2 {
                    self.error(span, format!("'fold' expects 2 argument(s), got {}", args.len()));
                    self.infer_all(args);
                    return Ty::Unknown;
                }
                let init = self.infer(&args[0]);
                let ft = self.infer(&args[1]);
                match ft {
                    Ty::Unknown => {
                        // Closure type unknown: fall back to the init type as the result.
                        init
                    }
                    Ty::Func { params, ret }
                        if params.len() == 2
                            && compatible(&params[0], &init)
                            && compatible(&params[1], elem)
                            && compatible(&ret, &init) =>
                    {
                        init
                    }
                    other => {
                        self.error(
                            args[1].span,
                            format!(
                                "fold expects a function fn({init}, {elem}) -> {init}, found {other}"
                            ),
                        );
                        Ty::Unknown
                    }
                }
            }
            // sort_by(cmp: fn(T, T) -> int) -> nil (sorts in place, like sort)
            "sort_by" => {
                if args.len() != 1 {
                    self.error(
                        span,
                        format!("'sort_by' expects 1 argument(s), got {}", args.len()),
                    );
                    self.infer_all(args);
                    return Ty::Nil;
                }
                let ft = self.infer(&args[0]);
                match ft {
                    Ty::Unknown => Ty::Nil,
                    Ty::Func { params, ret }
                        if params.len() == 2
                            && compatible(&params[0], elem)
                            && compatible(&params[1], elem)
                            && compatible(&ret, &Ty::Int) =>
                    {
                        Ty::Nil
                    }
                    other => {
                        self.error(
                            args[0].span,
                            format!(
                                "sort_by expects a comparator fn({elem}, {elem}) -> int, found {other}"
                            ),
                        );
                        Ty::Nil
                    }
                }
            }
            _ => unreachable!("infer_list_hof called with non-HOF method {method}"),
        }
    }

    // ===== small helpers =====

    fn one_arg(&mut self, name: &str, args: &[Expr], span: Span) -> Ty {
        self.check_arity(name, 1, args, span);
        args.first().map(|a| self.infer(a)).unwrap_or(Ty::Unknown)
    }

    fn infer_all(&mut self, args: &[Expr]) {
        for a in args {
            self.infer(a);
        }
    }

    /// Check argument count and each argument's type against a known parameter list.
    fn check_args(&mut self, name: &str, params: &[Ty], args: &[Expr], span: Span) {
        if params.len() != args.len() {
            self.error(
                span,
                format!("'{name}' expects {} argument(s), got {}", params.len(), args.len()),
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let at = self.infer(arg);
            if let Some(pt) = params.get(i)
                && !compatible(pt, &at)
            {
                self.error(
                    arg.span,
                    format!("argument {} of '{name}': expected {pt}, found {at}", i + 1),
                );
            }
        }
    }

    fn check_arity(&mut self, name: &str, n: usize, args: &[Expr], span: Span) {
        if args.len() != n {
            self.error(span, format!("{name}() expects {n} argument(s), got {}", args.len()));
        }
    }

    fn expect_bool(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer(e);
        if t != Ty::Bool && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be bool, found {t}"));
        }
    }

    fn expect_int(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer(e);
        if t != Ty::Int && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be int, found {t}"));
        }
    }

    fn expect_int_val(&mut self, e: &Expr) {
        self.expect_int(e, "argument");
    }
}

/// Result type of a numeric binary op: float if either side is float, else int.
fn numeric_result(l: &Ty, r: &Ty) -> Ty {
    if *l == Ty::Float || *r == Ty::Float {
        Ty::Float
    } else {
        Ty::Int
    }
}

fn op_sym(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        _ => "?",
    }
}

/// Built-in method signatures on `str` (M6). Must mirror the runtime handlers in both backends
/// (`interp::builtins::call_method` and `vm::Vm::do_method_call`).
fn str_method_sig(method: &str) -> Option<FnSig> {
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "upper" | "lower" | "trim" => (vec![], Ty::Str),
        "split" => (vec![Ty::Str], Ty::list(Ty::Str)),
        "join" => (vec![Ty::list(Ty::Str)], Ty::Str),
        "starts_with" | "contains" => (vec![Ty::Str], Ty::Bool),
        _ => return None,
    };
    Some(FnSig { params, ret })
}

/// Built-in method signatures on `list[T]` (M6). `elem` is the list's element type.
fn list_method_sig(method: &str, elem: &Ty) -> Option<FnSig> {
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "push" => (vec![elem.clone()], Ty::Nil),
        "pop" => (vec![], Ty::option(elem.clone())),
        "reverse" => (vec![], Ty::Nil),
        "contains" => (vec![elem.clone()], Ty::Bool),
        "index_of" => (vec![elem.clone()], Ty::Int),
        // `sum` is only valid on numeric lists; an unknown element type is tolerated
        // (it flows from an empty/unannotated list). Non-numeric is rejected at the call site.
        "sum" if elem.is_numeric() || elem.is_unknown() => (vec![], elem.clone()),
        // `sort` mutates in place (returns nil); only orderable element types (int/float/str).
        // Unknown is tolerated (empty/unannotated list). Non-orderable is rejected at the call site.
        "sort" if is_orderable(elem) || elem.is_unknown() => (vec![], Ty::Nil),
        _ => return None,
    };
    Some(FnSig { params, ret })
}

/// Element types that have a total order for `sort()`: the scalar comparables.
fn is_orderable(t: &Ty) -> bool {
    matches!(t, Ty::Int | Ty::Float | Ty::Str)
}

/// Valid `map` key types: the hashable scalars. `float` and composites are rejected (float
/// equality footgun; composites have no stable identity). `Unknown` is tolerated (no cascade).
fn is_hashable_key(t: &Ty) -> bool {
    matches!(t, Ty::Int | Ty::Str | Ty::Bool | Ty::Unknown)
}

/// Built-in method signatures on `map[K, V]` (gap #5). `k`/`v` are the key / value types.
fn map_method_sig(method: &str, k: &Ty, v: &Ty) -> Option<FnSig> {
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "has" => (vec![k.clone()], Ty::Bool),
        "get" => (vec![k.clone()], Ty::option(v.clone())),
        "keys" => (vec![], Ty::list(k.clone())),
        "values" => (vec![], Ty::list(v.clone())),
        "remove" => (vec![k.clone()], Ty::option(v.clone())),
        _ => return None,
    };
    Some(FnSig { params, ret })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod graph_tests {
    //! M4.5 cross-module type-checking tests (`check_graph` over real tempdir fixtures).
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("chezzi_chk_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, contents).unwrap();
            p
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn check_entry(entry: &Path) -> Result<(), Vec<CheckError>> {
        let graph = crate::resolver::build_graph(entry).expect("graph should build");
        check_graph(&graph)
    }

    fn errors(entry: &Path) -> Vec<String> {
        check_entry(entry).unwrap_err().into_iter().map(|e| e.message).collect()
    }

    // 18. A module member's type resolves: a correct use checks clean, a mismatch is rejected.
    #[test]
    fn module_member_type_resolves() {
        let t = TmpDir::new();
        t.write("a.chz", "fn read() -> int: return 5\n");
        let ok = t.write("ok.chz", "import a\nx: int = a.read()\nfn main(): print(x)\n");
        assert!(check_entry(&ok).is_ok(), "expected clean: {:?}", errors(&ok));

        let bad = t.write("bad.chz", "import a\nx: str = a.read()\nfn main(): print(x)\n");
        let errs = errors(&bad);
        assert!(
            errs.iter().any(|m| m.contains("cannot assign int to variable of type str")),
            "got: {errs:?}"
        );
    }

    // 19. A type error inside an imported module is reported (with a module label), and the entry's
    // own errors are still collected in the same run.
    #[test]
    fn type_error_across_module_boundary() {
        let t = TmpDir::new();
        // b returns str from an int-typed fn — a type error inside b.
        t.write("b.chz", "fn helper() -> int: return \"nope\"\n");
        let entry = t.write(
            "main.chz",
            // entry has its own error too: assigning int to a str var.
            "import helper from b\nbad: str = 5\nfn main(): print(helper())\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("in module 'b'")),
            "imported module error not labeled: {errs:?}"
        );
        assert!(
            errs.iter().any(|m| m.contains("cannot assign int to variable of type str")),
            "entry error not collected: {errs:?}"
        );
    }

    // 20. A `from` import of a name the module does not export is rejected at check time.
    #[test]
    fn unknown_imported_member_rejected() {
        let t = TmpDir::new();
        t.write("a.chz", "fn f(): print(\"f\")\n");
        let entry = t.write("main.chz", "import nope from a\nfn main(): print(1)\n");
        let errs = errors(&entry);
        assert!(errs.iter().any(|m| m.contains("has no member 'nope'")), "got: {errs:?}");
    }

    // 21. Type names are program-global in M4.5: the same struct in two loaded modules collides.
    #[test]
    fn cross_module_type_collision_rejected() {
        let t = TmpDir::new();
        t.write("a.chz", "struct Point:\n    x: int\nfn fa(): print(1)\n");
        t.write("b.chz", "struct Point:\n    y: int\nfn fb(): print(2)\n");
        let entry = t.write("main.chz", "import a\nimport b\nfn main(): print(1)\n");
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("'Point' is already defined")),
            "got: {errs:?}"
        );
    }
}
