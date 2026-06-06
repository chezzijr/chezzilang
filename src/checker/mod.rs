//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching the interpreter's hoist); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Expr, ExprKind, FnDecl, Import, LitPattern, MethodSig, Param,
    Pattern, Span, Stmt, StmtKind, Type, TypeParam, UnaryOp,
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
    /// Tuple scrutinee — arms are tuple patterns (gap #15). Carries the element types.
    Tuple(Vec<Ty>),
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

/// Prebuilt protocols a user program may use as bounds but must not redeclare (mirrors
/// [`prebuilt_protocols`]).
fn is_reserved_protocol(name: &str) -> bool {
    matches!(name, "Comparable" | "Stringable" | "Hashable" | "Add" | "Sub" | "Mul")
}

/// A function (or method) signature: parameter types and return type. `type_params` is non-empty
/// only for generic functions (`fn max[T: Comparable]`), where `params`/`ret` contain `Ty::Param`s.
#[derive(Clone)]
struct FnSig {
    params: Vec<Ty>,
    ret: Ty,
    type_params: Vec<TypeParam>,
}

impl FnSig {
    /// A non-generic signature (the common case).
    fn plain(params: Vec<Ty>, ret: Ty) -> FnSig {
        FnSig { params, ret, type_params: Vec::new() }
    }
}

/// A struct's shape: its generic type parameters (empty for a non-generic struct), ordered
/// `(field, type)` pairs, and its methods by name. Field/method types may contain `Ty::Param`s
/// naming the struct's type parameters; they're substituted at each use site.
struct StructInfo {
    type_params: Vec<TypeParam>,
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
}

/// A protocol's required method signatures, in declaration order. `Self` appears as `Ty::Param("Self")`
/// inside these sigs; conformance substitutes it with the candidate type.
#[derive(Clone)]
struct ProtocolInfo {
    methods: Vec<(String, FnSig)>,
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
    /// Native functions whose result type follows their argument type (int args → int, float args
    /// → float) instead of the fixed `FnSig` (gap #12: `std.math` `abs`/`min`/`max`). The `FnSig`
    /// still records arity; the result/param strictness is handled by `infer_numeric_poly`.
    numeric_poly: std::collections::HashSet<String>,
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
        sig.functions.insert(n.to_string(), FnSig::plain(params, ret));
    };
    match name {
        "std.math" => {
            // `abs` is numeric-polymorphic (int args → int, float args → float); the `FnSig` here
            // only fixes its arity — `infer_numeric_poly` does the real typing. (`min`/`max` moved
            // to `std.cmp` as generic `[T: Comparable]` functions, M7-G3.)
            func("abs", vec![Ty::Float], Ty::Float);
            sig.numeric_poly.insert("abs".into());
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
        "std.process" => {
            func("cmd", vec![Ty::Str], Ty::result(Ty::Str));
        }
        "std.fs" => {
            func("list_dir", vec![Ty::Str], Ty::result(Ty::list(Ty::Str)));
            func("exists", vec![Ty::Str], Ty::Bool);
            func("is_file", vec![Ty::Str], Ty::Bool);
            func("is_dir", vec![Ty::Str], Ty::Bool);
            func("size", vec![Ty::Str], Ty::result(Ty::Int));
            func("glob", vec![Ty::Str], Ty::result(Ty::list(Ty::Str)));
        }
        "std.time" => {
            func("now", vec![], Ty::Int);
            func("monotonic", vec![], Ty::Float);
            func("sleep_ms", vec![Ty::Int], Ty::Nil);
            func("format", vec![Ty::Int], Ty::Str);
        }
        "std.regex" => {
            // `Match` is the synthetic struct seeded in `seed_stdlib_structs`.
            let m = || Ty::Struct("Match".to_string(), vec![]);
            func("is_match", vec![Ty::Str, Ty::Str], Ty::result(Ty::Bool));
            func("find", vec![Ty::Str, Ty::Str], Ty::result(Ty::option(m())));
            func("find_all", vec![Ty::Str, Ty::Str], Ty::result(Ty::list(m())));
            func("replace_all", vec![Ty::Str, Ty::Str, Ty::Str], Ty::result(Ty::Str));
            func("split", vec![Ty::Str, Ty::Str], Ty::result(Ty::list(Ty::Str)));
        }
        "std.request" => {
            // `Response` is the synthetic struct seeded in `seed_stdlib_structs`.
            let resp = || Ty::Struct("Response".to_string(), vec![]);
            func("get", vec![Ty::Str], Ty::result(resp()));
            func("post", vec![Ty::Str, Ty::Str], Ty::result(resp()));
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
    /// Structural protocols by name. Program-global (like structs). Pre-seeded with `Comparable`.
    protocols: HashMap<String, ProtocolInfo>,
    /// Generic type parameters currently in scope (name → optional protocol bound), set while
    /// building/checking a generic fn's signature and body. Save/restore to nest.
    type_params: HashMap<String, Vec<String>>,
    /// enum name → its variant names, in declaration order (for exhaustiveness).
    enums: HashMap<String, Vec<String>>,
    /// enum name → its generic type parameters (empty for a non-generic enum). Used to build the
    /// substitution from `Tree[int]`'s args onto each variant's payload (which may name `T`).
    enum_type_params: HashMap<String, Vec<TypeParam>>,
    variants: HashMap<String, VariantInfo>,
    struct_names: std::collections::HashSet<String>,
    enum_names: std::collections::HashSet<String>,
    /// Transparent type aliases (`type UserId = int`): name → the aliased AST type, resolved on
    /// demand in `resolve_type`. `alias_resolving` is the active resolution stack (cycle guard).
    aliases: HashMap<String, Type>,
    alias_resolving: Vec<String>,
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
    /// `from`-imported names that are numeric-polymorphic native fns (`abs`/`min`/`max`), so a bare
    /// call resolves their result type by argument type instead of the float-only `FnSig` (gap #12).
    imported_poly: std::collections::HashSet<String>,
    /// Label of the module currently being checked (`None` = entry); prefixes its error messages.
    current_module_label: Option<String>,
    /// How many enclosing `for`/`while` loops we're inside *within the current function body*.
    /// Reset to 0 when descending into a (nested) function or closure body so an inner `break`
    /// can't escape into an outer loop. `> 0` ⇒ `break`/`continue` are legal here.
    loop_depth: usize,
}

impl Checker {
    fn new() -> Self {
        let mut c = Checker {
            errors: Vec::new(),
            scopes: Vec::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            protocols: prebuilt_protocols(),
            type_params: HashMap::new(),
            enums: HashMap::new(),
            enum_type_params: HashMap::new(),
            variants: HashMap::new(),
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            aliases: HashMap::new(),
            alias_resolving: Vec::new(),
            current_ret: Ty::Nil,
            inferring_ret: false,
            collected_rets: Vec::new(),
            module_sigs: HashMap::new(),
            imported_modules: HashMap::new(),
            imported_poly: std::collections::HashSet::new(),
            current_module_label: None,
            loop_depth: 0,
        };
        c.seed_stdlib_structs();
        c
    }

    /// Register the synthetic struct shapes that native std modules return (M9): `Match`
    /// (`std.regex`) and `Response` (`std.request`). They have no AST, so their field layouts are
    /// seeded here; `infer_field` then types `m.text`, `resp.status`, etc. Like all type names in
    /// M4.5 these are program-global, so `Match`/`Response` become reserved names (a user struct of
    /// the same name collides, as intended).
    fn seed_stdlib_structs(&mut self) {
        let mk = |fields: Vec<(&str, Ty)>| StructInfo {
            type_params: Vec::new(),
            fields: fields.into_iter().map(|(n, t)| (n.to_string(), t)).collect(),
            methods: HashMap::new(),
        };
        self.structs.insert(
            "Match".into(),
            mk(vec![
                ("text", Ty::Str),
                ("start", Ty::Int),
                ("end", Ty::Int),
                ("groups", Ty::list(Ty::Str)),
            ]),
        );
        self.struct_names.insert("Match".into());
        self.structs.insert(
            "Response".into(),
            mk(vec![
                ("status", Ty::Int),
                ("body", Ty::Str),
                ("headers", Ty::map(Ty::Str, Ty::Str)),
            ]),
        );
        self.struct_names.insert("Response".into());
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
        self.type_params.clear();
        self.imported_modules.clear();
        self.imported_poly.clear();
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
                        // Carry the numeric-polymorphism marker onto the imported name (gap #12).
                        if sig.numeric_poly.contains(member) {
                            self.imported_poly.insert(bind.clone());
                        }
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
                    // Cross-kind name clash: a struct and an enum can't share a name (they'd both
                    // register, the enum silently shadowed, and — sharing a `Name[args]` Display —
                    // produce nonsense like "cannot assign Foo[int] to … Foo[int]"). Same-kind dups
                    // are caught later in the resolve pass.
                    if self.enum_names.contains(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.struct_names.insert(name.clone());
                }
                StmtKind::Enum { name, .. } => {
                    if self.struct_names.contains(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.enum_names.insert(name.clone());
                }
                StmtKind::TypeAlias { name, ty } => {
                    if matches!(name.as_str(), "int" | "float" | "bool" | "str" | "nil")
                        || is_reserved_type(name)
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    } else if self.aliases.contains_key(name)
                        || self.struct_names.contains(name)
                        || self.enum_names.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    } else {
                        self.aliases.insert(name.clone(), ty.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// Second sub-pass: resolve and register signatures, fields, and variants. Redeclarations
    /// (a name defined twice) are reported here — otherwise "last write wins" would silently
    /// mis-type or, for struct methods, panic in pass 2 on a key that no longer exists.
    fn hoist(&mut self, stmts: &[Stmt]) {
        // Protocols first: function/struct signatures may reference them in type-parameter bounds.
        for s in stmts {
            if let StmtKind::Protocol { name, methods } = &s.kind {
                self.hoist_protocol(name, methods, s.span);
            }
        }
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    if self.functions.contains_key(&decl.name) {
                        self.error(s.span, format!("function '{}' is already defined", decl.name));
                    }
                    let sig = self.fn_sig(decl, s.span);
                    self.functions.insert(decl.name.clone(), sig);
                }
                StmtKind::Struct { name, type_params, fields, methods } => {
                    if is_reserved_type(name) {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    if self.structs.contains_key(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // The struct's type parameters are in scope across its field and method
                    // signatures (so `first: A` and `fn push(self, x: T)` resolve `A`/`T`).
                    let saved = self.enter_type_params(type_params);
                    let fields: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| (f.name.clone(), self.resolve_type(&f.ty, s.span)))
                        .collect();
                    let methods = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.exit_type_params(saved);
                    self.structs.insert(
                        name.clone(),
                        StructInfo { type_params: type_params.clone(), fields, methods },
                    );
                }
                StmtKind::Enum { name, type_params, variants } => {
                    if is_reserved_type(name) {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    if self.enums.contains_key(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // The enum's type parameters are in scope across its variant payloads (so a
                    // `Node(T, Tree[T])` resolves `T`). Validate each bound names a known protocol.
                    let saved = self.enter_type_params(type_params);
                    for tp in type_params {
                        for bound in &tp.bounds {
                            if !self.protocols.contains_key(bound) {
                                self.error(
                                    s.span,
                                    format!("unknown protocol '{bound}' in bound on '{}'", tp.name),
                                );
                            }
                        }
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
                    self.exit_type_params(saved);
                    self.enums.insert(name.clone(), names);
                    self.enum_type_params.insert(name.clone(), type_params.clone());
                }
                _ => {}
            }
        }
    }

    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type. The decl's
    /// generic `type_params` are installed (so `T` in annotations resolves to `Ty::Param("T")`) and
    /// each declared bound is validated against the known protocols.
    fn fn_sig(&mut self, decl: &FnDecl, span: Span) -> FnSig {
        let saved = self.enter_type_params(&decl.type_params);
        for tp in &decl.type_params {
            for bound in &tp.bounds {
                if !self.protocols.contains_key(bound) {
                    self.error(span, format!("unknown protocol '{bound}' in bound on '{}'", tp.name));
                }
            }
        }
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
        self.exit_type_params(saved);
        FnSig { params, ret, type_params: decl.type_params.clone() }
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
                StmtKind::Struct { name, type_params, methods, .. } => {
                    let self_ty = self.struct_self_ty(name);
                    let saved = self.enter_type_params(type_params);
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
                    self.exit_type_params(saved);
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
        let saved_tps = self.enter_type_params(&decl.type_params);
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
        self.exit_type_params(saved_tps);
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
                // A generic type parameter (`T`) or `Self`, in scope while checking a generic
                // fn signature/body or a protocol method — checked BEFORE type names so an
                // in-scope type parameter shadows a same-named struct/enum.
                _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
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
                _ if self.struct_names.contains(n) => {
                    // A generic struct written without type arguments is missing them.
                    let nparams = self.structs.get(n).map_or(0, |i| i.type_params.len());
                    if nparams > 0 {
                        self.error(span, format!("type '{n}' expects {nparams} type argument(s), got 0"));
                    }
                    Ty::strukt(n.clone())
                }
                _ if self.enum_names.contains(n) => {
                    // A generic enum written without type arguments is missing them.
                    let nparams = self.enum_type_params.get(n).map_or(0, |tps| tps.len());
                    if nparams > 0 {
                        self.error(span, format!("type '{n}' expects {nparams} type argument(s), got 0"));
                    }
                    Ty::Enum(n.clone(), Vec::new())
                }
                // A protocol name used as a value type (existential), e.g. `Error`.
                _ if self.protocols.contains_key(n) => Ty::Protocol(n.clone()),
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
                ("Result", [t, e]) => {
                    Ty::result_e(self.resolve_type(t, span), self.resolve_type(e, span))
                }
                ("Option", [inner]) => Ty::option(self.resolve_type(inner, span)),
                ("map", [k, v]) => {
                    let key = self.resolve_type(k, span);
                    let value = self.resolve_type(v, span);
                    if !self.is_hashable_key(&key) {
                        self.error(
                            span,
                            format!("map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {key}"),
                        );
                    }
                    Ty::map(key, value)
                }
                ("set", [t]) => {
                    let elem = self.resolve_type(t, span);
                    if !self.is_hashable_key(&elem) {
                        self.error(
                            span,
                            format!("set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {elem}"),
                        );
                    }
                    Ty::set(elem)
                }
                // A user-defined generic struct instantiated with type arguments: `Pair[int, str]`.
                _ if self.struct_names.contains(n) => {
                    let resolved: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, span)).collect();
                    // Clone the param list out so the borrow on `self.structs` is dropped before
                    // the `satisfies`/`error` calls below.
                    let tps = self.structs.get(n).map(|i| i.type_params.clone());
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
                                if let Err(msg) = self.satisfies(arg, bound) {
                                    self.error(span, msg);
                                }
                            }
                        }
                    }
                    Ty::Struct(n.clone(), resolved)
                }
                // A user-defined generic enum instantiated with type arguments: `Tree[int]`.
                _ if self.enum_names.contains(n) => {
                    let resolved: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, span)).collect();
                    let tps = self.enum_type_params.get(n).cloned();
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
                                if let Err(msg) = self.satisfies(arg, bound) {
                                    self.error(span, msg);
                                }
                            }
                        }
                    }
                    Ty::Enum(n.clone(), resolved)
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
                        if !self.assignable(&expected, &val_ty) {
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
            StmtKind::Struct { name, type_params, methods, .. } => {
                let self_ty = self.struct_self_ty(name);
                // The struct's type parameters are in scope across its method bodies.
                let saved = self.enter_type_params(type_params);
                for m in methods {
                    // Panic-safe: a redeclared struct name means `structs[name]` is a *different*
                    // struct whose method table may not contain `m.name`.
                    if let Some(sig) =
                        self.structs.get(name).and_then(|s| s.methods.get(&m.name)).cloned()
                    {
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
                self.exit_type_params(saved);
            }
            // Enums, imports, and protocols carry nothing to check in pass 2 (protocol method
            // signatures are validated during hoisting).
            StmtKind::Enum { .. }
            | StmtKind::Import(_)
            | StmtKind::Protocol { .. }
            | StmtKind::TypeAlias { .. } => {}
            StmtKind::If { branches, else_block } => {
                for (cond, body) in branches {
                    self.expect_bool(cond, "if condition");
                    self.check_block(body);
                }
                if let Some(body) = else_block {
                    self.check_block(body);
                }
            }
            StmtKind::For { vars, iter, body } => {
                let bindings = self.for_bindings(vars, iter);
                self.push_scope();
                for (name, ty) in bindings {
                    self.declare(&name, ty);
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
                    Ty::Struct(sname, targs) => {
                        let field_ty = self.structs.get(sname).and_then(|info| {
                            info.fields.iter().find(|(f, _)| f == name).map(|(_, ty)| {
                                subst(ty, &struct_param_map(info, targs))
                            })
                        });
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
                if !self.assignable(target_ty, val_ty) {
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
                } else if !self.assignable(&ret, &ty) {
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
        let saved_tps = self.enter_type_params(&decl.type_params);
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
        self.exit_type_params(saved_tps);
    }

    /// The element type produced by iterating `iter` in a `for` loop.
    /// The per-iteration bindings of a `for` loop: one name for the common form, or two
    /// (`for k, v in m:`) to destructure a map's entries. A range/list/str binds a single value; a
    /// map binds its key (1 name) or key+value (2 names). Any other arity/iterand combination is an
    /// error (a dummy `Unknown` binding is returned per name so checking continues).
    fn for_bindings(&mut self, vars: &[String], iter: &Expr) -> Vec<(String, Ty)> {
        let unknowns = |vars: &[String]| vars.iter().map(|v| (v.clone(), Ty::Unknown)).collect();
        // Ranges are syntactic and always yield a single int.
        if let ExprKind::Range { start, end } = &iter.kind {
            self.expect_int(start, "range bound");
            self.expect_int(end, "range bound");
            if vars.len() != 1 {
                self.error(iter.span, "a range binds a single loop variable; `for k, v` needs a map");
                return unknowns(vars);
            }
            return vec![(vars[0].clone(), Ty::Int)];
        }
        let it = self.infer(iter);
        match &it {
            Ty::Map(k, v) => match vars.len() {
                1 => vec![(vars[0].clone(), (**k).clone())],
                2 => vec![(vars[0].clone(), (**k).clone()), (vars[1].clone(), (**v).clone())],
                _ => {
                    self.error(iter.span, "a `for` over a map binds one (key) or two (key, value) names");
                    unknowns(vars)
                }
            },
            Ty::List(_) | Ty::Str | Ty::Set(_) if vars.len() != 1 => {
                self.error(iter.span, format!("`for k, v` requires a map, found {it}"));
                unknowns(vars)
            }
            Ty::List(inner) => vec![(vars[0].clone(), (**inner).clone())],
            Ty::Set(elem) => vec![(vars[0].clone(), (**elem).clone())],
            Ty::Str => vec![(vars[0].clone(), Ty::Str)],
            Ty::Unknown => unknowns(vars),
            other => {
                self.error(iter.span, format!("cannot iterate over {other}"));
                unknowns(vars)
            }
        }
    }

    /// How a `match` is being checked, derived from the scrutinee's type.
    fn match_kind(&mut self, scrutinee: &Expr) -> MatchKind {
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
                        let payload =
                            self.variants[&v].payload.iter().map(|p| subst(p, &map)).collect();
                        (v, payload)
                    })
                    .collect();
                MatchKind::Variants { label: name.clone(), variants }
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
            Ty::Unknown => MatchKind::Skip, // un-inferable: skip exhaustiveness
            other => {
                self.error(scrutinee.span, format!("cannot match on non-enum type {other}"));
                MatchKind::Skip
            }
        }
    }

    /// The substitution from a generic enum's type parameters to a concrete instantiation's type
    /// arguments (`Tree[int]` ⇒ `{T: int}`). Empty for a non-generic enum.
    fn enum_param_map(&self, name: &str, targs: &[Ty]) -> HashMap<String, Ty> {
        self.enum_type_params
            .get(name)
            .map(|tps| tps.iter().map(|tp| tp.name.clone()).zip(targs.iter().cloned()).collect())
            .unwrap_or_default()
    }

    /// The variant→payload map for an enum/Option/Result type, else `None`. Shared by `match_kind`
    /// and the nested-pattern checker (gap #15) so they agree on what counts as a variant.
    fn variants_of(&self, ty: &Ty) -> Option<HashMap<String, Vec<Ty>>> {
        match ty {
            Ty::Enum(name, targs) => {
                let map = self.enum_param_map(name, targs);
                let vs = self.enums.get(name)?;
                Some(
                    vs.iter()
                        .map(|v| {
                            let payload =
                                self.variants[v].payload.iter().map(|p| subst(p, &map)).collect();
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

    /// Type-check a *nested* sub-pattern (a variant payload slot or tuple element — gap #15) against
    /// its expected type `ty`, declaring any bindings into the current scope. Returns whether the
    /// sub-pattern is **irrefutable** (matches every value of `ty`): a binding/wildcard is, a
    /// literal/variant is not, a tuple is iff all its elements are.
    fn bind_subpattern(&mut self, pattern: &Pattern, ty: &Ty, span: Span) -> bool {
        match pattern {
            Pattern::Wildcard => true,
            Pattern::Ident(name) => {
                // A nested bare identifier is a binding. Guard the footgun where it shadows a
                // nullary variant of the matched type (`Cons(h, None)`): that's a variant the user
                // likely meant to match, which nested patterns don't support yet.
                if let Some(vmap) = self.variants_of(ty)
                    && vmap.get(name).is_some_and(|p| p.is_empty())
                {
                    self.error(
                        span,
                        format!("'{name}' is a variant of {ty}; nested nullary-variant patterns aren't supported — use a nested match"),
                    );
                    return true;
                }
                self.declare(name, ty.clone());
                true
            }
            Pattern::Literal(lit) => {
                let lit_ty = lit_pattern_ty(lit);
                if !ty.is_unknown() && &lit_ty != ty {
                    self.error(span, format!("literal of type {lit_ty} cannot match a value of type {ty}"));
                }
                false
            }
            Pattern::Tuple(subs) => {
                match ty {
                    Ty::Tuple(tys) => {
                        if tys.len() != subs.len() {
                            self.error(
                                span,
                                format!("tuple pattern has {} element(s), but the value has {}", subs.len(), tys.len()),
                            );
                        }
                        let mut irref = true;
                        for (sub, t) in subs.iter().zip(tys.iter()) {
                            irref &= self.bind_subpattern(sub, t, span);
                        }
                        irref
                    }
                    Ty::Unknown => {
                        let mut irref = true;
                        for sub in subs {
                            irref &= self.bind_subpattern(sub, &Ty::Unknown, span);
                        }
                        irref
                    }
                    other => {
                        self.error(span, format!("tuple pattern cannot match a value of type {other}"));
                        for sub in subs {
                            self.bind_subpattern(sub, &Ty::Unknown, span);
                        }
                        false
                    }
                }
            }
            Pattern::Variant { name, bindings } => {
                match self.variants_of(ty) {
                    Some(vmap) => match vmap.get(name) {
                        Some(payload) => {
                            if payload.len() != bindings.len() {
                                self.error(
                                    span,
                                    format!("variant '{name}' binds {} value(s), but {} given", payload.len(), bindings.len()),
                                );
                            }
                            for (b, t) in bindings.iter().zip(payload.iter()) {
                                self.bind_subpattern(b, t, span);
                            }
                        }
                        None => {
                            self.error(span, format!("'{name}' is not a variant of {ty}"));
                            for b in bindings {
                                self.bind_subpattern(b, &Ty::Unknown, span);
                            }
                        }
                    },
                    None if ty.is_unknown() => {
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                    None => {
                        self.error(span, format!("variant pattern '{name}' cannot match a value of type {ty}"));
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                }
                false
            }
        }
    }

    /// Push a scope and bind one arm's pattern, recording coverage + diagnostics. Returns `true` if
    /// this arm is **irrefutable** (a `_` wildcard, or a tuple of irrefutable sub-patterns — either
    /// makes the match exhaustive). The caller must `pop_scope` after the arm body.
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
                // Un-inferable scrutinee: accept the pattern shape permissively, binding everything
                // as `Unknown`. Still scope so the caller can `pop_scope` uniformly.
                self.push_scope();
                match pattern {
                    Pattern::Variant { name, bindings } => {
                        covered.insert(name.clone());
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                    Pattern::Tuple(subs) => {
                        for s in subs {
                            self.bind_subpattern(s, &Ty::Unknown, span);
                        }
                    }
                    _ => {}
                }
            }
            MatchKind::Variants { label, variants } => {
                self.push_scope();
                match pattern {
                    Pattern::Variant { name, bindings } => {
                        let payload = variants.get(name).cloned();
                        if payload.is_none() {
                            self.error(span, format!("'{name}' is not a variant of {label}"));
                        }
                        if !covered.insert(name.clone()) {
                            self.error(span, format!("duplicate match arm '{name}'"));
                        }
                        match &payload {
                            Some(payload) => {
                                if payload.len() != bindings.len() {
                                    self.error(
                                        span,
                                        format!("variant '{name}' binds {} value(s), but {} given", payload.len(), bindings.len()),
                                    );
                                }
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    self.bind_subpattern(b, t, span);
                                }
                            }
                            None => {
                                for b in bindings {
                                    self.bind_subpattern(b, &Ty::Unknown, span);
                                }
                            }
                        }
                    }
                    Pattern::Literal(_) => self.error(span, format!("cannot match a literal against {label}")),
                    Pattern::Tuple(_) => self.error(span, format!("cannot match a tuple against {label}")),
                    Pattern::Ident(_) | Pattern::Wildcard => {
                        unreachable!("ident/wildcard handled elsewhere")
                    }
                }
            }
            MatchKind::Literal(ty) => {
                self.push_scope();
                match pattern {
                    Pattern::Literal(lit) => {
                        let lit_ty = lit_pattern_ty(lit);
                        if &lit_ty != ty {
                            self.error(
                                span,
                                format!("literal of type {lit_ty} cannot match scrutinee of type {ty}"),
                            );
                        }
                    }
                    Pattern::Variant { .. } => self.error(span, format!("cannot match a variant against {ty}")),
                    Pattern::Tuple(_) => self.error(span, format!("cannot match a tuple against {ty}")),
                    Pattern::Ident(_) | Pattern::Wildcard => {
                        unreachable!("ident/wildcard handled elsewhere")
                    }
                }
            }
            MatchKind::Tuple(tys) => {
                self.push_scope();
                if let Pattern::Tuple(subs) = pattern {
                    if tys.len() != subs.len() {
                        self.error(
                            span,
                            format!("tuple pattern has {} element(s), but the value has {}", subs.len(), tys.len()),
                        );
                    }
                    let mut irref = true;
                    for (sub, t) in subs.iter().zip(tys.iter()) {
                        irref &= self.bind_subpattern(sub, t, span);
                    }
                    return irref;
                }
                self.error(span, "a tuple scrutinee requires a tuple pattern (or `_`)".to_string());
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
            MatchKind::Tuple(_) => {
                // A tuple match is exhaustive only via an irrefutable arm (a `_`, or a tuple of
                // all-binding sub-patterns). `has_wildcard` already captured that.
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
            ExprKind::Set(elems) => self.infer_set(elems),
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
            ExprKind::DecodeCall { obj, ty, arg } => self.infer_decode(obj, ty, arg, expr.span),
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
        // A nullary user variant used as a value (e.g. `Red`, or `Leaf` of a generic `Tree[T]`).
        // A nullary variant carries no payload to infer type arguments from, so a generic enum's
        // args are left `Unknown` (e.g. `Leaf` is `Tree[?]`), unified later against a typed slot.
        if let Some(v) = self.variants.get(name)
            && v.payload.is_empty()
        {
            let nparams = self.enum_type_params.get(&v.enum_name).map_or(0, |tps| tps.len());
            return Ty::Enum(v.enum_name.clone(), vec![Ty::Unknown; nparams]);
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
    fn infer_set(&mut self, elems: &[Expr]) -> Ty {
        let mut elem = Ty::Unknown;
        for e in elems {
            let et = self.infer(e);
            if !et.is_unknown() && !self.is_hashable_key(&et) {
                self.error(
                    e.span,
                    format!("set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {et}"),
                );
            }
            if elem.is_unknown() {
                elem = et;
            } else if !et.is_unknown() && !compatible(&elem, &et) {
                self.error(e.span, format!("set elements differ: {elem} vs {et}"));
            }
        }
        Ty::set(elem)
    }

    fn infer_map(&mut self, entries: &[(Expr, Expr)]) -> Ty {
        let mut key = Ty::Unknown;
        let mut value = Ty::Unknown;
        for (k_expr, v_expr) in entries {
            let kt = self.infer(k_expr);
            let vt = self.infer(v_expr);
            if !kt.is_unknown() && !self.is_hashable_key(&kt) {
                self.error(
                    k_expr.span,
                    format!("map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {kt}"),
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
                } else if let Some(t) = self.op_overload_result(&l, &r, "Add") {
                    t
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply + to {l} and {r}"));
                    Ty::Unknown
                }
            }
            // `-`/`*` overload via the `Sub`/`Mul` protocols on same-typed structs; `/`/`%` stay
            // numeric-only (no protocol).
            Sub | Mul => {
                let proto = if op == Sub { "Sub" } else { "Mul" };
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) = self.op_overload_result(&l, &r, proto) {
                    t
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply {} to {l} and {r}", op_sym(op)));
                    Ty::Unknown
                }
            }
            Div | Mod => {
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
                let ok = (l.is_numeric() && r.is_numeric())
                    || (l == Ty::Str && r == Ty::Str)
                    || self.ordering_allowed(&l, &r);
                if !ok && !either_unknown {
                    self.error(lhs.span, format!("cannot compare {l} and {r}"));
                }
                Ty::Bool
            }
            // Bitwise/shift ops are int-only (gap #13).
            BitAnd | BitOr | BitXor | Shl | Shr => {
                if l == Ty::Int && r == Ty::Int {
                    Ty::Int
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!("bitwise operator {} requires int operands, found {l} and {r}", op_sym(op)),
                    );
                    Ty::Unknown
                }
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
            Ty::Struct(sname, targs) => {
                if let Some(info) = self.structs.get(sname) {
                    let map = struct_param_map(info, targs);
                    if let Some((_, ty)) = info.fields.iter().find(|(f, _)| f == name) {
                        return subst(ty, &map);
                    }
                    if let Some(sig) = info.methods.get(name) {
                        let params = sig.params.iter().map(|t| subst(t, &map)).collect();
                        return Ty::Func { params, ret: Box::new(subst(&sig.ret, &map)) };
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
        let ret_err = match &self.current_ret {
            Ty::Result(_, e) => Some((**e).clone()),
            Ty::Option(_) | Ty::Nil => None,
            other => {
                self.error(
                    span,
                    format!("'?' used in a function that returns {other}, not Result or Option"),
                );
                None
            }
        };
        match t {
            Ty::Result(ok, err) => {
                // Propagating an `Err` early-returns it as the enclosing function's error, so the
                // inner error type must fit the enclosing one (Rust-like). Skip when the enclosing
                // returns Option/Nil (no error slot to check against).
                if let Some(re) = ret_err
                    && !self.assignable(&re, &err)
                {
                    self.error(
                        span,
                        format!("'?' propagates error {err}, but the enclosing function's error type is {re}"),
                    );
                }
                *ok
            }
            Ty::Option(inner) => *inner,
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(span, format!("'?' expects Result or Option, found {other}"));
                Ty::Unknown
            }
        }
    }

    /// `json.decode[T](s)` — the source must be `str`, the target `T` must be decodable. Yields
    /// `Result[T]`. (`obj` is the json-module expression; we infer it only to surface a bad-module
    /// error, but place no constraint on it — any module exposing `parse` works at runtime.)
    fn infer_decode(&mut self, obj: &Expr, ty: &Type, arg: &Expr, span: Span) -> Ty {
        let _ = self.infer(obj);
        let arg_ty = self.infer(arg);
        if !compatible(&Ty::Str, &arg_ty) {
            self.error(span, format!("decode source must be str, found {arg_ty}"));
        }
        let target = self.resolve_type(ty, span);
        if let Err(msg) = self.is_decodable(&target, &mut Vec::new()) {
            self.error(span, msg);
            return Ty::Unknown;
        }
        Ty::result(target)
    }

    /// Whether `json.decode` can produce a value of this type. Mirrors `json_decode::from_type`'s
    /// acceptance (kept in sync): scalars, `list`/`map[str,_]`/`Option` of decodables, and
    /// non-generic, non-recursive structs of decodable fields. `visiting` rejects recursive structs.
    fn is_decodable(&self, ty: &Ty, visiting: &mut Vec<String>) -> Result<(), String> {
        match ty {
            Ty::Int | Ty::Float | Ty::Str | Ty::Bool => Ok(()),
            Ty::Unknown => Ok(()), // an error was already reported; don't pile on
            Ty::List(t) | Ty::Option(t) => self.is_decodable(t, visiting),
            Ty::Map(k, v) => {
                if !matches!(**k, Ty::Str) {
                    return Err(format!("decode: map keys must be str, found {k}"));
                }
                self.is_decodable(v, visiting)
            }
            Ty::Struct(name, args) => {
                if !args.is_empty() {
                    return Err(format!("decode: cannot decode into generic struct {ty}"));
                }
                if visiting.iter().any(|s| s == name) {
                    return Err(format!(
                        "decode: recursive struct '{name}' is not decodable; use the Json enum instead"
                    ));
                }
                let Some(info) = self.structs.get(name) else {
                    return Err(format!("decode: '{name}' is not a decodable type"));
                };
                visiting.push(name.clone());
                let fields = info.fields.clone();
                for (_, fty) in &fields {
                    self.is_decodable(fty, visiting)?;
                }
                visiting.pop();
                Ok(())
            }
            other => Err(format!("decode: cannot decode into {other}")),
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
                if !self.assignable(&declared, &body_ty) {
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
            "ord" => {
                self.check_arity("ord", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer(a) {
                        Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("ord() expects a str, got {other}")),
                    }
                }
                Some(Ty::Int)
            }
            "chr" => {
                self.check_arity("chr", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer(a) {
                        Ty::Int | Ty::Unknown => {}
                        other => self.error(a.span, format!("chr() expects an int, got {other}")),
                    }
                }
                Some(Ty::Str)
            }
            "set" => {
                // `set()` → empty set (element inferred from later use, like `{}` for maps);
                // `set(xs)` → a set from a list, deduped.
                match args.len() {
                    0 => Some(Ty::set(Ty::Unknown)),
                    1 => {
                        let elem = match self.infer(&args[0]) {
                            Ty::List(inner) => *inner,
                            Ty::Unknown => Ty::Unknown,
                            other => {
                                self.error(args[0].span, format!("set() expects a list, got {other}"));
                                Ty::Unknown
                            }
                        };
                        if !elem.is_unknown() && !self.is_hashable_key(&elem) {
                            self.error(
                                span,
                                format!("set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {elem}"),
                            );
                        }
                        Some(Ty::set(elem))
                    }
                    _ => {
                        self.error(span, "set() expects set() or set(list)");
                        Some(Ty::set(Ty::Unknown))
                    }
                }
            }
            // Generic built-in constructors for Result / Option.
            // `Ok(x)`: success type known, error type open (unifies with the declared `E`).
            "Ok" => Some(Ty::result_e(self.one_arg(name, args, span), Ty::Unknown)),
            "Some" => Some(Ty::option(self.one_arg(name, args, span))),
            // `Err(x)`: error type known (`typeof x`), success type open.
            "Err" => Some(Ty::result_e(Ty::Unknown, self.one_arg(name, args, span))),
            _ => {
                // Struct constructor?
                if let Some((tps, fields)) =
                    self.structs.get(name).map(|i| (i.type_params.clone(), i.fields.clone()))
                {
                    let field_tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
                    if tps.is_empty() {
                        self.check_args(name, &field_tys, args, span);
                        return Some(Ty::strukt(name.to_string()));
                    }
                    // Generic struct: infer its type arguments by unifying the declared field types
                    // (which contain the struct's `Ty::Param`s) against the argument types.
                    let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
                    if arg_tys.len() != field_tys.len() {
                        self.check_arity(name, field_tys.len(), args, span);
                    }
                    let mut sub = HashMap::new();
                    for (decl, actual) in field_tys.iter().zip(&arg_tys) {
                        unify(decl, actual, &mut sub);
                    }
                    for (decl, (actual, arg)) in field_tys.iter().zip(arg_tys.iter().zip(args)) {
                        let expected = subst(decl, &sub);
                        if !self.assignable(&expected, actual) {
                            self.error(
                                arg.span,
                                format!("argument to '{name}' has type {actual}, expected {expected}"),
                            );
                        }
                    }
                    // Enforce each type parameter's protocol bounds against its inferred argument.
                    for tp in &tps {
                        if let Some(concrete) = sub.get(&tp.name) {
                            for bound in &tp.bounds {
                                if let Err(msg) = self.satisfies(concrete, bound) {
                                    self.error(span, msg);
                                }
                            }
                        }
                    }
                    let targs =
                        tps.iter().map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown)).collect();
                    return Some(Ty::Struct(name.to_string(), targs));
                }
                // User enum variant constructor?
                if let Some(v) = self.variants.get(name).cloned() {
                    let tps =
                        self.enum_type_params.get(&v.enum_name).cloned().unwrap_or_default();
                    if tps.is_empty() {
                        self.check_args(name, &v.payload, args, span);
                        return Some(Ty::Enum(v.enum_name, Vec::new()));
                    }
                    // Generic enum: infer the enum's type arguments by unifying the variant's
                    // declared payload types (which contain the enum's `Ty::Param`s) against the
                    // argument types, then check each argument against the substituted payload.
                    let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
                    if arg_tys.len() != v.payload.len() {
                        self.check_arity(name, v.payload.len(), args, span);
                    }
                    let mut sub = HashMap::new();
                    for (decl, actual) in v.payload.iter().zip(&arg_tys) {
                        unify(decl, actual, &mut sub);
                    }
                    for (decl, (actual, arg)) in v.payload.iter().zip(arg_tys.iter().zip(args)) {
                        let expected = subst(decl, &sub);
                        if !self.assignable(&expected, actual) {
                            self.error(
                                arg.span,
                                format!("argument to '{name}' has type {actual}, expected {expected}"),
                            );
                        }
                    }
                    for tp in &tps {
                        if let Some(concrete) = sub.get(&tp.name) {
                            for bound in &tp.bounds {
                                if let Err(msg) = self.satisfies(concrete, bound) {
                                    self.error(span, msg);
                                }
                            }
                        }
                    }
                    let targs =
                        tps.iter().map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown)).collect();
                    return Some(Ty::Enum(v.enum_name, targs));
                }
                // Global function?
                if let Some(sig) = self.functions.get(name).cloned() {
                    // A `from`-imported numeric-polymorphic native fn (abs/min/max) types by its
                    // argument type, not the float-only `FnSig` (gap #12).
                    if self.imported_poly.contains(name) {
                        return Some(self.infer_numeric_poly(name, sig.params.len(), args, span));
                    }
                    // A generic function: infer its type parameters from the arguments, enforce
                    // bounds, and substitute into the return type.
                    if !sig.type_params.is_empty() {
                        return Some(self.infer_generic_call(name, &sig, args, span));
                    }
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
                let sig = self.imported_modules.get(mname).and_then(|id| self.module_sigs.get(id));
                let is_poly = sig.is_some_and(|s| s.numeric_poly.contains(method));
                let fsig = sig.and_then(|s| s.functions.get(method).cloned());
                // Numeric-polymorphic native fns (gap #12): result type follows the argument type.
                if is_poly {
                    let arity = fsig.as_ref().map_or(2, |f| f.params.len());
                    return self.infer_numeric_poly(method, arity, args, span);
                }
                if let Some(fsig) = fsig {
                    // A generic module function (`cmp.max`): infer its type parameters from the
                    // arguments, enforce bounds, and substitute into the return type.
                    if !fsig.type_params.is_empty() {
                        return self.infer_generic_call(method, &fsig, args, span);
                    }
                    self.check_args(method, &fsig.params, args, span);
                    return fsig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("module '{mname}' has no member '{method}'"));
                Ty::Unknown
            }
            // A protocol existential (e.g. `Error`): only the protocol's own methods are callable.
            Ty::Protocol(pname) => {
                let sig = self
                    .protocols
                    .get(pname)
                    .and_then(|pinfo| pinfo.methods.iter().find(|(m, _)| m == method))
                    .map(|(_, msig)| msig.clone());
                if let Some(msig) = sig {
                    // First param is the implicit receiver; explicit args correspond to params[1..].
                    let expected = msig.params.get(1..).unwrap_or(&[]).to_vec();
                    self.check_args(method, &expected, args, span);
                    return msig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {pname} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Struct(sname, targs) => {
                // Substitute the struct's type arguments into the method signature, so calling
                // `Stack[int].push(x)` checks `x` against `int`, not the parameter `T`.
                let resolved = self.structs.get(sname).and_then(|info| {
                    info.methods.get(method).map(|sig| {
                        let map = struct_param_map(info, targs);
                        let params: Vec<Ty> = sig.params.iter().map(|t| subst(t, &map)).collect();
                        (params, subst(&sig.ret, &map))
                    })
                });
                if let Some((params, ret)) = resolved {
                    // The first param is the receiver (bound implicitly from `obj`), so the call's
                    // explicit args correspond to params[1..]. A method with NO params has no
                    // receiver slot — both engines prepend the receiver and would error at runtime,
                    // so reject the call here instead.
                    match params.split_first() {
                        Some((_receiver, expected)) => self.check_args(method, expected, args, span),
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
                // `sort()` works on any list whose element is Comparable: the scalar orderables
                // (int/float/str) OR a struct that satisfies the `Comparable` protocol. The runtime
                // dispatches the struct case to each element's `compare`. Handled here (not in the
                // fixed `list_method_sig` table) because it needs `self.satisfies`.
                if method == "sort" {
                    self.check_arity("sort", 0, args, span);
                    let elem = (**elem).clone();
                    if is_orderable(&elem) || elem.is_unknown() || self.satisfies(&elem, "Comparable").is_ok() {
                        return Ty::Nil;
                    }
                    self.error(
                        span,
                        format!("sort() requires a list of Comparable values (int, float, str, or a struct with a `compare` method), found list[{elem}]"),
                    );
                    return Ty::Nil;
                }
                if let Some(sig) = list_method_sig(method, elem) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                if method == "sum" {
                    self.error(span, format!("sum() requires a numeric list, found list[{elem}]"));
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
            Ty::Set(elem) => {
                if let Some(sig) = set_method_sig(method, elem) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            // A bound generic type parameter exposes its protocol's methods (e.g. `a.compare(b)`
            // where `a: T` and `T: Comparable`).
            Ty::Param(pname) => {
                // Search the param's bounds for a protocol that declares `method` (multi-bound
                // `T: Add + Mul` exposes the union of both protocols' methods).
                let bounds = self.type_params.get(pname).cloned().unwrap_or_default();
                let msig = bounds.iter().find_map(|proto| {
                    self.protocols
                        .get(proto)
                        .and_then(|p| p.methods.iter().find(|(n, _)| n == method).map(|(_, s)| s.clone()))
                });
                if let Some(msig) = msig {
                    let map = HashMap::from([("Self".to_string(), obj_ty.clone())]);
                    let expected: Vec<Ty> = match msig.params.split_first() {
                        Some((_recv, rest)) => rest.iter().map(|t| subst(t, &map)).collect(),
                        None => Vec::new(),
                    };
                    self.check_args(method, &expected, args, span);
                    return subst(&msig.ret, &map);
                }
                self.infer_all(args);
                self.error(span, format!("type parameter {pname} has no method '{method}'"));
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

    /// Type a numeric-polymorphic native call (`std.math` `abs`/`min`/`max`): every argument must be
    /// the *same* numeric type (int or float — no implicit int/float mix, matching the language's
    /// no-implicit-widening rule), and the result type is that argument type. `Ty::Unknown` args are
    /// tolerated (no cascade); an all-unknown call yields `Ty::Unknown`.
    fn infer_numeric_poly(&mut self, method: &str, arity: usize, args: &[Expr], span: Span) -> Ty {
        self.check_arity(method, arity, args, span);
        let mut saw_int = false;
        let mut saw_float = false;
        let mut bad = false;
        for a in args {
            match self.infer(a) {
                Ty::Int => saw_int = true,
                Ty::Float => saw_float = true,
                Ty::Unknown => {}
                other => {
                    self.error(a.span, format!("argument of '{method}': expected int or float, found {other}"));
                    bad = true;
                }
            }
        }
        if saw_int && saw_float {
            self.error(
                span,
                format!("'{method}' arguments must be the same numeric type (no implicit int/float mix)"),
            );
            return Ty::Unknown;
        }
        if bad {
            return Ty::Unknown;
        }
        if saw_float {
            Ty::Float
        } else if saw_int {
            Ty::Int
        } else {
            Ty::Unknown
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
                && !self.assignable(pt, &at)
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

    // ===== generics & protocols =====

    /// The `Self` type for a struct's own methods: `Struct(name, [Param(p) for each type param])`,
    /// so inside `struct Stack[T]` the receiver is `Stack[T]` and `self.items` is `list[T]`.
    fn struct_self_ty(&self, name: &str) -> Ty {
        let args = self
            .structs
            .get(name)
            .map(|i| i.type_params.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
            .unwrap_or_default();
        Ty::Struct(name.to_string(), args)
    }

    /// Install `tps` as the in-scope generic type parameters, returning the previous map to restore.
    fn enter_type_params(&mut self, tps: &[TypeParam]) -> HashMap<String, Vec<String>> {
        let saved = self.type_params.clone();
        for tp in tps {
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        saved
    }

    fn exit_type_params(&mut self, saved: HashMap<String, Vec<String>>) {
        self.type_params = saved;
    }

    /// Register a `protocol` declaration's method signatures. `Self` resolves to `Ty::Param("Self")`.
    fn hoist_protocol(&mut self, name: &str, methods: &[MethodSig], span: Span) {
        if is_reserved_protocol(name) {
            self.error(span, format!("protocol '{name}' is reserved (builtin)"));
            return;
        }
        if self.protocols.contains_key(name) {
            self.error(span, format!("protocol '{name}' is already defined"));
        }
        let mut saved = self.type_params.clone();
        std::mem::swap(&mut self.type_params, &mut saved); // start clean, with only Self visible
        self.type_params.insert("Self".to_string(), Vec::new());
        let sigs = methods
            .iter()
            .map(|m| {
                let params = m
                    .params
                    .iter()
                    .map(|p| match &p.ty {
                        Some(t) => self.resolve_type(t, span),
                        None if p.name == "self" => Ty::Unknown,
                        None => {
                            self.error(span, format!("protocol method parameter '{}' needs a type", p.name));
                            Ty::Unknown
                        }
                    })
                    .collect();
                let ret = m.ret.as_ref().map(|t| self.resolve_type(t, span)).unwrap_or(Ty::Nil);
                (m.name.clone(), FnSig::plain(params, ret))
            })
            .collect();
        self.type_params = saved; // restore
        self.protocols.insert(name.to_string(), ProtocolInfo { methods: sigs });
    }

    /// Does concrete `ty` structurally satisfy `protocol`? Read-only. Primitives intrinsically
    /// satisfy `Comparable`; structs satisfy any protocol whose methods they all implement.
    /// Valid `map` key / `set` element types: anything that satisfies the `Hashable` protocol —
    /// the scalars `int`/`str`/`bool` intrinsically, or a struct defining `hash(self) -> int`.
    /// `float` is rejected (NaN/equality footgun); `Unknown` is tolerated (no cascade). With this,
    /// user structs can be map keys / set elements, hashed via their `hash()` at runtime.
    fn is_hashable_key(&self, t: &Ty) -> bool {
        self.satisfies(t, "Hashable").is_ok()
    }

    /// Assignability with protocol-existential awareness. Like the free [`compatible`], but a
    /// concrete type is assignable to a `Protocol(P)` slot iff it satisfies `P` — which needs the
    /// protocol/struct registry, so it can't live in the context-free `compatible`. Recurses through
    /// compound types so a nested existential (the `E` in `Result[T, Error]`) is checked structurally.
    fn assignable(&self, expected: &Ty, actual: &Ty) -> bool {
        use Ty::*;
        match (expected, actual) {
            (Unknown, _) | (_, Unknown) => true,
            (Protocol(p), a) => self.satisfies(a, p).is_ok(),
            (List(e), List(a)) | (Option(e), Option(a)) | (Set(e), Set(a)) => self.assignable(e, a),
            (Result(et, ee), Result(at, ae)) => {
                self.assignable(et, at) && self.assignable(ee, ae)
            }
            (Map(ek, ev), Map(ak, av)) => self.assignable(ek, ak) && self.assignable(ev, av),
            (Struct(n, ea), Struct(m, aa)) | (Enum(n, ea), Enum(m, aa)) => {
                n == m && ea.len() == aa.len() && ea.iter().zip(aa).all(|(x, y)| self.assignable(x, y))
            }
            (Tuple(e), Tuple(a)) => {
                e.len() == a.len() && e.iter().zip(a).all(|(x, y)| self.assignable(x, y))
            }
            (Func { params: p1, ret: r1 }, Func { params: p2, ret: r2 }) => {
                p1.len() == p2.len()
                    && p1.iter().zip(p2).all(|(a, b)| self.assignable(a, b))
                    && self.assignable(r1, r2)
            }
            _ => compatible(expected, actual),
        }
    }

    fn satisfies(&self, ty: &Ty, protocol: &str) -> Result<(), String> {
        let Some(pinfo) = self.protocols.get(protocol) else {
            return Err(format!("unknown protocol '{protocol}'"));
        };
        if let Ty::Unknown = ty {
            return Ok(()); // don't cascade
        }
        if protocol == "Comparable" && matches!(ty, Ty::Int | Ty::Float | Ty::Str) {
            return Ok(());
        }
        // `Hashable` is satisfied intrinsically by the scalar key types (mirrors the map/set key
        // restriction; float is excluded — its equality is a hazard). Struct conformance falls
        // through to the structural check (needs a `hash(self) -> int` method).
        if protocol == "Hashable" && matches!(ty, Ty::Int | Ty::Str | Ty::Bool) {
            return Ok(());
        }
        // `str` conforms to `Error` intrinsically (Go-style: its message is itself).
        if protocol == "Error" && matches!(ty, Ty::Str) {
            return Ok(());
        }
        // A protocol existential value satisfies a protocol iff it IS that protocol.
        if let Ty::Protocol(p) = ty {
            return if p == protocol {
                Ok(())
            } else {
                Err(format!("type {ty} does not satisfy {protocol}"))
            };
        }
        // The numeric operator protocols are satisfied intrinsically by int/float (their `+ - *` are
        // the primitive ops), so a `[T: Add + Mul]` generic works over numbers as well as structs.
        if matches!(protocol, "Add" | "Sub" | "Mul") && matches!(ty, Ty::Int | Ty::Float) {
            return Ok(());
        }
        // A bound type parameter satisfies a protocol if that protocol is among its declared bounds —
        // this is what lets a generic forward its `T: P` value into another `[U: P]` call.
        if let Ty::Param(name) = ty {
            return if self.type_params.get(name).is_some_and(|bs| bs.iter().any(|b| b == protocol)) {
                Ok(())
            } else {
                Err(format!("type {ty} does not satisfy {protocol}"))
            };
        }
        match ty {
            Ty::Struct(sname, _) => {
                let Some(info) = self.structs.get(sname) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                for (mname, msig) in &pinfo.methods {
                    match info.methods.get(mname) {
                        Some(actual) if method_matches(msig, actual, ty) => {}
                        Some(_) => {
                            return Err(format!(
                                "type {ty} does not satisfy {protocol} (method '{mname}' has the wrong signature)"
                            ));
                        }
                        None => {
                            return Err(format!(
                                "type {ty} does not satisfy {protocol} (missing method '{mname}')"
                            ));
                        }
                    }
                }
                Ok(())
            }
            _ => Err(format!("type {ty} does not satisfy {protocol}")),
        }
    }

    /// Result type of an overloaded arithmetic operator (`+`/`-`/`*`) on two operands of the *same*
    /// struct or type-parameter that satisfies `protocol` (`Add`/`Sub`/`Mul`). The runtime dispatches
    /// to the `add`/`sub`/`mul` method; the result type is that same type. `None` ⇒ not overloadable.
    fn op_overload_result(&self, l: &Ty, r: &Ty, protocol: &str) -> Option<Ty> {
        let same = match (l, r) {
            (Ty::Struct(a, _), Ty::Struct(b, _)) => a == b,
            (Ty::Param(a), Ty::Param(b)) => a == b,
            _ => false,
        };
        if same && self.satisfies(l, protocol).is_ok() {
            Some(l.clone())
        } else {
            None
        }
    }

    /// Are `l < r` etc. allowed? True for same-named comparable type params, or same-named structs
    /// that satisfy `Comparable` (operator overloading dispatches to their `compare` at runtime).
    fn ordering_allowed(&self, l: &Ty, r: &Ty) -> bool {
        match (l, r) {
            (Ty::Param(a), Ty::Param(b)) if a == b => self
                .type_params
                .get(a)
                .is_some_and(|bs| bs.iter().any(|proto| self.protocol_has_method(proto, "compare"))),
            (Ty::Struct(a, _), Ty::Struct(b, _)) if a == b => self.satisfies(l, "Comparable").is_ok(),
            _ => false,
        }
    }

    fn protocol_has_method(&self, protocol: &str, method: &str) -> bool {
        self.protocols
            .get(protocol)
            .is_some_and(|p| p.methods.iter().any(|(n, _)| n == method))
    }

    /// Type-check a call to a generic function: infer each type parameter from the arguments,
    /// enforce the declared bounds, and substitute into the return type.
    fn infer_generic_call(&mut self, name: &str, sig: &FnSig, args: &[Expr], span: Span) -> Ty {
        if args.len() != sig.params.len() {
            self.check_arity(name, sig.params.len(), args, span);
        }
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
        // Infer the type-parameter substitution from positional arguments.
        let mut subst_map: HashMap<String, Ty> = HashMap::new();
        for (decl, actual) in sig.params.iter().zip(&arg_tys) {
            unify(decl, actual, &mut subst_map);
        }
        // Enforce declared bounds against each inferred binding.
        for tp in &sig.type_params {
            if let Some(concrete) = subst_map.get(&tp.name) {
                for bound in &tp.bounds {
                    if let Err(msg) = self.satisfies(concrete, bound) {
                        self.error(span, msg);
                    }
                }
            }
        }
        // Each argument must match its parameter's substituted type (catches a type param used in
        // two positions with conflicting types, e.g. `max(1, "x")`).
        for (decl, (actual, arg)) in sig.params.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &subst_map);
            if !self.assignable(&expected, actual) {
                self.error(
                    arg.span,
                    format!("argument to '{name}' has type {actual}, expected {expected}"),
                );
            }
        }
        subst(&sig.ret, &subst_map)
    }
}

/// The prebuilt protocols every program starts with. `Comparable` requires
/// `compare(self, other: Self) -> int`; primitives (int/float/str) satisfy it intrinsically.
fn prebuilt_protocols() -> HashMap<String, ProtocolInfo> {
    let mut m = HashMap::new();
    m.insert(
        "Comparable".to_string(),
        ProtocolInfo {
            // receiver `self` (Unknown), `other: Self` (Param "Self"), returning int.
            methods: vec![(
                "compare".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Int),
            )],
        },
    );
    m.insert(
        "Stringable".to_string(),
        ProtocolInfo {
            // receiver `self` (Unknown) only, returning str. A struct with `str(self) -> str`
            // satisfies it; `print`/`str()`/interpolation dispatch to that method at runtime.
            methods: vec![("str".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Str))],
        },
    );
    m.insert(
        // The default error type (Go-style `error`): one method `message(self) -> str`. `str`
        // conforms intrinsically; any struct with `message(self) -> str` conforms structurally.
        "Error".to_string(),
        ProtocolInfo {
            methods: vec![("message".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Str))],
        },
    );
    m.insert(
        "Hashable".to_string(),
        ProtocolInfo {
            // receiver `self` (Unknown) only, returning int. A struct with `hash(self) -> int`
            // satisfies it. WIRED TO MAP/SET KEYS: `map`/`set` are real hash tables (insertion-order
            // entries + a hash→position index), so any `Hashable` type can be a key/element — the
            // scalars int/str/bool intrinsically, or a struct via its `hash()` (dispatched at
            // runtime, hash confirmed by structural `==`). CONTRACT: two structurally-equal structs
            // (the `==` used to confirm a probe) MUST return the same `hash()` — the implementor owns
            // this (like Rust's `Hash`/`Eq`); the checker can't enforce purity.
            methods: vec![("hash".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Int))],
        },
    );
    // Per-operator numeric protocols (M10-G3): a struct satisfying `Add`/`Sub`/`Mul` (method
    // `add`/`sub`/`mul`(self, other: Self) -> Self) overloads `+`/`-`/`*`. `Self` for `other` and the
    // return makes them binary same-type operators (mirrors `Comparable`'s `compare`).
    for (proto, method) in [("Add", "add"), ("Sub", "sub"), ("Mul", "mul")] {
        m.insert(
            proto.to_string(),
            ProtocolInfo {
                methods: vec![(
                    method.to_string(),
                    FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Param("Self".into())),
                )],
            },
        );
    }
    m
}

/// The substitution from a struct's type parameters to a concrete instantiation's type arguments
/// (`Stack[int]` ⇒ `{T: int}`). Empty for a non-generic struct.
fn struct_param_map(info: &StructInfo, targs: &[Ty]) -> HashMap<String, Ty> {
    info.type_params.iter().map(|tp| tp.name.clone()).zip(targs.iter().cloned()).collect()
}

/// Substitute generic type parameters in `ty` using `map` (e.g. `Self ↦ Point`, `T ↦ int`).
/// Unmapped params are left as-is.
fn subst(ty: &Ty, map: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param(n) => map.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Ty::List(t) => Ty::list(subst(t, map)),
        Ty::Option(t) => Ty::option(subst(t, map)),
        Ty::Result(t, e) => Ty::result_e(subst(t, map), subst(e, map)),
        Ty::Map(k, v) => Ty::map(subst(k, map), subst(v, map)),
        Ty::Set(t) => Ty::set(subst(t, map)),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst(t, map)).collect()),
        Ty::Func { params, ret } => Ty::Func {
            params: params.iter().map(|t| subst(t, map)).collect(),
            ret: Box::new(subst(ret, map)),
        },
        Ty::Struct(n, args) => Ty::Struct(n.clone(), args.iter().map(|t| subst(t, map)).collect()),
        Ty::Enum(n, args) => Ty::Enum(n.clone(), args.iter().map(|t| subst(t, map)).collect()),
        other => other.clone(),
    }
}

/// Does a struct method `actual` match a protocol method `proto` (with `Self` bound to `self_ty`)?
fn method_matches(proto: &FnSig, actual: &FnSig, self_ty: &Ty) -> bool {
    if proto.params.len() != actual.params.len() {
        return false;
    }
    let map = HashMap::from([("Self".to_string(), self_ty.clone())]);
    proto
        .params
        .iter()
        .zip(&actual.params)
        .all(|(p, a)| compatible(&subst(p, &map), a))
        && compatible(&subst(&proto.ret, &map), &actual.ret)
}

/// Bind type parameters in `decl` to the corresponding concrete `actual` types (first binding wins;
/// `Unknown` actuals are ignored so an un-inferable argument doesn't pin a param).
fn unify(decl: &Ty, actual: &Ty, map: &mut HashMap<String, Ty>) {
    match (decl, actual) {
        (Ty::Param(n), a) => {
            if !a.is_unknown() && !map.contains_key(n) {
                map.insert(n.clone(), a.clone());
            }
        }
        (Ty::List(d), Ty::List(a)) | (Ty::Option(d), Ty::Option(a)) => unify(d, a, map),
        (Ty::Result(dt, de), Ty::Result(at, ae)) => {
            unify(dt, at, map);
            unify(de, ae, map);
        }
        (Ty::Map(dk, dv), Ty::Map(ak, av)) => {
            unify(dk, ak, map);
            unify(dv, av, map);
        }
        (Ty::Struct(dn, da), Ty::Struct(an, aa)) | (Ty::Enum(dn, da), Ty::Enum(an, aa))
            if dn == an && da.len() == aa.len() =>
        {
            da.iter().zip(aa).for_each(|(d, a)| unify(d, a, map));
        }
        (Ty::Tuple(ds), Ty::Tuple(as_)) if ds.len() == as_.len() => {
            ds.iter().zip(as_).for_each(|(d, a)| unify(d, a, map));
        }
        (Ty::Func { params: dp, ret: dr }, Ty::Func { params: ap, ret: ar })
            if dp.len() == ap.len() =>
        {
            dp.iter().zip(ap).for_each(|(d, a)| unify(d, a, map));
            unify(dr, ar, map);
        }
        _ => {}
    }
}

/// The type a literal match-pattern matches against.
fn lit_pattern_ty(lit: &LitPattern) -> Ty {
    match lit {
        LitPattern::Int(_) => Ty::Int,
        LitPattern::Str(_) => Ty::Str,
        LitPattern::Bool(_) => Ty::Bool,
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
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        _ => "?",
    }
}

/// Built-in method signatures on `str` (M6). Must mirror the runtime handlers in both backends
/// (`interp::builtins::call_method` and `vm::Vm::do_method_call`).
fn str_method_sig(method: &str) -> Option<FnSig> {
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "upper" | "lower" | "trim" => (vec![], Ty::Str),
        // `str` conforms to `Error`: `message()` returns the string itself.
        "message" => (vec![], Ty::Str),
        "split" => (vec![Ty::Str], Ty::list(Ty::Str)),
        "chars" => (vec![], Ty::list(Ty::Str)),
        "join" => (vec![Ty::list(Ty::Str)], Ty::Str),
        "starts_with" | "contains" => (vec![Ty::Str], Ty::Bool),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
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
        // `sort` is handled in `infer_method_call` (it needs `self.satisfies` to allow Comparable
        // structs), so it never reaches this table.
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Element types that have a total order for `sort()`: the scalar comparables.
fn is_orderable(t: &Ty) -> bool {
    matches!(t, Ty::Int | Ty::Float | Ty::Str)
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
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `set[T]`. `elem` is the set's element type.
fn set_method_sig(method: &str, elem: &Ty) -> Option<FnSig> {
    let set = Ty::set(elem.clone());
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "has" => (vec![elem.clone()], Ty::Bool),
        "add" => (vec![elem.clone()], Ty::Nil),
        "remove" => (vec![elem.clone()], Ty::Bool), // true if the element was present
        "union" | "intersection" | "difference" => (vec![set.clone()], set),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
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
