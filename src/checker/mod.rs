//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching the interpreter's hoist); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Bound, CompKind, DeferTarget, Expr, ExprKind, FnDecl, Import,
    LitPattern, MethodSig, Param, Pattern, Span, SpawnTarget, Stmt, StmtKind, Type, TypeParam,
    UnaryOp, WaitArm, WaitTarget,
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
    name == "Result" || name == "Option" || name == "Executor"
}

/// Prebuilt protocols a user program may use as bounds but must not redeclare (mirrors
/// [`prebuilt_protocols`]).
fn is_reserved_protocol(name: &str) -> bool {
    matches!(
        name,
        "Comparable"
            | "Stringable"
            | "Hashable"
            | "Add"
            | "Sub"
            | "Mul"
            | "Iterator"
            | "Index"
            | "IndexSet"
            | "Slice"
    )
}

/// A function (or method) signature: parameter types and return type. `type_params` is non-empty
/// only for generic functions (`fn max[T: Comparable]`), where `params`/`ret` contain `Ty::Param`s.
#[derive(Clone)]
struct FnSig {
    params: Vec<Ty>,
    ret: Ty,
    type_params: Vec<TypeParam>,
    /// D6c — the minimum number of arguments this signature accepts; `params.len()` for an ordinary
    /// fixed-arity signature (set by [`FnSig::plain`]), but smaller when trailing params are optional
    /// (the net socket ops' optional `timeout_ms`). [`Checker::check_args`] accepts any arg count in
    /// `min_params..=params.len()`.
    min_params: usize,
}

impl FnSig {
    /// A non-generic signature (the common case): every param is required (`min_params == params.len()`).
    fn plain(params: Vec<Ty>, ret: Ty) -> FnSig {
        let min_params = params.len();
        FnSig { params, ret, type_params: Vec::new(), min_params }
    }

    /// D6c — a non-generic signature whose last `optional` params may be omitted (the net socket ops'
    /// optional trailing `timeout_ms`). `check_args` accepts `params.len() - optional ..= params.len()`.
    fn optional_tail(params: Vec<Ty>, ret: Ty, optional: usize) -> FnSig {
        let min_params = params.len() - optional;
        FnSig { params, ret, type_params: Vec::new(), min_params }
    }
}

/// Where a struct was declared: a stdlib module (`std.*`) vs a user/entry module. Lets a sendability
/// rule key on the *builtin* `Ref[T]` (std.ref) without snaring a user struct that's merely named
/// `Ref` (the principled replacement for the old bare-name check).
#[derive(Clone, Copy, PartialEq, Eq)]
enum StructOrigin {
    Builtin,
    User,
}

/// A struct's shape: its generic type parameters (empty for a non-generic struct), ordered
/// `(field, type)` pairs, and its methods by name. Field/method types may contain `Ty::Param`s
/// naming the struct's type parameters; they're substituted at each use site.
struct StructInfo {
    type_params: Vec<TypeParam>,
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
    origin: StructOrigin,
}

/// A protocol's required method signatures, in declaration order. `Self` appears as `Ty::Param("Self")`
/// inside these sigs; conformance substitutes it with the candidate type. `type_params` are the
/// protocol's own parameters (`protocol Container[T]` ⇒ `["T"]`); a bound supplies concrete args for
/// them (`[X: Container[int]]`), substituted into the method sigs before structural matching. Empty
/// for a bare protocol; the built-in `Iterator` carries one (its element type).
#[derive(Clone)]
struct ProtocolInfo {
    type_params: Vec<String>,
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
        c.current_module_is_stdlib = lm.dotted.first().map(String::as_str) == Some("std");
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
            // Trig / exp / log intrinsics (additive): plain `float -> float`, out-of-domain → NaN.
            func("sin", vec![Ty::Float], Ty::Float);
            func("cos", vec![Ty::Float], Ty::Float);
            func("tan", vec![Ty::Float], Ty::Float);
            func("asin", vec![Ty::Float], Ty::Float);
            func("acos", vec![Ty::Float], Ty::Float);
            func("atan", vec![Ty::Float], Ty::Float);
            func("atan2", vec![Ty::Float, Ty::Float], Ty::Float);
            func("exp", vec![Ty::Float], Ty::Float);
            func("ln", vec![Ty::Float], Ty::Float);
            func("log2", vec![Ty::Float], Ty::Float);
            func("log10", vec![Ty::Float], Ty::Float);
            func("log", vec![Ty::Float, Ty::Float], Ty::Float);
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
            // `exit(code)` never returns, but the checker has no `never` type; `nil` is the
            // closest void-ish result and lets statements (unreachable in practice) follow it.
            func("exit", vec![Ty::Int], Ty::Nil);
        }
        "std.process" => {
            func("cmd", vec![Ty::Str], Ty::result(Ty::Str));
        }
        "std.net" => {
            // D6 — minimal TCP surface. `connect`/`listen` take a `"host:port"` address; the sockets
            // are non-blocking (a would-block op parks the fiber on the netpoller). The `connect`/
            // `listen`/`read`/`write`/`accept` calls are intercepted in the VM (they allocate a
            // `Socket`/`Listener` handle + register poller interest), not run as off-heap natives.
            func("connect", vec![Ty::Str], Ty::result(Ty::Socket));
            func("listen", vec![Ty::Str], Ty::result(Ty::Listener));
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
            // General verb + custom headers; verb wrappers for the common non-GET/POST methods.
            func(
                "request",
                vec![Ty::Str, Ty::Str, Ty::Str, Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str))],
                Ty::result(resp()),
            );
            func("put", vec![Ty::Str, Ty::Str], Ty::result(resp()));
            func("patch", vec![Ty::Str, Ty::Str], Ty::result(resp()));
            func("delete", vec![Ty::Str], Ty::result(resp()));
            func("head", vec![Ty::Str], Ty::result(resp()));
        }
        _ => {}
    }
    sig
}

struct Checker {
    errors: Vec<CheckError>,
    scopes: Vec<HashMap<String, Ty>>,
    /// Per-scope set of names bound as `for`-loop variables. Mirrors `scopes` index-for-index (a
    /// loop var is immutable — rebound fresh each iteration — so assigning to it is rejected; this
    /// sidesteps a VM/interp divergence where the VM's counter slot IS the loop var).
    loop_vars: Vec<std::collections::HashSet<String>>,
    functions: HashMap<String, FnSig>,
    structs: HashMap<String, StructInfo>,
    /// Structural protocols by name. Program-global (like structs). Pre-seeded with `Comparable`.
    protocols: HashMap<String, ProtocolInfo>,
    /// Generic type parameters currently in scope (name → its protocol bounds), set while
    /// building/checking a generic fn's signature and body. Save/restore to nest. Bounds carry their
    /// type args (`Iterator[T]`) so element-type recovery can read them in scope.
    type_params: HashMap<String, Vec<Bound>>,
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
    /// `> 0` while checking statements lexically inside a `recover:` block (within the current
    /// function — reset across nested fn/closure boundaries). A `?` here targets the recover
    /// boundary (yielding to `r`), not the enclosing function's return.
    recover_depth: u32,
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
    /// For each `spawn:` block body currently being checked, the local-scope depth (`scopes.len()`)
    /// at the point the task body opened. A binding living at a scope index *below* the innermost
    /// floor is a **captured** binding — read-only inside the task (assigning to it is an error).
    /// Empty outside any `spawn:` block.
    capture_floors: Vec<usize>,
    /// Like [`Self::capture_floors`] but for `defer:` block bodies. A `defer:` block runs in the
    /// **same task** (no airlock), so reads of an enclosing local are fine and non-sendable captures
    /// are legal — it does NOT engage the read sendability gate that `capture_floors` drives.
    /// However the block captures its free variables **by value** at the defer point, and neither
    /// engine can write back through that snapshot (the VM has no `SetCaptured` op; the interp would
    /// write a discarded copy), so *reassigning* an enclosing local is rejected at the reassign gate.
    /// Empty outside any `defer:` block.
    defer_floors: Vec<usize>,
    /// True while checking a `std.*` module — structs hoisted now are tagged `StructOrigin::Builtin`.
    current_module_is_stdlib: bool,
}

impl Checker {
    fn new() -> Self {
        let mut c = Checker {
            errors: Vec::new(),
            scopes: Vec::new(),
            loop_vars: Vec::new(),
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
            recover_depth: 0,
            inferring_ret: false,
            collected_rets: Vec::new(),
            module_sigs: HashMap::new(),
            imported_modules: HashMap::new(),
            imported_poly: std::collections::HashSet::new(),
            current_module_label: None,
            loop_depth: 0,
            capture_floors: Vec::new(),
            defer_floors: Vec::new(),
            current_module_is_stdlib: false,
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
            origin: StructOrigin::Builtin,
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
        self.loop_vars.clear();
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
        self.check_spawn_global_mutation(stmts);
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

    /// G1 (B3.3b): under `--parallel` a module global is **read-only after init** — cross-task
    /// mutable state must go through `Shared[T]` (the `value → Ref[T] → Shared[T]` mutation ladder's
    /// top rung). A reassignment of a module global reachable — directly or **transitively through
    /// free-function calls** — from a `spawn` task is an error. Flow-scoped to `spawn` reachability:
    /// the same global mutated only from sequential code stays legal (the default cooperative engine
    /// is single-heap and unaffected). Method-mediated chains (`obj.m()` / `spawn obj.m()`) are a
    /// documented gap that lands with method-task support (B3.3 thread flip).
    fn check_spawn_global_mutation(&mut self, stmts: &[Stmt]) {
        // Module globals = top-level `let` binding names.
        let mut globals: std::collections::HashSet<String> = std::collections::HashSet::new();
        for s in stmts {
            if let StmtKind::Let { names, .. } = &s.kind {
                globals.extend(names.iter().cloned());
            }
        }
        if globals.is_empty() {
            return;
        }

        // Free (top-level) functions by name — the only `Ident`-callable nodes in the call graph.
        let mut fns: HashMap<&str, &FnDecl> = HashMap::new();
        for s in stmts {
            if let StmtKind::Fn(d) = &s.kind {
                fns.insert(d.name.as_str(), d);
            }
        }

        // Spawn roots: every `spawn` anywhere in the module contributes its target free fn(s).
        let mut roots: Vec<String> = Vec::new();
        collect_spawn_roots(stmts, &fns, &mut roots);

        // Transitive closure over the free-function call graph (`reachable` doubles as the cycle guard).
        let mut reachable: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut work = roots;
        while let Some(name) = work.pop() {
            if !reachable.insert(name.clone()) {
                continue;
            }
            if let Some(d) = fns.get(name.as_str()) {
                let mut callees = Vec::new();
                let mut scopes = vec![d.params.iter().map(|p| p.name.clone()).collect()];
                collect_free_calls_block(&d.body, &fns, &mut scopes, &mut callees);
                for c in callees {
                    if !reachable.contains(&c) {
                        work.push(c);
                    }
                }
            }
        }

        // Each reachable free fn: flag its module-global reassignments (shadow-aware).
        let mut hits: Vec<(Span, String)> = Vec::new();
        for name in &reachable {
            if let Some(d) = fns.get(name.as_str()) {
                let mut scopes: Vec<std::collections::HashSet<String>> =
                    vec![d.params.iter().map(|p| p.name.clone()).collect()];
                find_global_mutations(&d.body, &globals, &mut scopes, &mut hits);
            }
        }
        // Deterministic diagnostic order (the reachable set's iteration order is not stable).
        hits.sort_by_key(|(sp, _)| (sp.line, sp.col));
        for (sp, name) in hits {
            self.error(
                sp,
                format!("cannot mutate module global '{name}' from a parallel task; use Shared[T]"),
            );
        }
    }

    // ===== scopes =====

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.loop_vars.push(std::collections::HashSet::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.loop_vars.pop();
    }
    fn declare(&mut self, name: &str, ty: Ty) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
        // Re-declaring a name (e.g. `:=` shadowing a loop var in the same scope) yields a fresh,
        // mutable binding — clear any loop-var mark so assignment to it isn't wrongly rejected.
        if let Some(set) = self.loop_vars.last_mut() {
            set.remove(name);
        }
    }
    fn lookup(&self, name: &str) -> Option<Ty> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }
    /// Mark `name` (already declared in the current scope) as an immutable `for`-loop variable.
    fn mark_loop_var(&mut self, name: &str) {
        if let Some(set) = self.loop_vars.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name`'s nearest binding a `for`-loop variable? Resolves to the binding's defining scope
    /// so an inner `:=` shadow (a fresh local) is correctly reported as not-a-loop-var.
    fn is_loop_var(&self, name: &str) -> bool {
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return self.loop_vars[i].contains(name);
            }
        }
        false
    }
    /// Is `name` a binding **captured** by an enclosing `spawn:` task — i.e. defined in a local
    /// scope below the innermost task's floor? Such bindings are read-only inside the task body
    /// (the airlock: a task gets its own copy, so reassigning the capture can't leak out). A
    /// task-local binding (declared inside the task) and a global/function are not captures.
    fn is_captured(&self, name: &str) -> bool {
        let Some(&floor) = self.capture_floors.last() else {
            return false;
        };
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return i < floor;
            }
        }
        false
    }
    /// Whether `name` is an enclosing local captured by the innermost `defer:` block (i.e. bound at a
    /// scope below the block's floor). Drives ONLY the reassign gate — unlike [`Self::is_captured`]
    /// it does not gate reads, since a same-task `defer:` block reads enclosing locals freely.
    fn is_defer_captured(&self, name: &str) -> bool {
        let Some(&floor) = self.defer_floors.last() else {
            return false;
        };
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return i < floor;
            }
        }
        false
    }
    /// Like [`is_captured`], but excludes module-level (scope 0) bindings — imports and top-level
    /// declarations are globals resolvable identically in every task (like free functions), not
    /// per-task value captures. Used by the *read* sendability gate so reading an imported module or
    /// a top-level closure inside a `spawn:` block isn't flagged; the *reassign* gate keeps the
    /// broader [`is_captured`] (writing a copy of any capture, global or not, can't leak out).
    fn is_local_capture(&self, name: &str) -> bool {
        let Some(&floor) = self.capture_floors.last() else {
            return false;
        };
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                return i > 0 && i < floor;
            }
        }
        false
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
            if let StmtKind::Protocol { name, type_params, methods } = &s.kind {
                self.hoist_protocol(name, type_params, methods, s.span);
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
                    let origin = if self.current_module_is_stdlib {
                        StructOrigin::Builtin
                    } else {
                        StructOrigin::User
                    };
                    self.structs.insert(
                        name.clone(),
                        StructInfo { type_params: type_params.clone(), fields, methods, origin },
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
                        self.check_bounds(&tp.bounds, &tp.name, s.span);
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
                StmtKind::Extern { fns, .. } => {
                    // Each extern C fn becomes a plain module-global signature, hoisted exactly like
                    // a top-level `fn` so calls type-check through the normal `infer_named_call` path.
                    // v1 marshals scalars only — every resolved param + return type must be
                    // C-marshallable (int/float/bool/str, or void return).
                    for ef in fns {
                        if self.functions.contains_key(&ef.name) {
                            self.error(
                                ef.span,
                                format!("function '{}' is already defined", ef.name),
                            );
                        }
                        let params: Vec<Ty> = ef
                            .params
                            .iter()
                            .map(|p| match &p.ty {
                                Some(t) => {
                                    let ty = self.resolve_type(t, ef.span);
                                    // A parameter must be a real C scalar — `nil` (void) is a
                                    // return-only sentinel and would panic the backend's `ctype_of`.
                                    self.assert_marshallable(&ty, &ef.name, ef.span, false);
                                    ty
                                }
                                None => {
                                    self.error(
                                        ef.span,
                                        format!(
                                            "extern parameter '{}' needs a type annotation",
                                            p.name
                                        ),
                                    );
                                    Ty::Unknown
                                }
                            })
                            .collect();
                        let ret = match &ef.ret {
                            Some(t) => {
                                let ty = self.resolve_type(t, ef.span);
                                // The return slot may be `nil` (void) in addition to the C scalars.
                                self.assert_marshallable(&ty, &ef.name, ef.span, true);
                                ty
                            }
                            // A void extern returns nothing observable; model it as `Nil`.
                            None => Ty::Nil,
                        };
                        self.functions.insert(ef.name.clone(), FnSig::plain(params, ret));
                    }
                }
                _ => {}
            }
        }
    }

    /// v1 C-ABI marshallability: an extern fn's param/return types must be C-scalar — `int`, `float`,
    /// `bool`, or `str` (`char*`). `Nil` (void) is accepted ONLY for the return slot (`allow_void`),
    /// never for a parameter: a `nil` param has no `CType` lowering and would panic the backend's
    /// `ctype_of`, while a void-returning extern's `Nil` value would otherwise satisfy it. Everything
    /// else (list/map/set/tuple/struct/enum/func/option/result/protocol/channel/…) is rejected with a
    /// single uniform error. Called on the **resolved** `Ty` (after `resolve_type`), so a transparent
    /// alias to a scalar is accepted. `Unknown` is already-errored and silently allowed (no cascade).
    fn assert_marshallable(&mut self, ty: &Ty, fn_name: &str, span: Span, allow_void: bool) {
        let ok = matches!(ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Unknown)
            || (allow_void && matches!(ty, Ty::Nil));
        if !ok {
            self.error(
                span,
                format!(
                    "type '{ty}' is not C-marshallable in extern fn '{fn_name}' \
                     (v1 supports only int, float, bool, str)"
                ),
            );
        }
    }

    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type. The decl's
    /// generic `type_params` are installed (so `T` in annotations resolves to `Ty::Param("T")`) and
    /// each declared bound is validated against the known protocols.
    fn fn_sig(&mut self, decl: &FnDecl, span: Span) -> FnSig {
        // A method's own `[U]` may not reuse a type parameter already in scope (the struct's `[T]`):
        // it would be a confusing double-binding. `self.type_params` is empty for a free fn, so this
        // only fires for methods declared inside a generic struct.
        for tp in &decl.type_params {
            if self.type_params.contains_key(&tp.name) {
                self.error(
                    span,
                    format!(
                        "method type parameter '{}' shadows the struct's type parameter '{}'",
                        tp.name, tp.name
                    ),
                );
            }
        }
        let saved = self.enter_type_params(&decl.type_params);
        for tp in &decl.type_params {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
        let params: Vec<Ty> = decl
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
        FnSig { min_params: params.len(), params, ret, type_params: decl.type_params.clone() }
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
                // The C5 escape hatch handle, non-generic (a bare `Executor` type annotation).
                "Executor" => Ty::Executor,
                // D6 — the std.net TCP handles, non-generic (bare `Socket` / `Listener` annotations).
                "Socket" => Ty::Socket,
                "Listener" => Ty::Listener,
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
                ("Channel", [inner]) => {
                    let elem = self.resolve_type(inner, span);
                    if !self.sendable(&elem) {
                        self.error(
                            span,
                            format!("Channel element type must be sendable, found {elem}"),
                        );
                    }
                    Ty::channel(elem)
                }
                // `Shared[T]` (C3): the cross-task mutable box. Unlike a `Channel`, its element type
                // isn't gated on sendability — the value lives in one owner and is copied in/out
                // through `get`/`set`; the *handle* is what crosses (always sendable).
                ("Shared", [inner]) => Ty::shared(self.resolve_type(inner, span)),
                // `Atomic[T]` (type annotation): the cross-task atomic box. Like `Shared`, its element
                // type isn't gated on sendability — the handle is what crosses.
                ("Atomic", [inner]) => Ty::atomic(self.resolve_type(inner, span)),
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
                                if let Err(msg) = self.satisfies(arg, &bound.name) {
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
                                if let Err(msg) = self.satisfies(arg, &bound.name) {
                                    self.error(span, msg);
                                }
                            }
                        }
                    }
                    Ty::Enum(n.clone(), resolved)
                }
                // A parameterized protocol may only be a bound (`[X: Container[int]]`), not an
                // existential value type — `Ty::Protocol` carries no args. Resolve the args anyway so
                // an unknown type inside is still reported.
                _ if self.protocols.contains_key(n) => {
                    for a in args {
                        let _ = self.resolve_type(a, span);
                    }
                    self.error(
                        span,
                        format!("parameterized protocol '{n}' can only be used as a bound, not as a value type"),
                    );
                    Ty::Unknown
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
            StmtKind::Struct { name, type_params, fields, methods } => {
                let self_ty = self.struct_self_ty(name);
                // The struct's type parameters are in scope across its method bodies.
                let saved = self.enter_type_params(type_params);
                // A constant-literal field default must be assignable to the field's type (checked
                // here so a wrong-typed default is caught at the declaration, not only when omitted).
                for field in fields {
                    if let Some(def) = &field.default {
                        let expected = self.resolve_type(&field.ty, def.span);
                        let actual = self.infer(def);
                        if !matches!(expected, Ty::Unknown) && !self.assignable(&expected, &actual) {
                            self.error(
                                def.span,
                                format!(
                                    "default value for field '{}': expected {expected}, found {actual}",
                                    field.name
                                ),
                            );
                        }
                    }
                }
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
            | StmtKind::Extern { .. }
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
                    // Loop vars are rebound each iteration → immutable; reassigning one diverges
                    // across engines, so the checker forbids it (see `check_assign`).
                    self.mark_loop_var(&name);
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
            StmtKind::Defer(DeferTarget::Call(e)) => {
                // Block-scoped defer: any indented block — including the module body — is a defer
                // scope, so top-level `defer` is legal (no `in_fn` requirement).
                // `defer` targets a method call or a call to a first-class callable value (a user
                // function/closure, or a name bound to one). Built-ins (`print`, `len`, …) and
                // struct/enum constructors are not first-class values — wrap them in a function.
                match &e.kind {
                    ExprKind::Call { callee, .. } => match &callee.kind {
                        ExprKind::Field { .. } => {} // method call
                        ExprKind::Ident(name)
                            if self.lookup(name).is_none() && !self.functions.contains_key(name) =>
                        {
                            self.error(
                                e.span,
                                "defer requires a function or method call (built-ins and \
                                 constructors must be wrapped in a function)",
                            );
                        }
                        _ => {} // a name bound to a callable, or an arbitrary value-producing callee
                    },
                    _ => self.error(e.span, "defer requires a function or method call"),
                }
                // Type-check the call (and its args); the result is discarded, like an expr stmt.
                self.infer(e);
            }
            StmtKind::Defer(DeferTarget::Block(body)) => {
                // `defer:` block — an ordinary nested scope checked in place. Unlike a `spawn:` block
                // it runs in the same task (no thread airlock), so we push NO `capture_floor`: reads
                // of enclosing locals (even non-sendable ones) are fine. We DO push a `defer_floor`
                // so the reassign gate rejects writing back through the by-value snapshot — neither
                // engine can do that (VM has no `SetCaptured`; the interp would write a discarded
                // copy), so allowing it would crash the VM and silently no-op the interp.
                let floor = self.scopes.len();
                self.defer_floors.push(floor);
                self.check_block(body);
                self.defer_floors.pop();
            }
            StmtKind::Parallel { body } => {
                self.push_scope();
                for stmt in body {
                    self.check_stmt(stmt);
                }
                self.pop_scope();
            }
            StmtKind::Spawn(target) => {
                // M-C implicit nurseries: a bare `spawn` is legal anywhere in a function body and at
                // the module top level — every function body (and the module top level) is an
                // implicit nursery that joins at its `return`/end, so there is no longer a
                // nursery-depth gate. The function-boundary rule (a task can't outlive the function
                // that spawned it) is enforced at runtime by the per-function implicit nursery.
                match target {
                    SpawnTarget::Call(e) => {
                        // `spawn` targets a method call or a call to a first-class callable (a user
                        // function/closure, or a name bound to one). Built-ins (`print`, `len`, …)
                        // and struct/enum constructors are not first-class values — wrap them in a
                        // function. Mirrors `defer`'s guard so the two features agree.
                        if let ExprKind::Call { callee, .. } = &e.kind {
                            match &callee.kind {
                                ExprKind::Field { .. } => {} // method call
                                ExprKind::Ident(name)
                                    if self.lookup(name).is_none()
                                        && !self.functions.contains_key(name) =>
                                {
                                    self.error(
                                        e.span,
                                        "spawn requires a function or method call (built-ins and \
                                         constructors must be wrapped in a function)",
                                    );
                                }
                                _ => {}
                            }
                        }
                        // Full type-check of the call (callee, arity, args) — the single source of
                        // type diagnostics for the sub-expressions.
                        self.infer(e);
                        // Every value crossing the airlock must be sendable: the arguments, and
                        // (for a method spawn) the receiver the task talks through. Re-inferring
                        // here would duplicate the type errors `infer(e)` already reported, so we
                        // truncate any errors this re-inference adds and keep only the sendability
                        // diagnostics.
                        if let ExprKind::Call { callee, args, .. } = &e.kind {
                            let checkpoint = self.errors.len();
                            let mut bad: Vec<(Span, String)> = Vec::new();
                            if let ExprKind::Field { obj, .. } = &callee.kind {
                                let rty = self.infer(obj);
                                if !self.sendable(&rty) {
                                    bad.push((
                                        obj.span,
                                        format!("cannot spawn on a non-sendable receiver of type {rty}"),
                                    ));
                                }
                            }
                            for arg in args {
                                let aty = self.infer(arg);
                                if !self.sendable(&aty) {
                                    bad.push((
                                        arg.span,
                                        format!("cannot pass a non-sendable value of type {aty} to a spawned task"),
                                    ));
                                }
                            }
                            self.errors.truncate(checkpoint);
                            for (sp, msg) in bad {
                                self.error(sp, msg);
                            }
                        }
                    }
                    SpawnTarget::Block(body) => {
                        // Bindings visible now are captured by the task and are read-only inside
                        // it (the airlock); bindings the body declares (at this floor or deeper)
                        // are task-local. `enter`/`leave` is balanced even if checking errors.
                        let floor = self.scopes.len();
                        self.capture_floors.push(floor);
                        self.push_scope();
                        for stmt in body {
                            self.check_stmt(stmt);
                        }
                        self.pop_scope();
                        self.capture_floors.pop();
                    }
                }
            }
            StmtKind::Wait { arms, else_block } => self.check_wait(arms, else_block.as_ref()),
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
                if self.is_loop_var(name) {
                    self.error(
                        target.span,
                        format!("cannot assign to loop variable '{name}' (loop variables are rebound each iteration)"),
                    );
                    return;
                }
                if self.is_captured(name) {
                    self.error(
                        target.span,
                        format!("cannot reassign captured binding '{name}' inside a spawned task (captures are read-only — communicate via a Channel or Shared)"),
                    );
                    return;
                }
                if self.is_defer_captured(name) {
                    self.error(
                        target.span,
                        format!("cannot reassign captured binding '{name}' inside a defer: block (the block captures its free variables by value at the defer point; declare a new binding with ':=' instead)"),
                    );
                    return;
                }
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
                    // A bounded `[C: IndexSet[K, V]]` type parameter is index-assignable in the body.
                    Ty::Param(name) => {
                        if let Some((k, v)) = self.param_indexset_kv(&name, target.span) {
                            let idx_ty = self.infer(index);
                            if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                                self.error(index.span, format!("index must be {k}, found {idx_ty}"));
                            }
                            self.check_assign_value(&v, op, &val_ty, target.span);
                        } else {
                            self.error(target.span, format!("cannot index-assign into {name}"));
                        }
                    }
                    other => {
                        // A struct satisfying `IndexSet` (has `index` + `set_index`) is mutable by index.
                        if let Some((k, v)) = self.index_set_kv(&other) {
                            let idx_ty = self.infer(index);
                            if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                                self.error(index.span, format!("index must be {k}, found {idx_ty}"));
                            }
                            self.check_assign_value(&v, op, &val_ty, target.span);
                        } else {
                            self.expect_int(index, "index");
                            self.error(target.span, format!("cannot index-assign into {other}"));
                        }
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
        // A nested fn opens a fresh `?`-target context: a `?` in this body targets this function,
        // not an enclosing recover at the definition site.
        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
        self.push_scope();
        for (i, param) in decl.params.iter().enumerate() {
            let ty = if param.name == "self" {
                self_ty.clone().unwrap_or(Ty::Unknown)
            } else {
                sig.params.get(i).cloned().unwrap_or(Ty::Unknown)
            };
            // A constant-literal default must itself be assignable to the parameter's type — checked
            // here (where type params are in scope) so a wrong-typed default is caught at the
            // declaration even when every call overrides it.
            if let Some(def) = &param.default {
                let actual = self.infer(def);
                if !matches!(ty, Ty::Unknown) && !self.assignable(&ty, &actual) {
                    self.error(
                        def.span,
                        format!(
                            "default value for parameter '{}': expected {ty}, found {actual}",
                            param.name
                        ),
                    );
                }
            }
            self.declare(&param.name, ty);
        }
        for stmt in &decl.body {
            self.check_stmt(stmt);
        }
        self.pop_scope();
        self.current_ret = saved_ret;
        self.inferring_ret = saved_inferring;
        self.loop_depth = saved_loop_depth;
        self.recover_depth = saved_recover;
        self.exit_type_params(saved_tps);
    }

    /// The element type produced by iterating `iter` in a `for` loop.
    /// The per-iteration bindings of a `for` loop: one name for the common form, or two
    /// (`for k, v in m:`) to destructure a map's entries. A range/list/str binds a single value; a
    /// map binds its key (1 name) or key+value (2 names). Any other arity/iterand combination is an
    /// error (a dummy `Unknown` binding is returned per name so checking continues).
    /// If `ty` is a user struct with a method `next(self) -> Option[E]` (self-only, no extra params),
    /// return the element type `E` (with the struct's type arguments substituted in). This is the
    /// structural "iterator protocol": such a struct is iterable in a `for`. Mirrors the type-arg
    /// substitution `infer_method_call` does for the `Ty::Struct` arm.
    fn struct_iter_elem(&self, ty: &Ty) -> Option<Ty> {
        let Ty::Struct(name, targs) = ty else { return None };
        let info = self.structs.get(name)?;
        let sig = info.methods.get("next")?;
        if sig.params.len() != 1 {
            return None; // (self) only — no extra args
        }
        let Ty::Option(inner) = &sig.ret else { return None };
        let map = struct_param_map(info, targs);
        Some(subst(inner, &map))
    }

    /// What iterating `ty` yields per step — the `Iterator` element type. Built-in collections yield
    /// intrinsically (list/set → element, str → str, map → key, matching the single-variable `for`);
    /// a user struct yields via its structural `next(self) -> Option[E]`. `None` ⇒ not iterable. This
    /// is the single source of truth shared by `for`-binding, `satisfies(Iterator)`, and the
    /// `Iterator[T]` element-recovery in `infer_generic_call`.
    fn iter_elem(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::List(e) | Ty::Set(e) => Some((**e).clone()),
            Ty::Str => Some(Ty::Str),
            Ty::Map(k, _) => Some((**k).clone()),
            _ => self.struct_iter_elem(ty),
        }
    }

    /// The `(key, value)` types of `obj[k]` — the `Index` protocol's args. Built-in collections
    /// intrinsically (list/str index by int, map by its key); a user struct via its structural
    /// `index(self, K) -> V`. Single source of truth for `Index` conformance, `infer_index`, and the
    /// `Index[K,V]` arg-recovery in generic calls. `None` ⇒ not indexable.
    fn index_kv(&self, ty: &Ty) -> Option<(Ty, Ty)> {
        match ty {
            Ty::List(e) => Some((Ty::Int, (**e).clone())),
            Ty::Str => Some((Ty::Int, Ty::Str)),
            Ty::Map(k, v) => Some(((**k).clone(), (**v).clone())),
            Ty::Struct(name, targs) => {
                let info = self.structs.get(name)?;
                let sig = info.methods.get("index")?;
                if sig.params.len() != 2 {
                    return None; // (self, key)
                }
                let map = struct_param_map(info, targs);
                Some((subst(&sig.params[1], &map), subst(&sig.ret, &map)))
            }
            _ => None,
        }
    }

    /// The `(key, value)` types of a mutable `obj[k] = v` — the `IndexSet` protocol's args. Built-in
    /// `list`/`map` are mutable intrinsically (handled directly in `check_assign`); this resolves the
    /// struct case via `set_index(self, K, V)`. `IndexSet` *requires* `index` too (Rust `IndexMut: Index`):
    /// a plain `=` only calls `set_index`, but a compound `b[k] += v` reads via `index` first, so a
    /// struct missing `index` would type-check then crash. `None` ⇒ not index-assignable.
    fn index_set_kv(&self, ty: &Ty) -> Option<(Ty, Ty)> {
        let Ty::Struct(name, targs) = ty else { return None };
        let info = self.structs.get(name)?;
        let sig = info.methods.get("set_index")?;
        if sig.params.len() != 3 {
            return None; // (self, key, val)
        }
        // Must also be readable — `index(self, key) -> val` — or compound index-assign would crash.
        let read = info.methods.get("index")?;
        if read.params.len() != 2 {
            return None; // (self, key)
        }
        let map = struct_param_map(info, targs);
        Some((subst(&sig.params[1], &map), subst(&sig.params[2], &map)))
    }

    /// The result type of `obj[a..b]` — the `Slice` protocol's arg. `list[T] → list[T]`, `str → str`;
    /// a user struct via `slice(self, int, int) -> R`. `None` ⇒ not sliceable.
    fn slice_result(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::List(_) | Ty::Str => Some(ty.clone()),
            Ty::Struct(name, targs) => {
                let info = self.structs.get(name)?;
                let sig = info.methods.get("slice")?;
                // The `Slice` protocol fixes the bounds: `slice(self, int, int) -> R`. Both engines
                // pass int start/end, so a non-conforming signature (wrong arity or non-int bounds)
                // is not a valid `Slice` impl — reject rather than green-light a runtime crash.
                if sig.params.len() != 3 || sig.params[1] != Ty::Int || sig.params[2] != Ty::Int {
                    return None;
                }
                let map = struct_param_map(info, targs);
                Some(subst(&sig.ret, &map))
            }
            _ => None,
        }
    }

    /// Resolve a bound's type argument (the `T` in `Iterator[T]`) to a `Ty` with the *callee's* type
    /// parameters in scope, so a bare param name becomes `Ty::Param` even at a call site where those
    /// params aren't otherwise visible. Restores the prior scope before returning.
    fn resolve_bound_arg(&mut self, arg: &Type, tps: &[TypeParam], span: Span) -> Ty {
        let saved = self.enter_type_params(tps);
        let ty = self.resolve_type(arg, span);
        self.exit_type_params(saved);
        ty
    }

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
            // Tuple-destructuring `for`: over a `list[(A, B, …)]` with N>1 names, bind each name to
            // the matching tuple element. One name still binds the whole tuple (the `Ty::List` arm
            // below). A list of non-tuples (or an arity mismatch) with N>1 names is an error.
            Ty::List(inner) if vars.len() > 1 => match &**inner {
                Ty::Tuple(ts) if ts.len() == vars.len() => {
                    vars.iter().cloned().zip(ts.iter().cloned()).collect()
                }
                Ty::Tuple(ts) => {
                    self.error(iter.span, format!(
                        "tuple-destructuring `for` binds {} names but the element has {} ({inner})",
                        vars.len(), ts.len()
                    ));
                    unknowns(vars)
                }
                Ty::Unknown => unknowns(vars),
                _ => {
                    self.error(iter.span, format!("`for k, v` requires a map or a list of tuples, found {it}"));
                    unknowns(vars)
                }
            },
            Ty::Str | Ty::Set(_) | Ty::Channel(_) if vars.len() != 1 => {
                if matches!(it, Ty::Channel(_)) {
                    self.error(iter.span, "a channel iterator binds a single loop variable");
                } else {
                    self.error(iter.span, format!("`for k, v` requires a map, found {it}"));
                }
                unknowns(vars)
            }
            Ty::List(inner) => vec![(vars[0].clone(), (**inner).clone())],
            Ty::Set(elem) => vec![(vars[0].clone(), (**elem).clone())],
            Ty::Str => vec![(vars[0].clone(), Ty::Str)],
            // `for v in ch:` over a `Channel[T]` blocks for each value and ends when the channel is
            // closed-and-drained (Go's `for v := range ch`). Binds a single element of type `T`.
            Ty::Channel(elem) => vec![(vars[0].clone(), (**elem).clone())],
            Ty::Unknown => unknowns(vars),
            Ty::Param(name) => {
                // A type parameter bounded `S: Iterator[T]` is iterable; bind the loop var to its
                // declared element type `T` (resolved with the surrounding params in scope).
                let arg = self.type_params.get(name).and_then(|bs| {
                    bs.iter().find(|b| b.name == "Iterator").and_then(|b| b.args.first().cloned())
                });
                match arg {
                    Some(_) if vars.len() != 1 => {
                        self.error(iter.span, "a struct iterator binds a single loop variable");
                        unknowns(vars)
                    }
                    Some(t) => vec![(vars[0].clone(), self.resolve_type(&t, iter.span))],
                    None => {
                        self.error(iter.span, format!("cannot iterate over {it}"));
                        unknowns(vars)
                    }
                }
            }
            _ if self.struct_iter_elem(&it).is_some() => {
                // A user struct with `next(self) -> Option[E]` is iterable; it binds a single element.
                let elem = self.struct_iter_elem(&it).expect("guarded by the match arm");
                if vars.len() != 1 {
                    self.error(iter.span, "a struct iterator binds a single loop variable");
                    return unknowns(vars);
                }
                vec![(vars[0].clone(), elem)]
            }
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
                // A nested bare identifier names either a nullary variant of the matched type (a
                // refutable variant match — `Some(None)`, `Ok(Err(e))`) or a fresh binding.
                if let Some(vmap) = self.variants_of(ty)
                    && let Some(payload) = vmap.get(name)
                {
                    if payload.is_empty() {
                        // A nullary variant of `ty`: a refutable variant match, binds nothing.
                        return false;
                    }
                    // A non-nullary variant used without its payload — needs `Name(...)`.
                    self.error(
                        span,
                        format!("variant '{name}' of {ty} requires its payload — write '{name}(...)'"),
                    );
                    return false;
                }
                // A globally-known variant name that ISN'T a variant of `ty` cannot be a binding: the
                // compiler routes it by the variant registry (a `MatchArm` test), so it would trap on
                // the VM while the interp binds. Reject it so all engines agree (rename to bind).
                if !ty.is_unknown()
                    && (self.variants.contains_key(name)
                        || matches!(name.as_str(), "Ok" | "Err" | "Some" | "None"))
                {
                    self.error(span, format!("'{name}' is not a variant of {ty}"));
                    return false;
                }
                self.declare(name, ty.clone());
                true
            }
            Pattern::Or(alts) => self.bind_or_alternatives(alts, ty, span),
            Pattern::Literal(lit) => {
                let lit_ty = lit_pattern_ty(lit);
                if !ty.is_unknown() && &lit_ty != ty {
                    self.error(span, format!("literal of type {lit_ty} cannot match a value of type {ty}"));
                }
                false
            }
            Pattern::Range { .. } => {
                // A range sub-pattern is int-only and always refutable.
                if !ty.is_unknown() && ty != &Ty::Int {
                    self.error(span, format!("range pattern cannot match a value of type {ty}"));
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

    /// Bind the alternatives of an or-pattern in a *sub-pattern* position against `ty`, enforcing
    /// that every alternative binds the EXACT same set of names with unifiable types, then declaring
    /// the agreed set once into the current scope. Returns `true` iff ANY alternative is
    /// irrefutable. Bounded by the finite pattern tree (recursion only descends sub-patterns).
    fn bind_or_alternatives(&mut self, alts: &[Pattern], ty: &Ty, span: Span) -> bool {
        // An or-pattern is irrefutable iff ANY alternative is irrefutable (one alt that always
        // matches makes the whole or-pattern always match) — OR, not AND.
        let mut irref = false;
        let mut binders: Vec<(usize, std::collections::BTreeMap<String, Ty>)> = Vec::new();
        for (i, alt) in alts.iter().enumerate() {
            self.push_scope();
            let alt_irref = self.bind_subpattern(alt, ty, span);
            irref |= alt_irref;
            // Snapshot the names this alternative introduced (its scratch scope's top frame).
            let snap: std::collections::BTreeMap<String, Ty> =
                self.scopes.last().cloned().unwrap_or_default().into_iter().collect();
            self.pop_scope();
            binders.push((i, snap));
        }
        self.enforce_or_consistency(&binders, span);
        irref
    }

    /// Enforce that all alternatives' binder snapshots agree on the bound-name set + unifiable types,
    /// then declare the agreed names once into the current (real) scope. `binders[0]` is the
    /// reference set; mismatches are reported once, clearly, and the first set is still declared so
    /// the arm body type-checks (no cascading "unknown name" errors).
    fn enforce_or_consistency(
        &mut self,
        binders: &[(usize, std::collections::BTreeMap<String, Ty>)],
        span: Span,
    ) {
        if binders.is_empty() {
            return;
        }
        let (_, first) = &binders[0];
        for (_, other) in &binders[1..] {
            if first.keys().ne(other.keys()) {
                let left: Vec<&str> = first.keys().map(|s| s.as_str()).collect();
                let right: Vec<&str> = other.keys().map(|s| s.as_str()).collect();
                self.error(
                    span,
                    format!(
                        "or-pattern alternatives must bind the same variables: left binds {{{}}}, right binds {{{}}}",
                        left.join(", "),
                        right.join(", "),
                    ),
                );
                break;
            }
            // Same key set — check per-name type compatibility (in either direction).
            for (name, lt) in first.iter() {
                if let Some(rt) = other.get(name)
                    && !compatible(lt, rt)
                    && !compatible(rt, lt)
                {
                    self.error(
                        span,
                        format!("or-pattern binds '{name}' as {lt} in one alternative and {rt} in another"),
                    );
                }
            }
        }
        // Declare the agreed set once into the real scope.
        for (name, ty) in first.iter() {
            self.declare(name, ty.clone());
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
        // An or-pattern at the top of an arm: bind each alternative into a scratch scope (threading
        // coverage so `Red | Green | Blue` closes the variant domain), enforce that all alternatives
        // bind the same names with unifiable types, then declare the agreed set into the arm scope.
        // Irrefutable iff ANY alternative is (e.g. `1 | _` is irrefutable via `_`). OR, not AND.
        if let Pattern::Or(alts) = pattern {
            self.push_scope(); // the arm scope the caller pops
            let mut irref = false;
            let mut binders: Vec<(usize, std::collections::BTreeMap<String, Ty>)> = Vec::new();
            for (i, alt) in alts.iter().enumerate() {
                // Recurse: this pushes a scratch scope, threads `covered`, binds the alternative.
                let alt_irref = self.bind_match_arm(alt, kind, span, covered);
                let snap: std::collections::BTreeMap<String, Ty> =
                    self.scopes.last().cloned().unwrap_or_default().into_iter().collect();
                self.pop_scope(); // discard the scratch scope (we re-declare into the arm scope)
                irref |= alt_irref;
                binders.push((i, snap));
            }
            self.enforce_or_consistency(&binders, span);
            return irref;
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
                    Pattern::Range { .. } => self.error(span, format!("cannot match a range against {label}")),
                    Pattern::Tuple(_) => self.error(span, format!("cannot match a tuple against {label}")),
                    Pattern::Ident(_) | Pattern::Wildcard | Pattern::Or(_) => {
                        unreachable!("ident/wildcard/or handled elsewhere")
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
                    Pattern::Range { .. } => {
                        // A range pattern is int-only; reject against str/bool scrutinees.
                        if ty != &Ty::Int {
                            self.error(span, format!("range pattern cannot match scrutinee of type {ty}"));
                        }
                    }
                    // int/str/bool have no nullary variants, so a bare top-level identifier here is a
                    // binding capturing the whole scrutinee value (irrefutable catch-all). The parser
                    // emits it as `Variant { bindings: [] }`; reinterpret it as a binding — UNLESS the
                    // name is a registered variant (e.g. `None`). The compiler routes by the variant
                    // registry, so a colliding name would bind in the interp but trap on the VM; reject
                    // it here so all engines agree. (Rename the binding to fix.)
                    Pattern::Variant { name, bindings } if bindings.is_empty() => {
                        // Match the compiler's variant registry: user enums PLUS the built-in
                        // Result/Option variants (which the checker special-cases elsewhere).
                        if self.variants.contains_key(name)
                            || matches!(name.as_str(), "Ok" | "Err" | "Some" | "None")
                        {
                            self.error(
                                span,
                                format!(
                                    "'{name}' is a variant name and cannot bind a scrutinee of type {ty}; rename the binding"
                                ),
                            );
                            return false;
                        }
                        self.declare(name, ty.clone());
                        return true;
                    }
                    Pattern::Variant { bindings, .. } => {
                        self.error(span, format!("cannot match a variant against {ty}"));
                        // Still bind the payload sub-patterns (as Unknown) so the arm body doesn't
                        // cascade into spurious "unknown name" errors — notably the desugared `?.`
                        // case, where the payload binding is an internal `__opt` temp the user can't
                        // see. (The `cannot match` error already flags the real problem.)
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                    Pattern::Tuple(_) => self.error(span, format!("cannot match a tuple against {ty}")),
                    Pattern::Ident(_) | Pattern::Wildcard | Pattern::Or(_) => {
                        unreachable!("ident/wildcard/or handled elsewhere")
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

    /// `wait:` — Chezzi's `select` (§6d). Each arm's channel expr must be a `Channel[T]`; the arm's
    /// target binds (`:=`)/assigns (`=`)/discards (`_`) the element `T`. `wait` is a runtime race, not
    /// a type match, so it is **not** exhaustive — no coverage analysis, ≥1 arm is the only structural
    /// rule (parser-enforced). Each arm body is its own lexical sub-scope (like a `match` arm).
    fn check_wait(&mut self, arms: &[WaitArm], else_block: Option<&Block>) {
        for arm in arms {
            let elem = match self.infer(&arm.chan) {
                Ty::Channel(e) => *e,
                Ty::Unknown => Ty::Unknown,
                other => {
                    self.error(
                        arm.chan.span,
                        format!("a wait arm must recv from a Channel, found {other}"),
                    );
                    Ty::Unknown
                }
            };
            self.push_scope();
            match &arm.target {
                WaitTarget::Bind(name) => self.declare(name, elem),
                // `=` assigns an existing outer lvalue — reuse the ordinary assignment checks
                // (assignability, type match, read-only/loop-var gates).
                WaitTarget::Assign(target) => self.check_assign(target, AssignOp::Eq, elem, arm.span),
                WaitTarget::Discard => {}
            }
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        if let Some(b) = else_block {
            self.check_block(b);
        }
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchArm]) {
        let kind = self.match_kind(scrutinee);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        for arm in arms {
            let irref = self.bind_match_arm(&arm.pattern, &kind, scrutinee.span, &mut covered);
            // The guard is type-checked with the arm's bindings in scope. A guarded arm is never
            // irrefutable — its guard may fail at runtime — so it can't make the match exhaustive.
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
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
            let irref = self.bind_match_arm(&arm.pattern, &kind, scrutinee.span, &mut covered);
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
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
            ExprKind::Comprehension { kind, key, elem, vars, iter, guard } => {
                self.infer_comprehension(*kind, key.as_deref(), elem, vars, iter, guard.as_deref())
            }
            ExprKind::Unary { op, expr: inner } => self.infer_unary(*op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs),
            ExprKind::Slice { obj, start, end } => self.infer_slice(obj, start, end, expr.span),
            ExprKind::Range { start, end } => {
                self.expect_int(start, "range bound");
                self.expect_int(end, "range bound");
                Ty::list(Ty::Int)
            }
            ExprKind::Call { callee, args, type_args, .. } => {
                self.infer_call(callee, args, type_args, expr.span)
            }
            ExprKind::Field { obj, name } => self.infer_field(obj, name),
            ExprKind::Index { obj, index } => self.infer_index(obj, index),
            ExprKind::Try(inner) => self.infer_try(inner, expr.span),
            // Optional-chaining `?.` / null-coalescing `??` are carrier nodes lowered to `match` by
            // the desugar pass (`resolver::build_graph` → `desugar::run`), which always runs before
            // the checker. Reaching here means the pipeline skipped desugar — an internal invariant
            // break, not a user error.
            ExprKind::OptChain { .. } | ExprKind::NullCoalesce { .. } => {
                unreachable!("`?.`/`??` must be lowered by the desugar pass before checking")
            }
            ExprKind::DecodeCall { obj, ty, arg } => self.infer_decode(obj, ty, arg, expr.span),
            ExprKind::Closure { params, ret, body } => self.infer_closure(params, ret.as_ref(), body),
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms),
            ExprKind::IfElse { cond, then, els } => self.infer_if_else(cond, then, els),
            ExprKind::Recover(block) => self.infer_recover(block),
        }
    }

    /// `recover: <block>` yields `Result[T, Error]` where `T` is the type of the block's trailing
    /// expression (or `nil`). Non-final statements are checked for their effects.
    fn infer_recover(&mut self, block: &Block) -> Ty {
        // A `recover:` block is a value, not a control-flow target: `return`/`break`/`continue` that
        // would escape it are rejected (both engines agree). `?` is fine — it propagates normally.
        if let Some((span, kw)) = recover_escaping_flow(block, false) {
            self.error(span, format!("'{kw}' is not allowed inside a recover block"));
        }
        self.push_scope();
        self.recover_depth += 1;
        let mut value_ty = Ty::Nil;
        if let Some((last, init)) = block.split_last() {
            for stmt in init {
                self.check_stmt(stmt);
            }
            match &last.kind {
                StmtKind::Expr(e) => value_ty = self.infer(e),
                _ => self.check_stmt(last),
            }
        }
        self.recover_depth -= 1;
        self.pop_scope();
        Ty::result(value_ty)
    }

    fn infer_ident(&mut self, name: &str, span: Span) -> Ty {
        if let Some(ty) = self.lookup(name) {
            // A function-local binding captured by an enclosing `spawn:` task crosses the airlock as
            // a copy; a *non-sendable* one (e.g. a captured closure that's then called) can't, so
            // reading it inside the task is an error — the read-side counterpart to the reassignment
            // gate. Module globals/imports are excluded (`is_local_capture`): they resolve in every
            // task like free functions, so reading an imported module here is fine.
            if self.is_local_capture(name) && !self.sendable(&ty) {
                self.error(
                    span,
                    format!(
                        "cannot use non-sendable captured binding '{name}' of type {ty} inside a \
                         spawned task (captures cross the airlock — communicate via a Channel or Shared)"
                    ),
                );
            }
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

    /// Infer a comprehension's type. Binds the loop variable(s) to the iterand's element type(s)
    /// via `for_bindings` (the exact path a `for` loop uses, so every iterable behaves the same),
    /// checks the optional guard is `Bool`, then infers the element (and key) in that scope. The
    /// result mirrors `infer_list`/`infer_set`/`infer_map`, including the Hashable check on set
    /// elements and map keys.
    fn infer_comprehension(
        &mut self,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        vars: &[String],
        iter: &Expr,
        guard: Option<&Expr>,
    ) -> Ty {
        let bindings = self.for_bindings(vars, iter);
        // A comprehension materializes eagerly, but a `Channel` is a blocking iteration form whose
        // termination depends on `close()`. Draining it into a list/set/map is out of scope and would
        // DIVERGE between engines (the VM's `compile_comprehension` reuses the channel-aware
        // `compile_for`, but the interp oracle's comprehension path can't iterate a channel). Reject on
        // both engines instead — the `for v in ch:` statement form is the way to drain a channel.
        if matches!(self.infer(iter), Ty::Channel(_)) {
            self.error(
                iter.span,
                "a channel cannot be drained in a comprehension; use the `for v in ch:` statement form",
            );
        }
        self.push_scope();
        for (name, ty) in bindings {
            // Intentionally NOT `mark_loop_var`: a comprehension body is an expression, so its
            // binding can't be assigned to — no divergence to guard against. If a statement-bearing
            // comprehension is ever added, mark these too (see `check_assign` / for-loop handling).
            self.declare(&name, ty);
        }
        if let Some(g) = guard {
            self.expect_bool(g, "comprehension guard");
        }
        let result = match kind {
            CompKind::List => Ty::list(self.infer(elem)),
            CompKind::Set => {
                let et = self.infer(elem);
                if !et.is_unknown() && !self.is_hashable_key(&et) {
                    self.error(
                        elem.span,
                        format!("set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {et}"),
                    );
                }
                Ty::set(et)
            }
            CompKind::Map => {
                let key = key.expect("a map comprehension always carries a key expression");
                let kt = self.infer(key);
                let vt = self.infer(elem);
                if !kt.is_unknown() && !self.is_hashable_key(&kt) {
                    self.error(
                        key.span,
                        format!("map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {kt}"),
                    );
                }
                Ty::map(kt, vt)
            }
        };
        self.pop_scope();
        result
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
            // A bounded `[C: Index[K, V]]` type parameter is indexable inside the generic body; its
            // value type is the bound's `V` arg (resolved with sibling params in scope).
            Ty::Param(name) => {
                if let Some((k, v)) = self.param_index_kv(&name, obj.span) {
                    let idx_ty = self.infer(index);
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        self.error(index.span, format!("index must be {k}, found {idx_ty}"));
                    }
                    return v;
                }
                self.expect_int(index, "index");
                self.error(obj.span, format!("cannot index into {name}"));
                Ty::Unknown
            }
            other => {
                // A user struct satisfying `Index` (has `index(self, K) -> V`) is indexable by `K`.
                if let Some((k, v)) = self.index_kv(&other) {
                    let idx_ty = self.infer(index);
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        self.error(index.span, format!("index must be {k}, found {idx_ty}"));
                    }
                    return v;
                }
                self.expect_int(index, "index");
                self.error(obj.span, format!("cannot index into {other}"));
                Ty::Unknown
            }
        }
    }

    /// The `(K, V)` of a bounded type parameter's `Index`/`IndexSet` bound, resolved with the
    /// surrounding params in scope. `None` ⇒ the param has no indexing bound.
    fn param_index_kv(&mut self, name: &str, span: Span) -> Option<(Ty, Ty)> {
        let bound = self
            .type_params
            .get(name)?
            .iter()
            .find(|b| matches!(b.name.as_str(), "Index" | "IndexSet"))
            .cloned()?;
        let k = bound.args.first().map(|a| self.resolve_type(a, span)).unwrap_or(Ty::Unknown);
        let v = bound.args.get(1).map(|a| self.resolve_type(a, span)).unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// The `(K, V)` of a bounded type parameter's `IndexSet` bound (write requires `IndexSet`
    /// specifically — a read-only `Index` bound is not assignable). `None` ⇒ no `IndexSet` bound.
    fn param_indexset_kv(&mut self, name: &str, span: Span) -> Option<(Ty, Ty)> {
        let bound = self.type_params.get(name)?.iter().find(|b| b.name == "IndexSet").cloned()?;
        let k = bound.args.first().map(|a| self.resolve_type(a, span)).unwrap_or(Ty::Unknown);
        let v = bound.args.get(1).map(|a| self.resolve_type(a, span)).unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// Type `obj[start..end]`. Bounds must be `int`; the result type follows the `Slice` protocol —
    /// `list[T] → list[T]`, `str → str`, or a struct's `slice(self, int, int) -> R`.
    fn infer_slice(&mut self, obj: &Expr, start: &Expr, end: &Expr, span: Span) -> Ty {
        self.expect_int(start, "slice bound");
        self.expect_int(end, "slice bound");
        let obj_ty = self.infer(obj);
        if obj_ty.is_unknown() {
            return Ty::Unknown;
        }
        // A bounded `[C: Slice[R]]` type parameter is sliceable inside the generic body; its result
        // type is the bound's `R` arg (resolved with sibling params in scope).
        if let Ty::Param(name) = &obj_ty
            && let Some(bound) = self
                .type_params
                .get(name)
                .and_then(|bs| bs.iter().find(|b| b.name == "Slice").cloned())
        {
            return bound.args.first().map(|a| self.resolve_type(a, span)).unwrap_or(Ty::Unknown);
        }
        match self.slice_result(&obj_ty) {
            Some(r) => r,
            None => {
                self.error(span, format!("cannot slice {obj_ty}"));
                Ty::Unknown
            }
        }
    }

    fn infer_try(&mut self, inner: &Expr, span: Span) -> Ty {
        let t = self.infer(inner);
        // Inside a `recover:` block, `?` short-circuits to the boundary (try-block style), not the
        // enclosing function. The boundary's error type is `Error`, and its result is `Result`-typed,
        // so only a `Result` operand fits — `?` on an `Option` is rejected here.
        if self.recover_depth > 0 {
            return match t {
                Ty::Result(ok, err) => {
                    if !self.assignable(&Ty::error_proto(), &err) {
                        self.error(
                            span,
                            format!("'?' inside a recover block propagates error {err}, which must satisfy Error"),
                        );
                    }
                    *ok
                }
                Ty::Unknown => Ty::Unknown,
                Ty::Option(_) => {
                    self.error(span, "'?' on an Option is not allowed inside a recover block (its result is Result-typed); use match instead".to_string());
                    Ty::Unknown
                }
                other => {
                    self.error(span, format!("'?' expects Result or Option, found {other}"));
                    Ty::Unknown
                }
            };
        }
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
        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
        // `?` inside the body targets THIS closure's return, not the enclosing function's. With no
        // annotation there is no Result/Option context, so `?` is rejected (`Unknown` → `infer_try`
        // errors). Mirrors `check_fn_body`'s `current_ret` handling.
        let declared_ret = ret.map(|t| self.resolve_type(t, body.span)).unwrap_or(Ty::Unknown);
        let saved_ret = std::mem::replace(&mut self.current_ret, declared_ret);
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
        self.recover_depth = saved_recover;
        self.current_ret = saved_ret;
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

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], type_args: &[Type], span: Span) -> Ty {
        // Explicit call-site type arguments `name[T, …](…)`. Resolved once here; only generic
        // by-name calls (fn / struct / variant constructors) can consume them.
        let targs: Vec<Ty> = type_args.iter().map(|t| self.resolve_type(t, span)).collect();
        // Method call: `obj.method(args)`. The parser never attaches type args to a method callee.
        if let ExprKind::Field { obj, name } = &callee.kind {
            return self.infer_method_call(obj, name, args, span);
        }
        if let ExprKind::Ident(name) = &callee.kind {
            // Shadowing local (e.g. a closure bound to a variable) wins over a global of the same name.
            if self.lookup(name).is_none()
                && let Some(ty) = self.infer_named_call(name, args, &targs, span)
            {
                return ty;
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
    fn infer_named_call(&mut self, name: &str, args: &[Expr], targs: &[Ty], span: Span) -> Option<Ty> {
        // Explicit call-site type arguments are only meaningful on a *generic* user fn / struct /
        // enum-variant constructor. Reject them on anything else (builtins, non-generic decls)
        // before the dispatch below, so the seeding logic only has to handle the generic paths.
        if !targs.is_empty() && !self.name_is_generic(name) {
            self.error(span, format!("'{name}' takes no type arguments"));
            for a in args {
                self.infer(a);
            }
            return Some(Ty::Unknown);
        }
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
            "Channel" => {
                // `Channel[T]()` — a fresh empty mailbox. The element type comes from the explicit
                // type argument (it can't be inferred from a no-arg call), and must be sendable.
                self.check_arity("Channel", 0, args, span);
                let elem = match targs {
                    [t] => t.clone(),
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
                    self.error(span, format!("Channel element type must be sendable, found {elem}"));
                }
                Some(Ty::channel(elem))
            }
            "Shared" => {
                // `Shared(v)` — a fresh cross-task box initialised with `v`. The element type is
                // inferred from the value (value-first, unlike `Channel[T]()`); a `[T]` type arg is
                // rejected upstream by the `name_is_generic` gate.
                let elem = self.one_arg("Shared", args, span);
                Some(Ty::shared(elem))
            }
            "Atomic" => {
                // `Atomic(v)` — a fresh cross-task atomic box initialised with `v`. Value-first like
                // `Shared`; a `[T]` type arg is rejected upstream by the `name_is_generic` gate.
                let elem = self.one_arg("Atomic", args, span);
                Some(Ty::atomic(elem))
            }
            "timer" => {
                // `timer(ms)` — a one-shot timeout channel: a `Channel[bool]` that delivers `true`
                // once, `ms` milliseconds after creation. The composable timeout primitive (recv it in
                // a `wait` arm). Takes an int; a `[T]` type arg is rejected upstream.
                self.check_args("timer", &[Ty::Int], args, span);
                Some(Ty::channel(Ty::Bool))
            }
            "Executor" => {
                // `Executor()` — a fresh, empty, explicitly-owned work queue (C5 escape hatch).
                // Non-generic and zero-arg; a `[T]` type arg is rejected upstream.
                self.check_arity("Executor", 0, args, span);
                Some(Ty::Executor)
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
                    // Generic struct: type arguments come from explicit call-site args (`S[int](…)`)
                    // when given, else are inferred by unifying the declared field types (which
                    // contain the struct's `Ty::Param`s) against the argument types.
                    let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
                    if arg_tys.len() != field_tys.len() {
                        self.check_arity(name, field_tys.len(), args, span);
                    }
                    let mut sub = self.seed_targs(name, &tps, targs, span);
                    for (decl, actual) in field_tys.iter().zip(&arg_tys) {
                        unify(decl, actual, &mut sub);
                    }
                    self.recover_iter_elems(&tps, &mut sub, span);
                    for (decl, (actual, arg)) in field_tys.iter().zip(arg_tys.iter().zip(args)) {
                        let expected = subst(decl, &sub);
                        if !self.assignable(&expected, actual) {
                            self.error(
                                arg.span,
                                format!("argument to '{name}' has type {actual}, expected {expected}"),
                            );
                        }
                    }
                    self.enforce_bounds(&tps, &sub, span);
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
                    // Generic enum: type arguments come from explicit call-site args
                    // (`Node[int](…)`) when given, else are inferred by unifying the variant's
                    // declared payload types (which contain the enum's `Ty::Param`s) against the
                    // argument types, then check each argument against the substituted payload.
                    let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
                    if arg_tys.len() != v.payload.len() {
                        self.check_arity(name, v.payload.len(), args, span);
                    }
                    let mut sub = self.seed_targs(name, &tps, targs, span);
                    for (decl, actual) in v.payload.iter().zip(&arg_tys) {
                        unify(decl, actual, &mut sub);
                    }
                    self.recover_iter_elems(&tps, &mut sub, span);
                    for (decl, (actual, arg)) in v.payload.iter().zip(arg_tys.iter().zip(args)) {
                        let expected = subst(decl, &sub);
                        if !self.assignable(&expected, actual) {
                            self.error(
                                arg.span,
                                format!("argument to '{name}' has type {actual}, expected {expected}"),
                            );
                        }
                    }
                    self.enforce_bounds(&tps, &sub, span);
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
                        return Some(self.infer_generic_call(name, &sig, args, targs, span));
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
                        return self.infer_generic_call(method, &fsig, args, &[], span);
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
                        (params, subst(&sig.ret, &map), sig.type_params.clone())
                    })
                });
                if let Some((params, ret, mtps)) = resolved {
                    // A generic method introduces its own type params `[U]` (beyond the struct's
                    // `[T]`, already substituted above). Infer them from the call arguments —
                    // mirrors the free generic-fn path (`infer_generic_call`).
                    if !mtps.is_empty() {
                        return self.infer_generic_method(method, &params, &ret, &mtps, &obj_ty, args, span);
                    }
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
                // No method named `method`: fall back to a function-typed *field* of the same name —
                // `recv.f(x)` where `f: fn(T) -> U` is field-access-then-call. (Parsed as a method
                // call; the desugar pass leaves fn-field names un-normalized so no method default is
                // injected here.) Mirrors `infer_field`'s field lookup + type-arg substitution.
                let field_fn = self.structs.get(sname).and_then(|info| {
                    let map = struct_param_map(info, targs);
                    info.fields
                        .iter()
                        .find(|(f, _)| f == method)
                        .map(|(_, ty)| subst(ty, &map))
                });
                if let Some(Ty::Func { params, ret }) = field_fn {
                    self.check_args(method, &params, args, span);
                    return *ret;
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
                if matches!(method, "map" | "filter" | "fold" | "sort_by" | "sort_by_key") {
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
            Ty::Channel(elem) => {
                // `send(v)` moves `v` across the airlock; `check_args` enforces it matches the
                // element type `T`, which is itself sendable-checked at the channel's construction
                // — so a well-typed `send` is always sendable.
                if let Some(sig) = channel_method_sig(method, elem) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Shared(elem) => {
                // `get()->T`, `set(T)->nil`, `update(fn(T)->T)->nil` — the same box API as `Ref[T]`,
                // but reachable across tasks.
                if let Some(sig) = shared_method_sig(method, elem) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Atomic(elem) => {
                // `load()->T`, `store(T)`, `exchange(T)->T`, `cas(T,T)->bool`; `add(T)->T`/`sub(T)->T`
                // only when `T` is numeric (gated inside `atomic_method_sig`).
                if let Some(sig) = atomic_method_sig(method, elem) {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Executor => {
                // `submit(fn() -> _)->nil`, `shutdown()->nil`, `shutdown_now()->nil` (C5 escape hatch).
                if let Some(sig) = executor_method_sig(method) {
                    // A3b (B3.6): `submit`'s closure runs on a pool thread under `--parallel`, so its
                    // captures cross the airlock exactly like a `spawn` task's. Push a capture floor at
                    // the current scope depth around the argument check; the submitted closure opens
                    // its own scope at that depth, so its params/locals are task-local while any outer
                    // binding it reads is flagged by the `infer_ident` read gate (mirrors `spawn:`).
                    if method == "submit" {
                        self.capture_floors.push(self.scopes.len());
                        self.check_args(method, &sig.params, args, span);
                        self.capture_floors.pop();
                    } else {
                        self.check_args(method, &sig.params, args, span);
                    }
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            // D6 — `Socket` / `Listener` (std.net): a small fixed method set. The runtime parks the
            // fiber on a would-block `read`/`write`/`accept`; from the type system they just return
            // their `Result`.
            Ty::Socket => {
                if let Some(sig) = socket_method_sig(method) {
                    self.check_args_range(method, &sig.params, sig.min_params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Listener => {
                if let Some(sig) = listener_method_sig(method) {
                    self.check_args_range(method, &sig.params, sig.min_params, args, span);
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
                let found = bounds.iter().find_map(|proto| {
                    self.protocols.get(&proto.name).and_then(|p| {
                        p.methods
                            .iter()
                            .find(|(n, _)| n == method)
                            .map(|(_, s)| (proto.clone(), s.clone()))
                    })
                });
                if let Some((proto, msig)) = found {
                    // Map `Self` to the receiver, plus the parameterized protocol's own params to the
                    // bound's concrete args (`Container[int]` ⇒ `T ↦ int`), so a method returning `T`
                    // resolves to `int` in the caller.
                    let mut map = HashMap::from([("Self".to_string(), obj_ty.clone())]);
                    let ptps = self
                        .protocols
                        .get(&proto.name)
                        .map(|p| p.type_params.clone())
                        .unwrap_or_default();
                    for (pname, parg) in ptps.iter().zip(&proto.args) {
                        let resolved = self.resolve_type(parg, span);
                        map.insert(pname.clone(), resolved);
                    }
                    let expected: Vec<Ty> = match msig.params.split_first() {
                        Some((_recv, rest)) => rest.iter().map(|t| subst(t, &map)).collect(),
                        None => Vec::new(),
                    };
                    self.check_args(method, &expected, args, span);
                    // `Iterator[T].next()` yields `Option[T]` — its return is the bound's element arg,
                    // not `Self` (the registered placeholder). Resolve the arg with sibling params in
                    // scope (we're inside the bounded type's own generic context).
                    if proto.name == "Iterator"
                        && method == "next"
                        && let Some(arg) = proto.args.first()
                    {
                        return Ty::Option(Box::new(self.resolve_type(arg, span)));
                    }
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
            // sort_by_key(f: fn(T) -> K) -> nil — sorts in place by a derived key (sugar over
            // sort_by). `K` must be orderable like `sort()`'s element: int/float/str or a struct
            // satisfying `Comparable`. Keys are compared by their natural order at runtime.
            "sort_by_key" => {
                if args.len() != 1 {
                    self.error(
                        span,
                        format!("'sort_by_key' expects 1 argument(s), got {}", args.len()),
                    );
                    self.infer_all(args);
                    return Ty::Nil;
                }
                let ft = self.infer(&args[0]);
                match ft {
                    Ty::Unknown => Ty::Nil,
                    Ty::Func { params, ret }
                        if params.len() == 1 && compatible(&params[0], elem) =>
                    {
                        let key = (*ret).clone();
                        if is_orderable(&key)
                            || key.is_unknown()
                            || self.satisfies(&key, "Comparable").is_ok()
                        {
                            Ty::Nil
                        } else {
                            self.error(
                                args[0].span,
                                format!("sort_by_key key type must be Comparable (int, float, str, or a struct with a `compare` method), found {key}"),
                            );
                            Ty::Nil
                        }
                    }
                    other => {
                        self.error(
                            args[0].span,
                            format!(
                                "sort_by_key expects a key function fn({elem}) -> K, found {other}"
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
        self.check_args_range(name, params, params.len(), args, span);
    }

    /// D6c — `check_args` generalized to an optional trailing tail: the arg count must fall in
    /// `min_params..=params.len()`, and each supplied arg must match its positional param. Used for the
    /// net socket ops whose `timeout_ms` is optional. `min_params == params.len()` reproduces the
    /// exact-arity behavior of [`Checker::check_args`].
    fn check_args_range(&mut self, name: &str, params: &[Ty], min_params: usize, args: &[Expr], span: Span) {
        if !(min_params..=params.len()).contains(&args.len()) {
            let want = if min_params == params.len() {
                format!("{}", params.len())
            } else {
                format!("{min_params}–{}", params.len())
            };
            self.error(span, format!("'{name}' expects {want} argument(s), got {}", args.len()));
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
    fn enter_type_params(&mut self, tps: &[TypeParam]) -> HashMap<String, Vec<Bound>> {
        let saved = self.type_params.clone();
        for tp in tps {
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        saved
    }

    fn exit_type_params(&mut self, saved: HashMap<String, Vec<Bound>>) {
        self.type_params = saved;
    }

    /// Validate the bounds declared on a type parameter: each names a known protocol, and the number
    /// of type args matches the protocol's arity (a parameterized `protocol Container[T]` requires
    /// one; a bare protocol requires none). `Iterator` additionally may appear at most once (its
    /// element recovery can't disambiguate two).
    fn check_bounds(&mut self, bounds: &[Bound], param: &str, span: Span) {
        let mut seen_iterator = false;
        for b in bounds {
            let Some(arity) = self.protocols.get(&b.name).map(|p| p.type_params.len()) else {
                self.error(span, format!("unknown protocol '{}' in bound on '{param}'", b.name));
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

    /// Register a `protocol` declaration's method signatures. `Self` resolves to `Ty::Param("Self")`.
    fn hoist_protocol(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        methods: &[MethodSig],
        span: Span,
    ) {
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
        // The protocol's own type params are in scope while resolving its method signatures, so
        // `fn get(self, i: int) -> T` resolves `T` to `Ty::Param("T")`.
        for tp in type_params {
            if tp.name == "Self" {
                self.error(span, "protocol type parameter cannot be named 'Self'".to_string());
            }
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        for tp in type_params {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
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
        self.protocols.insert(
            name.to_string(),
            ProtocolInfo {
                type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                methods: sigs,
            },
        );
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
        self.satisfies_args(ty, protocol, &[])
    }

    /// Do a declared bound's type args (AST `Type`s) match the `required` ones (resolved `Ty`s) for a
    /// forwarded parameterized bound? Read-only — used inside `satisfies_args`. Conservative: only a
    /// *fully concrete* mismatch is rejected (so a still-generic arg like a sibling type param keeps
    /// forwarding loosely, as before), which is what closes the `Container[str]`→`Container[int]` hole
    /// without breaking valid `[S: Iterator[T], T]` forwards.
    fn bound_args_match(&self, bound_args: &[Type], required: &[Ty]) -> bool {
        if bound_args.len() != required.len() {
            return false;
        }
        bound_args.iter().zip(required).all(|(ba, want)| {
            let bt = self.resolve_ty_ro(ba);
            !ty_fully_concrete(&bt) || !ty_fully_concrete(want) || compatible(&bt, want)
        })
    }

    /// Read-only type resolution (no error emission), for contexts that only hold `&self`. Returns
    /// `Ty::Unknown` for anything it can't resolve, which callers treat permissively.
    fn resolve_ty_ro(&self, t: &Type) -> Ty {
        match t {
            Type::Named(n) => match n.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "nil" => Ty::Nil,
                "Executor" => Ty::Executor,
                "Socket" => Ty::Socket,
                "Listener" => Ty::Listener,
                _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                _ if self.struct_names.contains(n) => Ty::strukt(n.clone()),
                _ if self.enum_names.contains(n) => Ty::Enum(n.clone(), Vec::new()),
                _ if self.protocols.contains_key(n) => Ty::Protocol(n.clone()),
                _ => Ty::Unknown,
            },
            Type::Generic(n, args) => match (n.as_str(), args.as_slice()) {
                ("list", [x]) => Ty::list(self.resolve_ty_ro(x)),
                ("set", [x]) => Ty::set(self.resolve_ty_ro(x)),
                ("Option", [x]) => Ty::option(self.resolve_ty_ro(x)),
                ("Channel", [x]) => Ty::channel(self.resolve_ty_ro(x)),
                ("Shared", [x]) => Ty::shared(self.resolve_ty_ro(x)),
                ("Atomic", [x]) => Ty::atomic(self.resolve_ty_ro(x)),
                ("Result", [x]) => Ty::result(self.resolve_ty_ro(x)),
                ("Result", [x, e]) => Ty::result_e(self.resolve_ty_ro(x), self.resolve_ty_ro(e)),
                ("map", [k, v]) => Ty::map(self.resolve_ty_ro(k), self.resolve_ty_ro(v)),
                _ if self.struct_names.contains(n) => {
                    Ty::Struct(n.clone(), args.iter().map(|a| self.resolve_ty_ro(a)).collect())
                }
                _ if self.enum_names.contains(n) => {
                    Ty::Enum(n.clone(), args.iter().map(|a| self.resolve_ty_ro(a)).collect())
                }
                _ => Ty::Unknown,
            },
            Type::Func { params, ret } => Ty::Func {
                params: params.iter().map(|p| self.resolve_ty_ro(p)).collect(),
                ret: Box::new(self.resolve_ty_ro(ret)),
            },
            Type::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve_ty_ro(t)).collect()),
        }
    }

    /// Does concrete `ty` satisfy `protocol` instantiated with `args` (the bound's type arguments,
    /// e.g. `[int]` for `Container[int]`)? `args` is empty for a bare protocol. For a parameterized
    /// protocol the structural check substitutes the protocol's type params with `args` before
    /// matching method signatures.
    fn satisfies_args(&self, ty: &Ty, protocol: &str, args: &[Ty]) -> Result<(), String> {
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
        // `Iterator` conformance is exactly "can be iterated" — built-in collections intrinsically,
        // a user struct via its structural `next(self) -> Option[E]`. Reusing `iter_elem` keeps this
        // in lockstep with what `for` accepts (single source of truth, no drift). A `Ty::Param` falls
        // through to the declared-bounds check below (so a `[S: Iterator[T]]` value forwards into
        // another iterator-generic call), since `iter_elem` can't see through a bare param.
        if protocol == "Iterator" && !matches!(ty, Ty::Param(_)) {
            return if self.iter_elem(ty).is_some() {
                Ok(())
            } else {
                Err(format!("type {ty} does not satisfy Iterator"))
            };
        }
        // `Index`/`IndexSet`/`Slice` — built-in `list`/`map`/`str` conform intrinsically (a struct
        // conforms structurally, falling through to the matcher below; a `Ty::Param` forwards to its
        // declared bounds). `str` is immutable, so it satisfies `Index`/`Slice` but NOT `IndexSet`.
        if matches!(protocol, "Index" | "IndexSet" | "Slice")
            && !matches!(ty, Ty::Param(_) | Ty::Struct(..))
        {
            let provided: Vec<Ty> = match protocol {
                "Slice" => match self.slice_result(ty) {
                    Some(r) => vec![r],
                    None => return Err(format!("type {ty} does not satisfy Slice")),
                },
                _ => {
                    if protocol == "IndexSet" && !matches!(ty, Ty::List(_) | Ty::Map(_, _)) {
                        return Err(format!("type {ty} does not satisfy IndexSet"));
                    }
                    match self.index_kv(ty) {
                        Some((k, v)) => vec![k, v],
                        None => return Err(format!("type {ty} does not satisfy {protocol}")),
                    }
                }
            };
            // Any args the bound supplied must match what the built-in actually provides.
            for (want, got) in args.iter().zip(&provided) {
                if !want.is_unknown() && !got.is_unknown() && !compatible(want, got) {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                }
            }
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
        // this is what lets a generic forward its `T: P` value into another `[U: P]` call. For a
        // parameterized protocol the bound's type args must also match the required ones, so a
        // `Container[str]` value is NOT accepted where `Container[int]` is required (forwarding hole).
        if let Ty::Param(name) = ty {
            let matched = self.type_params.get(name).is_some_and(|bs| {
                bs.iter().any(|b| b.name == protocol && self.bound_args_match(&b.args, args))
            });
            return if matched {
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
                // Substitute the protocol's own type params with the bound's args (`T ↦ int` for
                // `Container[int]`) before matching; `Self` is handled inside `method_matches`.
                let pmap: HashMap<String, Ty> =
                    pinfo.type_params.iter().cloned().zip(args.iter().cloned()).collect();
                for (mname, msig) in &pinfo.methods {
                    let subst_params: Vec<Ty> = msig.params.iter().map(|t| subst(t, &pmap)).collect();
                    let min_params = subst_params.len();
                    let want = FnSig {
                        params: subst_params,
                        ret: subst(&msig.ret, &pmap),
                        type_params: Vec::new(),
                        min_params,
                    };
                    let msig = &want;
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
                .is_some_and(|bs| bs.iter().any(|proto| self.protocol_has_method(&proto.name, "compare"))),
            (Ty::Struct(a, _), Ty::Struct(b, _)) if a == b => self.satisfies(l, "Comparable").is_ok(),
            _ => false,
        }
    }

    fn protocol_has_method(&self, protocol: &str, method: &str) -> bool {
        self.protocols
            .get(protocol)
            .is_some_and(|p| p.methods.iter().any(|(n, _)| n == method))
    }

    /// Whether `name` is a *generic* user fn / struct / enum-variant constructor (i.e. one that can
    /// accept explicit call-site type arguments). Non-generic decls and builtins return `false`.
    /// Can a value of this type cross a task boundary (`spawn` capture / argument, `Channel.send`)?
    /// Scalars, strings, and containers of sendable elements can; `Channel` (and `Shared`, C3)
    /// handles can. Closures/functions (bound to a heap), modules, and protocol existentials (which
    /// may wrap a closure) cannot. A struct/enum is sendable iff *all its field/payload types* are —
    /// inspected via the registry so a closure smuggled inside a struct field is caught. A generic
    /// type parameter (`Param`) is treated as sendable (the opaque-body case; concrete call sites
    /// resolve to a real type that is checked).
    fn sendable(&self, ty: &Ty) -> bool {
        self.sendable_rec(ty, &mut Vec::new())
    }

    /// `sendable` with a cycle guard (`stack` holds the struct/enum names currently being walked,
    /// so a recursive type like `Node { next: Option[Node] }` terminates).
    fn sendable_rec(&self, ty: &Ty, stack: &mut Vec<String>) -> bool {
        match ty {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Nil | Ty::Unknown | Ty::Param(_) => true,
            // A `Shared[T]` handle always crosses — that's its whole point (one box, many tasks);
            // its element type is *not* a constraint (the value never crosses, only the handle).
            Ty::Shared(_) => true,
            // An `Atomic[T]` handle crosses for the same reason as `Shared` — one box, many tasks;
            // the element type is not a constraint (only the handle crosses).
            Ty::Atomic(_) => true,
            // An `Executor` handle crosses the airlock like a `Channel`/`Shared` handle (the queue
            // lives outside every heap; tasks reach the one work queue).
            Ty::Executor => true,
            // D6 — a `Socket`/`Listener` handle crosses the airlock like the other core handles (the
            // fd lives in an `Arc`'d core outside every heap), so a `parallel:` accept-loop can
            // `spawn handle(conn)` onto a fiber.
            Ty::Socket | Ty::Listener => true,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Channel(t) => self.sendable_rec(t, stack),
            Ty::Map(k, v) => self.sendable_rec(k, stack) && self.sendable_rec(v, stack),
            Ty::Result(t, e) => self.sendable_rec(t, stack) && self.sendable_rec(e, stack),
            Ty::Tuple(elems) => elems.iter().all(|t| self.sendable_rec(t, stack)),
            Ty::Func { .. } | Ty::Module(_) | Ty::Protocol(_) => false,
            Ty::Struct(name, args) => {
                // The *builtin* `Ref[T]` (std.ref) is the in-task box: copying it across a spawn
                // would silently give each task its own box (a footgun), so it's non-sendable —
                // reach for the cross-task box `Shared[T]` instead (spec §7). Keyed on origin (not
                // the bare name), so a user struct that merely happens to be named `Ref` is sendable.
                if name == "Ref"
                    && self.structs.get(name).map(|i| i.origin) == Some(StructOrigin::Builtin)
                {
                    return false;
                }
                if !args.iter().all(|a| self.sendable_rec(a, stack)) {
                    return false;
                }
                if stack.contains(name) {
                    return true; // already being walked — the cycle adds no new type
                }
                match self.structs.get(name) {
                    Some(info) => {
                        let fields = info.fields.clone();
                        stack.push(name.clone());
                        let ok = fields.iter().all(|(_, fty)| self.sendable_rec(fty, stack));
                        stack.pop();
                        ok
                    }
                    None => true, // unknown struct: be permissive (any error is reported elsewhere)
                }
            }
            Ty::Enum(name, args) => {
                if !args.iter().all(|a| self.sendable_rec(a, stack)) {
                    return false;
                }
                if stack.contains(name) {
                    return true;
                }
                // Built-in Result/Option are erased here (their payloads are the type args, already
                // checked above); a user enum's variant payloads come from the registry.
                let payloads: Vec<Ty> = self
                    .variants
                    .values()
                    .filter(|v| &v.enum_name == name)
                    .flat_map(|v| v.payload.clone())
                    .collect();
                stack.push(name.clone());
                let ok = payloads.iter().all(|pty| self.sendable_rec(pty, stack));
                stack.pop();
                ok
            }
        }
    }

    fn name_is_generic(&self, name: &str) -> bool {
        // The built-in `Channel[T]()` constructor takes its element type as an explicit type arg.
        if name == "Channel" {
            return true;
        }
        if let Some(i) = self.structs.get(name) {
            return !i.type_params.is_empty();
        }
        if let Some(v) = self.variants.get(name) {
            return self.enum_type_params.get(&v.enum_name).is_some_and(|t| !t.is_empty());
        }
        if let Some(s) = self.functions.get(name) {
            return !s.type_params.is_empty();
        }
        false
    }

    /// Seed a substitution map from explicit call-site type arguments, validating their count
    /// against the declared type parameters. Empty `targs` (the inference-only case) yields an
    /// empty map. A count mismatch is reported but the overlapping prefix is still seeded so
    /// inference can recover.
    fn seed_targs(
        &mut self,
        name: &str,
        tps: &[TypeParam],
        targs: &[Ty],
        span: Span,
    ) -> HashMap<String, Ty> {
        let mut sub = HashMap::new();
        if !targs.is_empty() {
            if targs.len() != tps.len() {
                self.error(
                    span,
                    format!("'{name}' expects {} type argument(s), found {}", tps.len(), targs.len()),
                );
            }
            for (tp, ta) in tps.iter().zip(targs) {
                sub.insert(tp.name.clone(), ta.clone());
            }
        }
        sub
    }

    /// Recover element types from parameterized `Iterator[T]` bounds: for each type param already
    /// bound to a concrete iterand in `sub`, bind the bound's element arg `T` to the iterand's element
    /// type. Mutates `sub` (collects first to avoid borrowing it while iterating). Shared by every
    /// generic-call site (free fn, struct constructor, enum variant).
    fn recover_iter_elems(&mut self, tps: &[TypeParam], sub: &mut HashMap<String, Ty>, span: Span) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name).cloned() {
                for b in &tp.bounds {
                    if b.name == "Iterator"
                        && let Some(arg) = b.args.first()
                        && let Some(elem) = self.iter_elem(&concrete)
                    {
                        binds.push((self.resolve_bound_arg(arg, tps, span), elem));
                    }
                }
            }
        }
        for (arg_ty, elem) in &binds {
            // Bind the element param if it's still free; otherwise it was already pinned (an explicit
            // type arg, another argument position, or a concrete `Iterator[int]` bound) and the
            // recovered element MUST agree — `unify` is a silent no-op there, so check it ourselves.
            match arg_ty {
                Ty::Param(n) if !sub.contains_key(n) => {
                    if !elem.is_unknown() {
                        sub.insert(n.clone(), elem.clone());
                    }
                }
                _ => {
                    let pinned = match arg_ty {
                        Ty::Param(n) => sub.get(n).cloned().unwrap_or(Ty::Unknown),
                        other => other.clone(),
                    };
                    if !pinned.is_unknown()
                        && !elem.is_unknown()
                        && !self.assignable(&pinned, elem)
                    {
                        self.error(
                            span,
                            format!("iterator element type {elem} does not match the declared element type {pinned}"),
                        );
                    }
                }
            }
        }
    }

    /// Recover the `K`/`V` (`Index`/`IndexSet`) and `R` (`Slice`) type args of parameterized bounds
    /// from each type parameter's inferred binding — the indexing analogue of `recover_iter_elems`,
    /// so `fn first[C: Index[int, V], V](c: C) -> V` recovers `V` from the argument.
    fn recover_index_args(&mut self, tps: &[TypeParam], sub: &mut HashMap<String, Ty>, span: Span) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            let Some(concrete) = sub.get(&tp.name).cloned() else { continue };
            for b in &tp.bounds {
                match b.name.as_str() {
                    "Index" | "IndexSet" => {
                        if let Some((k, v)) = self.index_kv(&concrete) {
                            if let Some(a) = b.args.first() {
                                binds.push((self.resolve_bound_arg(a, tps, span), k));
                            }
                            if let Some(a) = b.args.get(1) {
                                binds.push((self.resolve_bound_arg(a, tps, span), v));
                            }
                        }
                    }
                    "Slice" => {
                        if let Some(r) = self.slice_result(&concrete)
                            && let Some(a) = b.args.first()
                        {
                            binds.push((self.resolve_bound_arg(a, tps, span), r));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (arg_ty, recovered) in &binds {
            // Bind the arg param if still free; otherwise it was already pinned and must agree.
            match arg_ty {
                Ty::Param(n) if !sub.contains_key(n) => {
                    if !recovered.is_unknown() {
                        sub.insert(n.clone(), recovered.clone());
                    }
                }
                _ => {
                    let pinned = match arg_ty {
                        Ty::Param(n) => sub.get(n).cloned().unwrap_or(Ty::Unknown),
                        other => other.clone(),
                    };
                    if !pinned.is_unknown()
                        && !recovered.is_unknown()
                        && !self.assignable(&pinned, recovered)
                    {
                        self.error(
                            span,
                            format!("index type {recovered} does not match the declared type {pinned}"),
                        );
                    }
                }
            }
        }
    }

    /// Enforce each type parameter's declared protocol bounds against its inferred binding. A
    /// parameterized bound (`Container[int]`) supplies type args, resolved here (sibling params in
    /// scope) and checked structurally with the protocol's params substituted.
    fn enforce_bounds(&mut self, tps: &[TypeParam], sub: &HashMap<String, Ty>, span: Span) {
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name) {
                for bound in &tp.bounds {
                    // Resolve the bound's args, then substitute any params recovered into `sub` (e.g.
                    // `Index[int, V]` with `V` recovered to `int`) so the structural/intrinsic check
                    // sees concrete args, not a still-free `Ty::Param`.
                    let bargs: Vec<Ty> = bound
                        .args
                        .iter()
                        .map(|a| subst(&self.resolve_bound_arg(a, tps, span), sub))
                        .collect();
                    if let Err(msg) = self.satisfies_args(concrete, &bound.name, &bargs) {
                        self.error(span, msg);
                    }
                }
            }
        }
    }

    /// Type-check a call to a generic function: infer each type parameter from the arguments,
    /// enforce the declared bounds, and substitute into the return type.
    fn infer_generic_call(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
    ) -> Ty {
        if args.len() != sig.params.len() {
            self.check_arity(name, sig.params.len(), args, span);
        }
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
        // Explicit call-site type arguments (`max[int](…)`) seed the substitution; remaining (or
        // all, when none given) parameters are inferred from positional arguments. `unify` only
        // binds a parameter that isn't already in the map, so explicit args take precedence and a
        // conflicting argument is caught by the per-argument check below.
        let mut subst_map: HashMap<String, Ty> = self.seed_targs(name, &sig.type_params, targs, span);
        for (decl, actual) in sig.params.iter().zip(&arg_tys) {
            unify(decl, actual, &mut subst_map);
        }
        // Recover element types from parameterized `Iterator[T]` bounds (bind `T` to the iterand's
        // element), then enforce every declared bound against its inferred binding.
        self.recover_iter_elems(&sig.type_params, &mut subst_map, span);
        self.recover_index_args(&sig.type_params, &mut subst_map, span);
        self.enforce_bounds(&sig.type_params, &subst_map, span);
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

    /// Infer a generic *method*'s own type parameters from the call arguments. `params`/`ret` are the
    /// method signature already substituted with the receiver struct's type arguments, so only the
    /// method's own `[U]` params remain free; `params[0]` is the receiver (bound from `obj`, not an
    /// explicit arg). Mirrors `infer_generic_call`'s tail. The parser never attaches call-site type
    /// args to a method callee, so inference is purely positional.
    #[allow(clippy::too_many_arguments)] // the method's resolved signature pieces + receiver + call
    fn infer_generic_method(
        &mut self,
        method: &str,
        params: &[Ty],
        ret: &Ty,
        mtps: &[TypeParam],
        recv_ty: &Ty,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        // The first parameter is the receiver (bound from `obj`). A method with NO params has no
        // receiver slot — reject, mirroring the non-generic path.
        let Some((receiver, expected)) = params.split_first() else {
            self.error(
                span,
                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
            );
            self.infer_all(args);
            return Ty::Unknown;
        };
        if args.len() != expected.len() {
            self.error(
                span,
                format!("'{method}' expects {} argument(s), got {}", expected.len(), args.len()),
            );
        }
        let arg_tys: Vec<Ty> = args.iter().map(|a| self.infer(a)).collect();
        let mut mmap: HashMap<String, Ty> = HashMap::new();
        // A method type param may appear in the receiver position (`fn f[U](u: U)`); bind it from the
        // actual receiver type so it isn't left unresolved.
        unify(receiver, recv_ty, &mut mmap);
        for (decl, actual) in expected.iter().zip(&arg_tys) {
            unify(decl, actual, &mut mmap);
        }
        // Recover element types from `Iterator[T]` bounds, then enforce every declared bound.
        self.recover_iter_elems(mtps, &mut mmap, span);
        self.recover_index_args(mtps, &mut mmap, span);
        self.enforce_bounds(mtps, &mmap, span);
        for (decl, (actual, arg)) in expected.iter().zip(arg_tys.iter().zip(args)) {
            let want = subst(decl, &mmap);
            if !self.assignable(&want, actual) {
                self.error(
                    arg.span,
                    format!("argument to '{method}' has type {actual}, expected {want}"),
                );
            }
        }
        subst(ret, &mmap)
    }
}

// ===== G1 (B3.3b) AST walkers: spawn-reachable module-global mutation =====
//
// Name resolution here is **scope-aware**: a call or `spawn` target `f()` references the free
// function `f` only when `f` is not shadowed by a local binding in scope (params, `let`, `for`/
// `match` binders, closure params, comprehension vars). Two indirect-dispatch cases are deliberately
// NOT modelled (the static call graph can't follow them), consistent with the documented
// method-mediated gap; both land with the B3.3 thread flip:
//   1. a module-global bound to a *closure* used as a `spawn`/call target (`g := fn(): …; spawn g()`);
//   2. method calls (`obj.m()` / `spawn obj.m()`).

/// Is `name` shadowed by a local binding currently in scope?
fn is_locally_shadowed(name: &str, scopes: &[std::collections::HashSet<String>]) -> bool {
    scopes.iter().any(|s| s.contains(name))
}

/// Collect `spawn` task roots across the whole module — the free functions a `spawn` makes reachable.
/// A `spawn` inside *any* function roots its target even if that function is only called sequentially
/// (the spawn still fires when the function runs). Scope-aware: a `spawn f()` whose `f` is shadowed by
/// a local targets that local, not the free fn, so it is not rooted.
fn collect_spawn_roots(stmts: &[Stmt], fns: &HashMap<&str, &FnDecl>, out: &mut Vec<String>) {
    let mut scopes: Vec<std::collections::HashSet<String>> = vec![std::collections::HashSet::new()];
    walk_spawn_roots(stmts, fns, &mut scopes, out);
}

/// Walk a block (under `scopes`) for `spawn` roots: descend nested control flow with pushed scopes,
/// and each `fn`/method body under a fresh parameter scope (chezzi has no nested `fn` declarations,
/// so a fresh stack per body is sound).
fn walk_spawn_roots(
    block: &[Stmt],
    fns: &HashMap<&str, &FnDecl>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
    out: &mut Vec<String>,
) {
    scopes.push(std::collections::HashSet::new());
    for s in block {
        match &s.kind {
            StmtKind::Let { names, .. } => {
                scopes.last_mut().unwrap().extend(names.iter().cloned());
            }
            StmtKind::Spawn(SpawnTarget::Call(e)) => {
                if let ExprKind::Call { callee, .. } = &e.kind
                    && let ExprKind::Ident(name) = &callee.kind
                    && fns.contains_key(name.as_str())
                    && !is_locally_shadowed(name, scopes)
                {
                    out.push(name.clone());
                }
            }
            StmtKind::Spawn(SpawnTarget::Block(body)) => {
                // The block runs as the task: free fns it calls are roots (scope-aware).
                collect_free_calls_block(body, fns, scopes, out);
                walk_spawn_roots(body, fns, scopes, out); // nested spawns
            }
            StmtKind::If { branches, else_block } => {
                for (_, b) in branches {
                    walk_spawn_roots(b, fns, scopes, out);
                }
                if let Some(b) = else_block {
                    walk_spawn_roots(b, fns, scopes, out);
                }
            }
            StmtKind::For { vars, body, .. } => {
                scopes.push(vars.iter().cloned().collect());
                walk_spawn_roots(body, fns, scopes, out);
                scopes.pop();
            }
            StmtKind::While { body, .. } | StmtKind::Parallel { body } => {
                walk_spawn_roots(body, fns, scopes, out)
            }
            StmtKind::Match { arms, .. } => {
                for a in arms {
                    scopes.push(pattern_bindings(&a.pattern));
                    walk_spawn_roots(&a.body, fns, scopes, out);
                    scopes.pop();
                }
            }
            StmtKind::Fn(d) => {
                let mut s2 = vec![d.params.iter().map(|p| p.name.clone()).collect()];
                walk_spawn_roots(&d.body, fns, &mut s2, out);
            }
            StmtKind::Struct { methods, .. } => {
                for m in methods {
                    let mut s2 = vec![m.params.iter().map(|p| p.name.clone()).collect()];
                    walk_spawn_roots(&m.body, fns, &mut s2, out);
                }
            }
            StmtKind::Wait { arms, else_block } => {
                for a in arms {
                    if let WaitTarget::Bind(n) = &a.target {
                        scopes.push(std::iter::once(n.clone()).collect());
                        walk_spawn_roots(&a.body, fns, scopes, out);
                        scopes.pop();
                    } else {
                        walk_spawn_roots(&a.body, fns, scopes, out);
                    }
                }
                if let Some(b) = else_block {
                    walk_spawn_roots(b, fns, scopes, out);
                }
            }
            _ => {}
        }
    }
    scopes.pop();
}

/// Collect the free functions called (by bare `Ident`) anywhere in `block`, scope-aware (a call to a
/// locally-shadowed name is not a free-fn reference). Backs both call-graph edges (seed `scopes` with
/// the caller's params) and a `spawn:` block's roots. Including the calls inside a nested `spawn` is
/// harmless — those targets are already roots via [`walk_spawn_roots`], so they never add reachability
/// the root set does not already have.
fn collect_free_calls_block(
    block: &[Stmt],
    fns: &HashMap<&str, &FnDecl>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
    out: &mut Vec<String>,
) {
    scopes.push(std::collections::HashSet::new());
    for s in block {
        match &s.kind {
            StmtKind::Let { names, value, .. } => {
                collect_free_calls_expr(value, fns, scopes, out);
                scopes.last_mut().unwrap().extend(names.iter().cloned());
            }
            StmtKind::Assign { target, value, .. } => {
                collect_free_calls_expr(target, fns, scopes, out);
                collect_free_calls_expr(value, fns, scopes, out);
            }
            StmtKind::Return(Some(e))
            | StmtKind::Expr(e)
            | StmtKind::Defer(DeferTarget::Call(e)) => {
                collect_free_calls_expr(e, fns, scopes, out)
            }
            StmtKind::Defer(DeferTarget::Block(body)) => {
                collect_free_calls_block(body, fns, scopes, out)
            }
            StmtKind::If { branches, else_block } => {
                for (c, b) in branches {
                    collect_free_calls_expr(c, fns, scopes, out);
                    collect_free_calls_block(b, fns, scopes, out);
                }
                if let Some(b) = else_block {
                    collect_free_calls_block(b, fns, scopes, out);
                }
            }
            StmtKind::For { vars, iter, body } => {
                collect_free_calls_expr(iter, fns, scopes, out);
                scopes.push(vars.iter().cloned().collect());
                collect_free_calls_block(body, fns, scopes, out);
                scopes.pop();
            }
            StmtKind::While { cond, body } => {
                collect_free_calls_expr(cond, fns, scopes, out);
                collect_free_calls_block(body, fns, scopes, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                collect_free_calls_expr(scrutinee, fns, scopes, out);
                for a in arms {
                    scopes.push(pattern_bindings(&a.pattern));
                    if let Some(g) = &a.guard {
                        collect_free_calls_expr(g, fns, scopes, out);
                    }
                    collect_free_calls_block(&a.body, fns, scopes, out);
                    scopes.pop();
                }
            }
            StmtKind::Parallel { body } => collect_free_calls_block(body, fns, scopes, out),
            StmtKind::Spawn(SpawnTarget::Call(e)) => collect_free_calls_expr(e, fns, scopes, out),
            StmtKind::Spawn(SpawnTarget::Block(body)) => {
                collect_free_calls_block(body, fns, scopes, out)
            }
            StmtKind::Wait { arms, else_block } => {
                for a in arms {
                    collect_free_calls_expr(&a.chan, fns, scopes, out);
                    if let WaitTarget::Assign(e) = &a.target {
                        collect_free_calls_expr(e, fns, scopes, out);
                    }
                    // A `:=` arm introduces an arm-local binding for its body.
                    if let WaitTarget::Bind(n) = &a.target {
                        scopes.push(std::iter::once(n.clone()).collect());
                        collect_free_calls_block(&a.body, fns, scopes, out);
                        scopes.pop();
                    } else {
                        collect_free_calls_block(&a.body, fns, scopes, out);
                    }
                }
                if let Some(b) = else_block {
                    collect_free_calls_block(b, fns, scopes, out);
                }
            }
            _ => {}
        }
    }
    scopes.pop();
}

/// Scope-aware free-fn call collection within a single expression (exhaustive over [`ExprKind`]).
/// Closure params and comprehension vars are pushed as scopes so a binder shadowing a free-fn name is
/// respected even in expression position.
fn collect_free_calls_expr(
    e: &Expr,
    fns: &HashMap<&str, &FnDecl>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
    out: &mut Vec<String>,
) {
    match &e.kind {
        ExprKind::Call { callee, args, named, .. } => {
            if let ExprKind::Ident(name) = &callee.kind {
                if fns.contains_key(name.as_str()) && !is_locally_shadowed(name, scopes) {
                    out.push(name.clone());
                }
            } else {
                collect_free_calls_expr(callee, fns, scopes, out);
            }
            for a in args {
                collect_free_calls_expr(a, fns, scopes, out);
            }
            for (_, a) in named {
                collect_free_calls_expr(a, fns, scopes, out);
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().for_each(|x| collect_free_calls_expr(x, fns, scopes, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            collect_free_calls_expr(k, fns, scopes, out);
            collect_free_calls_expr(v, fns, scopes, out);
        }),
        ExprKind::Comprehension { key, elem, iter, guard, vars, .. } => {
            collect_free_calls_expr(iter, fns, scopes, out); // iter binds in the OUTER scope
            scopes.push(vars.iter().cloned().collect());
            if let Some(k) = key {
                collect_free_calls_expr(k, fns, scopes, out);
            }
            collect_free_calls_expr(elem, fns, scopes, out);
            if let Some(g) = guard {
                collect_free_calls_expr(g, fns, scopes, out);
            }
            scopes.pop();
        }
        ExprKind::Unary { expr, .. } => collect_free_calls_expr(expr, fns, scopes, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs } => {
            collect_free_calls_expr(lhs, fns, scopes, out);
            collect_free_calls_expr(rhs, fns, scopes, out);
        }
        ExprKind::Range { start, end } => {
            collect_free_calls_expr(start, fns, scopes, out);
            collect_free_calls_expr(end, fns, scopes, out);
        }
        ExprKind::Field { obj, .. } => collect_free_calls_expr(obj, fns, scopes, out),
        ExprKind::Index { obj, index } => {
            collect_free_calls_expr(obj, fns, scopes, out);
            collect_free_calls_expr(index, fns, scopes, out);
        }
        ExprKind::Slice { obj, start, end } => {
            collect_free_calls_expr(obj, fns, scopes, out);
            collect_free_calls_expr(start, fns, scopes, out);
            collect_free_calls_expr(end, fns, scopes, out);
        }
        ExprKind::Try(inner) => collect_free_calls_expr(inner, fns, scopes, out),
        ExprKind::OptChain { obj, call, .. } => {
            collect_free_calls_expr(obj, fns, scopes, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| collect_free_calls_expr(a, fns, scopes, out));
                c.named.iter().for_each(|(_, a)| collect_free_calls_expr(a, fns, scopes, out));
            }
        }
        ExprKind::DecodeCall { obj, arg, .. } => {
            collect_free_calls_expr(obj, fns, scopes, out);
            collect_free_calls_expr(arg, fns, scopes, out);
        }
        ExprKind::Closure { params, body, .. } => {
            scopes.push(params.iter().map(|p| p.name.clone()).collect());
            collect_free_calls_expr(body, fns, scopes, out);
            scopes.pop();
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_free_calls_expr(scrutinee, fns, scopes, out);
            for a in arms {
                if let Some(g) = &a.guard {
                    collect_free_calls_expr(g, fns, scopes, out);
                }
                collect_free_calls_expr(&a.body, fns, scopes, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            collect_free_calls_expr(cond, fns, scopes, out);
            collect_free_calls_expr(then, fns, scopes, out);
            collect_free_calls_expr(els, fns, scopes, out);
        }
        ExprKind::Recover(block) => collect_free_calls_block(block, fns, scopes, out),
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_) => {}
    }
}

/// Walk a block for reassignments (`= / += / -=`) of a module global that is **not shadowed** by a
/// local binding in scope, recording `(span, global_name)` for each. `scopes` is a stack of
/// local-name sets (params at the base); nested blocks push/pop their own frame so a local shadowing
/// a global is excluded only within its scope. A `spawn:` block's *own* direct mutations are NOT
/// flagged here — the normal-pass `is_captured` gate already rejects them — but a `recover:` block
/// (an expression embedding a statement block) and closure/comprehension bodies are descended into so
/// a mutation hidden in one is still caught.
fn find_global_mutations(
    block: &Block,
    globals: &std::collections::HashSet<String>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
    out: &mut Vec<(Span, String)>,
) {
    scopes.push(std::collections::HashSet::new());
    for s in block {
        match &s.kind {
            StmtKind::Let { names, value, .. } => {
                find_mutations_in_expr(value, globals, scopes, out);
                for n in names {
                    scopes.last_mut().unwrap().insert(n.clone());
                }
            }
            StmtKind::Assign { target, value, op: _ } => {
                find_mutations_in_expr(value, globals, scopes, out);
                if let ExprKind::Ident(name) = &target.kind {
                    if !is_locally_shadowed(name, scopes) && globals.contains(name) {
                        out.push((target.span, name.clone()));
                    }
                } else {
                    find_mutations_in_expr(target, globals, scopes, out);
                }
            }
            StmtKind::Return(Some(e))
            | StmtKind::Expr(e)
            | StmtKind::Defer(DeferTarget::Call(e)) => {
                find_mutations_in_expr(e, globals, scopes, out)
            }
            StmtKind::Defer(DeferTarget::Block(body)) => {
                find_global_mutations(body, globals, scopes, out)
            }
            StmtKind::If { branches, else_block } => {
                for (c, b) in branches {
                    find_mutations_in_expr(c, globals, scopes, out);
                    find_global_mutations(b, globals, scopes, out);
                }
                if let Some(b) = else_block {
                    find_global_mutations(b, globals, scopes, out);
                }
            }
            StmtKind::For { vars, iter, body } => {
                find_mutations_in_expr(iter, globals, scopes, out);
                scopes.push(vars.iter().cloned().collect());
                find_global_mutations(body, globals, scopes, out);
                scopes.pop();
            }
            StmtKind::While { cond, body } => {
                find_mutations_in_expr(cond, globals, scopes, out);
                find_global_mutations(body, globals, scopes, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                find_mutations_in_expr(scrutinee, globals, scopes, out);
                for a in arms {
                    scopes.push(pattern_bindings(&a.pattern));
                    if let Some(g) = &a.guard {
                        find_mutations_in_expr(g, globals, scopes, out);
                    }
                    find_global_mutations(&a.body, globals, scopes, out);
                    scopes.pop();
                }
            }
            StmtKind::Parallel { body } => find_global_mutations(body, globals, scopes, out),
            StmtKind::Spawn(SpawnTarget::Call(e)) => {
                find_mutations_in_expr(e, globals, scopes, out)
            }
            StmtKind::Wait { arms, else_block } => {
                for a in arms {
                    find_mutations_in_expr(&a.chan, globals, scopes, out);
                    // A `=` arm to a global is itself a global mutation.
                    if let WaitTarget::Assign(target) = &a.target {
                        if let ExprKind::Ident(name) = &target.kind {
                            if !is_locally_shadowed(name, scopes) && globals.contains(name) {
                                out.push((target.span, name.clone()));
                            }
                        } else {
                            find_mutations_in_expr(target, globals, scopes, out);
                        }
                    }
                    if let WaitTarget::Bind(n) = &a.target {
                        scopes.push(std::iter::once(n.clone()).collect());
                        find_global_mutations(&a.body, globals, scopes, out);
                        scopes.pop();
                    } else {
                        find_global_mutations(&a.body, globals, scopes, out);
                    }
                }
                if let Some(b) = else_block {
                    find_global_mutations(b, globals, scopes, out);
                }
            }
            _ => {}
        }
    }
    scopes.pop();
}

/// Find module-global mutations hidden inside an expression: a `recover:` block (statement block in
/// expression position) is descended via [`find_global_mutations`]; closure/comprehension bodies push
/// their binders as scopes so a shadowing binder is respected. Exhaustive over [`ExprKind`].
fn find_mutations_in_expr(
    e: &Expr,
    globals: &std::collections::HashSet<String>,
    scopes: &mut Vec<std::collections::HashSet<String>>,
    out: &mut Vec<(Span, String)>,
) {
    match &e.kind {
        ExprKind::Recover(block) => find_global_mutations(block, globals, scopes, out),
        ExprKind::Closure { params, body, .. } => {
            scopes.push(params.iter().map(|p| p.name.clone()).collect());
            find_mutations_in_expr(body, globals, scopes, out);
            scopes.pop();
        }
        ExprKind::Comprehension { key, elem, iter, guard, vars, .. } => {
            find_mutations_in_expr(iter, globals, scopes, out);
            scopes.push(vars.iter().cloned().collect());
            if let Some(k) = key {
                find_mutations_in_expr(k, globals, scopes, out);
            }
            find_mutations_in_expr(elem, globals, scopes, out);
            if let Some(g) = guard {
                find_mutations_in_expr(g, globals, scopes, out);
            }
            scopes.pop();
        }
        ExprKind::Call { callee, args, named, .. } => {
            find_mutations_in_expr(callee, globals, scopes, out);
            for a in args {
                find_mutations_in_expr(a, globals, scopes, out);
            }
            for (_, a) in named {
                find_mutations_in_expr(a, globals, scopes, out);
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => {
            xs.iter().for_each(|x| find_mutations_in_expr(x, globals, scopes, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            find_mutations_in_expr(k, globals, scopes, out);
            find_mutations_in_expr(v, globals, scopes, out);
        }),
        ExprKind::Unary { expr, .. } => find_mutations_in_expr(expr, globals, scopes, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs } => {
            find_mutations_in_expr(lhs, globals, scopes, out);
            find_mutations_in_expr(rhs, globals, scopes, out);
        }
        ExprKind::Range { start, end } => {
            find_mutations_in_expr(start, globals, scopes, out);
            find_mutations_in_expr(end, globals, scopes, out);
        }
        ExprKind::Field { obj, .. } => find_mutations_in_expr(obj, globals, scopes, out),
        ExprKind::Index { obj, index } => {
            find_mutations_in_expr(obj, globals, scopes, out);
            find_mutations_in_expr(index, globals, scopes, out);
        }
        ExprKind::Slice { obj, start, end } => {
            find_mutations_in_expr(obj, globals, scopes, out);
            find_mutations_in_expr(start, globals, scopes, out);
            find_mutations_in_expr(end, globals, scopes, out);
        }
        ExprKind::Try(inner) => find_mutations_in_expr(inner, globals, scopes, out),
        ExprKind::OptChain { obj, call, .. } => {
            find_mutations_in_expr(obj, globals, scopes, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| find_mutations_in_expr(a, globals, scopes, out));
                c.named.iter().for_each(|(_, a)| find_mutations_in_expr(a, globals, scopes, out));
            }
        }
        ExprKind::DecodeCall { obj, arg, .. } => {
            find_mutations_in_expr(obj, globals, scopes, out);
            find_mutations_in_expr(arg, globals, scopes, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            find_mutations_in_expr(scrutinee, globals, scopes, out);
            for a in arms {
                if let Some(g) = &a.guard {
                    find_mutations_in_expr(g, globals, scopes, out);
                }
                find_mutations_in_expr(&a.body, globals, scopes, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            find_mutations_in_expr(cond, globals, scopes, out);
            find_mutations_in_expr(then, globals, scopes, out);
            find_mutations_in_expr(els, globals, scopes, out);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_) => {}
    }
}

/// The names a match pattern binds (variant payload slots, tuple elements, sub-bindings).
fn pattern_bindings(p: &Pattern) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn go(p: &Pattern, out: &mut std::collections::HashSet<String>) {
        match p {
            Pattern::Ident(n) => {
                out.insert(n.clone());
            }
            Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) | Pattern::Or(bindings) => {
                bindings.iter().for_each(|b| go(b, out))
            }
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
        }
    }
    go(p, &mut out);
    out
}

/// The prebuilt protocols every program starts with. `Comparable` requires
/// `compare(self, other: Self) -> int`; primitives (int/float/str) satisfy it intrinsically.
fn prebuilt_protocols() -> HashMap<String, ProtocolInfo> {
    let mut m = HashMap::new();
    m.insert(
        "Comparable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
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
            type_params: Vec::new(),
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
            type_params: Vec::new(),
            methods: vec![("message".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Str))],
        },
    );
    m.insert(
        "Hashable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
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
                type_params: Vec::new(),
                methods: vec![(
                    method.to_string(),
                    FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Param("Self".into())),
                )],
            },
        );
    }
    // `Iterator[T]` — the language's one parameterized protocol. The method shape mirrors the
    // structural detection (`next(self) -> Option[T]`); conformance is decided in `satisfies` via
    // `iter_elem` (built-ins intrinsically, user structs via their `next`), NOT the generic structural
    // loop, and the bound's `[T]` arg recovers the element type at call sites.
    m.insert(
        "Iterator".to_string(),
        ProtocolInfo {
            // One type param (the element) so the generic arity check in `check_bounds` treats
            // `Iterator[T]` uniformly; its conformance + element recovery stay special-cased.
            type_params: vec!["Elem".to_string()],
            methods: vec![(
                "next".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Option(Box::new(Ty::Param("Self".into())))),
            )],
        },
    );
    // The indexing pair (Rust `Index`/`IndexMut`-style) + `Slice`. Built-in `list`/`map`/`str`
    // satisfy these intrinsically (see `satisfies_args`); a user struct satisfies them structurally
    // via `index`/`set_index`/`slice`. `K`/`V`/`R` are recovered at call sites by `recover_index_args`
    // (mirrors `Iterator[T]`'s element recovery).
    m.insert(
        "Index".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            methods: vec![(
                "index".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("K".into())], Ty::Param("V".into())),
            )],
        },
    );
    m.insert(
        "IndexSet".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            methods: vec![
                (
                    "index".to_string(),
                    FnSig::plain(vec![Ty::Unknown, Ty::Param("K".into())], Ty::Param("V".into())),
                ),
                (
                    "set_index".to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("K".into()), Ty::Param("V".into())],
                        Ty::Nil,
                    ),
                ),
            ],
        },
    );
    m.insert(
        "Slice".to_string(),
        ProtocolInfo {
            type_params: vec!["R".to_string()],
            methods: vec![(
                "slice".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Int, Ty::Int], Ty::Param("R".into())),
            )],
        },
    );
    m
}

/// The substitution from a struct's type parameters to a concrete instantiation's type arguments
/// (`Stack[int]` ⇒ `{T: int}`). Empty for a non-generic struct.
fn struct_param_map(info: &StructInfo, targs: &[Ty]) -> HashMap<String, Ty> {
    info.type_params.iter().map(|tp| tp.name.clone()).zip(targs.iter().cloned()).collect()
}

/// Is `ty` fully concrete — free of any type parameter or `Unknown`? Used to decide when a forwarded
/// parameterized bound's args can be compared strictly (only a concrete-vs-concrete mismatch is an
/// error; anything still generic forwards loosely).
fn ty_fully_concrete(ty: &Ty) -> bool {
    match ty {
        Ty::Unknown | Ty::Param(_) => false,
        Ty::List(x) | Ty::Option(x) | Ty::Set(x) => ty_fully_concrete(x),
        Ty::Map(k, v) => ty_fully_concrete(k) && ty_fully_concrete(v),
        Ty::Result(a, b) => ty_fully_concrete(a) && ty_fully_concrete(b),
        Ty::Struct(_, a) | Ty::Enum(_, a) => a.iter().all(ty_fully_concrete),
        Ty::Tuple(ts) => ts.iter().all(ty_fully_concrete),
        Ty::Func { params, ret } => params.iter().all(ty_fully_concrete) && ty_fully_concrete(ret),
        _ => true,
    }
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
/// Find a control-flow statement that would escape a `recover:` block — a `return`, or a
/// `break`/`continue` not contained by a loop *inside* the block. Recurses through nested blocks but
/// stops at nested `fn` declarations (their control flow is their own). `?` is an expression, not a
/// statement, so it is never flagged.
fn recover_escaping_flow(stmts: &[Stmt], in_loop: bool) -> Option<(Span, &'static str)> {
    for s in stmts {
        match &s.kind {
            StmtKind::Return(_) => return Some((s.span, "return")),
            StmtKind::Break if !in_loop => return Some((s.span, "break")),
            StmtKind::Continue if !in_loop => return Some((s.span, "continue")),
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::If { branches, else_block } => {
                for (_, body) in branches {
                    if let Some(x) = recover_escaping_flow(body, in_loop) {
                        return Some(x);
                    }
                }
                if let Some(eb) = else_block
                    && let Some(x) = recover_escaping_flow(eb, in_loop)
                {
                    return Some(x);
                }
            }
            // A loop makes its own `break`/`continue` local; a `return` inside still escapes.
            StmtKind::For { body, .. } | StmtKind::While { body, .. } => {
                if let Some(x) = recover_escaping_flow(body, true) {
                    return Some(x);
                }
            }
            StmtKind::Match { arms, .. } => {
                for arm in arms {
                    if let Some(x) = recover_escaping_flow(&arm.body, in_loop) {
                        return Some(x);
                    }
                }
            }
            // A `parallel:` body and a `spawn:` task body run within this function frame, so an
            // escaping `return`/`break`/`continue` inside them must still be detected. A `for`/loop
            // is not introduced, so `in_loop` is unchanged.
            StmtKind::Parallel { body } => {
                if let Some(x) = recover_escaping_flow(body, in_loop) {
                    return Some(x);
                }
            }
            StmtKind::Spawn(SpawnTarget::Block(body)) => {
                if let Some(x) = recover_escaping_flow(body, in_loop) {
                    return Some(x);
                }
            }
            StmtKind::Fn(_) => {} // nested function: its control flow is its own
            _ => {}
        }
    }
    None
}

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
        // `concat` returns a NEW list (receiver + other); `extend` appends in place (returns nil).
        "concat" => (vec![Ty::list(elem.clone())], Ty::list(elem.clone())),
        "extend" => (vec![Ty::list(elem.clone())], Ty::Nil),
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
        // `merge` returns a NEW map (other wins on key clash); `update` writes into the receiver.
        "merge" => (vec![Ty::map(k.clone(), v.clone())], Ty::map(k.clone(), v.clone())),
        "update" => (vec![Ty::map(k.clone(), v.clone())], Ty::Nil),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `Channel[T]` (C2). `elem` is the channel's element type.
fn channel_method_sig(method: &str, elem: &Ty) -> Option<FnSig> {
    let (params, ret) = match method {
        "send" => (vec![elem.clone()], Ty::Nil),
        // `try_send` is the safe partner of `send` (mirrors `try_recv` vs `recv`): channels are
        // unbounded, so its only failure is a closed channel — returns `false` then, `true` on send.
        "try_send" => (vec![elem.clone()], Ty::Bool),
        "recv" => (vec![], elem.clone()),
        "try_recv" => (vec![], Ty::option(elem.clone())),
        // `close()` marks the channel closed (idempotent); a later `send` faults, `recv` drains then
        // faults, and `for v in ch:` ends cleanly once drained.
        "close" => (vec![], Ty::Nil),
        "len" => (vec![], Ty::Int),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `Shared[T]` (C3). `elem` is the box's element type. Mirrors the
/// `Ref[T]` box API (`std/ref.chz`) — one `get`/`set`/`update` shape across both boxes.
fn shared_method_sig(method: &str, elem: &Ty) -> Option<FnSig> {
    let (params, ret) = match method {
        "get" => (vec![], elem.clone()),
        "set" => (vec![elem.clone()], Ty::Nil),
        "update" => (
            vec![Ty::Func { params: vec![elem.clone()], ret: Box::new(elem.clone()) }],
            Ty::Nil,
        ),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `Atomic[T]` — the cross-task atomic box. `elem` is the box's
/// element type. `load`/`store`/`exchange`/`cas` work for any `T`; `add`/`sub` are arithmetic and
/// exist **only when `T` is numeric** (`int`/`float`) — for any other `T` they return `None`, so the
/// caller reports "no method 'add'".
fn atomic_method_sig(method: &str, elem: &Ty) -> Option<FnSig> {
    let (params, ret) = match method {
        "load" => (vec![], elem.clone()),
        "store" => (vec![elem.clone()], Ty::Nil),
        // swap in `x`, return the previous value.
        "exchange" => (vec![elem.clone()], elem.clone()),
        // compare-and-swap: if the box holds `expected`, replace with `new`; report whether it did.
        "cas" => (vec![elem.clone(), elem.clone()], Ty::Bool),
        // arithmetic RMW — numeric `T` only; returns the NEW value.
        "add" | "sub" if elem.is_numeric() => (vec![elem.clone()], elem.clone()),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// D6 — built-in method signatures on `Socket` (std.net). `read(n)` returns up to `n` bytes as a
/// `str` (empty on a clean EOF / peer close); `write(s)` returns the byte count written; both are
/// `Result` (I/O can fail). `close()` releases the fd.
fn socket_method_sig(method: &str) -> Option<FnSig> {
    match method {
        // D6c — `read`/`write` take an OPTIONAL trailing `timeout_ms: int` (`Err("timeout")` if no
        // data / not writable within it). The required first param (byte count / payload) stays.
        "read" => Some(FnSig::optional_tail(vec![Ty::Int, Ty::Int], Ty::result(Ty::Str), 1)),
        "write" => Some(FnSig::optional_tail(vec![Ty::Str, Ty::Int], Ty::result(Ty::Int), 1)),
        "close" => Some(FnSig::plain(vec![], Ty::Nil)),
        _ => None,
    }
}

/// D6 — built-in method signatures on `Listener` (std.net). `accept()` yields the next inbound
/// connection as a `Socket` (`Result` — accept can fail); `close()` releases the fd.
fn listener_method_sig(method: &str) -> Option<FnSig> {
    match method {
        // D6c — `accept` takes an OPTIONAL `timeout_ms: int` (`Err("timeout")` if no connection
        // arrives within it).
        "accept" => Some(FnSig::optional_tail(vec![Ty::Int], Ty::result(Ty::Socket), 1)),
        // `addr()` reports the bound local address as `"host:port"` — lets a `listen(":0")` caller
        // discover the OS-assigned port.
        "addr" => Some(FnSig::plain(vec![], Ty::result(Ty::Str))),
        "close" => Some(FnSig::plain(vec![], Ty::Nil)),
        _ => None,
    }
}

/// Built-in method signatures on `Executor` (C5 escape hatch). `submit` takes a detached, zero-arg
/// task closure (its return value is discarded — `Unknown` ret accepts any `fn() -> _`).
fn executor_method_sig(method: &str) -> Option<FnSig> {
    let (params, ret) = match method {
        "submit" => (vec![Ty::Func { params: vec![], ret: Box::new(Ty::Unknown) }], Ty::Nil),
        "shutdown" => (vec![], Ty::Nil),
        "shutdown_now" => (vec![], Ty::Nil),
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

    // 22. A1: `Channel[T].try_recv()` is typed `T?` (Option[T]). A correct `int?` return checks
    // clean; a `str?` return is a type mismatch (pins the element-typed Option result).
    #[test]
    fn channel_try_recv_returns_option_of_elem() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "fn f() -> int?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
        assert!(check_entry(&ok).is_ok(), "expected clean: {:?}", errors(&ok));

        let bad = t.write(
            "bad.chz",
            "fn f() -> str?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(errs.iter().any(|m| m.contains("str")), "expected Option type mismatch: {errs:?}");
    }

    // 23. A1: `try_recv` takes no arguments — a call with one is rejected on arity.
    #[test]
    fn channel_try_recv_arity_rejected() {
        let t = TmpDir::new();
        let bad = t.write(
            "bad.chz",
            "fn f() -> int?:\n    ch := Channel[int]()\n    return ch.try_recv(5)\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter().any(|m| m.contains("try_recv") && m.contains("argument")),
            "got: {errs:?}"
        );
    }
}
