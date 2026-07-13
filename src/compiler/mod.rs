//! Bytecode compiler (M5): lowers a resolved module graph (or a single `Module`) to a [`Program`]
//! of function prototypes for the stack VM. The compiler is the *only* place that knows about slots
//! — locals resolve to operand-stack slots, and (M19 Phase 2b) module globals resolve to stable
//! per-module global slots here; the rest (struct/variant names, builtins) is resolved by name,
//! matching the tree-walk interpreter's resolution order so the VM reproduces its semantics exactly.
//!
//! Two passes:
//!   1. **Hoist** — register every module's struct / enum declarations into the program-global
//!      type tables (with the interpreter's "type already defined" collision), plus the built-in
//!      `Ok`/`Err`/`Some`/`None` variants.
//!   2. **Compile** — for each module emit a `<toplevel>` proto (top-level `fn`s hoisted first so
//!      forward references resolve) and one proto per `fn` / method / closure.

use crate::ast::{
    AssignOp, BinaryOp, Block, CompClause, CompKind, DeferTarget, Expr, ExprKind, FnDecl, Import,
    LitPattern, MatchArm, MatchExprArm, Module, Pattern, Span, SpawnTarget, Stmt, StmtKind, Type,
    UnaryOp, WaitArm, WaitTarget,
};
use crate::interpolation::{Chunk, parse_interpolation};
use crate::native::cffi::CType;
use crate::resolver::{ModuleGraph, ResolvedImport};
use crate::vm::op::{
    CapEntry, CapSrc, CffiDef, LIFECYCLE_HOOKS, ModuleProto, NO_IC, Op, Program, Proto, ProtoId,
    StructDef, SuiteInfo, VariantDef, WaitMeta,
};
use std::collections::HashMap;
use std::collections::HashSet;

mod peephole;

/// A compile-time failure (e.g. a malformed string interpolation). Carries a span so the CLI can
/// report it like any other error. Most user errors are caught earlier by the type checker; this
/// covers what only the compiler can see.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileError {
    pub message: String,
    pub span: Span,
}

/// The name a builtin resolves to (mirrors `interp::builtins::is_builtin` + the special `print`).
/// ROOT REDESIGN — the module key used to qualify user-type identity keys in the single-source
/// (standalone / `<main>`) compile + run paths, where there is no module graph to derive a real
/// label from. The three engines must agree on it (parity), so it lives here as one constant. Only
/// the test-only standalone paths use it (the CLI always runs the module-graph path).
#[cfg(test)]
pub(crate) const STANDALONE_MODULE_KEY: &str = "<main>";

/// Names dispatched to `Op::CallBuiltin(name, argc)` (name-keyed to the VM's `do_builtin`). A PURE
/// READ of the native-prelude table: the `Intrinsic::Builtin` fns (`ord`/`chr`/`panic`) plus every
/// `Intrinsic::Ctor` row — the phase-2a scalar conversions (`int`/`float`/`str`/`bytes`/`bytearray`)
/// AND the phase-2b GENERIC / reserved-type container ctors (`range`/`List`/`Map`/`Set`). The table is
/// the single source of truth for this whitelist (drift-guarded by
/// `prelude_table_is_single_source_of_truth`); the container ctors' generic TYPE-IDENTITY still lives
/// in the checker's `resolve_type`/`infer_named_call` (dispatch here, identity there). `print` is
/// `Intrinsic::Print`, NOT here — it keeps its own `CallPrint`/`CallPrintSep` opcodes. (`panic(msg)` →
/// `CallBuiltin("panic", 1)`; the VM's `do_builtin` arm raises the recoverable `RuntimeError` instead
/// of pushing a value.)
pub(crate) fn is_builtin(name: &str) -> bool {
    matches!(
        crate::checker::prelude_fn(name).map(|p| p.intrinsic),
        Some(crate::checker::Intrinsic::Builtin | crate::checker::Intrinsic::Ctor)
    )
}

/// Compile a whole resolved module graph in dependency order.
pub fn compile_graph(graph: &ModuleGraph) -> Result<Program, CompileError> {
    let mut c = Compiler::new();
    // FFI ROOT FIX (fix4): resolve every extern fn's C signature ONCE in the checker (module-scoped,
    // every alias spelling), then lower each extern from that table — never re-resolving alias names
    // in the backend's flat bare map (the collision whack-a-mole's root). Keyed by graph module idx.
    c.extern_sigs = crate::checker::resolve_extern_signatures(graph);
    // Swift-style keyword args through a function VALUE: the checker resolves each value+keyword call
    // to a positional slot permutation (module-scoped, keyed like extern sigs); the value path lowers
    // it to a plain positional `Op::Call`, so the runtime ABI stays positional.
    c.keyword_calls = crate::checker::resolve_keyword_calls(graph);
    // Pass 0: collision pre-pass — assign runtime keys for module-scoped user types. A type name
    // declared in exactly one module keeps its BARE name (the common case → unchanged Display/print
    // output); a name declared in ≥2 modules that are BOTH in the program is disambiguated (the
    // first/entry-most keeps bare, the rest get `<dotted.path>::Name`).
    c.assign_type_keys(graph);
    // Alias transparency for the backend's float coercion sites (`type F = float`): built BEFORE any
    // hoist, so a struct field / fn param / `let` annotation spelled through an alias still coerces.
    c.float_aliases = FloatAliases::collect(&float_alias_inputs(graph));
    // Pass 1: hoist all type declarations across every module. (No flat alias gather: the multi-file
    // path lowers every extern type from the checker-resolved, module-scoped `extern_sigs` table.)
    for (idx, lm) in graph.modules.iter().enumerate() {
        c.hoist_types(idx, &lm.ast.stmts)?;
    }
    // Pass 2: compile each module's toplevel + functions. The entry module is last (deps first);
    // only its `test fn`s / suites are recorded for `chezzi test` discovery.
    let entry_idx = graph.modules.len().saturating_sub(1);
    for (idx, lm) in graph.modules.iter().enumerate() {
        let toplevel = c.compile_module(idx, &lm.ast, &lm.imports, idx == entry_idx)?;
        let global_slots = std::mem::take(&mut c.global_slots);
        c.program.modules.push(ModuleProto {
            id: lm.id.clone(),
            label: lm.label(),
            toplevel,
            imports: lm.imports.clone(),
            native: lm.native,
            global_slots,
        });
    }
    c.program.field_ic_sites = c.field_ic_next;
    c.program.method_ic_sites = c.method_ic_next;
    Ok(c.program)
}

/// Compile a single in-memory module (test helper — no imports, treated as the entry).
#[cfg(test)]
pub fn compile_module_standalone(module: &Module) -> Result<Program, CompileError> {
    // Mirror the file-backed path: normalize named/default call arguments before compiling.
    let mut module = module.clone();
    crate::desugar::run_standalone(&mut module).map_err(|e| CompileError {
        message: e.message,
        span: e.span,
    })?;
    let module = &module;
    let mut c = Compiler::new();
    // Single-module: every type is locally declared, keyed `<main>::Name` (the standalone module
    // key). Record the declared type names (for the engines' from-import skip) and the per-module set.
    c.module_types = vec![std::collections::HashSet::new()];
    for s in &module.stmts {
        if let StmtKind::Struct { name, .. }
        | StmtKind::Enum { name, .. }
        | StmtKind::NewType { name, .. }
        | StmtKind::TypeAlias { name, .. } = &s.kind
        {
            c.module_types[0].insert(name.clone());
            c.program.type_names.insert(name.clone());
            c.type_keys.insert(
                (0, name.clone()),
                format!("{STANDALONE_MODULE_KEY}::{name}"),
            );
        }
    }
    c.float_aliases =
        FloatAliases::collect(&[(alias_decls(&module.stmts), HashMap::new(), HashMap::new())]);
    c.hoist_types(0, &module.stmts)?;
    // SINGLE-RESOLVER: extern C types come from the checker's standalone pass — the SAME resolver the
    // multi-file CLI uses (no second backend resolver exists). The backend reads this table verbatim.
    c.extern_sigs = crate::checker::resolve_extern_signatures_standalone(&module.stmts);
    c.keyword_calls = crate::checker::resolve_keyword_calls_standalone(&module.stmts);
    let toplevel = c.compile_module(0, module, &[], true)?;
    let global_slots = std::mem::take(&mut c.global_slots);
    // A synthetic module id so the run driver has something to key the namespace cache on.
    let id = crate::resolver::ModuleId(std::path::PathBuf::from("<main>"));
    c.program.modules.push(ModuleProto {
        id,
        label: "<main>".to_string(),
        toplevel,
        imports: Vec::new(),
        native: None,
        global_slots,
    });
    c.program.field_ic_sites = c.field_ic_next;
    c.program.method_ic_sites = c.method_ic_next;
    Ok(c.program)
}

struct Compiler {
    program: Program,
    /// Struct name → declared fields (with types), kept for building `json.decode` descriptors.
    struct_fields: HashMap<String, Vec<crate::ast::Field>>,
    /// Struct key → its declared GENERIC type-param names (`struct S[F]` → `{"F"}`). Read by
    /// `compile_ctor_args`, whose field types are written in the STRUCT's scope, not the caller's:
    /// a field `v: F` must not be treated as a float alias when `F` is the struct's own type param.
    struct_generics: HashMap<String, std::collections::HashSet<String>>,
    /// The GENERIC type-param names currently in scope (enclosing type's `[T]` + the fn's own `[U]`).
    /// A type param SHADOWS a module-level `type F = float` alias, exactly as it does in the checker's
    /// scoped `resolve_type` — so every `FloatAliases` lookup below must exclude these names, or the
    /// backend coerces a value whose static type is the type VARIABLE (a runtime `Float` under a
    /// static `int`, or a hard fault on a `str` instantiation, on a check-clean program).
    float_shadow: std::collections::HashSet<String>,
    /// M19 Phase 2b — the current module's global name → slot map, rebuilt at the start of each
    /// `compile_module`. Shared across the toplevel proto and every fn/method/closure compiled for
    /// the module, so a global reference anywhere in the module resolves to the same slot.
    globals: HashMap<String, u32>,
    /// The CURRENT module's top-level `fn` names (rebuilt per `compile_module` in `collect_globals`).
    /// Drives the generic-fn-as-value turbofish erase: a `Name[TypeArgs]` whose `Name` is a
    /// (non-shadowed) top-level fn is a generic-fn turbofish the checker already accepted, so the index
    /// is dropped and only the plain fn value is loaded (runtime is generic-ERASED). The exact
    /// same-module set the checker's `local_fn_names` gates on, keeping accept ⟺ erase in lockstep.
    fn_names: std::collections::HashSet<String>,
    /// The current module's globals in slot order (slot `i` ⇒ `global_slots[i]`), recorded into the
    /// module's [`ModuleProto`] so the run driver can pre-size storage + build its name→slot index.
    global_slots: Vec<String>,
    /// M19 Phase 4 — next struct-field inline-cache site id. Allocated densely across the WHOLE
    /// program (every module shares this `Compiler`), so each `GetField`/`SetField` on a struct
    /// field gets a unique slot into the VM's `field_ic` vector. Recorded into `Program::field_ic_sites`.
    field_ic_next: u32,
    /// M19 Phase 6 — next method-call inline-cache site id, allocated densely across the whole program
    /// (like `field_ic_next`). Each `CallMethod` op gets a unique slot into the VM's `method_ic`
    /// vector. Recorded into `Program::method_ic_sites`.
    method_ic_next: u32,
    /// ROOT REDESIGN — module-scoped types: `(declaring module_idx, bare type name) → IDENTITY KEY`.
    /// The key is ALWAYS `<module-key>::<Name>` (no winner/loser, no bare keys) so every user
    /// struct/enum/variant/alias is unique by construction. Built once by
    /// [`Compiler::assign_type_keys`] before any module compiles. A name NOT in this map is a
    /// reserved/native type (`Result`/`Option`/`Match`/`Response`/`Ref`/`Iterator`/FFI widths),
    /// which keeps its bare name (those never module-key).
    type_keys: HashMap<(usize, String), String>,
    /// `module_idx → its declared user type names` (struct/enum/alias). Drives the collision detection
    /// and resolves a qualified `geo.X` to the right module's key.
    module_types: Vec<std::collections::HashSet<String>>,
    /// STATIC (associated) methods, keyed `"{type_runtime_key}\u{1}{method}"` — a struct/enum method
    /// whose first param is not `self` (the "no self ⇒ static" rule). Populated in `hoist_types` (all
    /// types across all modules are seen there before any body compiles), so a `Type.method(...)` call
    /// site in any module classifies the call as `Op::CallStatic` exactly like the checker does. The
    /// checker has already validated the call shape; this only drives compiler dispatch.
    static_methods: std::collections::HashSet<String>,
    /// The CURRENT module's bound module name → that module's index, rebuilt per `compile_module`
    /// (mirrors the checker's `imported_modules`). Lets `geo.Point(...)` resolve the qualified key.
    imported_modules: HashMap<String, usize>,
    /// The CURRENT module's index, set at the top of `compile_module` — the home module whose
    /// locally-declared types use its own `type_keys` entry.
    current_module_idx: usize,
    /// The CURRENT module's bare-resolvable type names → their runtime key: locally-declared types
    /// plus `from`-imported ones (rebuilt per `compile_module`). The bare struct/enum constructor
    /// only fires for a name in this set — a type merely present in the global `program.structs`
    /// (declared in another module, imported whole or not at all) is NOT bare-constructible here, so
    /// a `from`-imported function named like some other module's type still resolves as a call.
    bare_types: HashMap<String, String>,
    /// FFI ROOT FIX (fix4): the checker-resolved C signature of every `extern` fn, keyed by `(graph
    /// module index, fn name)`. The extern lowering consumes THIS instead of re-resolving alias names
    /// itself — so every qualified/imported/aliased width resolves in its DEFINING module's scope
    /// (collision-proof). Filled once in `compile_graph` (multi-file) or by
    /// `resolve_extern_signatures_standalone` (single-file) — the ONE extern-type resolver.
    extern_sigs: crate::checker::ExternTable,
    /// Swift-style keyword-arg resolution for VALUE calls, keyed `(graph module index, call span)`;
    /// `perm[i]` = index into `[positional args ++ named exprs]` that fills parameter slot `i`. Filled
    /// in `compile_graph`/`compile_module_standalone` from the checker's `resolve_keyword_calls`;
    /// consumed by the value path of `compile_call` to emit a positional `Op::Call`.
    keyword_calls: crate::checker::KeywordTable,
    /// String-interpolation fragment discriminators for the [`crate::checker::KeywordKey`]: the
    /// whole-string span + the fragment's 0-based ordinal, maintained (save/restore) around each
    /// fragment in `compile_str`. Mirrors the checker's `kw_frag_ctx`/`kw_frag_ord` so the keyword-call
    /// key computed at lookup matches the one the checker recorded. Inert (`Span::default()`/`0`)
    /// outside interpolation.
    kw_frag_ctx: crate::lexer::Span,
    kw_frag_ord: usize,
    /// One-way int→float widening — the element-coercion hint for the collection literal currently
    /// being compiled as a typed `let` value (`xs: List[float] = [..]`). Set transiently by
    /// `compile_stmt`'s `Let` arm around the value compile and consumed by the `List`/`Map`/`Set` arms
    /// of `compile_expr` so int ELEMENTS widen to float. `None` outside an annotated collection let.
    float_elem_hint: Option<crate::ast::ElemFloatHint>,
    /// Type-ALIAS names that mean `float` (`type F = float`, `type G = F`, `type H = m.F`). Every
    /// float coercion site below keys on the SYNTACTIC declared type, while the checker keys on the
    /// RESOLVED `Ty::Float` — without this table an alias-spelled `float` sink (`x: F = 1`,
    /// `fn g(z: F)`, `-> F`, a `v: F` field) would check clean and lower with NO `Op::CoerceFloat`,
    /// leaving a runtime `Int` under a static `float`. Built once per graph (see [`FloatAliases`]).
    float_aliases: FloatAliases,
}

/// The alias names that resolve to `float`, per module — the backend's alias-transparency table (the
/// checker gets this for free from `resolve_type`). Built once from the module graph, before any
/// type is hoisted, so every coercion site can ask "is this declared type a `float`?" in the scope of
/// the module that WROTE it.
#[derive(Default)]
struct FloatAliases {
    /// `(declaring module idx, alias name)` for every alias resolving to `float` — the target of a
    /// qualified `m.F` lookup, and of a struct field declared in another module.
    decl: std::collections::HashSet<(usize, String)>,
    /// module idx → the names usable BARE there that mean `float` (its own aliases + `from`-imported).
    bare: Vec<std::collections::HashSet<String>>,
    /// module idx → bound module name → that module's idx (for a qualified `m.F` annotation).
    binds: Vec<HashMap<String, usize>>,
}

impl FloatAliases {
    /// True iff the type `ty`, as WRITTEN in module `idx`, means `float` (directly or through an alias
    /// chain). Mirrors `crate::ast::is_float_ty` with alias resolution added. `shadow` holds the
    /// GENERIC type-param names in scope at the site: a type param SHADOWS a same-named module alias
    /// (the checker resolves it to a `Ty::Param`), so it is never a float sink.
    fn is_float(&self, idx: usize, ty: &Type, shadow: &std::collections::HashSet<String>) -> bool {
        match ty {
            Type::Named { name, .. } if shadow.contains(name) => false,
            Type::Named { name, .. } => {
                name == "float" || self.bare.get(idx).is_some_and(|s| s.contains(name))
            }
            Type::Qualified { module, name, args } if args.is_empty() => self
                .binds
                .get(idx)
                .and_then(|b| b.get(module))
                .is_some_and(|j| self.decl.contains(&(*j, name.clone()))),
            _ => false,
        }
    }

    /// The collection element-widening hint for a `let` annotation written in module `idx`:
    /// `List[float]` → `Elem`, `Map[_, float]` → `MapValue` (float ELEMENT aliases resolved, generic
    /// type params shadowed). Matches the SYNTACTIC `List[…]`/`Map[…]` shape only — a whole-collection
    /// alias (`type LF = List[float]`) is NOT a hint here, so the checker (whose twin gate keys on the
    /// same syntactic shape) must not license the widen for one either.
    fn elem_hint(
        &self,
        idx: usize,
        ty: &Type,
        shadow: &std::collections::HashSet<String>,
    ) -> Option<crate::ast::ElemFloatHint> {
        match ty {
            Type::Generic(n, args, ..)
                if n == "List" && args.len() == 1 && self.is_float(idx, &args[0], shadow) =>
            {
                Some(crate::ast::ElemFloatHint::Elem)
            }
            Type::Generic(n, args, ..)
                if n == "Map" && args.len() == 2 && self.is_float(idx, &args[1], shadow) =>
            {
                Some(crate::ast::ElemFloatHint::MapValue)
            }
            _ => None,
        }
    }

    /// Collect every `type … = float` alias (transitively, across modules). Each module contributes
    /// its top-level aliases, its bound module names, and its `from`-imported names (bound name →
    /// source module + declared name).
    fn collect(modules: &[AliasInputs]) -> FloatAliases {
        let n = modules.len();
        let mut decl: std::collections::HashSet<(usize, String)> = std::collections::HashSet::new();
        // Fixpoint over the alias graph (`type G = F` may precede `type F = float`, in this or another
        // module); it terminates in at most one round per alias, and a cycle simply never resolves.
        let total: usize = modules.iter().map(|(a, _, _)| a.len()).sum();
        for _ in 0..=total {
            let mut changed = false;
            for (i, (aliases, binds, froms)) in modules.iter().enumerate() {
                for (name, ty) in aliases {
                    if decl.contains(&(i, name.clone())) {
                        continue;
                    }
                    let is_float = match ty {
                        Type::Named { name: n2, .. } => {
                            n2 == "float"
                                || decl.contains(&(i, n2.clone()))
                                || froms
                                    .get(n2)
                                    .is_some_and(|(j, orig)| decl.contains(&(*j, orig.clone())))
                        }
                        Type::Qualified {
                            module,
                            name: n2,
                            args,
                        } if args.is_empty() => binds
                            .get(module)
                            .is_some_and(|j| decl.contains(&(*j, n2.clone()))),
                        _ => false,
                    };
                    if is_float {
                        decl.insert((i, name.clone()));
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        let bare = (0..n)
            .map(|i| {
                let mut names: std::collections::HashSet<String> = decl
                    .iter()
                    .filter(|(j, _)| *j == i)
                    .map(|(_, name)| name.clone())
                    .collect();
                // A `from`-imported alias is visible under its BOUND name (`import F as G from m`).
                for (bound, (j, orig)) in &modules[i].2 {
                    if decl.contains(&(*j, orig.clone())) {
                        names.insert(bound.clone());
                    }
                }
                names
            })
            .collect();
        let binds = modules.iter().map(|(_, b, _)| b.clone()).collect();
        FloatAliases { decl, bare, binds }
    }
}

/// Per-module inputs to [`FloatAliases::collect`]: top-level type aliases, bound module name → idx,
/// and `from`-imported BOUND name → (source module idx, declared name).
type AliasInputs = (
    Vec<(String, Type)>,
    HashMap<String, usize>,
    HashMap<String, (usize, String)>,
);

/// Gather [`AliasInputs`] for every module in the graph.
fn float_alias_inputs(graph: &ModuleGraph) -> Vec<AliasInputs> {
    let idx_of: HashMap<&crate::resolver::ModuleId, usize> = graph
        .modules
        .iter()
        .enumerate()
        .map(|(i, m)| (&m.id, i))
        .collect();
    graph
        .modules
        .iter()
        .map(|lm| {
            let aliases = alias_decls(&lm.ast.stmts);
            let mut binds = HashMap::new();
            let mut froms = HashMap::new();
            for imp in &lm.imports {
                let Some(&j) = idx_of.get(&imp.target) else {
                    continue;
                };
                match &imp.import {
                    crate::ast::Import::Module { path, alias, .. } => {
                        let bind = alias
                            .clone()
                            .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                        binds.insert(bind, j);
                    }
                    crate::ast::Import::From { names, .. } => {
                        for (name, alias) in names {
                            let bound = alias.clone().unwrap_or_else(|| name.clone());
                            froms.insert(bound, (j, name.clone()));
                        }
                    }
                }
            }
            (aliases, binds, froms)
        })
        .collect()
}

/// The top-level `type X = …` declarations of a module.
fn alias_decls(stmts: &[Stmt]) -> Vec<(String, Type)> {
    stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::TypeAlias { name, ty, .. } => Some((name.clone(), ty.clone())),
            _ => None,
        })
        .collect()
}

/// All-constant int→float widening peephole: true iff `exprs` contains BOTH an untyped float constant
/// and an untyped int constant (the trigger to widen the untyped-int-constant siblings of an
/// un-annotated mixed collection like `[1, 2.3]`, `[1, -2.5]`, `[1 + 1, 2.5]`). Only
/// [`crate::ast::const_num`] expressions count — anything TYPED (a variable, a call) is neither, so a
/// mixed collection with a typed int element never fires here. That is sound because the CHECKER now
/// rejects exactly those (`elem_widen_ok` licenses a widen only where this peephole — or the `let`
/// element hint below — is guaranteed to coerce); the two share `crate::ast::const_num`, so they
/// cannot drift.
pub(crate) fn literal_numeric_mix<'a>(exprs: impl Iterator<Item = &'a Expr>) -> bool {
    let mut has_int = false;
    let mut has_float = false;
    for e in exprs {
        match crate::ast::const_num(e) {
            Some(crate::ast::ConstNum::Int) => has_int = true,
            Some(crate::ast::ConstNum::Float) => has_float = true,
            None => {}
        }
    }
    has_int && has_float
}

/// M19 lever #2 — register an enum variant into BOTH program tables, assigning it the next dense
/// `variant_id` (`variants_by_id.len()`, a global gap-free counter — the analogue of how `StructDef`
/// gets its `tid` from `structs.len()`). Keeps `variants` (`(enum, variant)`→def) and `variants_by_id`
/// (id→def) in lockstep so cold-path id→names resolution is O(1). The map is keyed by the
/// `(enum, variant)` pair, so two enums may share a variant name yet get distinct ids.
/// The "no self ⇒ static" classification primitive (must match the checker exactly): a method whose
/// FIRST param is not named `self` — or which has no params — is a STATIC (associated) method.
fn is_static_method(m: &FnDecl) -> bool {
    m.params.first().is_none_or(|p| p.name != "self")
}

/// Key into [`Compiler::static_methods`]: the type's runtime key + the method name, joined by a
/// control char that can never appear in a type key or identifier.
fn static_key(type_key: &str, method: &str) -> String {
    format!("{type_key}\u{1}{method}")
}

/// The TYPE name in a declaration-site turbofish member-access head — the `obj` of a
/// `Type[T…].member`/`Type[T…].member(args)`. Both carriers converge: the SINGLE-arg `Index{Ident,
/// …}` (the parser can't tell it from `arr[i].field`, so the type args ride the index) and the
/// MULTI-arg `TypeApply{name, …}`. The type args are runtime-erased, so the compiler needs only the
/// name. Returns `None` for any other `obj` shape.
fn type_apply_head_name(kind: &ExprKind) -> Option<&str> {
    match kind {
        ExprKind::TypeApply { name, .. } => Some(name),
        ExprKind::Index { obj, .. } => match &obj.kind {
            ExprKind::Ident(n) => Some(n),
            _ => None,
        },
        _ => None,
    }
}

fn register_variant(program: &mut Program, enum_name: &str, variant: &str, arity: usize) {
    let variant_id = program.variants_by_id.len() as u32;
    let def = VariantDef {
        enum_name: enum_name.to_string(),
        name: variant.to_string(),
        arity,
        variant_id,
    };
    program.variants_by_id.push(def.clone());
    program
        .variants
        .insert((enum_name.to_string(), variant.to_string()), def);
}

impl Compiler {
    fn new() -> Self {
        let mut program = Program {
            protos: Vec::new(),
            structs: HashMap::new(),
            enum_methods: HashMap::new(),
            enum_home: HashMap::new(),
            newtype_methods: HashMap::new(),
            newtype_home: HashMap::new(),
            variants: HashMap::new(),
            variants_by_id: Vec::new(),
            modules: Vec::new(),
            field_ic_sites: 0,
            method_ic_sites: 0,
            cffi_defs: Vec::new(),
            tests: Vec::new(),
            suites: Vec::new(),
            type_names: std::collections::HashSet::new(),
        };
        // Built-in Result / Option variants, available without declaration. M19 lever #2 — these are
        // registered FIRST so they get the fixed dense ids `Ok`=VID_OK(0), `Err`=VID_ERR(1),
        // `Some`=VID_SOME(2), `None`=VID_NONE_VARIANT(3) that `?`/error-gating compare against (the
        // order of this array IS the id assignment; assert it matches the op.rs constants below).
        for (v, e, arity) in [
            ("Ok", "Result", 1),
            ("Err", "Result", 1),
            ("Some", "Option", 1),
            ("None", "Option", 0),
        ] {
            register_variant(&mut program, e, v, arity);
        }
        let vid =
            |p: &Program, e: &str, v: &str| p.variants[&(e.to_string(), v.to_string())].variant_id;
        debug_assert_eq!(vid(&program, "Result", "Ok"), crate::vm::op::VID_OK);
        debug_assert_eq!(vid(&program, "Result", "Err"), crate::vm::op::VID_ERR);
        debug_assert_eq!(vid(&program, "Option", "Some"), crate::vm::op::VID_SOME);
        debug_assert_eq!(
            vid(&program, "Option", "None"),
            crate::vm::op::VID_NONE_VARIANT
        );
        // M19 memory-layout lever #1 — register the synthetic native std structs (`Match` from
        // `std.regex`, `Response` from `std.request`, `ProcResult` from `std.process`). They have no
        // AST (the checker seeds their shapes in `seed_stdlib_structs`), so with the positional
        // struct layout the runtime must know their declaration-order field names HERE to resolve
        // field reads + Display. Order must match the native builders (`match_to_ret` /
        // `response_ret` / `proc_result_ret`) and the checker's seed.
        for (name, fields) in [
            ("Match", &["text", "start", "end", "groups"][..]),
            ("Response", &["status", "body", "headers"][..]),
            ("ProcResult", &["stdout", "stderr", "code"][..]),
        ] {
            let tid = program.structs.len() as u32;
            program.structs.insert(
                name.to_string(),
                StructDef {
                    fields: fields.iter().map(|f| f.to_string()).collect(),
                    methods: HashMap::new(),
                    module_idx: 0,
                    tid,
                    test_methods: Vec::new(),
                    // Native std structs are not module-keyed; key == display name.
                    display_name: name.to_string(),
                },
            );
        }
        Compiler {
            program,
            struct_fields: HashMap::new(),
            struct_generics: HashMap::new(),
            float_shadow: std::collections::HashSet::new(),
            globals: HashMap::new(),
            fn_names: std::collections::HashSet::new(),
            global_slots: Vec::new(),
            field_ic_next: 0,
            method_ic_next: 0,
            type_keys: HashMap::new(),
            module_types: Vec::new(),
            static_methods: std::collections::HashSet::new(),
            imported_modules: HashMap::new(),
            current_module_idx: 0,
            bare_types: HashMap::new(),
            extern_sigs: crate::checker::ExternTable::new(),
            keyword_calls: crate::checker::KeywordTable::new(),
            kw_frag_ctx: crate::lexer::Span::default(),
            kw_frag_ord: 0,
            float_elem_hint: None,
            float_aliases: FloatAliases::default(),
        }
    }

    /// ROOT REDESIGN — Pass 0: assign the canonical IDENTITY KEY to every module-scoped user type.
    /// Scans every (non-native) module's struct / enum / alias names and keys EACH one
    /// `<module-key>::<Name>` (via [`crate::resolver::module_keys`]) — no winner/loser, no bare keys,
    /// so every user type is unique by construction (a cross-module name clash is just two distinct
    /// keys). Reserved/native names are never user-declared here, so they keep their bare name (absent
    /// from this map). Also seeds `program.type_names` (the engines' `bind_import` skips from-imported
    /// type members) and `module_types` (per-module declared names, deps-first).
    fn assign_type_keys(&mut self, graph: &ModuleGraph) {
        self.module_types = vec![std::collections::HashSet::new(); graph.modules.len()];
        let mkeys = crate::resolver::module_keys(graph);
        for (idx, lm) in graph.modules.iter().enumerate() {
            if let Some(nat) = lm.native {
                // A native std module has no AST, but `std.regex`/`std.request`/`std.process` each own
                // ONE synthetic struct (`Match`/`Response`/`ProcResult`) the checker now treats as a
                // module-owned type. Register its bare name so `module.Struct(...)` / a bare import +
                // `Struct(...)` resolve to the `NewStruct` ctor (the struct's layout is registered in
                // `Compiler::new`). Name kept BARE (no `type_keys` entry → `type_key` falls back to it).
                if let Some(sname) = match nat {
                    "std.regex" => Some("Match"),
                    "std.request" => Some("Response"),
                    "std.process" => Some("ProcResult"),
                    _ => None,
                } {
                    self.module_types[idx].insert(sname.to_string());
                    self.program.type_names.insert(sname.to_string());
                }
                continue;
            }
            // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (no qualified
            // key entry → `type_key` falls back to bare), so `Ref`/`Iterator`/FFI widths resolve bare.
            let is_std = lm.is_std();
            for s in &lm.ast.stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::NewType { name, .. }
                | StmtKind::TypeAlias { name, .. } = &s.kind
                {
                    self.module_types[idx].insert(name.clone());
                    self.program.type_names.insert(name.clone());
                    if !is_std {
                        self.type_keys
                            .insert((idx, name.clone()), format!("{}::{name}", mkeys[idx]));
                    }
                }
            }
        }
    }

    /// The IDENTITY KEY for a type `name` declared in module `module_idx` (always `<module-key>::Name`
    /// for a user type). A name absent from the map is a reserved/native type — return it bare.
    fn type_key(&self, module_idx: usize, name: &str) -> String {
        self.type_keys
            .get(&(module_idx, name.to_string()))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// The TYPE name + module-scoped identity key of a QUALIFIED declaration-site turbofish head —
    /// the `mod.Type[int]` carrier (`Index{obj: Field{Ident(mod), Type}, ..}`) of a
    /// `mod.Type[int].Variant(args)` / `.staticmethod(args)`. The type args are runtime-erased, so
    /// only the key is needed. Gated on a non-local, non-captured bound module Ident declaring the
    /// type (mirrors the bare `type_apply_head_name` but for a whole-module-imported base — B1).
    /// Multi-arg (`mod.Type[int, str]`) has no qualified parser carrier, so it is not recognized here.
    fn qualified_turbofish_key(&self, fc: &FnComp, kind: &ExprKind) -> Option<(String, String)> {
        if let ExprKind::Index { obj, .. } = kind
            && let ExprKind::Field {
                obj: mobj, name, ..
            } = &obj.kind
            && let ExprKind::Ident(mname) = &mobj.kind
            && fc.resolve_local(mname).is_none()
            && !fc.captures(mname)
            && let Some(&tidx) = self.imported_modules.get(mname)
            && self
                .module_types
                .get(tidx)
                .is_some_and(|t| t.contains(name))
        {
            return Some((name.clone(), self.type_key(tidx, name)));
        }
        None
    }

    /// ROOT REDESIGN — build the module-aware [`crate::json_decode::DecodeEnv`] for the CURRENT module
    /// so `json.decode[T]` resolves its target (and nested field types) to qualified identity keys.
    fn decode_env(&self) -> CompilerDecodeEnv<'_> {
        CompilerDecodeEnv { c: self }
    }

    /// The IDENTITY KEY for a bare-written enum name `ename` in the CURRENT module: its `bare_types`
    /// key when it is a bare-visible user enum (local / `from`-imported / std). ROOT REDESIGN — a
    /// WHOLE-module-imported enum (`Color` matched as `Color.Red` against a `geo::Color` value) is NOT
    /// bare-visible, so resolve it through the imported modules: pick an imported module that declares
    /// `ename` whose qualified key is a registered enum (deterministic — a match pattern's enum is
    /// uniquely the scrutinee's, and a genuine local collision is resolved first via `bare_types`).
    /// Falls back to the bare name (built-in `Result`/`Option`, or a miss that fall-throughs).
    fn enum_bare_key(&self, ename: &str) -> String {
        if let Some(k) = self.bare_types.get(ename) {
            return k.clone();
        }
        for &tidx in self.imported_modules.values() {
            if self
                .module_types
                .get(tidx)
                .is_some_and(|t| t.contains(ename))
            {
                let key = self.type_key(tidx, ename);
                if self.program.variants.keys().any(|(e, _)| *e == key) {
                    return key;
                }
            }
        }
        ename.to_string()
    }

    /// M19 Phase 6 — allocate an inline-cache site id for a `CallMethod` op. Every method/module-member
    /// call gets a fresh dense id (the VM fills it only for struct-method dispatch; module/core-type
    /// calls leave it empty, costing one unused `method_ic` slot — negligible).
    fn next_method_ic(&mut self) -> u32 {
        let id = self.method_ic_next;
        self.method_ic_next += 1;
        id
    }

    /// M19 Phase 4 — allocate an inline-cache site id for a field op. Numeric field names are tuple
    /// element access (`t.0`/`t.1`; identifiers can never start with a digit), which dispatches to
    /// the tuple arm and never reads the IC — those get [`NO_IC`] and consume no slot. Every other
    /// (identifier) field op gets a fresh dense id.
    fn next_field_ic(&mut self, name: &str) -> u32 {
        if name.bytes().all(|b| b.is_ascii_digit()) {
            NO_IC
        } else {
            let id = self.field_ic_next;
            self.field_ic_next += 1;
            id
        }
    }

    /// M19 Phase 2b — resolve a module-global name to its compile-time slot. The checker rejects
    /// undefined names before compilation, so every name reaching a global load/store/define site is
    /// guaranteed to have been collected by [`Compiler::collect_globals`].
    fn global_slot(&self, name: &str) -> u32 {
        *self.globals.get(name).unwrap_or_else(|| {
            panic!("compiler: global '{name}' has no slot (checker should reject undefined names)")
        })
    }

    /// M19 Phase 2b — pre-scan a module's globals into `self.globals`/`self.global_slots` before any
    /// code is emitted, so forward references (a fn body reading a global declared later, an import
    /// used before its line) resolve to a stable slot. Order: imports, then top-level `fn`s, then
    /// top-level `let`s — only internal consistency matters (the run driver reads the same list).
    fn collect_globals(&mut self, imports: &[ResolvedImport], stmts: &[Stmt]) {
        use crate::ast::Import;
        self.globals.clear();
        self.global_slots.clear();
        self.fn_names.clear();
        let add = |name: String, globals: &mut HashMap<String, u32>, slots: &mut Vec<String>| {
            if !globals.contains_key(&name) {
                globals.insert(name.clone(), slots.len() as u32);
                slots.push(name);
            }
        };
        for imp in imports {
            match &imp.import {
                Import::Module { path, alias, .. } => {
                    let name = alias
                        .clone()
                        .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                    add(name, &mut self.globals, &mut self.global_slots);
                }
                Import::From { names, .. } => {
                    for (member, alias) in names {
                        add(
                            alias.clone().unwrap_or_else(|| member.clone()),
                            &mut self.globals,
                            &mut self.global_slots,
                        );
                    }
                }
            }
        }
        for stmt in stmts {
            if let StmtKind::Fn(decl) = &stmt.kind {
                add(decl.name.clone(), &mut self.globals, &mut self.global_slots);
                // Same-module top-level fn — licenses the generic-fn-as-value turbofish erase below.
                self.fn_names.insert(decl.name.clone());
            }
            // Each extern C fn is a module global bound at init (like a top-level `fn`).
            if let StmtKind::Extern { fns, .. } = &stmt.kind {
                for ef in fns {
                    add(ef.name.clone(), &mut self.globals, &mut self.global_slots);
                }
            }
        }
        for stmt in stmts {
            if let StmtKind::Let { names, .. } = &stmt.kind {
                for name in names {
                    add(name.clone(), &mut self.globals, &mut self.global_slots);
                }
            }
        }
    }

    /// Pass 1: register struct / enum declarations into the program-global tables under their
    /// module-scoped RUNTIME KEY (bare in the no-collision case; `<dotted>::Name` on a real clash).
    fn hoist_types(&mut self, module_idx: usize, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Struct {
                    name,
                    fields,
                    methods,
                    type_params,
                    ..
                } => {
                    let key = self.type_key(module_idx, name);
                    // The struct's own generic params — they shadow a module float alias in its FIELD
                    // types (read by `compile_ctor_args`, which runs in the CALLER's scope).
                    self.struct_generics.insert(
                        key.clone(),
                        type_params.iter().map(|tp| tp.name.clone()).collect(),
                    );
                    // Record each STATIC method (first param not `self`) so a `Type.method(...)` call
                    // site classifies it as `Op::CallStatic`, mirroring the checker's classification.
                    for m in methods {
                        if is_static_method(m) {
                            self.static_methods.insert(static_key(&key, &m.name));
                        }
                    }
                    if self.program.structs.contains_key(&key) {
                        return Err(CompileError {
                            message: format!("type '{name}' is already defined"),
                            span: stmt.span,
                        });
                    }
                    // M19 Phase 5b — a dense, declaration-order type id (the map only grows, dup names
                    // are rejected above, so the pre-insert count is a stable unique id per layout).
                    let tid = self.program.structs.len() as u32;
                    self.program.structs.insert(
                        key.clone(),
                        StructDef {
                            fields: fields.iter().map(|f| f.name.clone()).collect(),
                            methods: HashMap::new(),
                            // ROOT REDESIGN — set the DECLARING module here (pass 1) so it is correct
                            // even for a struct with no methods (pass 2 only filled it inside the
                            // methods loop). `json.decode` field-type resolution relies on this.
                            module_idx,
                            tid,
                            test_methods: Vec::new(), // filled in pass 2
                            // ROOT REDESIGN — bare name for display; `key` is the identity it's keyed by.
                            display_name: name.clone(),
                        },
                    );
                    self.struct_fields.insert(key, fields.clone());
                }
                // Type-erased: type parameters are checker-only, the runtime is identical for
                // `Tree[int]` and `Tree[str]`.
                StmtKind::Enum {
                    name,
                    variants,
                    methods,
                    ..
                } => {
                    // M19 lever #2 — assign each variant the next dense `variant_id` (user variants
                    // follow the fixed native ids at `4..`, in declaration order). The enum is keyed
                    // by its module-scoped runtime key (bare unless a cross-module clash).
                    let key = self.type_key(module_idx, name);
                    for v in variants {
                        register_variant(&mut self.program, &key, &v.name, v.payload.len());
                    }
                    for m in methods {
                        if is_static_method(m) {
                            self.static_methods.insert(static_key(&key, &m.name));
                        }
                    }
                }
                // A newtype's key must be known (via `newtype_home`) BEFORE any method body compiles,
                // so that a `Name(...)` ctor inside a method resolves as `Op::NewType`, not a global
                // call. Methods themselves are compiled (into `newtype_methods`) in pass 2.
                StmtKind::NewType { name, .. } => {
                    let key = self.type_key(module_idx, name);
                    self.program.newtype_home.insert(key, module_idx);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Pass 2: compile one module to a toplevel proto; record method protos into the type table.
    fn compile_module(
        &mut self,
        module_idx: usize,
        module: &Module,
        imports: &[ResolvedImport],
        is_entry: bool,
    ) -> Result<ProtoId, CompileError> {
        // M19 Phase 2b: assign a stable slot to every module global before emitting any code, so
        // forward references (method/fn bodies, imports used before their line) resolve to a slot.
        self.collect_globals(imports, &module.stmts);
        // Module-scoped types: record this module's index + its imported module bindings, so a
        // qualified `geo.Point(...)` resolves to the right module's runtime key.
        self.current_module_idx = module_idx;
        self.imported_modules.clear();
        // Bare-resolvable type names in THIS module → runtime key: locally declared first.
        self.bare_types.clear();
        if let Some(own) = self.module_types.get(module_idx) {
            for name in own.clone() {
                let key = self.type_key(module_idx, &name);
                self.bare_types.insert(name, key);
            }
        }
        for imp in imports {
            match &imp.import {
                Import::Module { path, alias, .. } => {
                    let bind = alias
                        .clone()
                        .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                    if let Some(tidx) = self.program.module_index(&imp.target) {
                        self.imported_modules.insert(bind, tidx);
                        // A whole-module import of a STDLIB module exposes its types BARE too (mirrors
                        // the checker's std exception — e.g. the `Ref` from `import std.ref`). User
                        // whole-module imports do NOT (their types are only reachable qualified).
                        if path.first().map(String::as_str) == Some("std")
                            && let Some(types) = self.module_types.get(tidx)
                        {
                            for name in types.clone() {
                                let key = self.type_key(tidx, &name);
                                self.bare_types.entry(name).or_insert(key);
                            }
                        }
                    }
                }
                Import::From { names, .. } => {
                    if let Some(tidx) = self.program.module_index(&imp.target) {
                        for (member, alias) in names {
                            // A `from`-imported user type becomes bare-resolvable under its bind name,
                            // keyed by the DECLARING module's runtime key.
                            if self
                                .module_types
                                .get(tidx)
                                .is_some_and(|t| t.contains(member))
                            {
                                let key = self.type_key(tidx, member);
                                let bind = alias.clone().unwrap_or_else(|| member.clone());
                                self.bare_types.insert(bind, key);
                            }
                        }
                    }
                }
            }
        }
        // `Ref` is a reserved global backing the `ref` keyword: std.ref is ALWAYS linked into the
        // graph, and its `struct Ref[T]` is keyed BARE ("Ref"), so expose the bare name import-free in
        // EVERY module (mirrors the checker's always-present `Ref` seed). Guarded on the struct actually
        // being registered so it's a no-op if std.ref somehow isn't present. `or_insert` never clobbers
        // a local — `Ref` is reserved, so there can be no user `struct Ref` to disambiguate.
        if self.program.structs.contains_key("Ref") {
            self.bare_types
                .entry("Ref".to_string())
                .or_insert_with(|| "Ref".to_string());
        }
        // Compile struct methods first, recording their proto ids + this module as their home.
        for stmt in &module.stmts {
            if let StmtKind::Struct {
                name,
                methods,
                fields,
                type_params,
                ..
            } = &stmt.kind
            {
                let key = self.type_key(module_idx, name);
                // The struct's `[T]`s are in scope for every method body + field default below (they
                // shadow a same-named module `type T = float` alias at each coercion site).
                let prev_shadow = std::mem::replace(
                    &mut self.float_shadow,
                    type_params.iter().map(|tp| tp.name.clone()).collect(),
                );
                let mut test_methods: Vec<String> = Vec::new();
                let mut suite_tests: Vec<(String, ProtoId)> = Vec::new();
                let mut hooks: HashMap<String, ProtoId> = HashMap::new();
                for m in methods {
                    let pid = self.compile_fn(m, false)?;
                    let def = self.program.structs.get_mut(&key).expect("hoisted");
                    def.module_idx = module_idx;
                    def.methods.insert(m.name.clone(), pid);
                    if m.is_test {
                        test_methods.push(m.name.clone());
                        suite_tests.push((m.name.clone(), pid));
                    } else if LIFECYCLE_HOOKS.contains(&m.name.as_str()) {
                        hooks.insert(m.name.clone(), pid);
                    }
                }
                if !test_methods.is_empty() {
                    let def = self.program.structs.get_mut(&key).expect("hoisted");
                    def.test_methods = test_methods;
                    // A struct with ≥1 `test fn` method is a suite. Emit a zero-arg constructor thunk
                    // (returns `Suite(<each field default>)`) so the runner builds the instance via
                    // `run_proto`, then record discovery metadata — entry module only.
                    if is_entry {
                        let new_thunk = self.compile_suite_new_thunk(name, fields, stmt.span)?;
                        // Only retain hooks that are actually lifecycle methods (already filtered above).
                        self.program.suites.push(SuiteInfo {
                            name: name.clone(),
                            new_thunk,
                            tests: suite_tests,
                            hooks,
                        });
                    }
                }
                self.float_shadow = prev_shadow;
            }
        }
        // Compile enum methods next (type-erased — no `StructDef`/`tid`), recording proto ids under
        // the enum's module-scoped runtime key. Mirrors the struct-method pass above.
        for stmt in &module.stmts {
            if let StmtKind::Enum {
                name,
                methods,
                type_params,
                ..
            } = &stmt.kind
            {
                if methods.is_empty() {
                    continue;
                }
                let key = self.type_key(module_idx, name);
                let prev_shadow = std::mem::replace(
                    &mut self.float_shadow,
                    type_params.iter().map(|tp| tp.name.clone()).collect(),
                );
                let mut compiled: HashMap<String, ProtoId> = HashMap::new();
                for m in methods {
                    let pid = self.compile_fn(m, false)?;
                    compiled.insert(m.name.clone(), pid);
                }
                self.float_shadow = prev_shadow;
                self.program
                    .enum_methods
                    .entry(key.clone())
                    .or_default()
                    .extend(compiled);
                self.program.enum_home.insert(key, module_idx);
            }
        }
        // Compile newtype methods (name-keyed, like enum methods), recording proto ids under the
        // newtype's module-scoped runtime key. A newtype ALWAYS gets a `newtype_home` entry (even
        // method-less) so the runtime can recognize the key as a newtype.
        for stmt in &module.stmts {
            if let StmtKind::NewType {
                name,
                methods,
                type_params,
                ..
            } = &stmt.kind
            {
                let key = self.type_key(module_idx, name);
                let prev_shadow = std::mem::replace(
                    &mut self.float_shadow,
                    type_params.iter().map(|tp| tp.name.clone()).collect(),
                );
                let mut compiled: HashMap<String, ProtoId> = HashMap::new();
                for m in methods {
                    let pid = self.compile_fn(m, false)?;
                    compiled.insert(m.name.clone(), pid);
                }
                self.float_shadow = prev_shadow;
                self.program
                    .newtype_methods
                    .entry(key.clone())
                    .or_default()
                    .extend(compiled);
                self.program.newtype_home.insert(key, module_idx);
            }
        }
        // The synthetic toplevel function: top-level `fn`s are hoisted as globals before the body.
        let mut fc = FnComp::new("<toplevel>".to_string(), 0, true);
        // Uniform by-reference capture: the module top level is a real frame too. A top-level
        // `for`-loop variable (or a block-local inside a top-level `if`/`for`/`while`) captured by a
        // nested fn / closure must box into a cell — otherwise the captured raw value hits
        // `CellLoad on a non-handle value` at runtime (check-OK / host-panic on BOTH engines). Its
        // boxed-name set is computed exactly like every other fn body (`compile_fn_captured`). Names
        // that resolve as GLOBALS here (top-level `let`s / hoisted fns) are never `add_local`'d, so
        // `is_boxed_slot` never fires for them — only genuine frame locals (loop vars, block-lets) box.
        fc.boxed_names = captured_names_of_body(&module.stmts, &[]);
        for stmt in &module.stmts {
            if let StmtKind::Fn(decl) = &stmt.kind {
                let pid = self.compile_fn(decl, false)?;
                fc.emit(Op::MakeFunc(pid), stmt.span);
                fc.emit(
                    Op::DefineGlobalSlot(self.global_slot(&decl.name)),
                    stmt.span,
                );
                // `chezzi test` discovery — a free `test fn` (entry module only).
                if decl.is_test && is_entry {
                    self.program.tests.push((decl.name.clone(), pid));
                }
            }
            // extern C fns: register a CffiDef and bind the name to a `Cffi` value at module init,
            // exactly like a top-level `fn`. SINGLE-RESOLVER (fix5): each param/return CType comes
            // VERBATIM from the CHECKER-RESOLVED `extern_sigs` table — the ONLY extern-type resolver
            // (the multi-file CLI fills it via `resolve_extern_signatures`, the standalone path via
            // `resolve_extern_signatures_standalone`). The backend does ZERO alias/qualified/struct
            // resolution of its own, so a second resolver cannot exist or drift.
            if let StmtKind::Extern { lib, fns } = &stmt.kind {
                for ef in fns {
                    let sig = self
                        .extern_sigs
                        .get(&(self.current_module_idx, ef.name.clone()));
                    let params: Vec<CType> = ef
                        .params
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            // The checker is the real marshallability gate; this `ok_or_else` is a
                            // never-panic backstop (a user program must never abort the VM) for any
                            // path that bypasses it — it does not fire for valid programs.
                            sig.and_then(|s| s.params.get(i).cloned().flatten())
                                .ok_or_else(|| CompileError {
                                    message: format!(
                                        "type '{}' is not C-marshallable in extern fn '{}' \
                                         (v1 supports only int, float, bool, str, ptr, and a flat \
                                         struct of those)",
                                        ffi_type_display(p.ty.as_ref()),
                                        ef.name
                                    ),
                                    span: stmt.span,
                                })
                        })
                        .collect::<Result<_, _>>()?;
                    // `None` ⇒ void: either no annotation, or an annotation resolving to `nil`
                    // (incl. an alias to `nil`). The checker guarantees a non-void return is a scalar.
                    let ret = sig.and_then(|s| s.ret.clone());
                    let id = self.program.cffi_defs.len() as u32;
                    self.program.cffi_defs.push(CffiDef {
                        lib: lib.clone(),
                        name: ef.name.clone(),
                        params,
                        ret,
                    });
                    fc.emit(Op::MakeCffi(id), stmt.span);
                    fc.emit(Op::DefineGlobalSlot(self.global_slot(&ef.name)), stmt.span);
                }
            }
        }
        // M-C: the module top level is itself an implicit nursery (joins at program exit, i.e. the
        // toplevel proto's `Return`). Gated like a function body — wraps the executable statements
        // (after the hoisted-fn defines, which only create function values).
        let implicit = block_has_bare_spawn(&module.stmts);
        if implicit {
            fc.has_implicit_nursery = true;
            fc.emit(Op::EnterNursery, Span { line: 1, col: 1 });
            fc.nursery_scopes += 1;
        }
        self.compile_block_flat(&mut fc, &module.stmts)?;
        if implicit {
            fc.nursery_scopes -= 1;
        }
        fc.emit(Op::Nil, Span { line: 1, col: 1 });
        fc.emit(Op::Return, Span { line: 1, col: 1 });
        Ok(self.finish(fc))
    }

    /// Emit a synthetic zero-arg constructor thunk for a test suite: `return Suite(<field defaults>)`.
    /// Reuses the ordinary struct-construction op (`NewStruct`) over each field's already-desugared
    /// default expr, so the runner can build the instance via `run_proto` without Rust knowing the
    /// field values. Every suite field must carry a default (a zero-arg suite cannot be built otherwise).
    fn compile_suite_new_thunk(
        &mut self,
        name: &str,
        fields: &[crate::ast::Field],
        span: Span,
    ) -> Result<ProtoId, CompileError> {
        let mut fc = FnComp::new(format!("__new_{name}"), 0, false);
        for f in fields {
            match &f.default {
                Some(d) => {
                    self.compile_expr(&mut fc, d)?;
                    // One-way int→float widening: a `float` suite field coerces its int default, so
                    // the constructed suite instance stores a genuine f64 (the suite thunk bypasses
                    // `compile_ctor_args`, which does this for a regular ctor).
                    if self.float_aliases.is_float(
                        self.current_module_idx,
                        &f.ty,
                        &self.float_shadow,
                    ) {
                        fc.emit(Op::CoerceFloat, d.span);
                    }
                }
                None => {
                    return Err(CompileError {
                        message: format!(
                            "test suite '{name}' field '{}' must have a default value (suites are constructed with no arguments)",
                            f.name
                        ),
                        span,
                    });
                }
            }
        }
        // ROOT REDESIGN — construct under the suite struct's IDENTITY KEY (always qualified), not its
        // bare name; the struct table is keyed by the qualified key. Suites are entry-module only.
        let key = self.type_key(self.current_module_idx, name);
        fc.emit(Op::NewStruct(key, fields.len()), span);
        fc.emit(Op::Return, span);
        Ok(self.finish(fc))
    }

    /// Compile a named function / method to its own proto. `params` occupy slots `0..arity`.
    fn compile_fn(&mut self, decl: &FnDecl, _is_method: bool) -> Result<ProtoId, CompileError> {
        self.compile_fn_captured(decl, Vec::new())
    }

    /// Compile a function body to a child proto. `captured_names` is empty for a top-level/method fn
    /// (a plain `MakeFunc` proto — byte-identical to the pre-nested-fn path) and NON-empty for a
    /// NESTED fn compiled as a closure-with-a-name (the enclosing-frame binding snapshot, in slot
    /// order), so the body's free names (captured outer locals AND the recursive self-name) resolve
    /// via `GetCaptured` against the cells `MakeClosure` populates.
    fn compile_fn_captured(
        &mut self,
        decl: &FnDecl,
        captured_names: Vec<String>,
    ) -> Result<ProtoId, CompileError> {
        // The fn's OWN generic params join the enclosing type's for the duration of this body: they
        // shadow any same-named module-level float alias at every coercion site below (and inside any
        // nested closure, which compiles within this same scope).
        let prev_shadow = self.float_shadow.clone();
        self.float_shadow
            .extend(decl.type_params.iter().map(|tp| tp.name.clone()));
        let r = self.compile_fn_body(decl, captured_names);
        self.float_shadow = prev_shadow;
        r
    }

    fn compile_fn_body(
        &mut self,
        decl: &FnDecl,
        captured_names: Vec<String>,
    ) -> Result<ProtoId, CompileError> {
        let mut fc = FnComp::new(decl.name.clone(), decl.params.len(), false);
        fc.captured_names = captured_names;
        // Uniform by-reference capture (Task A): compute this frame's boxed-name set (unwired).
        fc.boxed_names = captured_names_of_body(&decl.body, &decl.params);
        fc.is_generator = decl.is_generator;
        fc.is_test = decl.is_test;
        // One-way int→float widening: a `-> float` return type coerces every `return` value.
        fc.ret_is_float = decl.ret.as_ref().is_some_and(|t| {
            self.float_aliases
                .is_float(self.current_module_idx, t, &self.float_shadow)
        });
        for p in &decl.params {
            fc.add_local(p.name.clone());
        }
        // A `float` param coerces any int argument at the callee prologue — so EVERY caller (incl. an
        // int VARIABLE, not just a literal) widens.
        self.emit_float_param_prologue(&mut fc, &decl.params);
        // Uniform by-reference capture: box any param captured by a nested closure (after coercion).
        self.emit_box_param_prologue(&mut fc, &decl.params);
        // An inline-expr body (`fn a(): <expr>`) implicitly returns its single expression — exactly
        // like a closure `fn(x): expr` (see `compile_closure`): compile the expr and emit `Return`
        // instead of evaluating-then-discarding it and falling through to `Nil`/`Return`. An inline
        // body cannot hold a bare `spawn` (spawn is a statement, not an expression), so the implicit-
        // nursery dance below never applies to it.
        if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            self.compile_expr(&mut fc, e)?;
            if fc.ret_is_float {
                fc.emit(Op::CoerceFloat, e.span);
            }
            fc.emit(Op::Return, e.span);
            return Ok(self.finish(fc));
        }
        // M-C: if the body has a bare `spawn` (not inside an explicit `parallel:`), open an implicit
        // nursery at entry. It is NOT joined here — `do_return` joins it at every `return`/`?`/end (a
        // single join site), so we only emit the opening `EnterNursery` and flag the proto.
        let implicit = block_has_bare_spawn(&decl.body);
        if implicit {
            fc.has_implicit_nursery = true;
            fc.emit(Op::EnterNursery, Span { line: 1, col: 1 });
            fc.nursery_scopes += 1;
        }
        self.compile_block_scoped(&mut fc, &decl.body)?;
        if implicit {
            fc.nursery_scopes -= 1;
        }
        // Fall off the end → return Nil (do_return joins the implicit nursery).
        fc.emit(Op::Nil, Span { line: 1, col: 1 });
        fc.emit(Op::Return, Span { line: 1, col: 1 });
        Ok(self.finish(fc))
    }

    /// One-way int→float widening — emit the callee-prologue coercion for every `float`-typed param:
    /// `GetLocal(slot), CoerceFloat, SetLocal(slot)`. Done at the callee boundary (not the call site)
    /// so an int argument widens regardless of how it was passed (literal OR variable OR field) and
    /// regardless of which caller called — the single general coverage point for float params. The
    /// param slots are `0..params.len()` in declaration order (matching the `add_local` loop). A
    /// non-`float` (incl. a generic `T`) param emits nothing, so a fn with no float params is
    /// byte-identical to before.
    fn emit_float_param_prologue(&mut self, fc: &mut FnComp, params: &[crate::ast::Param]) {
        for (i, p) in params.iter().enumerate() {
            if !p.is_variadic
                && p.ty.as_ref().is_some_and(|t| {
                    self.float_aliases
                        .is_float(self.current_module_idx, t, &self.float_shadow)
                })
            {
                let slot = i;
                let span = Span { line: 1, col: 1 };
                fc.emit_get_local_raw(slot, span);
                fc.emit(Op::CoerceFloat, span);
                fc.emit_set_local_raw(slot, span);
            }
        }
    }

    /// Uniform by-reference capture — box every param captured by a nested closure/`spawn`/`defer`.
    /// After arg binding (and any float coercion), replace the raw arg in the boxed slot with a fresh
    /// cell wrapping it: `GetLocal(slot); NewCell; SetLocal(slot)`. Runs AFTER
    /// [`emit_float_param_prologue`] so the value is already coerced before it is boxed. The param
    /// slots are `0..params.len()` in declaration order. A fn with no captured params emits nothing.
    fn emit_box_param_prologue(&mut self, fc: &mut FnComp, params: &[crate::ast::Param]) {
        for i in 0..params.len() {
            if fc.is_boxed_slot(i) {
                let span = Span { line: 1, col: 1 };
                fc.emit_get_local_raw(i, span);
                fc.emit(Op::NewCell, span);
                fc.emit_set_local_raw(i, span);
            }
        }
    }

    fn finish(&mut self, fc: FnComp) -> ProtoId {
        let pid = self.program.protos.len();
        // M19: peephole pass — const-fold + superinstruction fusion, with jump relocation.
        let (code, lines) = peephole::optimize(fc.code, fc.lines);
        self.program.protos.push(Proto {
            name: fc.name,
            arity: fc.arity,
            n_slots: fc.max_slots,
            code,
            lines,
            has_implicit_nursery: fc.has_implicit_nursery,
            is_generator: fc.is_generator,
            is_test: fc.is_test,
            // Lever #3: cold-path capture-name metadata in slot order (empty for non-closures).
            capture_names: fc.captured_names,
        });
        pid
    }

    // ----- statements -----

    /// Compile statements into the current scope without opening a new one (used for the toplevel
    /// and for blocks whose scope is managed by the caller).
    fn compile_block_flat(&mut self, fc: &mut FnComp, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            self.compile_stmt(fc, stmt)?;
        }
        Ok(())
    }

    /// Compile a block in a fresh lexical scope (locals don't leak past the block).
    fn compile_block_scoped(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        self.compile_block_flat(fc, stmts)?;
        fc.end_scope();
        Ok(())
    }

    /// Compile a lexical block that is also a **defer scope**: any `defer` directly inside it runs
    /// when this block exits, not when the whole frame does. When the block statically holds a
    /// `defer` we bracket it with `EnterDeferScope`/`LeaveDeferScope`; otherwise this is exactly
    /// `compile_block_scoped` and emits nothing extra (defer-free code is byte-identical).
    fn compile_defer_scoped_block(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        let has_defer = block_has_defer(stmts);
        if has_defer {
            fc.emit(Op::EnterDeferScope, stmts[0].span);
            fc.defer_scopes += 1;
        }
        self.compile_block_scoped(fc, stmts)?;
        if has_defer {
            fc.emit(Op::LeaveDeferScope, stmts[stmts.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// A statement-form `match` arm body as a defer scope. The arm's lexical scope is already
    /// opened/closed by `compile_match_general`, so this brackets the *flat* body with defer-scope
    /// ops when (and only when) it directly contains a `defer`.
    fn compile_defer_scoped_arm(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        let has_defer = block_has_defer(stmts);
        if has_defer {
            fc.emit(Op::EnterDeferScope, stmts[0].span);
            fc.defer_scopes += 1;
        }
        self.compile_block_flat(fc, stmts)?;
        if has_defer {
            fc.emit(Op::LeaveDeferScope, stmts[stmts.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// Emit one `LeaveDeferScope` per defer scope between the current point and the enclosing loop
    /// body (inclusive), draining them LIFO before a `break`/`continue` jumps away. These ops run on
    /// the jump path only; the blocks' own end-of-scope `LeaveDeferScope`s are skipped by the jump,
    /// so each marker is popped exactly once. The compiler's `defer_scopes` count is unchanged — the
    /// scopes remain lexically open and emit their natural leaves on the fall-through path.
    fn emit_loop_body_drain(&mut self, fc: &mut FnComp, span: Span) {
        let Some(floor) = fc.loops.last().map(|c| c.defer_floor) else {
            return; // no enclosing loop — the checker rejects this `break`/`continue`
        };
        for _ in 0..(fc.defer_scopes - floor) {
            fc.emit(Op::LeaveDeferScope, span);
        }
    }

    fn compile_stmt(&mut self, fc: &mut FnComp, stmt: &Stmt) -> Result<(), CompileError> {
        match &stmt.kind {
            StmtKind::Let { names, value, ty, .. } => {
                // One-way int→float widening: a collection-element annotation (`List[float]` /
                // `Map[_, float]`) widens int ELEMENTS at the literal-compile site (hint consumed by
                // `compile_expr`'s `List`/`Map` arms); a scalar `float` annotation coerces the whole
                // value below. (A later plain `x = <int>` to a float local is rejected by the checker —
                // strict assign target — so it needs no runtime coercion.)
                let elem_hint = ty.as_ref().and_then(|t| {
                    self.float_aliases
                        .elem_hint(self.current_module_idx, t, &self.float_shadow)
                });
                let prev_hint = std::mem::replace(&mut self.float_elem_hint, elem_hint);
                self.compile_expr(fc, value)?;
                self.float_elem_hint = prev_hint;
                if names.len() == 1
                    && ty.as_ref().is_some_and(|t| {
                        self.float_aliases
                            .is_float(self.current_module_idx, t, &self.float_shadow)
                    })
                {
                    fc.emit(Op::CoerceFloat, value.span);
                }
                if names.len() > 1 {
                    // destructuring let `a, b := value`: stash the tuple in a hidden local, then for
                    // each binding load it and read element `.i` (the tuple-aware `GetField`). No new
                    // index op — `GetField("i")` on a tuple is the element access.
                    let tuple_slot = fc.add_hidden();
                    fc.emit_hidden_set(tuple_slot, stmt.span);
                    for (i, name) in names.iter().enumerate() {
                        fc.emit_hidden_get(tuple_slot, stmt.span);
                        fc.emit(Op::GetField { name: i.to_string(), ic: NO_IC }, stmt.span); // tuple element
                        if fc.is_global_scope() {
                            fc.emit(Op::DefineGlobalSlot(self.global_slot(name)), stmt.span);
                        } else {
                            fc.emit_decl_named(name.clone(), stmt.span);
                        }
                    }
                } else if fc.is_global_scope() {
                    fc.emit(Op::DefineGlobalSlot(self.global_slot(&names[0])), stmt.span);
                } else {
                    fc.emit_decl_named(names[0].clone(), stmt.span);
                }
                Ok(())
            }
            StmtKind::Assign { target, op, value } => self.compile_assign(fc, target, *op, value, stmt.span),
            StmtKind::Expr(expr) => {
                self.compile_expr(fc, expr)?;
                // `PopExprStmt` (not `Pop`): an unhandled `Err`/`None` from a top-level expression
                // statement exits the program (the runtime checks the frame). Use `expr.span` (not
                // `stmt.span`) so the error location matches the interpreter exactly.
                fc.emit(Op::PopExprStmt, expr.span);
                Ok(())
            }
            // Top-level fns are hoisted; struct/enum/import are no-ops at execution time. A `fn`
            // statement nested in a block is a FIRST-CLASS LOCAL function — a closure-with-a-name:
            // it captures outer bindings by reference (uniform cell model, like a closure expression)
            // and may RECURSE (letrec: its name is bound into its own cell BEFORE the body captures
            // it). Routed through `MakeClosure`, NOT the non-capturing `MakeFunc` path.
            StmtKind::Fn(decl) => {
                if fc.is_global_scope() {
                    Ok(()) // already hoisted
                } else if fc.is_boxed_name(&decl.name) {
                    // The name is captured (recursive self-reference and/or captured by a deeper
                    // sibling closure), so it lives in a CELL. LETREC: create the empty cell in the
                    // name's slot FIRST, snapshot it into the child's capture env, build the closure,
                    // then store the finished closure back INTO that same cell. Ordering is
                    // load-bearing — the cell handle captured by the body must be the same handle the
                    // closure is stored into, so a self-call resolves to this very closure.
                    let slot = fc.add_local(decl.name.clone());
                    fc.emit(Op::Nil, stmt.span);
                    fc.emit(Op::NewCell, stmt.span);
                    fc.emit_set_local_raw(slot, stmt.span);
                    // Free-variable capture (Finding D): keep only the names the body references
                    // (relative to its own params). The recursive self-name is free in a recursive
                    // body → stays captured → the self-call resolves through the cell above.
                    let entries = filter_entries_free_block(&fc.snapshot_entries(), &decl.body, &decl.params);
                    let captured_names: Vec<String> =
                        entries.iter().map(|e| e.name.clone()).collect();
                    let pid = self.compile_fn_captured(decl, captured_names)?;
                    fc.emit(Op::MakeClosure(pid, entries), stmt.span);
                    // Stack: [closure]; push the cell handle → [closure, handle]; `CellStore` pops
                    // handle-first then value → cell := closure. Stack drained.
                    fc.emit_get_local_raw(slot, stmt.span);
                    fc.emit(Op::CellStore, stmt.span);
                    Ok(())
                } else {
                    // Not captured: snapshot the enclosing bindings BEFORE declaring the name, build
                    // the closure, and bind it plainly. (Still a closure so it can capture outer
                    // locals — it just needs no self-cell because nothing captures it.)
                    // Free-variable capture (Finding D): keep only referenced names.
                    let entries = filter_entries_free_block(&fc.snapshot_entries(), &decl.body, &decl.params);
                    let captured_names: Vec<String> =
                        entries.iter().map(|e| e.name.clone()).collect();
                    let pid = self.compile_fn_captured(decl, captured_names)?;
                    fc.emit(Op::MakeClosure(pid, entries), stmt.span);
                    fc.emit_decl_named(decl.name.clone(), stmt.span);
                    Ok(())
                }
            }
            StmtKind::Struct { .. }
            | StmtKind::Enum { .. }
            | StmtKind::NewType { .. } // methods compiled in compile_module; ctor is a named call
            | StmtKind::Protocol { .. }
            | StmtKind::Extern { .. } // bound at module init (see compile_module), like top-level fn
            // A `native fn`/`native ctor` decl is a compile-time SIGNATURE source only — it gets no
            // bytecode, no binding, and is never a callable user fn (dispatch stays name-keyed).
            | StmtKind::Native(_)
            // A `native struct` decl is likewise a compile-time SIGNATURE source only (checker-harvested
            // from a companion stub); its runtime layout stays native (name-keyed), so no bytecode.
            | StmtKind::NativeStruct { .. }
            // A `native enum` decl (Option/Result shape mirror) is a compile-time SIGNATURE source only;
            // construction/match stay native + Rust-wired, so it emits no bytecode.
            | StmtKind::NativeEnum { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Import(_) => Ok(()),
            StmtKind::Return(value) => {
                match value {
                    Some(e) => {
                        self.compile_expr(fc, e)?;
                        // One-way int→float widening: a `-> float` fn coerces its return value.
                        if fc.ret_is_float {
                            fc.emit(Op::CoerceFloat, e.span);
                        }
                    }
                    None => fc.emit(Op::Nil, stmt.span),
                }
                fc.emit(Op::Return, stmt.span);
                Ok(())
            }
            // `yield <expr>` — evaluate the operand and suspend (experimental generators). The value
            // is left on the stack for `Op::Yield`'s runtime handler to hand back to `.next()`.
            StmtKind::Yield(e) => {
                self.compile_expr(fc, e)?;
                fc.emit(Op::Yield, stmt.span);
                Ok(())
            }
            StmtKind::Break => {
                // Drain the current iteration's loop-body defers (and any nested block defers) before
                // jumping out, so they run at the `break`, not at function return.
                self.emit_loop_body_drain(fc, stmt.span);
                // TASK B — cancel-and-report any `parallel:` nursery this `break` leaves before its
                // join. Order on the jump path is body defers first, then the nursery reclaim
                // (cancel-and-report) — distinct from the fall-through order (JoinNursery then the
                // block's LeaveDeferScope), because a `break` cancels the nursery rather than joining
                // its children.
                self.emit_loop_nursery_drain(fc, stmt.span);
                let j = fc.emit_jump(Op::Jump(0), stmt.span);
                match fc.current_loop() {
                    Some(ctx) => ctx.break_jumps.push(j),
                    None => return Err(CompileError {
                        message: "break outside loop".to_string(),
                        span: stmt.span,
                    }),
                }
                Ok(())
            }
            StmtKind::Continue => {
                self.emit_loop_body_drain(fc, stmt.span);
                self.emit_loop_nursery_drain(fc, stmt.span);
                let j = fc.emit_jump(Op::Jump(0), stmt.span);
                match fc.current_loop() {
                    Some(ctx) => ctx.continue_jumps.push(j),
                    None => return Err(CompileError {
                        message: "continue outside loop".to_string(),
                        span: stmt.span,
                    }),
                }
                Ok(())
            }
            // `pass` — a no-op statement; emits no bytecode.
            StmtKind::Pass => Ok(()),
            StmtKind::Assert { cond, msg } => {
                // Lazy message evaluation, byte-identical to the interpreter (which evaluates `msg`
                // only on failure): compile `cond`, and only on the false path compile `msg` then
                // `Op::Assert` (which always faults). A passing assert never touches `msg`, so a
                // side-effecting/faulting message expression behaves identically across both engines.
                // `Op::Assert` carries `stmt.span` so the fault location matches the interpreter.
                self.compile_expr(fc, cond)?;
                let to_fail = fc.emit_jump(Op::JumpIfFalse(0), stmt.span);
                let to_end = fc.emit_jump(Op::Jump(0), stmt.span);
                fc.patch_jump(to_fail);
                if let Some(m) = msg {
                    self.compile_expr(fc, m)?;
                }
                fc.emit(Op::Assert { has_msg: msg.is_some() }, stmt.span);
                fc.patch_jump(to_end);
                Ok(())
            }
            StmtKind::Defer(target) => self.compile_defer(fc, target, stmt.span),
            StmtKind::If { branches, else_block } => self.compile_if(fc, branches, else_block.as_deref(), stmt.span),
            StmtKind::While { cond, body } => self.compile_while(fc, cond, body),
            StmtKind::For {
                vars, iter, body, ..
            } => self.compile_for(fc, vars, iter, body, stmt.span),
            StmtKind::Match { scrutinee, arms } => self.compile_match(fc, scrutinee, arms, stmt.span),
            // Concurrency C4 — sequential, run-to-completion executor (mirrors the interpreter).
            StmtKind::Parallel { body } => self.compile_parallel(fc, body, stmt.span),
            StmtKind::Spawn(target) => self.compile_spawn(fc, target, stmt.span),
            StmtKind::Wait { arms, else_block } => {
                self.compile_wait(fc, arms, else_block.as_deref(), stmt.span)
            }
        }
    }

    /// `wait:` — Chezzi's `select` (§6d). Evaluate each arm's channel once (source order) → N handles
    /// on the stack, then a single `Op::WaitPoll` that polls them and jumps to the chosen arm's body
    /// (or `else`), or parks. Each arm body is a lexical sub-scope (like a `match` arm): the selected
    /// value arrives on the stack top and the prologue binds (`:=`) / assigns (`=`) / drops (`_`) it.
    fn compile_wait(
        &mut self,
        fc: &mut FnComp,
        arms: &[WaitArm],
        else_block: Option<&[Stmt]>,
        span: Span,
    ) -> Result<(), CompileError> {
        // Evaluate each arm's channel expression once, source order (handles left on the stack).
        for arm in arms {
            self.compile_expr(fc, &arm.chan)?;
        }
        // Placeholder; back-patched with the arm/else targets once the bodies are laid out.
        let poll_at = fc.emit_jump(
            Op::WaitPoll(Box::new(WaitMeta {
                n: arms.len(),
                arm_targets: Vec::new(),
                else_target: None,
            })),
            span,
        );
        let mut arm_targets = Vec::with_capacity(arms.len());
        let mut end_jumps = Vec::new();
        for arm in arms {
            arm_targets.push(fc.here());
            fc.begin_scope();
            // The selected value is on the stack top — deliver it per the arm's target.
            match &arm.target {
                WaitTarget::Bind(name) => {
                    fc.emit_decl_named(name.clone(), arm.span);
                }
                WaitTarget::Discard => fc.emit(Op::Pop, arm.span),
                WaitTarget::Assign(target) => self.emit_wait_assign(fc, target, arm.span)?,
            }
            self.compile_defer_scoped_arm(fc, &arm.body)?;
            fc.end_scope();
            end_jumps.push(fc.emit_jump(Op::Jump(0), arm.span));
        }
        let else_target = if let Some(b) = else_block {
            let t = fc.here();
            self.compile_defer_scoped_block(fc, b)?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), span));
            Some(t)
        } else {
            None
        };
        let end = fc.here();
        for j in end_jumps {
            fc.patch_jump_to(j, end);
        }
        fc.set_code(
            poll_at,
            Op::WaitPoll(Box::new(WaitMeta {
                n: arms.len(),
                arm_targets,
                else_target,
            })),
        );
        Ok(())
    }

    /// A `wait` `=` arm: store the value on the stack top into an existing lvalue. `Ident` pops
    /// straight into the binding; `Field`/`Index` stash the value in a hidden temp, evaluate the
    /// object (and index), then reload it — so the `[obj, (index,) value]` order `SetField`/`SetIndex`
    /// expect is reconstructed even though the value was produced first by `WaitPoll`.
    fn emit_wait_assign(
        &mut self,
        fc: &mut FnComp,
        target: &Expr,
        span: Span,
    ) -> Result<(), CompileError> {
        match &target.kind {
            ExprKind::Ident(name) => self.emit_store(fc, name, span),
            ExprKind::Field { obj, name, .. } => {
                let tmp = fc.add_hidden();
                fc.emit_hidden_set(tmp, span);
                self.compile_expr(fc, obj)?;
                fc.emit_hidden_get(tmp, span);
                let ic = self.next_field_ic(name);
                fc.emit(
                    Op::SetField {
                        name: name.clone(),
                        ic,
                    },
                    span,
                );
            }
            ExprKind::Index { obj, index } => {
                let tmp = fc.add_hidden();
                fc.emit_hidden_set(tmp, span);
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                fc.emit_hidden_get(tmp, span);
                fc.emit(Op::SetIndex, span);
            }
            _ => {
                return Err(CompileError {
                    message: "invalid wait-arm assignment target".to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// `parallel:` — open a nursery, run the body (spawns register tasks; inline statements run
    /// immediately), join at the dedent, THEN flush the block's defers. The defer scope brackets the
    /// body but its closing `LeaveDeferScope` lands AFTER `JoinNursery`, so a `defer` inside the block
    /// runs after its spawned children join — same order as the implicit function-body nursery (whose
    /// `do_return` joins before running body defers). Emit sequence: EnterNursery, [EnterDeferScope],
    /// body, JoinNursery, [LeaveDeferScope]. We inline the bracketing (rather than call
    /// `compile_defer_scoped_block`) because there is no seam to slip `JoinNursery` between the
    /// helper's paired defer-scope ops. Defer-free blocks are byte-identical (the `has_defer` gate
    /// skips both defer-scope ops, exactly as the helper does).
    fn compile_parallel(
        &mut self,
        fc: &mut FnComp,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), CompileError> {
        fc.emit(Op::EnterNursery, span);
        // TASK B — track the open nursery scope so a `break`/`continue` inside `body` knows to emit a
        // `ReclaimNursery` (cancel-and-report) before its loop-exit jump. Mirrors `defer_scopes`.
        fc.nursery_scopes += 1;
        let has_defer = block_has_defer(body);
        if has_defer {
            fc.emit(Op::EnterDeferScope, body[0].span);
            fc.defer_scopes += 1;
        }
        // The counter bracketing must wrap this body compile exactly as before (so
        // `emit_loop_body_drain`/`emit_loop_nursery_drain` emit the right count on a break/continue
        // out of the block); only the fall-through JoinNursery/LeaveDeferScope order changes.
        self.compile_block_scoped(fc, body)?;
        fc.nursery_scopes -= 1;
        fc.emit(Op::JoinNursery, span);
        if has_defer {
            fc.emit(Op::LeaveDeferScope, body[body.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// TASK B — emit one `ReclaimNursery` per `parallel:` scope between the current point and the
    /// enclosing loop body (inclusive), cancelling-and-reporting each escaped nursery before a
    /// `break`/`continue` jumps away. These run on the jump path only; the blocks' own `JoinNursery`s
    /// are skipped by the jump. Mirrors `emit_loop_body_drain` for defer scopes. The compiler's
    /// `nursery_scopes` count is unchanged — the scopes remain lexically open and emit their natural
    /// `JoinNursery` on the fall-through path.
    fn emit_loop_nursery_drain(&mut self, fc: &mut FnComp, span: Span) {
        let Some(floor) = fc.loops.last().map(|c| c.nursery_floor) else {
            return; // no enclosing loop — the checker rejects this `break`/`continue`
        };
        for _ in 0..(fc.nursery_scopes - floor) {
            fc.emit(Op::ReclaimNursery, span);
        }
    }

    /// `spawn` — register a task on the innermost nursery. Form 1 (`spawn f(args)` / `spawn
    /// recv.m(args)`) evaluates the callee/receiver + args here and emits `SpawnCall`/`SpawnMethod`
    /// (mirrors `compile_defer`). Form 2 (`spawn:` block) compiles the block as a synthetic zero-arg
    /// proto and emits `SpawnBlock`, capturing the enclosing bindings (like a closure).
    fn compile_spawn(
        &mut self,
        fc: &mut FnComp,
        target: &SpawnTarget,
        span: Span,
    ) -> Result<(), CompileError> {
        match target {
            SpawnTarget::Call(call) => {
                let ExprKind::Call {
                    callee,
                    args,
                    named,
                    ..
                } = &call.kind
                else {
                    return Err(CompileError {
                        message: "spawn requires a function or method call".to_string(),
                        span,
                    });
                };
                if let ExprKind::Field { obj, name, .. } = &callee.kind {
                    self.compile_expr(fc, obj)?;
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::SpawnMethod(name.clone(), args.len()), call.span);
                } else if !named.is_empty()
                    && let Some(perm) = self
                        .keyword_calls
                        .get(&crate::checker::keyword_key(
                            self.current_module_idx,
                            self.kw_frag_ctx,
                            self.kw_frag_ord,
                            named,
                            call.span,
                        ))
                        .cloned()
                {
                    // A spawned VALUE call carrying keyword arguments: reorder to positional by the
                    // checker-recorded permutation, then spawn positionally (same as the eager form).
                    self.compile_expr(fc, callee)?;
                    for &ci in &perm {
                        let e = if ci < args.len() {
                            &args[ci]
                        } else {
                            &named[ci - args.len()].1
                        };
                        self.compile_expr(fc, e)?;
                    }
                    fc.emit(Op::SpawnCall(perm.len()), call.span);
                } else {
                    self.compile_expr(fc, callee)?;
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::SpawnCall(args.len()), call.span);
                }
                Ok(())
            }
            SpawnTarget::Block(body) => {
                // Capture only the names this block references (its free-variable set); the values
                // are deep-copied across the airlock at `SpawnBlock`. The block becomes a synthetic
                // zero-arg proto whose free names resolve via `GetCaptured`. Free-variable capture
                // (Finding D) avoids dragging unused non-sendable siblings across the airlock.
                let entries = filter_entries_free_block(&fc.snapshot_entries(), body, &[]);
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<spawned task>".to_string(), 0, false);
                // Uniform by-reference capture (Task A): the block's own boxed-name set (unwired).
                child.boxed_names = captured_names_of_body(body, &[]);
                child.captured_names = captured_names;
                // M-C: a `spawn:` block is its own function body — a nested bare `spawn` inside it
                // binds to the block's *own* implicit nursery, joined when the task returns.
                let implicit = block_has_bare_spawn(body);
                if implicit {
                    child.has_implicit_nursery = true;
                    child.emit(Op::EnterNursery, span);
                    child.nursery_scopes += 1;
                }
                self.compile_block_scoped(&mut child, body)?;
                if implicit {
                    child.nursery_scopes -= 1;
                }
                child.emit(Op::Nil, span);
                child.emit(Op::Return, span);
                let pid = self.finish(child);
                fc.emit(Op::SpawnBlock(pid, entries), span);
                Ok(())
            }
        }
    }

    fn compile_assign(
        &mut self,
        fc: &mut FnComp,
        target: &Expr,
        op: AssignOp,
        value: &Expr,
        span: Span,
    ) -> Result<(), CompileError> {
        match &target.kind {
            ExprKind::Ident(name) => match op.to_binop() {
                None => {
                    self.compile_expr(fc, value)?;
                    self.emit_store(fc, name, span);
                }
                Some(bin) => {
                    self.emit_load(fc, name, span);
                    self.compile_expr(fc, value)?;
                    fc.emit(binary_op(bin), span);
                    self.emit_store(fc, name, span);
                }
            },
            // `obj.f = v` → [obj, v] SetField; compound dups `obj` to read-modify-write.
            ExprKind::Field { obj, name, .. } => {
                self.compile_expr(fc, obj)?;
                if let Some(bin) = op.to_binop() {
                    let ic = self.next_field_ic(name);
                    fc.emit(Op::Dup, span);
                    fc.emit(
                        Op::GetField {
                            name: name.clone(),
                            ic,
                        },
                        target.span,
                    );
                    self.compile_expr(fc, value)?;
                    fc.emit(binary_op(bin), span);
                } else {
                    self.compile_expr(fc, value)?;
                }
                let ic = self.next_field_ic(name);
                fc.emit(
                    Op::SetField {
                        name: name.clone(),
                        ic,
                    },
                    span,
                );
            }
            // `obj[i] = v` → [obj, i, v] SetIndex; compound dups `[obj, i]` to read-modify-write.
            ExprKind::Index { obj, index } => {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                // No `AsInt`: the index may be a map key (str/bool). `GetIndex`/`SetIndex`
                // validate int-ness in their list/str arms at runtime.
                if let Some(bin) = op.to_binop() {
                    fc.emit(Op::Dup2, span);
                    fc.emit(Op::GetIndex, target.span);
                    self.compile_expr(fc, value)?;
                    fc.emit(binary_op(bin), span);
                } else {
                    self.compile_expr(fc, value)?;
                }
                fc.emit(Op::SetIndex, span);
            }
            // `a, b = b, a` — multi-target tuple assignment (op is always `Eq`; the parser enforces
            // it). Evaluate the FULL RHS tuple into a hidden local FIRST (Python semantics — so a
            // swap whose index appears on both sides is correct), then store each element into its
            // target left-to-right. Mirrors the destructuring-let lowering.
            ExprKind::Tuple(targets) => {
                // `value` is either a tuple literal (`b, a`) or any tuple-valued expression
                // (`f()` returning a tuple). Either way `compile_expr` leaves the full tuple on the
                // stack; the checker has verified arity.
                self.compile_expr(fc, value)?; // builds the full RHS tuple on the stack
                let tuple_slot = fc.add_hidden();
                fc.emit_hidden_set(tuple_slot, span);
                for (i, t) in targets.iter().enumerate() {
                    self.compile_assign_element(fc, t, tuple_slot, i, span)?;
                }
            }
            _ => {
                return Err(CompileError {
                    message: "invalid assignment target".to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// Lower one element of a multi-target tuple assignment: load the stashed RHS tuple's element
    /// `i` (`GetLocal(slot)` + `GetField(i)`) onto the stack, then store it into `target` with plain
    /// `=` semantics across the ident / field / index target shapes.
    fn compile_assign_element(
        &mut self,
        fc: &mut FnComp,
        target: &Expr,
        tuple_slot: usize,
        i: usize,
        span: Span,
    ) -> Result<(), CompileError> {
        match &target.kind {
            ExprKind::Ident(name) => {
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                self.emit_store(fc, name, span);
            }
            ExprKind::Field { obj, name, .. } => {
                self.compile_expr(fc, obj)?;
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                let ic = self.next_field_ic(name);
                fc.emit(
                    Op::SetField {
                        name: name.clone(),
                        ic,
                    },
                    span,
                );
            }
            ExprKind::Index { obj, index } => {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                fc.emit(Op::SetIndex, span);
            }
            _ => {
                return Err(CompileError {
                    message: "invalid assignment target".to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// Store the value on top of the stack into an existing binding (`=`/`+=`/`-=` semantics:
    /// never creates — a global that doesn't exist is a runtime error).
    fn emit_store(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            // A boxed owner-side local writes through its cell (`emit_set_named`).
            Some(slot) => fc.emit_set_named(slot, span),
            // Uniform by-reference capture: a `defer:`/`spawn:` frame WRITING a captured name mutates
            // the shared cell. Captures are uniformly cells (spec B4), so this is `GetCaptured; <val
            // already on stack>; CellStore` (CellStore pops handle-first, then value — the Task-A
            // operand convention). This REPLACES the old fall-through to `SetGlobalSlot` (the
            // phantom-global write bug: A2 printed `0`).
            None if fc.captures(name) => {
                let slot = fc
                    .captured_names
                    .iter()
                    .position(|n| n == name)
                    .expect("captures() implies a capture slot") as u32;
                fc.emit(Op::GetCaptured(slot), span);
                fc.emit(Op::CellStore, span);
            }
            None => fc.emit(Op::SetGlobalSlot(self.global_slot(name)), span),
        }
    }

    /// Load a name's value (local → captured → global), mirroring the interpreter's lookup order.
    fn emit_load(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            // A boxed owner-side local dereferences its cell (`emit_get_named`).
            Some(slot) => fc.emit_get_named(slot, span),
            None if fc.captures(name) => {
                // Positional capture (lever #3): the slot is the name's index in this closure's
                // `captured_names` (the snapshot order `MakeClosure` populated). `captures` just
                // confirmed membership, so `position` always finds it. Captures are uniformly cells
                // (spec B4), so every captured read dereferences the handle with a trailing `CellLoad`.
                let slot = fc
                    .captured_names
                    .iter()
                    .position(|n| n == name)
                    .expect("captures() implies a capture slot") as u32;
                fc.emit(Op::GetCaptured(slot), span);
                fc.emit(Op::CellLoad, span);
            }
            None => fc.emit(Op::GetGlobalSlot(self.global_slot(name)), span),
        }
    }

    fn compile_if(
        &mut self,
        fc: &mut FnComp,
        branches: &[(Expr, Block)],
        else_block: Option<&[Stmt]>,
        _span: Span,
    ) -> Result<(), CompileError> {
        let mut end_jumps = Vec::new();
        for (cond, body) in branches {
            self.compile_expr(fc, cond)?;
            fc.emit(Op::AsBool, cond.span);
            let skip = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
            self.compile_defer_scoped_block(fc, body)?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), cond.span));
            fc.patch_jump(skip);
        }
        if let Some(body) = else_block {
            self.compile_defer_scoped_block(fc, body)?;
        }
        for j in end_jumps {
            fc.patch_jump(j);
        }
        Ok(())
    }

    fn compile_while(
        &mut self,
        fc: &mut FnComp,
        cond: &Expr,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        let loop_start = fc.here();
        self.compile_expr(fc, cond)?;
        fc.emit(Op::AsBool, cond.span);
        let exit = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
        fc.loops.push(LoopCtx {
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
            defer_floor: fc.defer_scopes,
            nursery_floor: fc.nursery_scopes,
        });
        self.compile_defer_scoped_block(fc, body)?;
        fc.emit(Op::Jump(loop_start), cond.span);
        fc.patch_jump(exit);
        let ctx = fc.loops.pop().expect("loop ctx pushed above");
        // `break` → loop exit (here, past the back-edge); `continue` → re-test the condition.
        let exit_target = fc.here();
        for j in ctx.break_jumps {
            fc.patch_jump_to(j, exit_target);
        }
        for j in ctx.continue_jumps {
            fc.patch_jump_to(j, loop_start);
        }
        Ok(())
    }

    fn compile_for(
        &mut self,
        fc: &mut FnComp,
        vars: &[String],
        iter: &Expr,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        fc.loops.push(LoopCtx {
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
            defer_floor: fc.defer_scopes,
            nursery_floor: fc.nursery_scopes,
        });
        if let ExprKind::Range { start, end } = &iter.kind {
            // Lazy counting loop — the range is never materialized. The checker guarantees a single
            // loop variable for a range.
            self.compile_expr(fc, end)?;
            fc.emit(Op::AsInt, end.span);
            let end_slot = fc.add_hidden();
            fc.emit_hidden_set(end_slot, span);
            self.compile_expr(fc, start)?;
            fc.emit(Op::AsInt, start.span);
            let i_slot = fc.add_local(vars[0].clone());
            // The COUNTER is a plain int mutated in place: the user var's own slot when it isn't
            // boxed, else a hidden slot (the boxed user cell is refreshed from it each iteration).
            let counter = fc.loopvar_raw_slot(i_slot);
            fc.emit_set_local_raw(counter, span);

            let loop_start = fc.here();
            fc.emit_get_local_raw(counter, span);
            fc.emit_hidden_get(end_slot, span);
            fc.emit(Op::Lt, span);
            let exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            // Each iteration binds the user var to a fresh cell (when boxed) from the counter's value.
            fc.emit_loopvar_refresh(i_slot, counter, span);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` must land HERE — on the increment, not the condition (re-testing without
            // advancing `i` would loop forever) and not after it (skipping the advance also hangs).
            let inc_target = fc.here();
            fc.emit_get_local_raw(counter, span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit_set_local_raw(counter, span);
            fc.emit(Op::Jump(loop_start), span);
            fc.patch_jump(exit);
            self.patch_loop(fc, inc_target);
        } else if vars.len() == 1 {
            // Single loop variable. The iterand may be a sequence (list/map-keys/set/str), a user
            // struct implementing the iterator protocol (`next(self) -> Option[T]`), OR a `Channel[T]`
            // (`for v in ch:` — block per value, end on close). The compiler is type-erased, so we
            // branch at RUNTIME on `IsChannel`/`IsStruct`: the channel and struct paths are both driven
            // LAZILY by an `Option`-producing step (so an infinite iterator with a `break` terminates,
            // and a channel blocks then ends on close); anything else is indexed as a snapshotted list.
            // The channel and struct steps converge on ONE shared `Option` (None ⇒ exit, Some ⇒ bind)
            // decoder — they differ only in how they produce the `Option`.
            self.compile_expr(fc, iter)?;
            let iter_slot = fc.add_hidden();
            fc.emit_hidden_set(iter_slot, span);
            // ONE-TIME pure-`Iterable` conversion: a struct with `iter()` but no `next()` becomes its
            // cursor here (then drives via the seq path); every other iterand (struct-with-`next`,
            // generator, collection) passes through unchanged, so their fast paths are byte-identical.
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IterableToCursor, iter.span);
            fc.emit_hidden_set(iter_slot, span);
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsChannel, span);
            let chan_mode_slot = fc.add_hidden(); // true ⇒ channel path (ChanRecvOrClosed)
            fc.emit_hidden_set(chan_mode_slot, span);
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsStruct, span);
            let struct_mode_slot = fc.add_hidden(); // true ⇒ struct-iterator path (next())
            fc.emit_hidden_set(struct_mode_slot, span);
            // A generator result (experimental, VM-only) answers `next()` intrinsically, so it rides
            // the exact same lazy step as a struct iterator: force `struct_mode` true when the iterand
            // is a generator. (Kept off the seq path, which would wrongly snapshot it to a list.)
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsGenerator, span);
            let not_gen = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::True, span);
            fc.emit_hidden_set(struct_mode_slot, span);
            fc.patch_jump(not_gen);
            // A builtin cursor (`Obj::Iter`, an `.iter()` result — possibly the one just produced by
            // `IterableToCursor`) also answers `next()` intrinsically. Force `struct_mode` true so a
            // NAMED cursor rides the lazy `next()` step and DRIVES the shared heap cursor in place
            // (advancing the original), keeping `for` consistent with `.next()`/`List()`/`Set()` and
            // with struct iterators — instead of the seq path, which would snapshot a private copy.
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsCursor, span);
            let not_cursor = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::True, span);
            fc.emit_hidden_set(struct_mode_slot, span);
            fc.patch_jump(not_cursor);
            // The loop variable, plus the seq-path bookkeeping slots (allocated unconditionally; the
            // lazy paths simply never touch them) and the lazy paths' `Option` result slot. When the
            // loop var is boxed (captured), the loop MECHANISM writes a hidden raw slot and the user
            // cell is refreshed from it per iteration (fresh cell per iteration, C1).
            let item_slot = fc.add_local(vars[0].clone());
            let item_raw = fc.loopvar_raw_slot(item_slot);
            let lst_slot = fc.add_hidden();
            let len_slot = fc.add_hidden();
            let idx_slot = fc.add_hidden();
            let opt_slot = fc.add_hidden();

            // Seq init (skipped on BOTH lazy paths): snapshot the iterand to a list, take its length,
            // start the index at 0. Skip when channel OR struct.
            fc.emit_hidden_get(chan_mode_slot, span);
            let chan_to_check_struct = fc.emit_jump(Op::JumpIfFalse(0), span); // not chan ⇒ check struct
            let chan_skip_init = fc.emit_jump(Op::Jump(0), span); // chan ⇒ skip seq init
            fc.patch_jump(chan_to_check_struct);
            fc.emit_hidden_get(struct_mode_slot, span);
            let to_seq_init = fc.emit_jump(Op::JumpIfFalse(0), span); // not struct ⇒ run seq init
            let struct_skip_init = fc.emit_jump(Op::Jump(0), span); // struct ⇒ skip seq init
            fc.patch_jump(to_seq_init);
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit_hidden_set(lst_slot, span);
            fc.emit_hidden_get(lst_slot, span);
            fc.emit(Op::ArrLen, span);
            fc.emit_hidden_set(len_slot, span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit_hidden_set(idx_slot, span);
            fc.patch_jump(chan_skip_init);
            fc.patch_jump(struct_skip_init);

            let loop_head = fc.here();
            // Channel step: `ChanRecvOrClosed` → opt_slot (blocks on empty-open, None on closed+drained).
            fc.emit_hidden_get(chan_mode_slot, span);
            let chan_to_struct = fc.emit_jump(Op::JumpIfFalse(0), span); // not chan ⇒ struct/seq
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::ChanRecvOrClosed, iter.span);
            fc.emit_hidden_set(opt_slot, span);
            let chan_to_opt = fc.emit_jump(Op::Jump(0), span); // ⇒ shared Option decoder
            fc.patch_jump(chan_to_struct);
            // Struct vs seq split.
            fc.emit_hidden_get(struct_mode_slot, span);
            let to_seq_step = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ seq step
            // ----- struct step: call next() → opt_slot -----
            fc.emit_hidden_get(iter_slot, span);
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: "next".to_string(),
                    argc: 0,
                    ic,
                },
                span,
            );
            fc.emit_hidden_set(opt_slot, span);
            // ----- shared Option decoder (channel + struct): None ⇒ exit, Some(v) ⇒ bind v -----
            fc.patch_jump(chan_to_opt);
            fc.emit(Op::EnsureEnum(opt_slot), iter.span);
            // Test `None` first: a match falls through to the exit jump; a mismatch goes to `to_some`.
            let none_arm = fc.emit_jump(
                Op::MatchArm {
                    scrut: opt_slot,
                    variant: "None".to_string(),
                    variant_id: crate::vm::op::VID_NONE_VARIANT,
                    enum_name: None,
                    nbind: 0,
                    bind_start: 0,
                    next: 0,
                },
                iter.span,
            );
            let lazy_exit = fc.emit_jump(Op::Jump(0), span); // None matched ⇒ leave the loop
            fc.patch_jump(none_arm); // not None ⇒ try Some here
            // `Some(v)`: a match binds the payload into the loop variable's MECHANISM slot (`item_raw`)
            // and falls through to the body jump; a non-Some jumps to the trap below.
            let some_arm = fc.emit_jump(
                Op::MatchArm {
                    scrut: opt_slot,
                    variant: "Some".to_string(),
                    variant_id: crate::vm::op::VID_SOME,
                    enum_name: None,
                    nbind: 1,
                    bind_start: item_raw,
                    next: 0,
                },
                iter.span,
            );
            let to_body = fc.emit_jump(Op::Jump(0), span); // Some matched ⇒ run the body
            fc.patch_jump(some_arm); // neither None nor Some ⇒ the trap
            fc.emit(Op::MatchNoArm(opt_slot), iter.span); // not Option ⇒ runtime trap
            // ----- seq step: bounds-check the index, read the element -----
            fc.patch_jump(to_seq_step);
            fc.emit_hidden_get(idx_slot, span);
            fc.emit_hidden_get(len_slot, span);
            fc.emit(Op::Lt, span);
            let seq_exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit_hidden_get(lst_slot, span);
            fc.emit_hidden_get(idx_slot, span);
            fc.emit(Op::GetIndex, span);
            fc.emit_set_local_raw(item_raw, span);

            fc.patch_jump(to_body);
            // Fresh cell per iteration for a boxed (captured) loop var (C1); no-op when unboxed.
            fc.emit_loopvar_refresh(item_slot, item_raw, span);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the advance step. For a channel/struct, "advance" is just
            // re-looping (the next lazy step); for a sequence, it's the index increment.
            let inc_target = fc.here();
            fc.emit_hidden_get(chan_mode_slot, span);
            let inc_check_struct = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // channel: re-loop, ChanRecvOrClosed advances
            fc.patch_jump(inc_check_struct);
            fc.emit_hidden_get(struct_mode_slot, span);
            let to_seq_inc = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // struct: re-loop, next() advances
            fc.patch_jump(to_seq_inc);
            fc.emit_hidden_get(idx_slot, span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit_hidden_set(idx_slot, span);
            fc.emit(Op::Jump(loop_head), span);
            // All exit paths land here (past the back-edge).
            fc.patch_jump(lazy_exit);
            fc.patch_jump(seq_exit);
            self.patch_loop(fc, inc_target);
        } else {
            // Multi-name `for`: either `for k, v in m` over a MAP (key, value) or tuple-destructuring
            // `for a, b, … in xs` over a `List[(A, B, …)]`. The compiler is type-erased, so we branch
            // at RUNTIME on `IsMap` (mirroring the single-var `IsStruct` split):
            //   - map: snapshot keys + values up front and index them in lockstep (so a body that
            //     mutates the map mid-loop can't perturb the bindings; matches the interpreter);
            //   - list of tuples: index the list, then destructure each element tuple into the N
            //     loop vars via `GetField(j)` (the destructure-`:=` pattern, generalized to N).
            self.compile_expr(fc, iter)?;
            let src_slot = fc.add_hidden();
            fc.emit_hidden_set(src_slot, span);
            fc.emit_hidden_get(src_slot, span);
            fc.emit(Op::IsMap, span);
            let mode_slot = fc.add_hidden(); // true ⇒ map path
            fc.emit_hidden_set(mode_slot, span);

            let lst = fc.add_hidden(); // the list we index (map keys, or the list of tuples)
            let vals = fc.add_hidden(); // map values snapshot (map path only)
            let len = fc.add_hidden();
            let idx = fc.add_hidden();
            let elem = fc.add_hidden(); // the element read at lst[idx]
            let var_slots: Vec<usize> = vars.iter().map(|v| fc.add_local(v.clone())).collect();
            // Each loop var's MECHANISM slot: a hidden raw slot when boxed (its user cell is refreshed
            // per iteration), else the user slot itself (byte-identical when uncaptured).
            let var_raws: Vec<usize> = var_slots.iter().map(|&s| fc.loopvar_raw_slot(s)).collect();

            // ----- init: branch map vs list -----
            fc.emit_hidden_get(mode_slot, span);
            let to_list_init = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ list init
            // map init: keys snapshot into `lst`, values snapshot into `vals` (same instant/order)
            fc.emit_hidden_get(src_slot, span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit_hidden_set(lst, span);
            fc.emit_hidden_get(src_slot, span);
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: "values".to_string(),
                    argc: 0,
                    ic,
                },
                span,
            );
            fc.emit_hidden_set(vals, span);
            let after_init = fc.emit_jump(Op::Jump(0), span);
            // list init: clone the list of tuples into `lst`
            fc.patch_jump(to_list_init);
            fc.emit_hidden_get(src_slot, span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit_hidden_set(lst, span);
            fc.patch_jump(after_init);
            // common: len = lst.len(), idx = 0
            fc.emit_hidden_get(lst, span);
            fc.emit(Op::ArrLen, span);
            fc.emit_hidden_set(len, span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit_hidden_set(idx, span);

            let loop_start = fc.here();
            fc.emit_hidden_get(idx, span);
            fc.emit_hidden_get(len, span);
            fc.emit(Op::Lt, span);
            let exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            // elem = lst[idx]
            fc.emit_hidden_get(lst, span);
            fc.emit_hidden_get(idx, span);
            fc.emit(Op::GetIndex, span);
            fc.emit_hidden_set(elem, span);
            // ----- bind: branch map vs list (into the MECHANISM slots `var_raws`) -----
            fc.emit_hidden_get(mode_slot, span);
            let to_list_bind = fc.emit_jump(Op::JumpIfFalse(0), span);
            // map bind: var[0] = key (elem), var[1] = vals[idx]
            fc.emit_hidden_get(elem, span);
            fc.emit_set_local_raw(var_raws[0], span);
            fc.emit_hidden_get(vals, span);
            fc.emit_hidden_get(idx, span);
            fc.emit(Op::GetIndex, span);
            fc.emit_set_local_raw(var_raws[1], span);
            let after_bind = fc.emit_jump(Op::Jump(0), span);
            // list bind: destructure the tuple element into each loop var (var[j] = elem.j)
            fc.patch_jump(to_list_bind);
            for (j, &vr) in var_raws.iter().enumerate() {
                fc.emit_hidden_get(elem, span);
                fc.emit(
                    Op::GetField {
                        name: j.to_string(),
                        ic: NO_IC,
                    },
                    span,
                ); // tuple element
                fc.emit_set_local_raw(vr, span);
            }
            fc.patch_jump(after_bind);

            // Fresh cell per iteration for each boxed (captured) loop var (C1); no-op when unboxed.
            for (i, &vs) in var_slots.iter().enumerate() {
                fc.emit_loopvar_refresh(vs, var_raws[i], span);
            }
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the index increment, so the loop advances instead of looping.
            let inc_target = fc.here();
            fc.emit_hidden_get(idx, span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit_hidden_set(idx, span);
            fc.emit(Op::Jump(loop_start), span);
            fc.patch_jump(exit);
            self.patch_loop(fc, inc_target);
        }
        fc.end_scope();
        Ok(())
    }

    /// Compile a comprehension by reusing `compile_for`: seed a hidden accumulator, then run a
    /// synthesized loop body that appends the element (`push`/`add`) or inserts the key→value pair.
    /// This means comprehensions iterate every collection — and the struct-iterator protocol —
    /// exactly like a `for` loop, with no duplicated iteration logic. Multiple `for` clauses nest
    /// (first clause outermost, last innermost) by folding the clauses RIGHT-TO-LEFT: the innermost
    /// body is the accumulator append; each clause wraps it in that clause's guards (an `if`) and
    /// then in a `compile_for`. `compile_for` opens a scope and binds the clause's vars, so a later
    /// clause's `iter`/guards (which sit inside an earlier `compile_for`'s body) see the earlier
    /// bindings. The finished accumulator is left on the stack as the expression's value.
    fn compile_comprehension(
        &mut self,
        fc: &mut FnComp,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        clauses: &[CompClause],
        span: Span,
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        // Seed the accumulator and store it in a hidden-named local. The synthesized body refers to
        // it by name; `$comp` can't collide with a user identifier (the lexer never produces `$`).
        match kind {
            CompKind::List => fc.emit(Op::NewList(0), span),
            CompKind::Set => fc.emit(Op::NewSet(0), span),
            CompKind::Map => fc.emit(Op::NewMap(0), span),
        }
        let acc_name = "$comp".to_string();
        // `$comp` is synthesized, never referenced by a user closure, so it is never boxed — the named
        // emit helpers reduce to plain `SetLocal`/`GetLocal` for it.
        let acc_slot = fc.emit_decl_named(acc_name.clone(), span);

        let acc = Expr {
            kind: ExprKind::Ident(acc_name),
            span,
        };
        let innermost_stmt = match kind {
            CompKind::List => method_call_stmt(acc, "push", vec![elem.clone()], span),
            CompKind::Set => method_call_stmt(acc, "add", vec![elem.clone()], span),
            CompKind::Map => {
                let key = key.expect("a map comprehension carries a key").clone();
                Stmt {
                    kind: StmtKind::Assign {
                        target: Expr {
                            kind: ExprKind::Index {
                                obj: Box::new(acc),
                                index: Box::new(key),
                            },
                            span,
                        },
                        op: AssignOp::Eq,
                        value: elem.clone(),
                    },
                    span,
                }
            }
        };

        // Build the synthesized nested-loop body from the inside out, then compile only the
        // outermost `for` (which contains all the inner ones). Folding right-to-left makes clause 0
        // the outermost loop, matching the interp's left-to-right recursion (parity).
        let mut body: Vec<Stmt> = vec![innermost_stmt];
        for clause in clauses.iter().rev() {
            // Wrap in this clause's guards (each `if` filters the body; chained guards nest).
            for g in clause.guards.iter().rev() {
                body = vec![Stmt {
                    kind: StmtKind::If {
                        branches: vec![(g.clone(), body)],
                        else_block: None,
                    },
                    span,
                }];
            }
            // Wrap in this clause's `for`. The first clause is compiled last (outermost).
            body = vec![Stmt {
                kind: StmtKind::For {
                    vars: clause.vars.clone(),
                    // Synthesized: comprehension clauses carry no binding spans, so default
                    // (sentinel) spans — they never collide with a real decl-site hover.
                    var_spans: vec![Span::default(); clause.vars.len()],
                    iter: (*clause.iter).clone(),
                    body,
                },
                span,
            }];
        }

        // `body` is now a single outermost `for` statement. Compile it via `compile_for`.
        let Stmt {
            kind:
                StmtKind::For {
                    vars,
                    iter,
                    body: inner,
                    ..
                },
            ..
        } = &body[0]
        else {
            unreachable!("a comprehension always has at least one for clause")
        };
        self.compile_for(fc, vars, iter, inner, span)?;
        // The comprehension's value is the finished accumulator.
        fc.emit_get_named(acc_slot, span);
        fc.end_scope();
        Ok(())
    }

    /// Pop the innermost `LoopCtx` and patch its pending jumps: `continue` → `inc_target` (the
    /// loop's increment), `break` → the current position (the loop exit, past the back-edge).
    fn patch_loop(&self, fc: &mut FnComp, inc_target: usize) {
        let ctx = fc.loops.pop().expect("for-loop ctx pushed in compile_for");
        let exit_target = fc.here();
        for j in ctx.break_jumps {
            fc.patch_jump_to(j, exit_target);
        }
        for j in ctx.continue_jumps {
            fc.patch_jump_to(j, inc_target);
        }
    }

    /// True if these arms form a literal/range/wildcard/binding match (no enum or tuple patterns), so
    /// it takes the lighter `compile_match_lit` path (no `EnsureEnum`, no `MatchNoArm`). A bare
    /// top-level identifier is a *binding* unless it names a known enum variant — that distinction is
    /// type-free thanks to the program-global variant registry, mirroring the runtime.
    fn arms_are_literal<'a>(&self, patterns: impl Iterator<Item = &'a Pattern>) -> bool {
        patterns.into_iter().all(|p| self.pattern_is_literal(p))
    }

    /// Resolve a variant reference to its `(enum, variant)` registry key. The enum is the explicit
    /// qualifier when present (user variants are always qualified post-check); a bare name resolves
    /// only as a built-in (`Ok`/`Err`→`Result`, `Some`/`None`→`Option`). `None` for any other bare
    /// name (a binding, not a variant).
    fn variant_pair(&self, enum_name: Option<&str>, name: &str) -> Option<(String, String)> {
        let en = match enum_name {
            // A pattern carries the BARE written enum name (`Color` from `Color.Red`); resolve it to
            // the module-scoped runtime key the construction path uses (`enum_bare_key`), so a
            // disambiguated enum (`cb::Color`) MATCHES — producer and consumer agree on the key. In
            // the no-collision common case this resolves back to the bare name, unchanged.
            Some(en) => self.enum_bare_key(en),
            None => match name {
                "Ok" | "Err" => "Result".to_string(),
                "Some" | "None" => "Option".to_string(),
                _ => return None,
            },
        };
        Some((en, name.to_string()))
    }

    /// Whether `(enum_name, name)` is a registered NULLARY variant (`None`, a user enum's
    /// empty-payload variant). A nested bare `Ident` naming a built-in nullary variant is a refutable
    /// variant match (the checker has promoted it), not a binding — routed by the same registry the
    /// runtime uses.
    fn is_nullary_variant(&self, enum_name: Option<&str>, name: &str) -> bool {
        self.variant_pair(enum_name, name)
            .and_then(|k| self.program.variants.get(&k))
            .is_some_and(|v| v.arity == 0)
    }

    /// M19 lever #2 — the dense `variant_id` of `(enum_name, name)`, baked into `Op::NewEnum`/
    /// `Op::MatchArm` so the VM stamps / compares it without a runtime hash lookup. `VID_NONE` if
    /// unregistered (the compiler always emits these for known variants, so the fallback is defensive).
    fn variant_id_of(&self, enum_name: Option<&str>, name: &str) -> u32 {
        self.variant_pair(enum_name, name)
            .and_then(|k| self.program.variants.get(&k))
            .map_or(crate::vm::op::VID_NONE, |v| v.variant_id)
    }

    /// Like `variant_id_of`, but `enum_key` is the ALREADY-RESOLVED module-scoped runtime key (the
    /// `type_key`/`enum_bare_key` the *construction call site* computed to decide WHICH module's enum
    /// it is building). Looks `(enum_key, name)` up directly — NO second pass through `enum_bare_key`.
    /// This is the construction-side entry point: re-resolving the key against the currently-compiled
    /// module's `bare_types` would mis-key a qualified `mod.E.V` whenever the *constructing* module
    /// also declares a colliding `E` (it'd pick the local loser's id), so the produced value could
    /// never match in its declaring module. `variant_id_of` stays the pattern/built-in entry point.
    fn variant_id_of_key(&self, enum_key: &str, name: &str) -> u32 {
        self.program
            .variants
            .get(&(enum_key.to_string(), name.to_string()))
            .map_or(crate::vm::op::VID_NONE, |v| v.variant_id)
    }

    /// The sorted, de-duplicated *binding* names introduced by an or-pattern's first alternative
    /// (the checker has verified every alternative binds the same set, so the first is canonical).
    /// A nested bare `Ident` naming a nullary variant is a variant match, NOT a binding, so it is
    /// excluded. Recurses into nested patterns; bounded by the finite pattern tree.
    fn or_binding_names(&self, alts: &[Pattern]) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        if let Some(first) = alts.first() {
            self.collect_binding_names(first, &mut names);
        }
        names.into_iter().collect()
    }

    fn collect_binding_names(&self, p: &Pattern, out: &mut std::collections::BTreeSet<String>) {
        match p {
            Pattern::Ident(n, _) if !self.is_nullary_variant(None, n) => {
                out.insert(n.clone());
            }
            Pattern::Ident(..) => {}
            Pattern::Variant { bindings, .. }
            | Pattern::Tuple(bindings)
            | Pattern::Or(bindings) => {
                for b in bindings {
                    self.collect_binding_names(b, out);
                }
            }
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
        }
    }

    /// Whether a single pattern is literal-eligible (literal/range/wildcard/binding-only), recursing
    /// into or-patterns (eligible iff every alternative is). Bounded by the finite pattern tree.
    fn pattern_is_literal(&self, p: &Pattern) -> bool {
        match p {
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => true,
            // empty-binding `Name`: a binding unless it's a real variant.
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                ..
            } if bindings.is_empty() => self
                .variant_pair(enum_name.as_deref(), name)
                .is_none_or(|k| !self.program.variants.contains_key(&k)),
            Pattern::Or(alts) => alts.iter().all(|a| self.pattern_is_literal(a)),
            _ => false,
        }
    }

    fn compile_match(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
    ) -> Result<(), CompileError> {
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, |s, fc, body| {
                s.compile_defer_scoped_arm(fc, body)
            });
        }
        self.compile_match_general(fc, scrutinee, arms, span, |s, fc, body| {
            s.compile_defer_scoped_arm(fc, body)
        })
    }

    /// Lower a `match` whose arms are variant and/or tuple patterns, with arbitrary nesting (gap
    /// #15). Each arm tests its pattern against the scrutinee (via `emit_pattern`); on a mismatch it
    /// jumps to the next arm, on a match it binds and runs the body. Serves both the statement and
    /// expression forms via the `run_body` closure.
    fn compile_match_general<A, F>(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[A],
        span: Span,
        mut run_body: F,
    ) -> Result<(), CompileError>
    where
        A: MatchArmLike,
        F: FnMut(&mut Self, &mut FnComp, &A::Body) -> Result<(), CompileError>,
    {
        fc.begin_scope();
        self.compile_expr(fc, scrutinee)?;
        let scrut = fc.add_hidden();
        fc.emit_hidden_set(scrut, span);
        // Variant matches keep the `EnsureEnum` guard so a non-enum scrutinee (possible only when
        // the checker couldn't infer the type) is a clean runtime error, not a panic. Tuple matches
        // need no such guard.
        if arms
            .iter()
            .any(|a| matches!(a.pattern(), Pattern::Variant { .. }))
        {
            fc.emit(Op::EnsureEnum(scrut), scrutinee.span);
        }
        let mut end_jumps = Vec::new();
        for arm in arms {
            fc.begin_scope();
            let mut fails = Vec::new();
            self.emit_pattern(fc, arm.pattern(), scrut, &mut fails, scrutinee.span)?;
            // The guard runs with the pattern's bindings in scope; a false guard joins `fails` and
            // falls through to the next arm.
            self.emit_guard(fc, arm.guard(), &mut fails)?;
            run_body(self, fc, arm.body())?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), span));
            // Any failed test in this arm jumps here — the start of the next arm.
            let next = fc.here();
            for j in fails {
                fc.patch_jump_to(j, next);
            }
            fc.end_scope();
        }
        fc.emit(Op::MatchNoArm(scrut), scrutinee.span); // exhaustive (checker) → a trap
        for j in end_jumps {
            fc.patch_jump(j);
        }
        fc.end_scope();
        Ok(())
    }

    /// Emit an optional arm guard `if <expr>`: compile the bool expr and, on `false`, jump to the
    /// next arm (the jump is pushed onto `fails`, which the caller patches). A `None` guard emits
    /// nothing.
    fn emit_guard(
        &mut self,
        fc: &mut FnComp,
        guard: Option<&Expr>,
        fails: &mut Vec<usize>,
    ) -> Result<(), CompileError> {
        if let Some(g) = guard {
            self.compile_expr(fc, g)?;
            fails.push(fc.emit_jump(Op::JumpIfFalse(0), g.span));
        }
        Ok(())
    }

    /// Emit code testing the value in local `scrut` against `pattern`. Failed tests push their jump
    /// onto `fails` (the caller patches them to the next arm); successful matches bind every name in
    /// the pattern to a fresh local in the current scope. Recurses for nested tuple/variant
    /// patterns. No new opcodes — reuses `MatchArm` (variant), `GetField` (tuple element), and
    /// `Eq`+`JumpIfFalse` (literal).
    fn emit_pattern(
        &mut self,
        fc: &mut FnComp,
        pattern: &Pattern,
        scrut: usize,
        fails: &mut Vec<usize>,
        span: Span,
    ) -> Result<(), CompileError> {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Ident(name, _) => {
                // A nested bare identifier naming a known NULLARY variant is a refutable
                // variant match (`Some(None)`, `Ok(Err(e))` — the checker has promoted it); it
                // binds nothing and is tested like a top-level nullary variant. Otherwise it is a
                // binding capturing the whole sub-value.
                if self.is_nullary_variant(None, name) {
                    let bind_start = fc.next_slot();
                    let variant_id = self.variant_id_of(None, name);
                    let arm_op = fc.emit_jump(
                        Op::MatchArm {
                            scrut,
                            variant: name.clone(),
                            variant_id,
                            // A nested bare nullary variant is always a BUILT-IN (`None`); user
                            // variants are qualified. The id compare is sufficient — no fallback key.
                            enum_name: None,
                            nbind: 0,
                            bind_start,
                            next: 0,
                        },
                        span,
                    );
                    fails.push(arm_op);
                } else {
                    // Bind the whole scrutinee to `name` (a fresh cell when captured).
                    fc.emit_hidden_get(scrut, span);
                    fc.emit_decl_named(name.clone(), span);
                }
            }
            Pattern::Literal(lit) => {
                fc.emit_hidden_get(scrut, span);
                emit_lit_const(fc, lit, span);
                fc.emit(Op::Eq, span);
                fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
            }
            Pattern::Range { start, end } => {
                emit_range_test(fc, scrut, *start, *end, fails, span);
            }
            Pattern::Tuple(subs) => {
                for (i, sub) in subs.iter().enumerate() {
                    fc.emit_hidden_get(scrut, span);
                    fc.emit(
                        Op::GetField {
                            name: i.to_string(),
                            ic: NO_IC,
                        },
                        span,
                    ); // tuple element `.i`
                    let elem = fc.add_hidden();
                    fc.emit_hidden_set(elem, span);
                    self.emit_pattern(fc, sub, elem, fails, span)?;
                }
            }
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                ..
            } => {
                // One slot per payload element, written positionally by `MatchArm`. An UNBOXED plain
                // `Ident` binding names its slot directly (so `Some(c)` binds `c` with no copy); a
                // BOXED plain ident (captured by a closure in the arm) needs a cell, but `MatchArm`
                // writes a raw value — so it gets a hidden RAW slot here and the user cell is bound
                // from it after the arm. A nested nullary-variant `Ident` (e.g. the `None` in
                // `Some(None)`) and any other sub-pattern also get a hidden slot to test/destructure.
                let bind_start = fc.next_slot();
                for b in bindings {
                    match b {
                        Pattern::Ident(n, _)
                            if !self.is_nullary_variant(None, n) && !fc.is_boxed_name(n) =>
                        {
                            fc.add_local(n.clone());
                        }
                        _ => {
                            fc.add_hidden();
                        }
                    }
                }
                let variant_id = self.variant_id_of(enum_name.as_deref(), name);
                let arm_op = fc.emit_jump(
                    Op::MatchArm {
                        scrut,
                        variant: name.clone(),
                        variant_id,
                        // SCRUTINEE-DRIVEN fallback — carry the BARE written enum qualifier so the
                        // VM can resolve an id-compare MISS against the scrutinee's own enum key
                        // (two whole-imported same-named enums). `None` for a built-in (`Ok`/`Err`).
                        enum_name: enum_name.clone(),
                        nbind: bindings.len(),
                        bind_start,
                        next: 0,
                    },
                    span,
                );
                fails.push(arm_op);
                for (i, b) in bindings.iter().enumerate() {
                    match b {
                        // Unboxed plain binding — the VM wrote it straight into its user slot.
                        Pattern::Ident(n, _)
                            if !self.is_nullary_variant(None, n) && !fc.is_boxed_name(n) => {}
                        // Boxed plain binding — bind the user cell from the raw slot the VM wrote.
                        Pattern::Ident(n, _) if !self.is_nullary_variant(None, n) => {
                            fc.emit_hidden_get(bind_start + i, span);
                            fc.emit_decl_named(n.clone(), span);
                        }
                        // Nested / nullary-variant sub-pattern: test/destructure the raw slot.
                        _ => self.emit_pattern(fc, b, bind_start + i, fails, span)?,
                    }
                }
            }
            Pattern::Or(alts) => {
                // Pre-allocate ONE canonical slot per agreed binding name (the checker has verified
                // every alternative binds the same set). Each alternative binds into fresh scratch
                // slots, then copies its values into the canonical slots before jumping to a shared
                // matched-label; the body reads the canonical slots regardless of which alt matched.
                let names = self.or_binding_names(alts);
                let canon: Vec<(String, usize)> = names
                    .iter()
                    .map(|n| {
                        let slot = fc.add_local(n.clone());
                        // A BOXED canonical binding needs its cell allocated up front — each
                        // alternative `CellStore`s its value into this one shared cell.
                        if fc.is_boxed_slot(slot) {
                            fc.emit(Op::Nil, span);
                            fc.emit(Op::NewCell, span);
                            fc.emit_set_local_raw(slot, span);
                        }
                        (n.clone(), slot)
                    })
                    .collect();
                let mut matched_jumps = Vec::new();
                for (idx, alt) in alts.iter().enumerate() {
                    // Scope each alternative's scratch slots so they don't leak between alternatives.
                    fc.begin_scope();
                    let mut alt_fails = Vec::new();
                    self.emit_pattern(fc, alt, scrut, &mut alt_fails, span)?;
                    // Copy this alternative's bindings into the canonical slots, then jump to the
                    // shared matched-label (fall-through past the remaining alternatives). Reading via
                    // `emit_get_named` (CellLoad if the alt's binding is boxed) and writing via
                    // `emit_set_named` (CellStore into the canonical cell) keeps the unboxed case
                    // byte-identical to a plain `GetLocal`/`SetLocal`.
                    for (name, slot) in &canon {
                        let src = fc.resolve_local(name).expect("alt binds the agreed name");
                        if src != *slot {
                            fc.emit_get_named(src, span);
                            fc.emit_set_named(*slot, span);
                        }
                    }
                    matched_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    // A miss in this alternative falls through to the next alternative (or, for the
                    // last alternative, joins the arm `fails` → next arm).
                    let next = fc.here();
                    if idx + 1 < alts.len() {
                        for j in alt_fails {
                            fc.patch_jump_to(j, next);
                        }
                    } else {
                        fails.extend(alt_fails);
                    }
                    fc.end_scope();
                }
                // All matched-jumps land here: bindings are in the canonical slots, run guard+body.
                for j in matched_jumps {
                    fc.patch_jump(j);
                }
            }
        }
        Ok(())
    }

    /// Lower a literal/wildcard `match` (no `EnsureEnum` — a literal `match` on an int must not
    /// raise "cannot match on int"). Each literal arm does `scrut == literal`, jumping to the next
    /// arm on a miss; the (checker-guaranteed) `_` wildcard is the unconditional fallback. The
    /// `run_body` closure compiles an arm body — a statement block or a value-expression — so this
    /// serves both the statement and expression forms.
    fn compile_match_lit<A, F>(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[A],
        span: Span,
        mut run_body: F,
    ) -> Result<(), CompileError>
    where
        A: MatchArmLike,
        F: FnMut(&mut Self, &mut FnComp, &A::Body) -> Result<(), CompileError>,
    {
        fc.begin_scope();
        self.compile_expr(fc, scrutinee)?;
        let scrut = fc.add_hidden();
        fc.emit_hidden_set(scrut, span);
        let mut end_jumps = Vec::new();
        for arm in arms {
            match arm.pattern() {
                Pattern::Literal(lit) => {
                    fc.begin_scope();
                    fc.emit_hidden_get(scrut, scrutinee.span);
                    emit_lit_const(fc, lit, span);
                    fc.emit(Op::Eq, span);
                    let mut fails = vec![fc.emit_jump(Op::JumpIfFalse(0), span)];
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Range { start, end } => {
                    fc.begin_scope();
                    let mut fails = Vec::new();
                    emit_range_test(fc, scrut, *start, *end, &mut fails, span);
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Wildcard => {
                    // A bare `_` is the unconditional fallback (the checker guarantees exactly one
                    // covers every reachable path). A *guarded* `_ if g:` is refutable: its guard may
                    // fail, so it tests the guard and falls through to the next arm on a miss.
                    fc.begin_scope();
                    let mut fails = Vec::new();
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                // A bare top-level identifier that isn't a known variant is a binding catch-all
                // capturing the whole scrutinee (irrefutable, like `_` but named).
                Pattern::Variant { name, bindings, .. } if bindings.is_empty() => {
                    fc.begin_scope();
                    fc.emit_hidden_get(scrut, span);
                    fc.emit_decl_named(name.clone(), span);
                    let mut fails = Vec::new();
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Or(alts) => {
                    // All-literal or-pattern = OR-of-equality chain: any alternative hit runs the
                    // body; only when EVERY alternative misses do we fall through to the next arm.
                    // Literal/range alternatives bind nothing; a bare-binding/wildcard alternative is
                    // an unconditional hit (an irrefutable catch-all inside the or).
                    fc.begin_scope();
                    let mut hit_jumps = Vec::new();
                    let mut fails = Vec::new();
                    for (idx, alt) in alts.iter().enumerate() {
                        let last = idx + 1 == alts.len();
                        match alt {
                            Pattern::Literal(lit) => {
                                fc.emit_hidden_get(scrut, scrutinee.span);
                                emit_lit_const(fc, lit, span);
                                fc.emit(Op::Eq, span);
                                if last {
                                    // Last alternative: a miss joins `fails` → next arm.
                                    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
                                } else {
                                    // Earlier alternative: a miss falls through to the next
                                    // alternative's test; a hit jumps to the body.
                                    let miss = fc.emit_jump(Op::JumpIfFalse(0), span);
                                    hit_jumps.push(fc.emit_jump(Op::Jump(0), span));
                                    fc.patch_jump(miss);
                                }
                            }
                            Pattern::Range { start, end } => {
                                let mut alt_fails = Vec::new();
                                emit_range_test(fc, scrut, *start, *end, &mut alt_fails, span);
                                if last {
                                    fails.extend(alt_fails);
                                } else {
                                    // On a hit (no fail), jump to the body; patch the range's fails
                                    // to the next alternative.
                                    hit_jumps.push(fc.emit_jump(Op::Jump(0), span));
                                    let next = fc.here();
                                    for j in alt_fails {
                                        fc.patch_jump_to(j, next);
                                    }
                                }
                            }
                            Pattern::Wildcard | Pattern::Variant { .. } => {
                                // An unconditional catch-all alternative — always hits.
                                hit_jumps.push(fc.emit_jump(Op::Jump(0), span));
                            }
                            _ => unreachable!(
                                "literal or-pattern has only literal/range/wildcard/binding alternatives"
                            ),
                        }
                    }
                    // All hit_jumps land here (the body); a last-alt miss in `fails` skips it.
                    for j in hit_jumps {
                        fc.patch_jump(j);
                    }
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Variant { .. } | Pattern::Tuple(_) | Pattern::Ident(..) => {
                    unreachable!(
                        "literal match has only literal/range/wildcard/binding arms (arms_are_literal)"
                    )
                }
            }
        }
        for j in end_jumps {
            fc.patch_jump(j);
        }
        fc.end_scope();
        Ok(())
    }

    // ----- expressions -----

    fn compile_expr(&mut self, fc: &mut FnComp, expr: &Expr) -> Result<(), CompileError> {
        // One-way int→float widening — the collection-element hint set by a typed `let` value applies
        // to the IMMEDIATE collection literal only. Take it (clearing the field) so any non-collection
        // value, a nested element, or a call argument does NOT inherit it; the `List`/`Map` arms below
        // re-read it from this local.
        let elem_hint = self.float_elem_hint.take();
        match &expr.kind {
            ExprKind::Int(n) => fc.emit(Op::ConstInt(*n), expr.span),
            ExprKind::Float(x) => fc.emit(Op::ConstFloat(*x), expr.span),
            ExprKind::Bool(b) => fc.emit(if *b { Op::True } else { Op::False }, expr.span),
            ExprKind::Str(raw) => self.compile_str(fc, raw, expr.span)?,
            // Raw string: emit the literal directly — does NOT go through `compile_str` /
            // `parse_interpolation`, so braces stay literal and backslashes are verbatim.
            ExprKind::RawStr(s) => fc.emit(Op::ConstStr(s.clone()), expr.span),
            ExprKind::Bytes(b) => fc.emit(Op::ConstBytes(b.clone().into_boxed_slice()), expr.span),
            ExprKind::Ident(name) => self.compile_ident(fc, name, expr.span),
            ExprKind::List(items) => {
                // One-way int→float widening for THIS list: widen an element when the `List[float]`
                // annotation says so OR the constant peephole fires (≥1 untyped float CONSTANT sibling
                // → widen the untyped int CONSTANT siblings). The checker accepts nothing else.
                let annotated = elem_hint == Some(crate::ast::ElemFloatHint::Elem);
                let peephole = literal_numeric_mix(items.iter());
                for it in items {
                    self.compile_expr(fc, it)?;
                    if annotated || (peephole && crate::ast::untyped_int_const(it)) {
                        fc.emit(Op::CoerceFloat, it.span);
                    }
                }
                fc.emit(Op::NewList(items.len()), expr.span);
            }
            ExprKind::Tuple(items) => {
                // A tuple is heterogeneous (positional types), so no element widening.
                for it in items {
                    self.compile_expr(fc, it)?;
                }
                fc.emit(Op::NewTuple(items.len()), expr.span);
            }
            ExprKind::Map(entries) => {
                // Push `[k0, v0, k1, v1, …]`, then build the map (last duplicate key wins at runtime).
                // One-way int→float widening on the VALUE position only (keys are never float): the
                // `Map[_, float]` annotation, or the constant peephole over the value column.
                let annotated = elem_hint == Some(crate::ast::ElemFloatHint::MapValue);
                let peephole = literal_numeric_mix(entries.iter().map(|(_, v)| v));
                for (k, v) in entries {
                    self.compile_expr(fc, k)?;
                    self.compile_expr(fc, v)?;
                    if annotated || (peephole && crate::ast::untyped_int_const(v)) {
                        fc.emit(Op::CoerceFloat, v.span);
                    }
                }
                fc.emit(Op::NewMap(entries.len()), expr.span);
            }
            ExprKind::Set(elems) => {
                // No widening: a `float` is not Hashable, so a set element is never float.
                for e in elems {
                    self.compile_expr(fc, e)?;
                }
                fc.emit(Op::NewSet(elems.len()), expr.span);
            }
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.compile_comprehension(fc, *kind, key.as_deref(), elem, clauses, expr.span)?,
            ExprKind::Unary { op, expr: inner } => {
                self.compile_expr(fc, inner)?;
                match op {
                    UnaryOp::Neg => fc.emit(Op::Neg, expr.span),
                    UnaryOp::Not => {
                        fc.emit(Op::AsBool, inner.span);
                        fc.emit(Op::Not, expr.span);
                    }
                }
            }
            ExprKind::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                lhs,
                rhs,
            } => {
                // Short-circuit: lhs is always bool-checked; rhs only when needed; result is bool.
                self.compile_expr(fc, lhs)?;
                fc.emit(Op::AsBool, lhs.span);
                let jump = match op {
                    BinaryOp::And => fc.emit_jump(Op::JumpIfFalseKeep(0), expr.span),
                    _ => fc.emit_jump(Op::JumpIfTrueKeep(0), expr.span),
                };
                fc.emit(Op::Pop, expr.span);
                self.compile_expr(fc, rhs)?;
                fc.emit(Op::AsBool, rhs.span);
                fc.patch_jump(jump);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.compile_expr(fc, lhs)?;
                self.compile_expr(fc, rhs)?;
                fc.emit(binary_op(*op), expr.span);
            }
            ExprKind::Range { .. } => {
                // A bare range has no runtime value: it is lowered ONLY as a `for`/comprehension
                // iterable (a counting loop) or a slice receiver (materialize + slice), none of
                // which route through here.
                //
                // DEFENSIVE BACKSTOP — unreachable from a check-clean program. The checker rejects
                // every value use of a range up front (`RANGE_NOT_A_VALUE`, checker/pattern.rs),
                // so its accepted set is now a subset of what this compiler can lower; that fix is
                // what stops this error from surfacing at RUN time on a program `chezzi check`
                // called clean. It stays because the compiler is also driven WITHOUT the checker:
                // synthesized ASTs (difftest / panicfuzz) and the VM test helpers `run_capture` /
                // `run_capture_parallel` skip type-checking entirely.
                return Err(CompileError {
                    message: "a range can only be used as the iterable of a `for` loop".to_string(),
                    span: expr.span,
                });
            }
            // `type_args` are type-erased — the compiler never sees them (checker already used them).
            ExprKind::Call {
                callee,
                args,
                named,
                ..
            } => self.compile_call(fc, callee, args, named, expr.span)?,
            ExprKind::Field { obj, name, .. } => {
                // `module.Enum.Variant` (nullary, qualified value form) → construct the variant.
                if let ExprKind::Field {
                    obj: inner,
                    name: ename,
                    ..
                } = &obj.kind
                    && let ExprKind::Ident(mname) = &inner.kind
                    && fc.resolve_local(mname).is_none()
                    && !fc.captures(mname)
                    && let Some(&tidx) = self.imported_modules.get(mname)
                    && self
                        .module_types
                        .get(tidx)
                        .is_some_and(|t| t.contains(ename))
                    && self
                        .program
                        .variants
                        .get(&(self.type_key(tidx, ename), name.clone()))
                        .is_some_and(|d| d.arity == 0)
                {
                    let ekey = self.type_key(tidx, ename);
                    let variant_id = self.variant_id_of_key(&ekey, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: 0,
                        },
                        expr.span,
                    );
                    return Ok(());
                }
                // `Type[T…].Variant` (nullary, declaration-site turbofish value form) → construct
                // the variant. The type args are runtime-erased — identical bytecode to the bare
                // `Enum.Variant` value form. Both carriers converge via `type_apply_head_name`.
                if let Some(tname) = type_apply_head_name(&obj.kind)
                    && fc.resolve_local(tname).is_none()
                    && !fc.captures(tname)
                    && let ekey = self.enum_bare_key(tname)
                    && self
                        .program
                        .variants
                        .get(&(ekey.clone(), name.clone()))
                        .is_some_and(|d| d.arity == 0)
                {
                    let variant_id = self.variant_id_of_key(&ekey, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: 0,
                        },
                        expr.span,
                    );
                    return Ok(());
                }
                // `Enum.Variant` (nullary) → construct the variant, mirroring bare `compile_ident`.
                // A real binding (local/captured) named like the enum wins, matching the checker.
                // The enum resolves to its bare-visible runtime key (`enum_bare_key`).
                if let ExprKind::Ident(ename) = &obj.kind
                    && fc.resolve_local(ename).is_none()
                    && !fc.captures(ename)
                    && let ekey = self.enum_bare_key(ename)
                    && self
                        .program
                        .variants
                        .get(&(ekey.clone(), name.clone()))
                        .is_some_and(|d| d.arity == 0)
                {
                    let variant_id = self.variant_id_of_key(&ekey, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: 0,
                        },
                        expr.span,
                    );
                } else {
                    self.compile_expr(fc, obj)?;
                    let ic = self.next_field_ic(name);
                    fc.emit(
                        Op::GetField {
                            name: name.clone(),
                            ic,
                        },
                        expr.span,
                    );
                }
            }
            ExprKind::Index { obj, index } => {
                // Generic-fn-as-value turbofish erase: `ident[int]` where `ident` is a (non-shadowed)
                // top-level fn is a generic-fn turbofish the checker already validated/accepted. The
                // runtime is generic-ERASED — the value IS the underlying function — so drop the type
                // index and load only the plain fn value. The checker rejects EVERY non-generic-fn
                // `fn`-typed Index, so the only fn Index that reaches codegen is exactly this case;
                // a shadowing local/capture (`xs := [1,2]; xs[0]`) is never in `fn_names`, so a real
                // index still compiles below.
                if let ExprKind::Ident(name) = &obj.kind
                    && self.fn_names.contains(name)
                    && fc.resolve_local(name).is_none()
                    && !fc.captures(name)
                {
                    self.compile_expr(fc, obj)?;
                    return Ok(());
                }
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                // No `AsInt`: the index may be a map key (str/bool), not just a list/str int.
                // `GetIndex` validates int-ness in its list/str arm at runtime.
                fc.emit(Op::GetIndex, expr.span);
            }
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => {
                // Slicing a range literal `(a..b)[start:end:step]` materializes the (ascending,
                // step-1) range to a `List[int]` first (via the `range` builtin), then slices that
                // list through the shared `GetSlice` path — reusing the `::step` machinery, not a
                // parallel one. A bare range still has no value anywhere else (the error below fires
                // for `let x = 0..10`); only this slice-receiver position is unblocked.
                if let ExprKind::Range { start: rs, end: re } = &obj.kind {
                    self.compile_expr(fc, rs)?;
                    self.compile_expr(fc, re)?;
                    fc.emit(Op::CallBuiltin("range".to_string(), 2), obj.span);
                } else {
                    self.compile_expr(fc, obj)?;
                }
                // Each omitted component compiles to `nil` (mapped to `None`/default at runtime).
                for comp in [start, end, step] {
                    match comp {
                        Some(e) => self.compile_expr(fc, e)?,
                        None => fc.emit(Op::Nil, expr.span),
                    }
                }
                fc.emit(Op::GetSlice, expr.span);
            }
            ExprKind::Try(inner) => {
                self.compile_expr(fc, inner)?;
                fc.emit(Op::Try, expr.span);
            }
            // `?.`/`??` carriers are lowered to `match` by the desugar pass before compilation.
            ExprKind::OptChain { .. } | ExprKind::NullCoalesce { .. } => {
                unreachable!("`?.`/`??` must be lowered by the desugar pass before compiling")
            }
            // A `TypeApply` (`Type[T1, T2]`) is only ever the receiver of a member access / call;
            // the checker resolves it into a variant ctor / static-method call (`infer_call`) or a
            // nullary variant value (`infer_field`). Once type-checking passes, the compiler walks
            // the resolved call/field, so a bare `TypeApply` never reaches codegen.
            ExprKind::TypeApply { name, .. } => {
                unreachable!(
                    "type-application head `{name}[…]` must be consumed by the checker before compiling"
                )
            }
            ExprKind::DecodeCall { obj, ty, arg } => {
                // Reuse the module's own `parse` (`obj.parse(arg)` → Result[Json]), then coerce the
                // parsed value into the target type with a descriptor built from `ty`.
                let parse_call = Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            kind: ExprKind::Field {
                                obj: obj.clone(),
                                name: "parse".to_string(),
                                name_span: expr.span,
                            },
                            span: expr.span,
                        }),
                        args: vec![(**arg).clone()],
                        named: Vec::new(),
                        type_args: Vec::new(),
                    },
                    span: expr.span,
                };
                self.compile_expr(fc, &parse_call)?;
                // ROOT REDESIGN — resolve the decode target (and its nested field struct types) to
                // their qualified IDENTITY KEYS via a module-aware env, so the descriptor tags the
                // produced struct with the right key and decodes against the right layout.
                let env = self.decode_env();
                let desc = crate::json_decode::from_type(
                    ty,
                    self.current_module_idx,
                    &env,
                    &mut Vec::new(),
                )
                .map_err(|message| CompileError {
                    message,
                    span: expr.span,
                })?;
                fc.emit(Op::JsonDecode(desc), expr.span);
            }
            ExprKind::Closure { params, body, .. } => {
                self.compile_closure(fc, params, body, expr.span)?
            }
            ExprKind::Match { scrutinee, arms } => {
                self.compile_match_expr(fc, scrutinee, arms, expr.span)?
            }
            ExprKind::IfElse { cond, then, els } => self.compile_if_expr(fc, cond, then, els)?,
            ExprKind::Recover(block) => self.compile_recover(fc, block, expr.span)?,
        }
        Ok(())
    }

    /// `recover: <block>` — install a handler over the block; on the happy path wrap the block's
    /// trailing-expression value in `Ok`, on a caught fault the VM has pushed the message `str` and
    /// we wrap it in `Err`. Both paths leave exactly one `Result` value on the stack.
    fn compile_recover(
        &mut self,
        fc: &mut FnComp,
        block: &[Stmt],
        span: Span,
    ) -> Result<(), CompileError> {
        // The handler target is `done`. Three paths converge there, each leaving one `Result`:
        //   • normal: wrap the trailing value in `Ok`, drop the handler, fall through;
        //   • panic: the VM unwinds, pushes `Err(message)`, and jumps to `done`;
        //   • `?` Err/None in the block: `do_try` pushes the propagated value and jumps to `done`.
        let push = fc.emit_jump(Op::PushHandler(0), span);
        fc.begin_scope();
        if let Some((last, init)) = block.split_last() {
            for stmt in init {
                self.compile_stmt(fc, stmt)?;
            }
            match &last.kind {
                StmtKind::Expr(e) => self.compile_expr(fc, e)?, // trailing value
                // A trailing statement-form `match`/`if` whose every arm/branch produces a value is
                // the block's value expression — compile it to leave ONE value on the stack (the
                // unified arm/branch value), exactly like an inline expression tail. The
                // `crate::ast` predicate is the SAME one the checker used to type this as
                // `Result[T]`, so the two stages agree on which tail is a value (never `Ok(nil)`).
                StmtKind::Match { scrutinee, arms } if crate::ast::match_tail_is_value(arms) => {
                    self.compile_match_value_tail(fc, scrutinee, arms, last.span)?;
                }
                StmtKind::If {
                    branches,
                    else_block,
                } if crate::ast::if_tail_is_value(branches, else_block) => {
                    self.compile_if_value(fc, branches, else_block.as_deref(), last.span)?;
                }
                _ => {
                    self.compile_stmt(fc, last)?;
                    fc.emit(Op::Nil, span);
                }
            }
        } else {
            fc.emit(Op::Nil, span);
        }
        fc.end_scope();
        // The recover block is a defer scope: drain its own defers at the boundary (Ok path), before
        // wrapping the value. The fault and `?` paths drain via the handler in the VM. Only emit when
        // the block directly holds a `defer`, keeping defer-free recovers byte-identical.
        if block_has_defer(block) {
            fc.emit(Op::DrainHandlerDefers, span);
        }
        fc.emit(
            Op::NewEnum {
                variant: "Ok".to_string(),
                variant_id: crate::vm::op::VID_OK,
                argc: 1,
            },
            span,
        );
        fc.emit(Op::PopHandler, span);
        let done = fc.here();
        fc.patch_jump_to(push, done);
        Ok(())
    }

    /// A statement-form `match` used as the `recover:` block's VALUE tail: dispatch through the
    /// generic `compile_match_lit`/`compile_match_general` (identical control flow to a statement
    /// match) but with a `run_body` that leaves the arm's trailing-expression VALUE on the stack, so
    /// the whole match converges one value at the join — exactly like `compile_match_expr`. Only
    /// reached when [`crate::ast::match_tail_is_value`] holds (every arm body ends in an `Expr`).
    fn compile_match_value_tail(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
    ) -> Result<(), CompileError> {
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, |s, fc, body| {
                s.compile_arm_tail_value(fc, body)
            });
        }
        self.compile_match_general(fc, scrutinee, arms, span, |s, fc, body| {
            s.compile_arm_tail_value(fc, body)
        })
    }

    /// Compile a statement-`match` arm body as a VALUE (recover-tail): the arm's lexical scope is
    /// already opened/closed by `compile_match_{lit,general}`, so this is the value analog of
    /// `compile_defer_scoped_arm` — defer-bracket the *flat* body (when it directly holds a `defer`),
    /// compile init statements, and leave the trailing `Expr`'s value on the stack. The `defer` drain
    /// (`LeaveDeferScope`) touches only `frame.deferred`, never the operand stack, so the value
    /// survives it. `match_tail_is_value` guarantees a non-empty body with a trailing `Expr`.
    fn compile_arm_tail_value(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        let has_defer = block_has_defer(stmts);
        if has_defer {
            fc.emit(Op::EnterDeferScope, stmts[0].span);
            fc.defer_scopes += 1;
        }
        self.compile_tail_value_body(fc, stmts)?;
        if has_defer {
            fc.emit(Op::LeaveDeferScope, stmts[stmts.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// Statement-form `if/else` used as the `recover:` block's VALUE tail: the value analog of
    /// `compile_if`. Each branch (and the mandatory `else`) leaves exactly one value via
    /// `compile_branch_tail_value`, so one value remains at the join. Only reached when
    /// [`crate::ast::if_tail_is_value`] holds (has an `else`; every branch/else body ends in an `Expr`).
    fn compile_if_value(
        &mut self,
        fc: &mut FnComp,
        branches: &[(Expr, Block)],
        else_block: Option<&[Stmt]>,
        _span: Span,
    ) -> Result<(), CompileError> {
        let mut end_jumps = Vec::new();
        for (cond, body) in branches {
            self.compile_expr(fc, cond)?;
            fc.emit(Op::AsBool, cond.span);
            let skip = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
            self.compile_branch_tail_value(fc, body)?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), cond.span));
            fc.patch_jump(skip);
        }
        // `if_tail_is_value` guarantees an `else` — control always reaches a value-leaving branch.
        if let Some(body) = else_block {
            self.compile_branch_tail_value(fc, body)?;
        }
        for j in end_jumps {
            fc.patch_jump(j);
        }
        Ok(())
    }

    /// Compile an `if`/`else` branch body as a VALUE (recover-tail): the value analog of
    /// `compile_defer_scoped_block` — own lexical scope (locals don't leak), defer-bracketed, leaving
    /// the trailing `Expr`'s value on the stack. `if_tail_is_value` guarantees a non-empty body with a
    /// trailing `Expr`.
    fn compile_branch_tail_value(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        let has_defer = block_has_defer(stmts);
        if has_defer {
            fc.emit(Op::EnterDeferScope, stmts[0].span);
            fc.defer_scopes += 1;
        }
        self.compile_tail_value_body(fc, stmts)?;
        if has_defer {
            fc.emit(Op::LeaveDeferScope, stmts[stmts.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        fc.end_scope();
        Ok(())
    }

    /// Compile a value-producing statement block's init statements for effect, then leave the
    /// trailing `Expr`'s value on the stack. Shared by the recover-tail match-arm and if-branch value
    /// helpers so they can never disagree on how a tail block yields its value. The caller's
    /// `crate::ast` predicate guarantees a non-empty block with a trailing `Expr`; the defensive `_`
    /// arm keeps a `nil` if that ever changes (never reached from the recover-tail dispatch).
    fn compile_tail_value_body(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        let (last, init) = stmts
            .split_last()
            .expect("tail-value predicate guarantees a non-empty block");
        for stmt in init {
            self.compile_stmt(fc, stmt)?;
        }
        match &last.kind {
            StmtKind::Expr(e) => self.compile_expr(fc, e)?, // trailing value
            _ => {
                self.compile_stmt(fc, last)?;
                fc.emit(Op::Nil, last.span);
            }
        }
        Ok(())
    }

    /// Expression-position `match`: like `compile_match`, but each arm body is compiled as an
    /// expression that leaves its value on the stack, so the whole `match` yields one value.
    fn compile_match_expr(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[MatchExprArm],
        span: Span,
    ) -> Result<(), CompileError> {
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, |s, fc, body| {
                s.compile_expr(fc, body) // leaves the arm's value on the stack
            });
        }
        self.compile_match_general(fc, scrutinee, arms, span, |s, fc, body| {
            s.compile_expr(fc, body) // leaves the arm's value on the stack
        })
    }

    /// Expression-position `if c: a else: b`: condition, then jump to whichever branch; both
    /// branches leave exactly one value, so one value remains at the join.
    fn compile_if_expr(
        &mut self,
        fc: &mut FnComp,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_expr(fc, cond)?;
        fc.emit(Op::AsBool, cond.span);
        let skip = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
        self.compile_expr(fc, then)?;
        let end = fc.emit_jump(Op::Jump(0), cond.span);
        fc.patch_jump(skip);
        self.compile_expr(fc, els)?;
        fc.patch_jump(end);
        Ok(())
    }

    fn compile_ident(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        // A bare nullary *built-in* variant used as a value (`None`) — resolved before any env
        // lookup, exactly like the interpreter. User variants are qualified (handled in the `Field`
        // arm), so only built-ins resolve bare here.
        if let Some(def) = self
            .variant_pair(None, name)
            .and_then(|k| self.program.variants.get(&k))
            && def.arity == 0
        {
            let variant_id = def.variant_id;
            fc.emit(
                Op::NewEnum {
                    variant: name.to_string(),
                    variant_id,
                    argc: 0,
                },
                span,
            );
            return;
        }
        // A first-class universe builtin fn (`print`/`ord`/`chr`/`panic`) used in VALUE position
        // (`f := ord`, HOF arg, bare `defer print(...)`) — emit a dedicated `LoadBuiltin` that pushes
        // an `Obj::Builtin` handle. DIRECT calls never reach here: `compile_call` intercepts `print`
        // and `is_builtin(name)` before the generic value fallthrough, so `print(x)`/`ord(c)` keep
        // their specialized `CallPrint`/`CallBuiltin` opcodes (no hot-path change).
        //
        // A USER BINDING SHADOWS THE BUILTIN. `is_reserved_name` bans only `fn print`/type/import-alias
        // declarations — NOT local/param/loop/global bindings (`ord := 5`, `fn f(ord: int)`,
        // `for chr in xs`), so those are legal. The checker's `infer_ident` resolves `lookup(name)`
        // (locals/params/globals) BEFORE the first-class-builtin arm, so it types the binding; the
        // runtime MUST match, or a shadowed name type-checks as the binding but prints `<builtin fn …>`.
        // Emit `LoadBuiltin` ONLY when no local/capture/global binding owns the name.
        if crate::checker::is_firstclass_builtin_fn(name)
            && fc.resolve_local(name).is_none()
            && !fc.captures(name)
            && !self.globals.contains_key(name)
        {
            fc.emit(Op::LoadBuiltin(name.to_string()), span);
            return;
        }
        self.emit_load(fc, name, span);
    }

    /// `defer <call>` — evaluate the receiver/args now (Go semantics) and register a deferred call
    /// on the frame; the call runs LIFO when the frame exits. Mirrors `compile_call`'s method-vs-value
    /// split: `DeferMethod` for `obj.m(a)`, `DeferCall` for a value callee.
    fn compile_defer(
        &mut self,
        fc: &mut FnComp,
        target: &DeferTarget,
        span: Span,
    ) -> Result<(), CompileError> {
        let call = match target {
            DeferTarget::Call(call) => call,
            DeferTarget::Block(body) => {
                // `defer:` block → a synthetic zero-arg closure capturing the referenced bindings by
                // reference at the defer point (exactly `compile_spawn`'s Block arm, minus the
                // airlock), then defer-invoke it with 0 args. Reuses `MakeClosure` + `DeferCall` — no
                // new op. Free-variable capture (Finding D): only names the block references.
                let entries = filter_entries_free_block(&fc.snapshot_entries(), body, &[]);
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<deferred block>".to_string(), 0, false);
                // Uniform by-reference capture (Task A): the block's own boxed-name set (unwired).
                child.boxed_names = captured_names_of_body(body, &[]);
                child.captured_names = captured_names;
                // M-C: a `defer:` block is its own function body (it runs as a closure in its own
                // frame at frame exit) — a bare `spawn` inside it binds to the block's own implicit
                // nursery, joined when the deferred block returns. Mirrors `compile_spawn`'s block arm
                // (and the interp's `run_block_task`, which gates deferred blocks the same way).
                let implicit = block_has_bare_spawn(body);
                if implicit {
                    child.has_implicit_nursery = true;
                    child.emit(Op::EnterNursery, span);
                    child.nursery_scopes += 1;
                }
                self.compile_block_scoped(&mut child, body)?;
                if implicit {
                    child.nursery_scopes -= 1;
                }
                child.emit(Op::Nil, span);
                child.emit(Op::Return, span);
                let pid = self.finish(child);
                fc.emit(Op::MakeClosure(pid, entries), span);
                fc.emit(Op::DeferCall(0), span);
                return Ok(());
            }
        };
        let ExprKind::Call {
            callee,
            args,
            named,
            ..
        } = &call.kind
        else {
            return Err(CompileError {
                message: "defer requires a function or method call".to_string(),
                span,
            });
        };
        if let ExprKind::Field { obj, name, .. } = &callee.kind {
            self.compile_expr(fc, obj)?;
            for a in args {
                self.compile_expr(fc, a)?;
            }
            fc.emit(Op::DeferMethod(name.clone(), args.len()), call.span);
            return Ok(());
        }
        // A deferred VALUE call carrying keyword arguments (Swift-style): reorder the combined
        // `[positional ++ named]` args by the checker-recorded permutation, then defer positionally —
        // same lowering as the eager value keyword call, just via `DeferCall`.
        if !named.is_empty()
            && let Some(perm) = self
                .keyword_calls
                .get(&crate::checker::keyword_key(
                    self.current_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    named,
                    call.span,
                ))
                .cloned()
        {
            self.compile_expr(fc, callee)?;
            for &ci in &perm {
                let e = if ci < args.len() {
                    &args[ci]
                } else {
                    &named[ci - args.len()].1
                };
                self.compile_expr(fc, e)?;
            }
            fc.emit(Op::DeferCall(perm.len()), call.span);
            return Ok(());
        }
        self.compile_expr(fc, callee)?;
        for a in args {
            self.compile_expr(fc, a)?;
        }
        fc.emit(Op::DeferCall(args.len()), call.span);
        Ok(())
    }

    /// Compile a struct constructor's positional argument list, coercing any argument whose declared
    /// field type is `float` (one-way int→float widening) with `Op::CoerceFloat` right after that
    /// argument is pushed — so the value sits on the stack as a genuine `f64` before `NewStruct`
    /// consumes it. The field types come from the desugar-completed `struct_fields[key]` (defaults are
    /// already filled, so `args.len()` matches the field count). A generic field typed `T` is not
    /// `float`, so it is left untouched (matching the no-generic-widening carve-out). With no float
    /// fields this is byte-identical to the old flat `for a in args { compile_expr }` loop.
    fn compile_ctor_args(
        &mut self,
        fc: &mut FnComp,
        key: &str,
        args: &[Expr],
    ) -> Result<(), CompileError> {
        // Snapshot the per-field float-ness up front so we don't borrow `self.struct_fields` across
        // the `&mut self` call to `compile_expr`.
        // Field types are written in the struct's DECLARING module, so an alias (`v: F`) resolves in
        // that module's scope — not the constructing one.
        let home = self
            .program
            .structs
            .get(key)
            .map(|d| d.module_idx)
            .unwrap_or(self.current_module_idx);
        // …and in the struct's OWN generic scope: a field `v: F` of `struct S[F]` is the type VARIABLE
        // `F`, never a module `type F = float` alias (the checker resolves it that way too).
        let empty = std::collections::HashSet::new();
        let shadow = self.struct_generics.get(key).unwrap_or(&empty);
        let float_field: Vec<bool> = self
            .struct_fields
            .get(key)
            .map(|fields| {
                fields
                    .iter()
                    .map(|f| self.float_aliases.is_float(home, &f.ty, shadow))
                    .collect()
            })
            .unwrap_or_default();
        for (i, a) in args.iter().enumerate() {
            self.compile_expr(fc, a)?;
            if float_field.get(i).copied().unwrap_or(false) {
                fc.emit(Op::CoerceFloat, a.span);
            }
        }
        Ok(())
    }

    fn compile_call(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        span: Span,
    ) -> Result<(), CompileError> {
        // Method / module-member call: `obj.name(args)`.
        if let ExprKind::Field { obj, name, .. } = &callee.kind {
            // `module.Struct(args)` → qualified struct constructor. `module` is a bound module name
            // whose target declares struct `name`; emit `NewStruct` keyed by that module's runtime key.
            if let ExprKind::Ident(mname) = &obj.kind
                && fc.resolve_local(mname).is_none()
                && !fc.captures(mname)
                && let Some(&tidx) = self.imported_modules.get(mname)
                && self
                    .module_types
                    .get(tidx)
                    .is_some_and(|t| t.contains(name))
            {
                let key = self.type_key(tidx, name);
                if self.program.structs.contains_key(&key) {
                    self.compile_ctor_args(fc, &key, args)?;
                    fc.emit(Op::NewStruct(key, args.len()), span);
                    return Ok(());
                }
                // `module.NewType(args)` → qualified newtype constructor; emit `Op::NewType` keyed
                // by the target module's runtime key (mirrors the bare newtype ctor below).
                if self.program.newtype_home.contains_key(&key) {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::NewType(key), span);
                    return Ok(());
                }
            }
            // `module.Enum.Variant(args)` → qualified payload-variant constructor. `obj` is the
            // qualified `module.Enum`; resolve the enum's runtime key in the target module.
            if let ExprKind::Field {
                obj: inner,
                name: ename,
                ..
            } = &obj.kind
                && let ExprKind::Ident(mname) = &inner.kind
                && fc.resolve_local(mname).is_none()
                && !fc.captures(mname)
                && let Some(&tidx) = self.imported_modules.get(mname)
                && self
                    .module_types
                    .get(tidx)
                    .is_some_and(|t| t.contains(ename))
            {
                let ekey = self.type_key(tidx, ename);
                if self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
                {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    let variant_id = self.variant_id_of_key(&ekey, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
            }
            // `module.Type.method(args)` → QUALIFIED static method call on a struct/enum reached
            // through a bound module name. The qualified-variant arm ran first (variant-first), so a
            // variant always wins; here the member is a STATIC method (the checker validated it). Emit
            // the SAME `Op::CallStatic` (NO receiver pushed) the bare `Type.method()` form emits, keyed
            // by the type's module-scoped runtime key — byte-identical bytecode regardless of spelling.
            if let ExprKind::Field {
                obj: inner,
                name: tname,
                ..
            } = &obj.kind
                && let ExprKind::Ident(mname) = &inner.kind
                && fc.resolve_local(mname).is_none()
                && !fc.captures(mname)
                && let Some(&tidx) = self.imported_modules.get(mname)
                && self
                    .module_types
                    .get(tidx)
                    .is_some_and(|t| t.contains(tname))
                && let key = self.type_key(tidx, tname)
                && self.static_methods.contains(&static_key(&key, name))
            {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(
                    Op::CallStatic {
                        type_key: key,
                        method: name.clone(),
                        argc: args.len(),
                    },
                    span,
                );
                return Ok(());
            }
            // `Enum.Variant(args)` → variant constructor, mirroring the bare-ident variant path
            // below. Gated like the value form: an unbound enum name dotted with one of its variants.
            // The enum name resolves to its bare-visible runtime key (`enum_bare_key`).
            if let ExprKind::Ident(ename) = &obj.kind
                && fc.resolve_local(ename).is_none()
                && !fc.captures(ename)
                && let ekey = self.enum_bare_key(ename)
                && self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
            {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                let variant_id = self.variant_id_of_key(&ekey, name);
                fc.emit(
                    Op::NewEnum {
                        variant: name.clone(),
                        variant_id,
                        argc: args.len(),
                    },
                    span,
                );
                return Ok(());
            }
            // `Type.method(args)` → STATIC method call (the "no self ⇒ static" rule). A bare,
            // unbound struct/enum type name dotted with a static method. The checker has already
            // validated the shape; emit `Op::CallStatic` (NO receiver pushed) keyed by the type's
            // runtime key. The enum-variant branch ran first, so a variant always wins.
            if let ExprKind::Ident(tname) = &obj.kind
                && fc.resolve_local(tname).is_none()
                && !fc.captures(tname)
                && let Some(key) = self.bare_types.get(tname).cloned()
                && self.static_methods.contains(&static_key(&key, name))
            {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(
                    Op::CallStatic {
                        type_key: key,
                        method: name.clone(),
                        argc: args.len(),
                    },
                    span,
                );
                return Ok(());
            }
            // `Type[T…].Variant(args)` → declaration-site turbofish VARIANT constructor
            // (`Box[int].Full(9)`, `E[int, str].Pair(…)`). The type args are RUNTIME-erased (they
            // only drove the checker), so emit `Op::NewEnum` by the bare key — identical bytecode to
            // the bare `Enum.Variant(args)` form. Both carriers converge: single-arg `Index{Ident}`
            // and multi-arg `TypeApply{name}`. VARIANT-FIRST (a same-named static is barred at decl
            // time), mirroring the checker.
            if let Some(tname) = type_apply_head_name(&obj.kind)
                && fc.resolve_local(tname).is_none()
                && !fc.captures(tname)
                && let ekey = self.enum_bare_key(tname)
                && self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
            {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                let variant_id = self.variant_id_of_key(&ekey, name);
                fc.emit(
                    Op::NewEnum {
                        variant: name.clone(),
                        variant_id,
                        argc: args.len(),
                    },
                    span,
                );
                return Ok(());
            }
            // `Type[T…].method(args)` → generic-static turbofish (`Box[int].empty()`). Single-arg
            // parses as `Field{obj: Index{obj: Ident(Type), index}, name}` and multi-arg as
            // `Field{obj: TypeApply{name}, name}`. The type args are RUNTIME-erased (they only drive
            // the checker's types) so we ignore them and emit `Op::CallStatic` by the bare key.
            if let Some(tname) = type_apply_head_name(&obj.kind)
                && fc.resolve_local(tname).is_none()
                && !fc.captures(tname)
                && let Some(key) = self.bare_types.get(tname).cloned()
                && self.static_methods.contains(&static_key(&key, name))
            {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(
                    Op::CallStatic {
                        type_key: key,
                        method: name.clone(),
                        argc: args.len(),
                    },
                    span,
                );
                return Ok(());
            }
            // `module.Type[T…].Variant(args)` / `.staticmethod(args)` → QUALIFIED declaration-site
            // turbofish (B1). The single-arg carrier is `Field{obj: Index{obj: Field{Ident(mod),
            // Type}, idx}, name}`; `type_apply_head_name` misses it (its Index.obj is a Field, not an
            // Ident), so recognize the qualified base here. Type args are runtime-erased, so emit the
            // SAME `Op::NewEnum` (variant-first) / `Op::CallStatic` bytecode as the bare turbofish
            // forms, keyed by the type's module-scoped runtime key — byte-identical to `mod.Type.X`.
            if let Some((_, key)) = self.qualified_turbofish_key(fc, &obj.kind) {
                if self
                    .program
                    .variants
                    .contains_key(&(key.clone(), name.clone()))
                {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    let variant_id = self.variant_id_of_key(&key, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
                if self.static_methods.contains(&static_key(&key, name)) {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(
                        Op::CallStatic {
                            type_key: key,
                            method: name.clone(),
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
            }
            // `module.Ctor(args)` → a qualified native builtin CONSTRUCTOR (`concurrency.Shared(0)`,
            // aliased `c.Shared(0)`, `time.timer(100)`). Lower to the SAME opcode the bare name emits
            // (3387-3429) so the bytecode — and thus the runtime value — is byte-identical regardless
            // of how the ctor was spelled. The discriminator is the imported module's `.native` path
            // (NOT `module_types`: `assign_type_keys` does not register opaque builtin names for a
            // native module). Gated on a non-local, non-captured module Ident so a local var named
            // `concurrency` can't be hijacked. A non-matching (native-module, name) pair falls through
            // to `CallMethod` (so `time.now()` and qualified methods still dispatch normally). The
            // type-only handles (net.Socket/Listener, ffi widths/ptr) have no ctor — the checker
            // already rejected `net.Socket(...)`, so they never reach here.
            if let ExprKind::Ident(mname) = &obj.kind
                && fc.resolve_local(mname).is_none()
                && !fc.captures(mname)
                && let Some(&tidx) = self.imported_modules.get(mname)
                && let Some(nat) = self.program.modules.get(tidx).and_then(|m| m.native)
            {
                let op = match (nat, name.as_str()) {
                    ("std.concurrency", "Shared") => Some(Op::NewShared),
                    ("std.concurrency", "RwShared") => Some(Op::NewRwShared),
                    ("std.concurrency", "Atomic") => Some(Op::NewAtomic),
                    ("std.concurrency", "Executor") => Some(Op::NewExecutor),
                    ("std.time", "timer") => Some(Op::NewTimer),
                    _ => None,
                };
                if let Some(op) = op {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(op, span);
                    return Ok(());
                }
            }
            self.compile_expr(fc, obj)?;
            for a in args {
                self.compile_expr(fc, a)?;
            }
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: name.clone(),
                    argc: args.len(),
                    ic,
                },
                span,
            );
            return Ok(());
        }
        // Combined member-side turbofish: `Type[T].member[U](args)` (and the bare `Type.member[U]`).
        // The trailing method `[U]` wraps the `Field` in an `Index`, so the callee is an `Index` over
        // a `Field`, NOT a `Field` — it lands here. The type args (both enclosing AND method) are
        // RUNTIME-erased, so peel the index and emit the SAME `Op::NewEnum` (variant-first) /
        // `Op::CallStatic` (generic static) bytecode as the type-side forms above. The head is the
        // enclosing-type carrier — a type-applied `Box[int]` (`type_apply_head_name`) or a bare
        // `Ident(Box)`. Gate on a KNOWN, NON-local struct/enum so `arr[i].field[k](x)` (head a value)
        // stays ordinary index-then-call.
        if let ExprKind::Index {
            obj: callee_obj, ..
        } = &callee.kind
            && let ExprKind::Field {
                obj: head, name, ..
            } = &callee_obj.kind
        {
            // Combined QUALIFIED turbofish `mod.Type[int].member[U](args)` — the checker accepts it
            // once it recognizes the qualified enclosing head, so lower it the same way (variant-first,
            // then static), keyed by the type's module-scoped runtime key. Both method + enclosing type
            // args are runtime-erased (B1).
            if let Some((_, key)) = self.qualified_turbofish_key(fc, &head.kind) {
                if self
                    .program
                    .variants
                    .contains_key(&(key.clone(), name.clone()))
                {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    let variant_id = self.variant_id_of_key(&key, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
                if self.static_methods.contains(&static_key(&key, name)) {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(
                        Op::CallStatic {
                            type_key: key,
                            method: name.clone(),
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
            }
            let tname = type_apply_head_name(&head.kind).or(match &head.kind {
                ExprKind::Ident(n) => Some(n.as_str()),
                _ => None,
            });
            if let Some(tname) = tname
                && fc.resolve_local(tname).is_none()
                && !fc.captures(tname)
            {
                // VARIANT-FIRST (a same-named static is barred at decl time), mirroring the checker.
                let ekey = self.enum_bare_key(tname);
                if self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
                {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    let variant_id = self.variant_id_of_key(&ekey, name);
                    fc.emit(
                        Op::NewEnum {
                            variant: name.clone(),
                            variant_id,
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
                if let Some(key) = self.bare_types.get(tname).cloned()
                    && self.static_methods.contains(&static_key(&key, name))
                {
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(
                        Op::CallStatic {
                            type_key: key,
                            method: name.clone(),
                            argc: args.len(),
                        },
                        span,
                    );
                    return Ok(());
                }
            }
        }
        // Bare-ident callees resolve by name in the interpreter's order:
        // print → builtin → struct ctor → variant ctor → value.
        if let ExprKind::Ident(name) = &callee.kind {
            // Concurrency C4: `Channel[T]()` → a fresh mailbox; `Shared(v)` → a fresh box over the
            // deep-copied init value. The checker validated arity (Channel: 0 args, Shared: 1).
            if name == "Channel" {
                fc.emit(Op::NewChannel, span);
                return Ok(());
            }
            if name == "Shared" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewShared, span);
                return Ok(());
            }
            // `RwShared(v)` → a fresh read-write box over the deep-copied init value (checker: 1 arg).
            if name == "RwShared" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewRwShared, span);
                return Ok(());
            }
            // `Atomic(v)` → a fresh atomic box over the deep-copied init value (checker validated 1 arg).
            if name == "Atomic" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewAtomic, span);
                return Ok(());
            }
            // `timer(ms)` → a fresh one-shot timeout channel (checker validated 1 int arg).
            if name == "timer" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewTimer, span);
                return Ok(());
            }
            // C5: `Executor()` → a fresh work queue (checker validated 0 args).
            if name == "Executor" {
                fc.emit(Op::NewExecutor, span);
                return Ok(());
            }
            // Native-prelude direct-call dispatch (single source of truth): a table row lowers by its
            // `intrinsic`. `Print` → the dedicated `CallPrint`/`CallPrintSep` opcodes; `Builtin`
            // (ord/chr/panic) and `Ctor` → `CallBuiltin` — the phase-2a scalar conversions
            // (int/float/str/bytes/bytearray) AND the phase-2b GENERIC / reserved-type container ctors
            // (range/List/Map/Set). This is byte-identical to the old `if name == "print"` +
            // `is_builtin` arms; type-args are type-erased before the compiler, so `List[int]()`
            // resolves to `Ident("List")` → the SAME `CallBuiltin`.
            if let Some(p) = crate::checker::prelude_fn(name) {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                match p.intrinsic {
                    crate::checker::Intrinsic::Print => {
                        if named.is_empty() {
                            // Plain `print(...)`: byte-identical (space-join, trailing newline).
                            fc.emit(Op::CallPrint(args.len()), span);
                        } else {
                            // `print(..., sep=, end=)`: push `sep` then `end` (each the user expr or
                            // its default str), then a dedicated op joins+terminates. Eval order
                            // matches the interpreter: positional args, then sep, then end.
                            let sep = named.iter().find(|(k, _)| k == "sep").map(|(_, v)| v);
                            let end = named.iter().find(|(k, _)| k == "end").map(|(_, v)| v);
                            match sep {
                                Some(e) => self.compile_expr(fc, e)?,
                                None => fc.emit(Op::ConstStr(" ".to_string()), span),
                            }
                            match end {
                                Some(e) => self.compile_expr(fc, e)?,
                                None => fc.emit(Op::ConstStr("\n".to_string()), span),
                            }
                            fc.emit(Op::CallPrintSep { argc: args.len() }, span);
                        }
                    }
                    // `Builtin` (ord/chr/panic) and `Ctor` (int/float/str/bytes/bytearray, phase 2a)
                    // both lower to the name-keyed `CallBuiltin` — byte-identical to the old
                    // `is_builtin` fall-through the scalar ctors took before the table folded them in.
                    crate::checker::Intrinsic::Builtin | crate::checker::Intrinsic::Ctor => {
                        fc.emit(Op::CallBuiltin(name.clone(), args.len()), span);
                    }
                }
                return Ok(());
            }
            // DEFENSIVE / never-taken for a DIRECT call after phase 2b: every `is_builtin` name is now
            // a PRELUDE `Builtin`/`Ctor` row, so the `prelude_fn` arm above already emitted its
            // `CallBuiltin` and returned. Kept as a belt-and-suspenders fall-through (minimal diff) — if
            // a future builtin lands in `is_builtin` without a table row it still lowers correctly.
            if is_builtin(name) {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::CallBuiltin(name.clone(), args.len()), span);
                return Ok(());
            }
            // A bare newtype ctor: `UserId(x)` wraps the single arg. Resolved exactly like the struct
            // ctor — only a BARE-resolvable newtype in THIS module — keyed by its runtime key.
            if let Some(nt_key) = self.bare_types.get(name).cloned()
                && self.program.newtype_home.contains_key(&nt_key)
            {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewType(nt_key), span);
                return Ok(());
            }
            // A bare struct ctor: only a BARE-resolvable struct in THIS module (locally declared,
            // `from`-imported, or a std type) — keyed by its declaring module's runtime key. A struct
            // merely present in the global `program.structs` (another module's, imported whole or not
            // imported) is NOT bare-constructible here, so the name falls through (e.g. to a
            // `from`-imported FUNCTION of the same name).
            if let Some(struct_key) = self.bare_types.get(name).cloned()
                && self.program.structs.contains_key(&struct_key)
            {
                self.compile_ctor_args(fc, &struct_key, args)?;
                fc.emit(Op::NewStruct(struct_key, args.len()), span);
                return Ok(());
            }
            // A bare *built-in* variant constructor (`Ok(x)`, `Some(x)`) — user variants are qualified
            // (handled in the `Field` arm above), so only built-ins resolve bare here.
            if let Some(def) = self
                .variant_pair(None, name)
                .and_then(|k| self.program.variants.get(&k))
            {
                let variant_id = def.variant_id;
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(
                    Op::NewEnum {
                        variant: name.clone(),
                        variant_id,
                        argc: args.len(),
                    },
                    span,
                );
                return Ok(());
            }
        }
        // General callable value.
        // Swift-style keyword arguments through a function VALUE (`g(name="Bob")`): the checker
        // recorded a slot PERMUTATION over the combined `[positional args ++ named exprs]` list. Emit
        // the callee, then the combined exprs in slot order, and a plain positional `Op::Call` — the
        // runtime ABI is unchanged. Positional-only calls (`named` empty) never consult the table.
        if !named.is_empty()
            && let Some(perm) = self
                .keyword_calls
                .get(&crate::checker::keyword_key(
                    self.current_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    named,
                    span,
                ))
                .cloned()
        {
            self.compile_expr(fc, callee)?;
            for &ci in &perm {
                let e = if ci < args.len() {
                    &args[ci]
                } else {
                    &named[ci - args.len()].1
                };
                self.compile_expr(fc, e)?;
            }
            fc.emit(Op::Call(perm.len()), span);
            return Ok(());
        }
        self.compile_expr(fc, callee)?;
        for a in args {
            self.compile_expr(fc, a)?;
        }
        fc.emit(Op::Call(args.len()), span);
        Ok(())
    }

    fn compile_closure(
        &mut self,
        fc: &mut FnComp,
        params: &[crate::ast::Param],
        body: &Expr,
        span: Span,
    ) -> Result<(), CompileError> {
        // Capture ONLY the names this body actually references from the enclosing frame (its
        // free-variable set), not every visible local. Capturing all visible locals (the old model)
        // dragged unused non-sendable siblings (a closure value / live generator) across the spawn
        // airlock → check-OK/run-fault (Finding D). `free_names_expr` is a trusted over-approximation
        // (it also drives cell-boxing), so this is behavior-identical and strictly smaller.
        let mut free: HashSet<String> = HashSet::new();
        let params_set: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
        free_names_expr(body, &params_set, &mut free);
        let entries: Vec<CapEntry> = fc
            .snapshot_entries()
            .into_iter()
            .filter(|e| free.contains(&e.name))
            .collect();
        let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

        let mut child = FnComp::new("<closure>".to_string(), params.len(), false);
        // Uniform by-reference capture (Task A): this closure's own boxed-name set (unwired).
        child.boxed_names = captured_names_of_closure(body, params);
        child.captured_names = captured_names;
        for p in params {
            child.add_local(p.name.clone());
        }
        // A `float`-typed closure param coerces at the prologue, like a named-fn param. (A closure
        // declares no return type, so there is no return-coercion here.)
        self.emit_float_param_prologue(&mut child, params);
        // Uniform by-reference capture: box any param captured by a nested closure (after coercion).
        self.emit_box_param_prologue(&mut child, params);
        self.compile_expr(&mut child, body)?;
        child.emit(Op::Return, span);
        let pid = self.finish(child);

        fc.emit(Op::MakeClosure(pid, entries), span);
        Ok(())
    }

    /// Compile a string literal, splitting `{expr}` interpolations (pre-parsed at compile time)
    /// from literal text. `{{`/`}}` are literal braces. A literal-only string is a single
    /// `ConstStr`; an interpolated one builds its chunks and concatenates with `BuildStr`.
    fn compile_str(&mut self, fc: &mut FnComp, raw: &str, span: Span) -> Result<(), CompileError> {
        let chunks = parse_interpolation(raw, span).map_err(|e| CompileError {
            message: e.message,
            span: e.span,
        })?;
        if let [Chunk::Lit(s)] = chunks.as_slice() {
            fc.emit(Op::ConstStr(s.clone()), span);
            return Ok(());
        }
        if chunks.is_empty() {
            fc.emit(Op::ConstStr(String::new()), span);
            return Ok(());
        }
        let n = chunks.len();
        // A value+keyword call inside a `{…}` fragment is keyed by (string span, fragment ordinal) so
        // the lookup matches the checker's record (each fragment re-lexes from a fresh source, so span
        // alone can't tell two fragments apart). Save/restore for nested interpolations.
        let saved_ctx = self.kw_frag_ctx;
        let saved_ord = self.kw_frag_ord;
        let mut ord = 0usize;
        for chunk in chunks {
            match chunk {
                Chunk::Lit(s) => fc.emit(Op::ConstStr(s), span),
                Chunk::Expr(e, spec) => {
                    self.kw_frag_ctx = span;
                    self.kw_frag_ord = ord;
                    self.compile_expr(fc, &e)?;
                    match spec {
                        None => fc.emit(Op::ToStr, span),
                        Some(fs) => fc.emit(Op::ToStrFmt(Box::new(fs)), span),
                    }
                    ord += 1;
                }
            }
        }
        self.kw_frag_ctx = saved_ctx;
        self.kw_frag_ord = saved_ord;
        fc.emit(Op::BuildStr(n), span);
        Ok(())
    }
}

/// Build a synthesized `obj.method(args)` expression-statement (used to desugar comprehension
/// accumulation into a method call the existing codegen already handles).
fn method_call_stmt(obj: Expr, method: &str, args: Vec<Expr>, span: Span) -> Stmt {
    let callee = Expr {
        kind: ExprKind::Field {
            obj: Box::new(obj),
            name: method.to_string(),
            name_span: span,
        },
        span,
    };
    Stmt {
        kind: StmtKind::Expr(Expr {
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args,
                named: Vec::new(),
                type_args: Vec::new(),
            },
            span,
        }),
        span,
    }
}

fn binary_op(op: BinaryOp) -> Op {
    match op {
        BinaryOp::Add => Op::Add,
        BinaryOp::Sub => Op::Sub,
        BinaryOp::Mul => Op::Mul,
        BinaryOp::Div => Op::Div,
        BinaryOp::Mod => Op::Mod,
        BinaryOp::Lt => Op::Lt,
        BinaryOp::LtEq => Op::LtEq,
        BinaryOp::Gt => Op::Gt,
        BinaryOp::GtEq => Op::GtEq,
        BinaryOp::Eq => Op::Eq,
        BinaryOp::NotEq => Op::NotEq,
        BinaryOp::BitAnd => Op::BitAnd,
        BinaryOp::BitOr => Op::BitOr,
        BinaryOp::BitXor => Op::BitXor,
        BinaryOp::Shl => Op::Shl,
        BinaryOp::Shr => Op::Shr,
        BinaryOp::In => Op::Contains,
        BinaryOp::And | BinaryOp::Or => unreachable!("and/or handled by short-circuit path"),
    }
}

// ===== match lowering helpers =====

/// A `match` arm uniform over the statement form (`MatchArm`) and expression form (`MatchExprArm`),
/// so `compile_match_lit` can drive both.
trait MatchArmLike {
    type Body;
    fn pattern(&self) -> &Pattern;
    fn guard(&self) -> Option<&Expr>;
    fn body(&self) -> &Self::Body;
}

impl MatchArmLike for MatchArm {
    type Body = Block;
    fn pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn guard(&self) -> Option<&Expr> {
        self.guard.as_ref()
    }
    fn body(&self) -> &Block {
        &self.body
    }
}

impl MatchArmLike for MatchExprArm {
    type Body = Expr;
    fn pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn guard(&self) -> Option<&Expr> {
        self.guard.as_ref()
    }
    fn body(&self) -> &Expr {
        &self.body
    }
}

/// Emit a half-open range test `start <= scrut < end`: two comparisons, each jumping to the next
/// arm on failure (jumps pushed onto `fails`). Reuses `GtEq`/`Lt` + `JumpIfFalse` (no new opcode).
fn emit_range_test(
    fc: &mut FnComp,
    scrut: usize,
    start: i64,
    end: i64,
    fails: &mut Vec<usize>,
    span: Span,
) {
    // scrut >= start
    fc.emit_hidden_get(scrut, span);
    fc.emit(Op::ConstInt(start), span);
    fc.emit(Op::GtEq, span);
    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
    // scrut < end
    fc.emit_hidden_get(scrut, span);
    fc.emit(Op::ConstInt(end), span);
    fc.emit(Op::Lt, span);
    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
}

/// Emit the constant op that pushes a literal pattern's value (mirrors `compile_expr`'s literal
/// lowering; a pattern's string is plain — never interpolated).
fn emit_lit_const(fc: &mut FnComp, lit: &LitPattern, span: Span) {
    match lit {
        LitPattern::Int(n) => fc.emit(Op::ConstInt(*n), span),
        LitPattern::Str(s) => fc.emit(Op::ConstStr(s.clone()), span),
        LitPattern::Bool(b) => fc.emit(if *b { Op::True } else { Op::False }, span),
    }
}

// ===== per-function compile state =====

struct LocalVar {
    name: String,
    depth: usize,
}

/// Compile-time state for one function (or the module toplevel): its code buffer, parallel spans,
/// local-slot allocation, and (for closures) the set of captured names.
/// Pending jumps for the innermost loop being compiled. `break` jumps land at the loop exit;
/// `continue` jumps land at the loop's increment/condition. Both targets are unknown when the
/// jump is emitted, so we collect placeholder `Op::Jump(0)` indices and patch them once the
/// targets are known (see `compile_while` / `compile_for`).
struct LoopCtx {
    continue_jumps: Vec<usize>,
    break_jumps: Vec<usize>,
    /// Number of open defer scopes enclosing this loop (captured at loop entry, before the body's
    /// own defer scope). A `break`/`continue` drains every defer scope from the break point down to
    /// (and including) the loop-body scope — i.e. `fc.defer_scopes - defer_floor` of them.
    defer_floor: usize,
    /// TASK B — number of open `parallel:` nursery scopes enclosing this loop (captured at loop entry).
    /// A `break`/`continue` inside the loop body leaves every `parallel:` scope opened within it before
    /// the matching `JoinNursery` runs; the compiler emits one `ReclaimNursery` per such escaped scope
    /// (`fc.nursery_scopes - nursery_floor`), mirroring the `LeaveDeferScope` drain for defers.
    nursery_floor: usize,
}

/// Whether a block directly contains a `defer` statement (so it needs defer-scope brackets). Nested
/// blocks own their own scope, so they are NOT inspected here.
fn block_has_defer(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s.kind, StmtKind::Defer(_)))
}

// ===== Uniform by-reference capture (Task A): the compile-time capture pre-pass. =====
// Purely syntactic, name-based over-approximation (spec B7). Computes THIS frame's local names that
// are captured by a directly-nested capture boundary (a closure body, `defer:` block, or `spawn:`
// block) so their slots must be boxed (`Obj::Cell`). Never DROPS a captured name (the free-var side
// is conservative); intersects with this frame's binds so only slots that live in this frame box.
// UNWIRED this task — nothing reads the result yet (emit-routing + boxing land in a later task).

/// Compute this frame's boxed-name set for `body` (its params are `params`). See the section comment.
fn captured_names_of_body(body: &Block, params: &[crate::ast::Param]) -> HashSet<String> {
    // 1. Free names of every DIRECTLY-nested capture boundary. The free-var walk descends THROUGH
    //    inner closures subtracting their binds, so a name captured two levels down still surfaces
    //    here (via the direct child's free set) — do NOT flat-collect deeper boundaries as direct
    //    children (that would leak a middle closure's own locals into this frame's set).
    let mut captured = HashSet::new();
    find_boundary_free_block(body, &mut captured);
    // 2. Names bound in THIS frame: params + every binding reachable through this body's control
    //    flow, but NOT inside a nested capture boundary (those are separate frames).
    let mut frame_binds: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    collect_frame_binds(body, &mut frame_binds);
    // 3. A boxed local is a captured name whose slot lives in THIS frame.
    captured.retain(|n| frame_binds.contains(n));
    captured
}

/// Collect the binding names of a `match`/tuple/variant [`Pattern`] into `out`.
fn pattern_binds(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Ident(n, _) => {
            out.insert(n.clone());
        }
        Pattern::Variant { bindings, .. } => bindings.iter().for_each(|b| pattern_binds(b, out)),
        Pattern::Tuple(ps) | Pattern::Or(ps) => ps.iter().for_each(|b| pattern_binds(b, out)),
        Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
    }
}

/// Names bound in THIS frame reachable through `stmts`'s control flow (descends if/for/while/match/
/// wait/parallel sub-blocks — those share the frame — but STOPS at capture boundaries, whose bodies
/// are separate frames). Adds let/for/match/wait/nested-fn binding names to `out`.
fn collect_frame_binds(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { names, value, .. } => {
                out.extend(names.iter().cloned());
                // A binding inside the VALUE expression (a `match`/`if`/comprehension/`recover`
                // expression) is a frame-level local too — collect it so it boxes when captured.
                collect_frame_binds_expr(value, out);
            }
            // A nested named fn binds its name in this frame (its body is a separate frame).
            StmtKind::Fn(decl) => {
                out.insert(decl.name.clone());
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    collect_frame_binds_expr(cond, out);
                    collect_frame_binds(body, out);
                }
                if let Some(eb) = else_block {
                    collect_frame_binds(eb, out);
                }
            }
            StmtKind::For {
                vars, iter, body, ..
            } => {
                out.extend(vars.iter().cloned());
                collect_frame_binds_expr(iter, out);
                collect_frame_binds(body, out);
            }
            StmtKind::While { cond, body } => {
                collect_frame_binds_expr(cond, out);
                collect_frame_binds(body, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                collect_frame_binds_expr(scrutinee, out);
                for arm in arms {
                    pattern_binds(&arm.pattern, out);
                    if let Some(g) = &arm.guard {
                        collect_frame_binds_expr(g, out);
                    }
                    collect_frame_binds(&arm.body, out);
                }
            }
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    collect_frame_binds_expr(&arm.chan, out);
                    match &arm.target {
                        WaitTarget::Bind(n) => {
                            out.insert(n.clone());
                        }
                        WaitTarget::Assign(e) => collect_frame_binds_expr(e, out),
                        WaitTarget::Discard => {}
                    }
                    collect_frame_binds(&arm.body, out);
                }
                if let Some(eb) = else_block {
                    collect_frame_binds(eb, out);
                }
            }
            StmtKind::Parallel { body } => collect_frame_binds(body, out),
            StmtKind::Assign { target, value, .. } => {
                collect_frame_binds_expr(target, out);
                collect_frame_binds_expr(value, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => {
                collect_frame_binds_expr(e, out)
            }
            StmtKind::Assert { cond, msg } => {
                collect_frame_binds_expr(cond, out);
                if let Some(m) = msg {
                    collect_frame_binds_expr(m, out);
                }
            }
            // `defer f(x)` / `spawn f(x)` evaluate the call in THIS frame, so a binding inside the
            // call expression is a frame local. The BLOCK forms are separate frames (skipped).
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                collect_frame_binds_expr(e, out)
            }
            // `defer:` / `spawn:` blocks are SEPARATE frames — their binds are not this frame's.
            // Everything else binds no frame-local name.
            _ => {}
        }
    }
}

/// Walk `stmts`, and at each DIRECTLY-nested capture boundary (closure / `defer:` / `spawn:` block)
/// add that boundary's FREE names to `out`. Descends non-boundary structure to find boundaries, but
/// does NOT descend past a boundary (its transitive captures are already in its free set).
fn find_boundary_free_block(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } => find_boundary_free_expr(value, out),
            StmtKind::Assign { target, value, .. } => {
                find_boundary_free_expr(target, out);
                find_boundary_free_expr(value, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => {
                find_boundary_free_expr(e, out)
            }
            StmtKind::Assert { cond, msg } => {
                find_boundary_free_expr(cond, out);
                if let Some(m) = msg {
                    find_boundary_free_expr(m, out);
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, body) in branches {
                    find_boundary_free_expr(c, out);
                    find_boundary_free_block(body, out);
                }
                if let Some(eb) = else_block {
                    find_boundary_free_block(eb, out);
                }
            }
            StmtKind::For { iter, body, .. } => {
                find_boundary_free_expr(iter, out);
                find_boundary_free_block(body, out);
            }
            StmtKind::While { cond, body } => {
                find_boundary_free_expr(cond, out);
                find_boundary_free_block(body, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                find_boundary_free_expr(scrutinee, out);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        find_boundary_free_expr(g, out);
                    }
                    find_boundary_free_block(&arm.body, out);
                }
            }
            StmtKind::Parallel { body } => find_boundary_free_block(body, out),
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    find_boundary_free_expr(&arm.chan, out);
                    if let WaitTarget::Assign(e) = &arm.target {
                        find_boundary_free_expr(e, out);
                    }
                    find_boundary_free_block(&arm.body, out);
                }
                if let Some(eb) = else_block {
                    find_boundary_free_block(eb, out);
                }
            }
            // `defer:` / `spawn:` blocks ARE capture boundaries — collect their free names (relative
            // to an empty scope) and stop. `defer f(x)` / `spawn f(x)` evaluate the call in THIS
            // frame, so scan the call expression for closure boundaries instead.
            StmtKind::Defer(DeferTarget::Block(b)) | StmtKind::Spawn(SpawnTarget::Block(b)) => {
                free_names_block(b, &HashSet::new(), out)
            }
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                find_boundary_free_expr(e, out)
            }
            // A nested named fn IS a capture boundary (a closure-with-a-name): collect its body's
            // free names relative to its PARAMS ONLY — NOT its own name — so BOTH captured outer
            // locals AND the recursive self-name surface as free here. Intersected with this frame's
            // binds (which include the fn's name, via `collect_frame_binds`) → the captured names box
            // in THIS frame, exactly like a closure's captures. (Descending no further: the fn's own
            // inner boundaries capture ITS frame, already folded into its free set.)
            StmtKind::Fn(decl) => {
                let params: HashSet<String> = decl.params.iter().map(|p| p.name.clone()).collect();
                free_names_block(&decl.body, &params, out);
            }
            // Remaining statements contain no capture boundary.
            _ => {}
        }
    }
}

/// Best-effort: the interpolation sub-expressions of a string literal (`"a{x}b"` → the `x` expr).
/// Used by the capture pre-pass so a name referenced ONLY inside a `{…}` interpolation is still seen
/// as a free variable (and therefore boxed) — the interpolation exprs are embedded in the `Str`
/// literal and parsed at compile time, so the AST walk would otherwise miss them. A malformed
/// interpolation yields no exprs here; the real `compile_str` surfaces that error.
fn interp_exprs(raw: &str) -> Vec<Expr> {
    match parse_interpolation(raw, Span { line: 1, col: 1 }) {
        Ok(chunks) => chunks
            .into_iter()
            .filter_map(|c| match c {
                Chunk::Expr(e, _) => Some(e),
                Chunk::Lit(_) => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Walk `e`, and at each closure boundary add its FREE names to `out` (stopping at the closure).
/// Descends every non-closure sub-expression to find closures nested in them.
fn find_boundary_free_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Closure { params, body, .. } => {
            let bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
            free_names_expr(body, &bound, out);
        }
        // A string literal may carry `{…}` interpolation exprs (a closure could nest in one).
        ExprKind::Str(raw) => {
            for ie in interp_exprs(raw) {
                find_boundary_free_expr(&ie, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|x| find_boundary_free_expr(x, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            find_boundary_free_expr(k, out);
            find_boundary_free_expr(v, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            for c in clauses {
                find_boundary_free_expr(&c.iter, out);
                c.guards
                    .iter()
                    .for_each(|g| find_boundary_free_expr(g, out));
            }
            if let Some(k) = key {
                find_boundary_free_expr(k, out);
            }
            find_boundary_free_expr(elem, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => find_boundary_free_expr(expr, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs } => {
            find_boundary_free_expr(lhs, out);
            find_boundary_free_expr(rhs, out);
        }
        ExprKind::Range { start, end } => {
            find_boundary_free_expr(start, out);
            find_boundary_free_expr(end, out);
        }
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
            find_boundary_free_expr(callee, out);
            args.iter().for_each(|a| find_boundary_free_expr(a, out));
            named
                .iter()
                .for_each(|(_, v)| find_boundary_free_expr(v, out));
        }
        ExprKind::Field { obj, .. } => find_boundary_free_expr(obj, out),
        ExprKind::OptChain { obj, call, .. } => {
            find_boundary_free_expr(obj, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| find_boundary_free_expr(a, out));
                c.named
                    .iter()
                    .for_each(|(_, v)| find_boundary_free_expr(v, out));
            }
        }
        ExprKind::Index { obj, index } => {
            find_boundary_free_expr(obj, out);
            find_boundary_free_expr(index, out);
        }
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            find_boundary_free_expr(obj, out);
            for o in [start, end, step].into_iter().flatten() {
                find_boundary_free_expr(o, out);
            }
        }
        ExprKind::DecodeCall { obj, arg, .. } => {
            find_boundary_free_expr(obj, out);
            find_boundary_free_expr(arg, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            find_boundary_free_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    find_boundary_free_expr(g, out);
                }
                find_boundary_free_expr(&arm.body, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            find_boundary_free_expr(cond, out);
            find_boundary_free_expr(then, out);
            find_boundary_free_expr(els, out);
        }
        ExprKind::Recover(block) => find_boundary_free_block(block, out),
    }
}

/// Keep only the capture entries whose name is FREE in a statement-block body (`bound` = the body's
/// own params). Used at the nested-fn / `spawn:` / `defer:` MakeClosure sites so a closure captures
/// only the names it references — not every visible local (Finding D: over-capture dragged unused
/// non-sendable siblings across the spawn airlock). Positional GetCaptured slot indices stay aligned
/// because both `MakeClosure`'s entries and the child's `captured_names` derive from THIS filtered vec.
fn filter_entries_free_block(
    entries: &[CapEntry],
    stmts: &[Stmt],
    params: &[crate::ast::Param],
) -> Vec<CapEntry> {
    let mut free: HashSet<String> = HashSet::new();
    let bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    free_names_block(stmts, &bound, &mut free);
    entries
        .iter()
        .filter(|e| free.contains(&e.name))
        .cloned()
        .collect()
}

/// Free names of a capture-boundary BLOCK body (`defer:`/`spawn:`): names referenced but not bound
/// within, relative to `bound`. Threads bindings left-to-right (a later stmt sees earlier lets).
pub(crate) fn free_names_block(stmts: &[Stmt], bound: &HashSet<String>, out: &mut HashSet<String>) {
    let mut b = bound.clone();
    for s in stmts {
        match &s.kind {
            StmtKind::Let { names, value, .. } => {
                free_names_expr(value, &b, out);
                b.extend(names.iter().cloned());
            }
            StmtKind::Assign { target, value, .. } => {
                free_names_expr(target, &b, out);
                free_names_expr(value, &b, out);
            }
            StmtKind::Fn(decl) => {
                // Over-approximate: a nested fn body may reference an outer name → treat as free
                // (relative to its params + name), so any capture surfaces (never dropped).
                b.insert(decl.name.clone());
                let mut inner = b.clone();
                inner.extend(decl.params.iter().map(|p| p.name.clone()));
                free_names_block(&decl.body, &inner, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => {
                free_names_expr(e, &b, out)
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Pass => {}
            StmtKind::Assert { cond, msg } => {
                free_names_expr(cond, &b, out);
                if let Some(m) = msg {
                    free_names_expr(m, &b, out);
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, body) in branches {
                    free_names_expr(c, &b, out);
                    free_names_block(body, &b, out);
                }
                if let Some(eb) = else_block {
                    free_names_block(eb, &b, out);
                }
            }
            StmtKind::For {
                vars, iter, body, ..
            } => {
                free_names_expr(iter, &b, out);
                let mut b2 = b.clone();
                b2.extend(vars.iter().cloned());
                free_names_block(body, &b2, out);
            }
            StmtKind::While { cond, body } => {
                free_names_expr(cond, &b, out);
                free_names_block(body, &b, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                free_names_expr(scrutinee, &b, out);
                for arm in arms {
                    let mut b2 = b.clone();
                    pattern_binds(&arm.pattern, &mut b2);
                    if let Some(g) = &arm.guard {
                        free_names_expr(g, &b2, out);
                    }
                    free_names_block(&arm.body, &b2, out);
                }
            }
            StmtKind::Parallel { body } => free_names_block(body, &b, out),
            StmtKind::Defer(DeferTarget::Block(blk)) | StmtKind::Spawn(SpawnTarget::Block(blk)) => {
                free_names_block(blk, &b, out)
            }
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                free_names_expr(e, &b, out)
            }
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    free_names_expr(&arm.chan, &b, out);
                    let mut b2 = b.clone();
                    match &arm.target {
                        WaitTarget::Bind(n) => {
                            b2.insert(n.clone());
                        }
                        WaitTarget::Assign(e) => free_names_expr(e, &b, out),
                        WaitTarget::Discard => {}
                    }
                    free_names_block(&arm.body, &b2, out);
                }
                if let Some(eb) = else_block {
                    free_names_block(eb, &b, out);
                }
            }
            // Type/import/native/extern decls inside a block reference no frame value.
            _ => {}
        }
    }
}

/// Free names of an expression relative to `bound`: referenced idents not in `bound`. Descends
/// THROUGH nested closures/comprehensions/matches, subtracting each inner scope's binds.
pub(crate) fn free_names_expr(e: &Expr, bound: &HashSet<String>, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !bound.contains(n) {
                out.insert(n.clone());
            }
        }
        // A string literal's `{…}` interpolation exprs reference names too (parsed at compile time,
        // so absent from the AST) — collect their free names or a capture-via-interpolation is missed.
        ExprKind::Str(raw) => {
            for ie in interp_exprs(raw) {
                free_names_expr(&ie, bound, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|x| free_names_expr(x, bound, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            free_names_expr(k, bound, out);
            free_names_expr(v, bound, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            let mut b = bound.clone();
            for c in clauses {
                free_names_expr(&c.iter, &b, out);
                b.extend(c.vars.iter().cloned());
                c.guards.iter().for_each(|g| free_names_expr(g, &b, out));
            }
            if let Some(k) = key {
                free_names_expr(k, &b, out);
            }
            free_names_expr(elem, &b, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => free_names_expr(expr, bound, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs } => {
            free_names_expr(lhs, bound, out);
            free_names_expr(rhs, bound, out);
        }
        ExprKind::Range { start, end } => {
            free_names_expr(start, bound, out);
            free_names_expr(end, bound, out);
        }
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
            free_names_expr(callee, bound, out);
            args.iter().for_each(|a| free_names_expr(a, bound, out));
            named
                .iter()
                .for_each(|(_, v)| free_names_expr(v, bound, out));
        }
        ExprKind::Field { obj, .. } => free_names_expr(obj, bound, out),
        ExprKind::OptChain { obj, call, .. } => {
            free_names_expr(obj, bound, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| free_names_expr(a, bound, out));
                c.named
                    .iter()
                    .for_each(|(_, v)| free_names_expr(v, bound, out));
            }
        }
        ExprKind::Index { obj, index } => {
            free_names_expr(obj, bound, out);
            free_names_expr(index, bound, out);
        }
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            free_names_expr(obj, bound, out);
            for o in [start, end, step].into_iter().flatten() {
                free_names_expr(o, bound, out);
            }
        }
        ExprKind::DecodeCall { obj, arg, .. } => {
            free_names_expr(obj, bound, out);
            free_names_expr(arg, bound, out);
        }
        ExprKind::Closure { params, body, .. } => {
            let mut b = bound.clone();
            b.extend(params.iter().map(|p| p.name.clone()));
            free_names_expr(body, &b, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            free_names_expr(scrutinee, bound, out);
            for arm in arms {
                let mut b = bound.clone();
                pattern_binds(&arm.pattern, &mut b);
                if let Some(g) = &arm.guard {
                    free_names_expr(g, &b, out);
                }
                free_names_expr(&arm.body, &b, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            free_names_expr(cond, bound, out);
            free_names_expr(then, bound, out);
            free_names_expr(els, bound, out);
        }
        ExprKind::Recover(block) => free_names_block(block, bound, out),
    }
}

/// The closure/expression analogue of [`captured_names_of_body`]: compute the boxed-name set for a
/// closure whose body is the single expression `body` with parameters `params`. (A `fn(x): expr`
/// closure has no statement block — its frame binds come from `params` plus any `match`/`recover`/
/// comprehension binds inside the body expression.)
fn captured_names_of_closure(body: &Expr, params: &[crate::ast::Param]) -> HashSet<String> {
    let mut captured = HashSet::new();
    find_boundary_free_expr(body, &mut captured);
    let mut frame_binds: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    collect_frame_binds_expr(body, &mut frame_binds);
    captured.retain(|n| frame_binds.contains(n));
    captured
}

/// Names bound in THIS frame by an EXPRESSION body (`match`/`recover`/comprehension/if-else binds),
/// descending non-closure sub-expressions but STOPPING at a nested closure (a separate frame).
fn collect_frame_binds_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        // A nested closure is a separate frame — its binds are not this frame's.
        ExprKind::Closure { .. } => {}
        // A `{…}` interpolation expr may itself bind (a `match`/comprehension expression); its binds
        // land in this frame (the interpolation is compiled inline).
        ExprKind::Str(raw) => {
            for ie in interp_exprs(raw) {
                collect_frame_binds_expr(&ie, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|x| collect_frame_binds_expr(x, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            collect_frame_binds_expr(k, out);
            collect_frame_binds_expr(v, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            for c in clauses {
                collect_frame_binds_expr(&c.iter, out);
                out.extend(c.vars.iter().cloned());
                c.guards
                    .iter()
                    .for_each(|g| collect_frame_binds_expr(g, out));
            }
            if let Some(k) = key {
                collect_frame_binds_expr(k, out);
            }
            collect_frame_binds_expr(elem, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => collect_frame_binds_expr(expr, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs } => {
            collect_frame_binds_expr(lhs, out);
            collect_frame_binds_expr(rhs, out);
        }
        ExprKind::Range { start, end } => {
            collect_frame_binds_expr(start, out);
            collect_frame_binds_expr(end, out);
        }
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
            collect_frame_binds_expr(callee, out);
            args.iter().for_each(|a| collect_frame_binds_expr(a, out));
            named
                .iter()
                .for_each(|(_, v)| collect_frame_binds_expr(v, out));
        }
        ExprKind::Field { obj, .. } => collect_frame_binds_expr(obj, out),
        ExprKind::OptChain { obj, call, .. } => {
            collect_frame_binds_expr(obj, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| collect_frame_binds_expr(a, out));
                c.named
                    .iter()
                    .for_each(|(_, v)| collect_frame_binds_expr(v, out));
            }
        }
        ExprKind::Index { obj, index } => {
            collect_frame_binds_expr(obj, out);
            collect_frame_binds_expr(index, out);
        }
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            collect_frame_binds_expr(obj, out);
            for o in [start, end, step].into_iter().flatten() {
                collect_frame_binds_expr(o, out);
            }
        }
        ExprKind::DecodeCall { obj, arg, .. } => {
            collect_frame_binds_expr(obj, out);
            collect_frame_binds_expr(arg, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_frame_binds_expr(scrutinee, out);
            for arm in arms {
                pattern_binds(&arm.pattern, out);
                if let Some(g) = &arm.guard {
                    collect_frame_binds_expr(g, out);
                }
                collect_frame_binds_expr(&arm.body, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            collect_frame_binds_expr(cond, out);
            collect_frame_binds_expr(then, out);
            collect_frame_binds_expr(els, out);
        }
        ExprKind::Recover(block) => collect_frame_binds(block, out),
    }
}

/// M-C implicit nurseries: does this body contain a bare `spawn` that is NOT already inside an
/// explicit `parallel:` (so it would bind to the *implicit* function/module nursery)? Drives the
/// gate in `compile_fn`/`compile_module` — a body with no such spawn emits byte-identical bytecode to
/// pre-M-C (zero overhead). Recurses through control flow but **stops** at boundaries that are *their
/// own* function-like body (and so get their own implicit nursery, gated separately): `parallel:` (its
/// spawns belong to that explicit nursery), nested `fn`, a `spawn:` block, and a `defer:` block (each
/// runs in its own frame, so a bare `spawn` inside it joins at *that* body's end, not this one's).
/// Map an extern fn's surface [`Type`] annotation to its runtime [`CType`]. Only the v1 marshallable
/// set (`int`/`float`/`bool`/`str`/`ptr`) is supported, resolving transparent type aliases (`type Len = int`)
/// through `aliases` first. Everything else (incl. a `None` annotation) returns `None`. The checker
/// ROOT REDESIGN — the compiler's [`crate::json_decode::DecodeEnv`]: resolves a decode target and its
/// nested field struct types to qualified identity keys, using the compiler's per-module type tables.
struct CompilerDecodeEnv<'a> {
    c: &'a Compiler,
}

impl crate::json_decode::DecodeEnv for CompilerDecodeEnv<'_> {
    fn resolve_bare(&self, module_idx: usize, name: &str) -> Option<String> {
        // In the CALL module, a bare name may be local / from-imported / std — use `bare_types`.
        // In any other (declaring) module, a nested field type resolves to that module's own type.
        if module_idx == self.c.current_module_idx
            && let Some(k) = self.c.bare_types.get(name)
        {
            return Some(k.clone());
        }
        self.c
            .type_keys
            .get(&(module_idx, name.to_string()))
            .cloned()
    }

    fn resolve_qualified(&self, _module_idx: usize, binder: &str, name: &str) -> Option<String> {
        // A qualified `binder.name` is only written at the call site; resolve `binder` against the
        // current module's imported-module bindings, then key `name` in that target module.
        let tidx = *self.c.imported_modules.get(binder)?;
        if self
            .c
            .module_types
            .get(tidx)
            .is_some_and(|t| t.contains(name))
        {
            self.c.type_keys.get(&(tidx, name.to_string())).cloned()
        } else {
            None
        }
    }

    fn struct_def(&self, key: &str) -> Option<(usize, &[crate::ast::Field])> {
        let module_idx = self.c.program.structs.get(key)?.module_idx;
        let fields = self.c.struct_fields.get(key)?;
        Some((module_idx, fields.as_slice()))
    }

    fn display_of(&self, key: &str) -> String {
        self.c
            .program
            .structs
            .get(key)
            .map(|d| d.display_name.clone())
            .unwrap_or_else(|| bare_display(key))
    }
}

/// ROOT REDESIGN — strip a qualified identity key `<module-key>::Name` back to its bare `Name` for a
/// display fallback when no `StructDef` is registered (defensive). Splits on the LAST `::`.
pub(crate) fn bare_display(key: &str) -> String {
    key.rsplit("::").next().unwrap_or(key).to_string()
}

/// Render an extern param/return source `Type` for a marshallability error message (the surface
/// spelling the user wrote, e.g. `cdefs.DivT`). Used only by the never-panic backstop, so it covers
/// the spellings that reach an extern boundary; anything else falls back to a plain placeholder.
fn ffi_type_display(ty: Option<&Type>) -> String {
    match ty {
        Some(Type::Named { name: n, .. }) => n.clone(),
        Some(Type::Qualified { module, name, .. }) => format!("{module}.{name}"),
        Some(Type::Generic(n, ..)) => n.clone(),
        Some(_) => "<unsupported>".to_string(),
        None => "nil".to_string(),
    }
}

pub(crate) fn block_has_bare_spawn(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_bare_spawn)
}

fn stmt_has_bare_spawn(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Spawn(_) => true,
        // Boundaries: spawns here do not belong to the enclosing implicit nursery.
        StmtKind::Parallel { .. } | StmtKind::Fn(_) => false,
        // Recurse through ordinary control flow.
        StmtKind::If {
            branches,
            else_block,
        } => {
            branches.iter().any(|(_, b)| block_has_bare_spawn(b))
                || else_block.as_ref().is_some_and(|b| block_has_bare_spawn(b))
        }
        StmtKind::For { body, .. } | StmtKind::While { body, .. } => block_has_bare_spawn(body),
        StmtKind::Match { arms, .. } => arms.iter().any(|a| block_has_bare_spawn(&a.body)),
        StmtKind::Wait { arms, else_block } => {
            arms.iter().any(|a| block_has_bare_spawn(&a.body))
                || else_block.as_ref().is_some_and(|b| block_has_bare_spawn(b))
        }
        _ => false,
    }
}

struct FnComp {
    name: String,
    arity: usize,
    is_toplevel: bool,
    code: Vec<Op>,
    lines: Vec<Span>,
    locals: Vec<LocalVar>,
    scope_depth: usize,
    /// Number of slots currently in use (== next free slot).
    slot_count: usize,
    max_slots: usize,
    /// Names this function captures from an enclosing scope (closures only).
    captured_names: Vec<String>,
    /// Uniform by-reference capture — this frame's local names that are captured by a nested
    /// closure / `defer:` / `spawn:` body, so their slots are BOXED (`Obj::Cell`): declared with a
    /// `NewCell`, read via `CellLoad`, written via `CellStore` (see [`emit_get_named`]/
    /// [`emit_set_named`]). Computed by [`captured_names_of_body`] at each fn/closure body construction.
    boxed_names: std::collections::HashSet<String>,
    /// Stack of enclosing loops (innermost last), for `break`/`continue` jump patching. Empty
    /// outside any loop. A closure compiles in its own `FnComp` with an empty stack, so a
    /// `break`/`continue` there has no current loop (the checker rejects it; we defend anyway).
    loops: Vec<LoopCtx>,
    /// Count of defer scopes currently open during compilation (incremented at `EnterDeferScope`,
    /// decremented at the matching `LeaveDeferScope`). Read by `break`/`continue` to know how many
    /// scopes to drain down to the loop body.
    defer_scopes: usize,
    /// TASK B — count of `parallel:` nursery scopes currently open during compilation (incremented at
    /// the `EnterNursery` emit in `compile_parallel`, decremented at the matching `JoinNursery`). Read
    /// by `break`/`continue` to know how many `ReclaimNursery`s to emit before jumping out of the loop.
    nursery_scopes: usize,
    /// M-C — this body opened an implicit nursery at entry (its `Op::EnterNursery` is the first body
    /// op) because it contains a bare `spawn`. Stamped onto the [`Proto`] in `finish`; the VM's
    /// `do_return` JOINS this nursery at the body's `return`/end.
    has_implicit_nursery: bool,
    /// Experimental — this proto is a generator body (its `Op::Yield`s suspend the generator).
    is_generator: bool,
    /// This proto is a `test fn` body (free test or suite method). Stamped onto the [`Proto`] in
    /// `finish`; used only by `chezzi test` discovery.
    is_test: bool,
    /// One-way int→float widening — this fn's declared return type is `float`, so every `return`
    /// (and an inline-expr body's implicit return) coerces its value with `Op::CoerceFloat` before
    /// `Op::Return`. Set from `FnDecl.ret` at the start of `compile_fn` (closures declare no ret type).
    ret_is_float: bool,
}

impl FnComp {
    fn new(name: String, arity: usize, is_toplevel: bool) -> Self {
        FnComp {
            name,
            arity,
            is_toplevel,
            code: Vec::new(),
            lines: Vec::new(),
            locals: Vec::new(),
            scope_depth: 0,
            slot_count: 0,
            max_slots: 0,
            captured_names: Vec::new(),
            boxed_names: std::collections::HashSet::new(),
            loops: Vec::new(),
            defer_scopes: 0,
            nursery_scopes: 0,
            has_implicit_nursery: false,
            is_generator: false,
            is_test: false,
            ret_is_float: false,
        }
    }

    /// The innermost loop being compiled, or `None` outside any loop.
    fn current_loop(&mut self) -> Option<&mut LoopCtx> {
        self.loops.last_mut()
    }

    /// Uniform by-reference capture — is local `slot`'s name in this frame's boxed set (so
    /// reads/writes go through an `Obj::Cell`)? Consulted by [`emit_get_named`]/[`emit_set_named`].
    fn is_boxed_slot(&self, slot: usize) -> bool {
        self.locals
            .get(slot)
            .is_some_and(|l| self.boxed_names.contains(&l.name))
    }

    /// Will a local NAMED `name` be boxed if declared now? (Same predicate as [`is_boxed_slot`] but by
    /// name — for deciding a binding's slot strategy BEFORE the slot is allocated, e.g. a `match`
    /// variant binding the VM writes positionally.)
    fn is_boxed_name(&self, name: &str) -> bool {
        self.boxed_names.contains(name)
    }

    // ----- slot emit routing (B1: exactly these are the only ways to touch a slot) -----
    //
    // A boxed slot holds an `Obj::Cell` HANDLE, never the value directly. A bare `GetLocal` on a
    // boxed slot would leave a raw cell handle on the stack — and the peephole fuser can then fold it
    // into an int superinstruction (pointer-as-int → crash). So every named user local read/write
    // routes through `emit_get_named`/`emit_set_named` (cell-aware), and every hidden compiler temp
    // routes through `emit_hidden_get`/`emit_hidden_set` (raw + a debug-assert that the slot is
    // genuinely unnamed). Param prologues, which legitimately move a boxed slot's handle before the
    // box exists, use the private raw helpers.

    /// Raw `GetLocal` — no cell-awareness, no assert. For the named/prologue paths that deliberately
    /// touch a boxed slot to move its handle.
    fn emit_get_local_raw(&mut self, slot: usize, span: Span) {
        self.emit(Op::GetLocal(slot), span);
    }

    /// Raw `SetLocal` — no cell-awareness, no assert. See [`emit_get_local_raw`].
    fn emit_set_local_raw(&mut self, slot: usize, span: Span) {
        self.emit(Op::SetLocal(slot), span);
    }

    /// Read a HIDDEN compiler temp (loop bookkeeping, match scrutinee, …). Debug-asserts the slot is
    /// unnamed so a misrouted named slot (which might be boxed) panics loudly instead of leaking a
    /// raw cell handle.
    fn emit_hidden_get(&mut self, slot: usize, span: Span) {
        debug_assert!(
            self.locals.get(slot).is_none_or(|l| l.name.is_empty()),
            "hidden get on a NAMED slot {slot}"
        );
        self.emit(Op::GetLocal(slot), span);
    }

    /// Write a HIDDEN compiler temp. See [`emit_hidden_get`].
    fn emit_hidden_set(&mut self, slot: usize, span: Span) {
        debug_assert!(
            self.locals.get(slot).is_none_or(|l| l.name.is_empty()),
            "hidden set on a NAMED slot {slot}"
        );
        self.emit(Op::SetLocal(slot), span);
    }

    /// Read a NAMED user local. A boxed slot dereferences its cell (`GetLocal; CellLoad`); a plain
    /// slot is a bare `GetLocal` (byte-identical to before boxing existed).
    fn emit_get_named(&mut self, slot: usize, span: Span) {
        self.emit_get_local_raw(slot, span);
        if self.is_boxed_slot(slot) {
            self.emit(Op::CellLoad, span);
        }
    }

    /// Store to a NAMED user local — the value is already on the stack top. A boxed slot pushes the
    /// cell handle on top (stack `[val, handle]`) and `CellStore`s (which pops handle-first, then
    /// value); a plain slot is a bare `SetLocal`.
    fn emit_set_named(&mut self, slot: usize, span: Span) {
        if self.is_boxed_slot(slot) {
            self.emit_get_local_raw(slot, span);
            self.emit(Op::CellStore, span);
        } else {
            self.emit_set_local_raw(slot, span);
        }
    }

    /// Declare a NEW named local `name` from the value currently on the stack top, returning its
    /// slot. A boxed name wraps the value in a FRESH cell (`NewCell`) and stores the handle; a plain
    /// name stores the value directly. Used at every named-binding site (`:=`, destructuring,
    /// `match`/`if let` bind, wait-assign) — each fresh binding gets its own cell.
    fn emit_decl_named(&mut self, name: String, span: Span) -> usize {
        let slot = self.add_local(name);
        if self.is_boxed_slot(slot) {
            self.emit(Op::NewCell, span);
        }
        self.emit_set_local_raw(slot, span);
        slot
    }

    /// The MECHANISM slot for a loop variable `user`: a fresh HIDDEN slot when `user` is boxed (the
    /// loop bookkeeping — `MatchArm` binds / index reads — writes the raw value here, and the user
    /// cell is refreshed from it per iteration by [`emit_loopvar_refresh`]), else `user` itself
    /// (byte-identical to before boxing when the loop var isn't captured).
    fn loopvar_raw_slot(&mut self, user: usize) -> usize {
        if self.is_boxed_slot(user) {
            self.add_hidden()
        } else {
            user
        }
    }

    /// Per-iteration fresh-cell wrap for a boxed loop variable (C1, Go ≥1.22): read the mechanism's
    /// raw slot, box it in a FRESH cell, store the handle in the user slot — so each iteration's
    /// closure captures its OWN cell. No-op when `user` isn't boxed (then `raw == user`). Emitted at
    /// the top of the loop body, after the item value is produced, before the body runs.
    fn emit_loopvar_refresh(&mut self, user: usize, raw: usize, span: Span) {
        if self.is_boxed_slot(user) {
            debug_assert_ne!(
                user, raw,
                "boxed loop var needs a separate raw mechanism slot"
            );
            self.emit_get_local_raw(raw, span);
            self.emit(Op::NewCell, span);
            self.emit_set_local_raw(user, span);
        }
    }

    /// Patch a previously-emitted jump to land at an explicit `target` instruction index (used for
    /// `continue`, whose target — the loop's increment — is *before* the current position).
    fn patch_jump_to(&mut self, at: usize, target: usize) {
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalseKeep(t)
            | Op::JumpIfTrueKeep(t)
            | Op::PushHandler(t)
            | Op::MatchArm { next: t, .. } => *t = target,
            other => panic!("patch_jump_to on non-jump op: {other:?}"),
        }
    }

    fn emit(&mut self, op: Op, span: Span) {
        self.code.push(op);
        self.lines.push(span);
    }

    /// Overwrite a previously-emitted op in place — used to back-patch `Op::WaitPoll`'s arm/else
    /// targets once the arm bodies have been laid out.
    fn set_code(&mut self, at: usize, op: Op) {
        self.code[at] = op;
    }

    /// Emit a jump-like op, returning its index so it can be patched once the target is known.
    fn emit_jump(&mut self, op: Op, span: Span) -> usize {
        let at = self.code.len();
        self.emit(op, span);
        at
    }

    /// The current instruction index — a jump target.
    fn here(&self) -> usize {
        self.code.len()
    }

    /// Patch a previously-emitted jump to land at the current position.
    fn patch_jump(&mut self, at: usize) {
        let target = self.code.len();
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalseKeep(t)
            | Op::JumpIfTrueKeep(t)
            | Op::PushHandler(t)
            | Op::MatchArm { next: t, .. } => *t = target,
            other => panic!("patch_jump on non-jump op: {other:?}"),
        }
    }

    /// At global scope only when this is the toplevel proto and no block is open.
    fn is_global_scope(&self) -> bool {
        self.is_toplevel && self.scope_depth == 0
    }

    fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    fn end_scope(&mut self) {
        self.scope_depth -= 1;
        while let Some(l) = self.locals.last() {
            if l.depth > self.scope_depth {
                self.locals.pop();
                self.slot_count -= 1;
            } else {
                break;
            }
        }
    }

    fn next_slot(&self) -> usize {
        self.slot_count
    }

    /// Add a named local, returning its slot. A redeclaration in the same scope shadows by getting
    /// a fresh slot (later lookups find the newest).
    fn add_local(&mut self, name: String) -> usize {
        let slot = self.slot_count;
        self.locals.push(LocalVar {
            name,
            depth: self.scope_depth,
        });
        self.slot_count += 1;
        if self.slot_count > self.max_slots {
            self.max_slots = self.slot_count;
        }
        slot
    }

    /// A hidden compiler-temp slot (loop bookkeeping, match scrutinee) — no name to collide with.
    fn add_hidden(&mut self) -> usize {
        self.add_local(String::new())
    }

    /// Resolve a name to a local slot (innermost first). `None` ⇒ not a local.
    fn resolve_local(&self, name: &str) -> Option<usize> {
        self.locals
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| l.name == name)
            .map(|(slot, _)| slot)
    }

    fn captures(&self, name: &str) -> bool {
        self.captured_names.iter().any(|n| n == name)
    }

    /// All bindings visible in this frame, to snapshot into a closure being created here. Locals
    /// (innermost wins) plus, if this frame is itself a closure, its captured names.
    fn snapshot_entries(&self) -> Vec<CapEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        // Innermost local of each name wins; skip the hidden (unnamed) temps.
        for (slot, l) in self.locals.iter().enumerate().rev() {
            if l.name.is_empty() || !seen.insert(l.name.clone()) {
                continue;
            }
            entries.push(CapEntry {
                name: l.name.clone(),
                src: CapSrc::Slot(slot),
            });
        }
        // A name not bound as a local here resolves against *this* frame's captured env (this frame
        // is itself a closure). Its enclosing-proto slot is its position in `self.captured_names`
        // (positional captures, lever #3) — stamp it so `MakeClosure` reads `captured[parent_slot]`.
        for (parent_slot, name) in self.captured_names.iter().enumerate() {
            if seen.insert(name.clone()) {
                entries.push(CapEntry {
                    name: name.clone(),
                    src: CapSrc::Captured(parent_slot as u32),
                });
            }
        }
        entries
    }
}

#[cfg(test)]
mod interp_tests {
    use super::*;

    fn sp() -> Span {
        Span { line: 1, col: 1 }
    }

    // The compiler's standalone-path `ctype_of`/`ctype_of_visiting` second resolver was DELETED in
    // fix5 (the single-resolver redesign); extern C types now come VERBATIM from the checker's
    // `resolve_extern_signatures{,_standalone}`. Its behavioral coverage (widths, owned/nullable str,
    // flat struct, cyclic-alias-no-overflow) lives in `checker::tests::resolve_extern_ctype`.

    #[test]
    fn parse_interpolation_attaches_spec() {
        let chunks = parse_interpolation("{x:>5}", sp()).unwrap();
        match &chunks[..] {
            [Chunk::Expr(_, Some(spec))] => {
                assert_eq!(spec.width, 5);
                assert_eq!(spec.align, Some(crate::fmtspec::Align::Right));
            }
            _ => panic!("expected one spec'd expr chunk"),
        }
    }

    #[test]
    fn parse_interpolation_bare_expr_has_no_spec() {
        let chunks = parse_interpolation("{x}", sp()).unwrap();
        assert!(matches!(&chunks[..], [Chunk::Expr(_, None)]));
    }

    #[test]
    fn parse_interpolation_width_cap_is_compile_error() {
        let err = parse_interpolation("{x:>99999999}", sp()).unwrap_err();
        assert!(
            err.message.contains("exceeds maximum 4096"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_interpolation_colon_inside_index_not_a_separator() {
        // The `:` inside the string-key index is NOT the spec separator; only the trailing one is.
        let chunks = parse_interpolation("{m[\"a:b\"]:>3}", sp()).unwrap();
        match &chunks[..] {
            [Chunk::Expr(_, Some(spec))] => assert_eq!(spec.width, 3),
            _ => panic!("expected spec'd expr"),
        }
        // And with no trailing spec, the inner `:` stays part of the expression.
        let chunks = parse_interpolation("{m[\"a:b\"]}", sp()).unwrap();
        assert!(matches!(&chunks[..], [Chunk::Expr(_, None)]));
    }
}

#[cfg(test)]
mod capture_layout_tests {
    //! M19 lever #3: captures are positional. `GetCaptured` carries a compile-time slot (u32), and
    //! each closure proto records its capture names in slot order (`Proto.capture_names`).
    use super::*;
    use crate::vm::op::Op;

    fn compile(src: &str) -> crate::vm::op::Program {
        let tokens = crate::lexer::tokenize(src).expect("lex");
        let module = crate::parser::parse(tokens).expect("parse");
        compile_module_standalone(&module).expect("compile")
    }

    /// Find the first proto that reads a capture, returning its (proto, GetCaptured slots in code order).
    fn captured_slots(prog: &crate::vm::op::Program) -> Vec<u32> {
        for p in &prog.protos {
            let slots: Vec<u32> = p
                .code
                .iter()
                .filter_map(|op| match op {
                    Op::GetCaptured(slot) => Some(*slot),
                    _ => None,
                })
                .collect();
            if !slots.is_empty() {
                return slots;
            }
        }
        panic!("no GetCaptured op emitted");
    }

    #[test]
    fn get_captured_carries_a_u32_slot() {
        // A closure reading one captured var emits GetCaptured(0) (a numeric slot, not a string).
        let prog = compile("fn make(n: int):\n    return fn(x: int) -> int: x + n\nmake(1)\n");
        assert_eq!(captured_slots(&prog), vec![0], "single capture → slot 0");
    }

    #[test]
    fn two_captures_get_distinct_slots_in_snapshot_order() {
        // `a` then `b` referenced; snapshot_entries orders innermost locals first (reverse decl).
        // Whatever the order, the two captures must occupy distinct, stable slots 0 and 1.
        let prog = compile(
            "fn make(a: int, b: int):\n    return fn(x: int) -> int: x + a + b\nmake(1, 2)\n",
        );
        let mut slots = captured_slots(&prog);
        slots.sort_unstable();
        assert_eq!(slots, vec![0, 1], "two captures → slots 0 and 1");
    }

    #[test]
    fn proto_records_capture_names_in_slot_order() {
        // The closure proto carries the captured names in slot order (cold-path metadata, mirrors
        // StructDef.fields). Slot i of capture_names is the name read by GetCaptured(i).
        let prog = compile(
            "fn make(a: int, b: int):\n    return fn(x: int) -> int: x + a + b\nmake(1, 2)\n",
        );
        let clo = prog
            .protos
            .iter()
            .find(|p| !p.capture_names.is_empty())
            .expect("a closure proto with captures");
        assert_eq!(clo.capture_names.len(), 2, "two captured names recorded");
        let mut names = clo.capture_names.clone();
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn non_closure_proto_has_no_capture_names() {
        // A plain top-level fn captures nothing; its proto's capture_names is empty.
        let prog = compile("fn plain(x: int) -> int:\n    return x + 1\nplain(1)\n");
        for p in &prog.protos {
            // Only the closure proto (none here) would be non-empty.
            assert!(
                p.capture_names.is_empty(),
                "plain fn proto has no capture names"
            );
        }
    }

    /// Native-prelude phase 1 — BYTE-IDENTICAL LOWERING PIN. Direct calls of the four first-class
    /// universe fns must lower to their specialized opcodes regardless of whether the intrinsic is
    /// selected by an ad-hoc match arm (old) or the synthetic `PRELUDE` table (new). This is the
    /// characterization lock the refactor must not move: `print(x)`→`CallPrint(1)`,
    /// `print(x, sep=…)`→`CallPrintSep{argc:1}`, `ord("a")`/`panic("m")`→`CallBuiltin(name, 1)`.
    fn all_ops(prog: &crate::vm::op::Program) -> Vec<Op> {
        prog.protos
            .iter()
            .flat_map(|p| p.code.iter().cloned())
            .collect()
    }

    #[test]
    fn direct_builtin_calls_lower_to_specialized_opcodes() {
        let ops = all_ops(&compile("print(1)\n"));
        assert!(
            ops.iter().any(|o| matches!(o, Op::CallPrint(1))),
            "print(x) must lower to CallPrint(1); got {ops:?}"
        );

        let ops = all_ops(&compile("print(1, sep=\"-\")\n"));
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CallPrintSep { argc: 1 })),
            "print(x, sep=…) must lower to CallPrintSep{{argc:1}}; got {ops:?}"
        );

        let ops = all_ops(&compile("ord(\"a\")\n"));
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n == "ord")),
            "ord(c) must lower to CallBuiltin(\"ord\", 1); got {ops:?}"
        );

        let ops = all_ops(&compile("panic(\"m\")\n"));
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n == "panic")),
            "panic(m) must lower to CallBuiltin(\"panic\", 1); got {ops:?}"
        );

        // Phase 2a — the scalar-conversion CTORS must STILL lower to the name-keyed CallBuiltin,
        // byte-identical to their old `is_builtin` fall-through (now table-driven via `Intrinsic::Ctor`).
        for name in ["int", "float", "bool", "str", "bytes", "bytearray"] {
            let ops = all_ops(&compile(&format!("{name}(\"5\")\n")));
            assert!(
                ops.iter()
                    .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n == name)),
                "{name}(x) must lower to CallBuiltin(\"{name}\", 1); got {ops:?}"
            );
        }

        // Phase 2b — the GENERIC / reserved-type container CTORS must STILL lower to the name-keyed
        // CallBuiltin, byte-identical to their old hard-coded `is_builtin` arm (now table-driven via
        // `Intrinsic::Ctor`). `range(5)`/`List()`/`Map()`/`Set()` each lower to `CallBuiltin(name, 1)`.
        for name in ["range", "List", "Map", "Set"] {
            let ops = all_ops(&compile(&format!("{name}([])\n")));
            assert!(
                ops.iter()
                    .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n == name)),
                "{name}([]) must lower to CallBuiltin(\"{name}\", 1); got {ops:?}"
            );
        }
        // A turbofished container ctor lowers to the IDENTICAL opcode — type args are type-erased
        // before the compiler, so `List[int]()` == `List()` at the opcode level (zero args here).
        let ops = all_ops(&compile("List[int]()\n"));
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CallBuiltin(n, 0) if n == "List")),
            "List[int]() must lower to CallBuiltin(\"List\", 0); got {ops:?}"
        );
    }
}

#[cfg(test)]
mod capture_prepass_tests {
    //! Uniform by-reference capture, Task A: the compile-time capture pre-pass computes the
    //! boxed-name set (names of this frame's locals captured by a nested closure/defer/spawn body).
    //! Unwired this task — nothing reads the set yet; these tests pin the pre-pass computation.
    use super::*;
    use std::collections::HashSet;

    /// Parse `src`, return the first top-level `fn`'s [`FnDecl`].
    fn first_fn(src: &str) -> FnDecl {
        let tokens = crate::lexer::tokenize(src).expect("lex");
        let module = crate::parser::parse(tokens).expect("parse");
        for s in &module.stmts {
            if let StmtKind::Fn(decl) = &s.kind {
                return decl.clone();
            }
        }
        panic!("no top-level fn in source");
    }

    #[test]
    fn prepass_boxes_captured_local_not_uncaptured() {
        // `x` is captured by the nested closure; `y` is not. Only `x` is boxed.
        let decl =
            first_fn("fn f():\n    x := 1\n    y := 2\n    g := fn() -> int: x\n    return g()\n");
        let boxed = captured_names_of_body(&decl.body, &decl.params);
        assert_eq!(boxed, HashSet::from(["x".to_string()]));
        assert!(!boxed.contains("y"), "uncaptured local y must not be boxed");
    }

    #[test]
    fn prepass_recurses_through_nested_closures() {
        // A grandparent local captured two closures deep must surface (free-var descent): the inner
        // closure references `x`, so `x` is free in the outer closure `g`, which is `f`'s direct child.
        let decl = first_fn(
            "fn f():\n    x := 1\n    g := fn() -> int: (fn() -> int: x)()\n    return g()\n",
        );
        let boxed = captured_names_of_body(&decl.body, &decl.params);
        assert!(
            boxed.contains("x"),
            "grandchild-captured x must be boxed; got {boxed:?}"
        );
    }

    #[test]
    fn prepass_boxes_captured_param() {
        // A parameter captured by a nested closure is attributed to this frame's slot.
        let decl = first_fn("fn f(n: int):\n    g := fn() -> int: n\n    return g()\n");
        let boxed = captured_names_of_body(&decl.body, &decl.params);
        assert!(
            boxed.contains("n"),
            "captured param n must be boxed; got {boxed:?}"
        );
    }

    #[test]
    fn is_boxed_slot_reads_boxed_names() {
        let mut fc = FnComp::new("f".to_string(), 0, false);
        fc.locals.push(LocalVar {
            name: "x".to_string(),
            depth: 0,
        });
        fc.locals.push(LocalVar {
            name: "y".to_string(),
            depth: 0,
        });
        fc.boxed_names = HashSet::from(["x".to_string()]);
        assert!(fc.is_boxed_slot(0), "slot 0 (x) is boxed");
        assert!(!fc.is_boxed_slot(1), "slot 1 (y) is not boxed");
    }
}
