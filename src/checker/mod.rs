//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching the interpreter's hoist); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Bound, CompClause, CompKind, DeferTarget, Expr, ExprKind, FnDecl,
    Import, LitPattern, MethodSig, Param, Pattern, Span, SpawnTarget, Stmt, StmtKind, Type,
    TypeParam, UnaryOp, WaitArm, WaitTarget,
};
use crate::native::cffi::CType;
use crate::resolver::{ModuleGraph, ModuleId, ResolvedImport};
use std::collections::HashMap;
use std::fmt;

pub use ty::Ty;
use ty::{compatible, ref_display};

/// The fully-resolved C signature of one `extern` fn, computed by the checker in the defining
/// module's import/alias scope (the single source of truth for every alias spelling). Each param /
/// the return is a width-bearing [`CType`] (NOT a `Ty`, which collapses `int8`..`int64` to `Ty::Int`
/// — the width is exactly what FFI marshalling needs). `None` for a param means an annotation the
/// checker could not lower (only reachable on an ill-typed program the marshallability gate already
/// rejected); `ret` is `None` for a `void` return (no annotation or one resolving to `nil`).
#[derive(Clone, Default)]
pub struct ExternCSig {
    pub params: Vec<Option<CType>>,
    pub ret: Option<CType>,
}

/// Resolved C signatures for every `extern` fn in a module graph, keyed by `(graph module index,
/// fn name)` — the SAME index both backends derive (`compile_graph`'s enumerate / interp's
/// `module_idx_of`). The backends consume these instead of re-resolving alias names themselves, so
/// every alias spelling (local chain, named-import hop, qualified hop, mixed) resolves in its own
/// module's scope, collision-proof by construction. Produced by [`resolve_extern_signatures`].
pub type ExternTable = HashMap<(usize, String), ExternCSig>;

/// What a `match` scrutinee is being matched against, threaded through the match-checking helpers.
enum MatchKind {
    /// Enum/Result/Option scrutinee — arms are variant patterns.
    Variants {
        label: String,
        variants: HashMap<String, Vec<Ty>>,
    },
    /// int/str/bool scrutinee — arms are literal patterns (+ a required `_` wildcard).
    Literal(Ty),
    /// Tuple scrutinee — arms are tuple patterns (gap #15). Carries the element types.
    Tuple(Vec<Ty>),
    /// Un-inferable (`Ty::Unknown`) scrutinee with only binding/`_` arms — skip exhaustiveness, bind
    /// permissively. A STRUCTURAL arm over an un-inferable scrutinee is rejected upstream (§4.1, in
    /// `reconstruct_unknown_kind` for the top-level arm and `bind_subpattern` for nested positions);
    /// a literal/range arm pins `MatchKind::Literal`. So `Skip` only carries the genuinely-permissive
    /// residue.
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
/// the redefinition at declaration. `Iterator` is likewise reserved: as a value type it names the
/// experimental generator existential (`Ty::Struct("Iterator", [T])`) that `iter_elem` / `.next()`
/// typing key on, so a user `struct Iterator[T]` would be silently shadowed (and crash at runtime
/// on a phantom `.next()`).
///
/// The same hazard applies to EVERY bare name `resolve_type` maps to a builtin: a user `struct int`
/// / `enum List` / `struct Socket` would type-check clean (the decl guards only consulted this set)
/// and then the use-site (`x: int` / `l: List[int]` / `s: Socket`) would silently resolve to the
/// builtin, yielding an unreachable user type + self-contradictory diagnostics (`expected Socket,
/// found Socket`). So the builtin SCALAR (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`),
/// CONTAINER (`List`/`Set`/`Map`/`Channel`/`range`), and HANDLE (`Socket`/`Listener`/`ptr`/
/// `owned_str`) type names are all reserved at declaration too — `struct X` is rejected with the same
/// `type 'X' is reserved (builtin)` error `struct Result` already gives. (The fixed-width FFI integer
/// names like `int32` are reserved via `native::ffi::TYPE_NAMES` at the decl guards, not listed here.)
fn is_reserved_type(name: &str) -> bool {
    name == "Result"
        || name == "Option"
        || name == "Iterator"
        // Builtin scalar primitives — a user `struct int` / `enum str` would shadow the scalar arm in
        // `resolve_type`.
        || name == "int"
        || name == "float"
        || name == "bool"
        || name == "str"
        || name == "bytes"
        || name == "bytearray"
        || name == "nil"
        // Builtin container/collection type names — recognized by `resolve_type`'s generic arms
        // (`List[T]`/`Set[T]`/`Map[K,V]`/`Channel[T]`) and `range` (a reserved callable whose ctor a
        // `struct range` would silently shadow).
        || name == "List"
        || name == "Set"
        || name == "Map"
        || name == "Channel"
        || name == "range"
        // Runtime handle TYPE names: the std.net TCP handles (`Socket`/`Listener`) and the FFI
        // marshalling primitives (`ptr` and the return-only `owned_str`). Reserved at declaration even
        // though their USE is import-gated (std.net / std.ffi) — the two gates are independent, exactly
        // as for the concurrency ctors below.
        || name == "Socket"
        || name == "Listener"
        || name == "ptr"
        || name == "owned_str"
        // The four runtime concurrency ctor/TYPE names stay RESERVED — a user `struct Shared` /
        // `struct Executor` is rejected at declaration (a clean `reserved` error), NOT silently
        // shadowed by the builtin ctor. This is SEPARATE from the `import std.concurrency` gate (which
        // governs USE): the names are reserved AND require the import to use the builtin. (`Executor`
        // was already reserved here; `Shared`/`RwShared`/`Atomic` join it so all four behave alike.)
        || name == "Shared"
        || name == "RwShared"
        || name == "Atomic"
        || name == "Executor"
        // `timer` is a runtime ctor/builtin name (not a real type), but it STAYS reserved here so a
        // user `struct timer` / `enum timer` / `type timer` is rejected at declaration rather than
        // silently shadowed by the opcode-backed builtin. SEPARATE from the `import std.time` gate
        // (which governs USE): the name is reserved AND requires the import to call `timer(ms)`.
        || name == "timer"
}

/// True iff `{a, b}` is an `int`/`float` mix in either order — the trigger for one-way int→float
/// widening when unifying a collection literal's element/value types (`[1, 2.3]` → `list[float]`).
fn numeric_mix(a: &Ty, b: &Ty) -> bool {
    matches!((a, b), (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int))
}

/// A short, surface-faithful label for a return-only extern `Type` in a marshallability error
/// (`owned_str`, `str?`, `owned_str?`). Only ever called on the forms `is_return_only_extern_type`
/// already matched, so non-matching shapes fall back to a generic label.
fn describe_extern_type(t: &Type) -> String {
    match t {
        Type::Named { name: n, .. } => n.clone(),
        Type::Generic(n, args, ..) if n == "Option" => match args.first() {
            Some(Type::Named { name: inner, .. }) => format!("{inner}?"),
            _ => "str?".to_string(),
        },
        _ => "owned_str".to_string(),
    }
}

/// Names an `extern` C fn may NOT take: a builtin (`len`/`range`/`int`/`float`/`str`/`ord`/`chr`/
/// `set`), `print`, or a runtime constructor (`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`). Both
/// backends resolve these names to a special op *before* a plain named call (`compiler::compile_call`
/// / `interp::eval_call`), so an extern fn with one of these names is silently shadowed — dead code
/// that the compiler's eager `MakeCffi` would still `dlsym` (aborting on a symbol it can never call).
/// Mirrors `compiler::is_builtin` + the constructor/`print` special cases. (Struct- and variant-name
/// collisions are caught separately against the built registries, since those are user-declared.)
/// Every reserved CALLABLE builtin name: the free functions (`print`/`panic`/`range`/`int`/`float`/
/// `str`/`ord`/`chr`) and the container/runtime constructors (`List`/`Set`/`Map`/`bytes`/`bytearray`/
/// `Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`). Every name here is CALLED (none is a
/// pure type marker), so each must have a [`builtin_sig`] entry for editor hover — enforced by the
/// `reserved_callables_all_have_builtin_sig` drift-guard test. (The generic ctors `Ok`/`Err`/`Some`
/// are NOT reserved — a user may shadow them — so they are intentionally out of this set and keep
/// hovering `None` for v1.) Mirrors `compiler::is_builtin` + the constructor/`print` special cases.
const RESERVED_CALLABLE: &[&str] = &[
    // builtins (mirrors compiler::is_builtin / interp::builtins::is_builtin)
    "range",
    "int",
    "float",
    "str",
    "ord",
    "chr",
    "Set",
    "List",
    "Map",
    "bytes",
    "bytearray",
    // the special print op
    "print",
    // the diverging panic(msg) op (raises a recoverable RuntimeError; bottom-typed)
    "panic",
    // runtime constructors the backends special-case before a plain call
    "Channel",
    "Shared",
    "RwShared",
    "Atomic",
    "timer",
    "Executor",
];

fn is_reserved_name(name: &str) -> bool {
    RESERVED_CALLABLE.contains(&name)
}

/// Prebuilt protocols a user program may use as bounds but must not redeclare (mirrors
/// [`prebuilt_protocols`]).
fn is_reserved_protocol(name: &str) -> bool {
    matches!(
        name,
        "Comparable"
            | "Stringable"
            | "Hashable"
            | "Error"
            | "Add"
            | "Sub"
            | "Mul"
            | "Div"
            | "Mod"
            | "Neg"
            | "Arithmetic"
            | "Iterator"
            | "Iterable"
            | "Index"
            | "IndexSet"
            | "Slice"
    )
}

/// True if `name` is one of the four recognized suite lifecycle hooks.
fn is_lifecycle_hook(name: &str) -> bool {
    crate::vm::op::LIFECYCLE_HOOKS.contains(&name)
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
    /// A struct/enum method whose first parameter is NOT `self` (or which has no params) is a STATIC
    /// (associated) method — called `Type.method(args)`, not `value.method(args)`. `false` for every
    /// instance method, free function, closure, and native sig (the default via `plain`/`optional_tail`);
    /// set only by [`Checker::fn_sig`] from the declaration's first param name.
    is_static: bool,
    /// Doc-comment from the declaration (set by [`Checker::fn_sig`] from `FnDecl::doc`; `None` for
    /// native/synthetic sigs). Purely informational — surfaced on LSP hover, never affects checking
    /// (excluded from `fn_sig_eq`). Covers free fns AND methods/static methods (all reuse `FnSig`).
    doc: Option<String>,
}

impl FnSig {
    /// A non-generic signature (the common case): every param is required (`min_params == params.len()`).
    fn plain(params: Vec<Ty>, ret: Ty) -> FnSig {
        let min_params = params.len();
        FnSig {
            params,
            ret,
            type_params: Vec::new(),
            min_params,
            is_static: false,
            doc: None,
        }
    }

    /// D6c — a non-generic signature whose last `optional` params may be omitted (the net socket ops'
    /// optional trailing `timeout_ms`). `check_args` accepts `params.len() - optional ..= params.len()`.
    fn optional_tail(params: Vec<Ty>, ret: Ty, optional: usize) -> FnSig {
        let min_params = params.len() - optional;
        FnSig {
            params,
            ret,
            type_params: Vec::new(),
            min_params,
            is_static: false,
            doc: None,
        }
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
#[derive(Clone)]
struct StructInfo {
    type_params: Vec<TypeParam>,
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
    origin: StructOrigin,
    /// The struct's own decl docstring (the `#` block above `struct X:`), captured in `capture_sig`
    /// from `name_docs` so it crosses the module boundary to an importer's hover. Editor-only; `None`
    /// for a builtin/native struct and until `capture_sig` populates it. Never read by checking/codegen.
    doc: Option<String>,
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
    /// Embedded (super-)protocols (M22): `protocol Vector: Add + Sub` carries `[Add, Sub]`. A type
    /// satisfies this protocol iff it satisfies every embed (transitively) AND has every OWN method
    /// in `methods`. Empty for an ordinary protocol. Reuses [`Bound`] — an embed ref is identical to
    /// a type-param bound (name + optional `[args]`). The builtin `Arithmetic` bundle is built with
    /// this same field (`embeds: [Add, Sub, Mul, Div]`, no own methods) — uniform machinery.
    embeds: Vec<Bound>,
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
    /// Resolved struct definitions this module declares (module-scoped types — D? feature). An
    /// importer injects these into its own per-module `structs`/`struct_names` so a `from`-imported
    /// or qualified-access struct resolves with the right field layout.
    struct_defs: HashMap<String, StructInfo>,
    /// Resolved enum definitions this module declares: variant names (in order), generic type
    /// params, and each variant's payload `VariantInfo`.
    enum_defs: HashMap<String, EnumSigInfo>,
    /// Resolved newtype definitions this module declares: the underlying `Ty` and the name-keyed
    /// methods. An importer injects these into its per-module `newtype_defs`/`newtype_names`.
    newtype_defs: HashMap<String, NewTypeSigInfo>,
    /// Transparent type aliases this module declares, with their body RESOLVED in the DEFINING
    /// module's scope (so a cross-module `type Len = int32` carries its FFI-width license). The bool
    /// records whether the alias was licensed (`ffi_alias_ok`) in the defining module. The
    /// `Option<String>` is the name of an embedded FFI width that the defining module did NOT import
    /// (so the alias is unlicensed): an importer must reject it with "unknown type" — the width can't
    /// be laundered through an unlicensed alias.
    type_aliases: HashMap<String, AliasSig>,
}

/// An exported type alias inside a `ModuleSig`: its body RESOLVED in the defining module's scope,
/// whether it carries an FFI-width license, and — if NOT licensed yet embeds an FFI width the
/// defining module never imported — that un-imported width's name (so an importer rejects it).
#[derive(Clone)]
struct AliasSig {
    body: Ty,
    licensed: bool,
    unlicensed_width: Option<String>,
    /// The alias body resolved to a width-bearing [`CType`] in the DEFINING module's scope (so a
    /// cross-module `type Len = int32` exports `int32`, not `Ty::Int`). `None` if the body is not
    /// C-marshallable (its FFI use is rejected by the checker). This is what carries the real C
    /// width across a `from`-import or a `module.Alias` hop — the `body: Ty` cannot, since `Ty`
    /// collapses every FFI width to `Ty::Int`.
    ctype: Option<CType>,
}

/// A newtype's exported shape inside a `ModuleSig`: its resolved underlying `Ty` and its name-keyed
/// methods, ferried across the module boundary so an imported newtype constructs/unwraps/dispatches.
#[derive(Clone)]
struct NewTypeSigInfo {
    underlying: Ty,
    /// The newtype's generic type params (empty for a scalar newtype), ferried across the module
    /// boundary so an imported generic newtype's instantiation/dispatch/cast-unwrap resolves.
    type_params: Vec<TypeParam>,
    methods: HashMap<String, FnSig>,
    /// The newtype's own decl docstring, carried across the module boundary for an importer's hover
    /// (see [`StructInfo::doc`]). Editor-only; never read by checking/codegen.
    doc: Option<String>,
}

/// An enum's exported shape inside a `ModuleSig`: variant names in declaration order, the enum's
/// generic type params, and each variant's resolved `VariantInfo` (payload types).
#[derive(Clone)]
struct EnumSigInfo {
    variant_names: Vec<String>,
    type_params: Vec<TypeParam>,
    variants: Vec<VariantInfo>,
    /// The enum's methods (`fn area(self) …`), name-keyed like `StructInfo.methods`. Ferried across
    /// the module boundary so an imported enum's methods resolve in the importer.
    methods: HashMap<String, FnSig>,
    /// The enum's own decl docstring, carried across the module boundary for an importer's hover
    /// (see [`StructInfo::doc`]). Editor-only; never read by checking/codegen.
    doc: Option<String>,
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
/// before dependents), accumulating all errors across all modules (Go-style). User types are
/// MODULE-SCOPED: a type declared in one module is private to it and visible elsewhere only via
/// import. The same type name may appear in several modules.
pub fn check_graph(graph: &ModuleGraph) -> Result<(), Vec<CheckError>> {
    let mut c = Checker::new();
    c.run_graph_pass(graph, false);
    if c.errors.is_empty() {
        Ok(())
    } else {
        Err(std::mem::take(&mut c.errors))
    }
}

/// Classification of the symbol a hover landed on, returned alongside the inferred type display so
/// editor tooling can label it (`local`/`param`/`fn`/`field`/`struct`/literal). Secondary metadata —
/// the type string is the load-bearing payload. `Param` is PRODUCED at param-DECL hover sites (fn /
/// method / closure signature); a param's body-USE still reports `Local` (different span). `Other`
/// covers a leaf the classifier can't bucket.
// `Struct` is part of the public hover-kind contract but not yet produced (a struct-name hover
// currently resolves through other paths) — these variants are also constructed by editor consumers,
// not this crate's default `chezzi` bin (which compiles `checker` privately and reaches hover only
// via the lib's `editor`), so allow the bin-only dead-code lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HoverKind {
    Local,
    Param,
    Func,
    Field,
    Struct,
    /// A TYPE token inside an annotation (`x: Id`, a param/return/field type, a `let` annotation):
    /// the hovered name resolves through `resolve_type` and the display is the resolved `Ty`.
    Type,
    Literal,
    Other,
}

/// Editor-tooling introspection (LSP hover): re-run the SAME deps-first checking pass as
/// [`check_graph`] over `graph`, but with a single-position PROBE armed on the ENTRY module. Returns
/// `Some((type_display, kind))` for the leaf expression / binding / field-name whose anchor position
/// is the 1-based `(line, col)` token start — and ONLY when the whole program type-checks (mirrors
/// the "no type when the program doesn't check" contract). `None` otherwise. Behavior-preserving:
/// the probe only records a type already computed by the normal pass; it changes no checker decision.
///
/// Consumed by the lib's `editor::hover` (and through it the `chezzi-lsp` server); the default
/// `chezzi` bin compiles `checker` privately without reaching it, so allow that target's dead-code lint.
#[allow(dead_code)]
pub fn hover_type(
    graph: &ModuleGraph,
    line: usize,
    col: usize,
) -> Option<(String, HoverKind, Option<String>)> {
    let mut c = Checker::new();
    c.hover_probe = Some((line, col));
    c.hover_entry = Some(graph.entry.clone());
    c.run_graph_pass(graph, false);
    if !c.errors.is_empty() {
        return None;
    }
    c.hover_result
        .take()
        .map(|(ty, kind, doc)| (ty.to_string(), kind, doc))
}

/// FFI ROOT FIX (fix4): resolve the fully-resolved, width-bearing C signature of every `extern` fn
/// in the graph, each in its DEFINING module's import/alias scope — the SINGLE resolver both backends
/// consume so every alias spelling (local chain, named-import hop, qualified hop, mixed) resolves
/// collision-proof by construction. Runs the SAME module-scoped pass as [`check_graph`] (deps-first,
/// `begin_module` / `bind_import` / `module_sigs`) but harvests the extern table and ignores type
/// errors (the error gate is `check_graph`, run separately by the CLI). The returned table is keyed
/// by `(graph module index, fn name)`.
pub fn resolve_extern_signatures(graph: &ModuleGraph) -> ExternTable {
    let mut c = Checker::new();
    c.run_graph_pass(graph, true);
    std::mem::take(&mut c.extern_sigs)
}

/// Resolve extern C signatures for a SINGLE-FILE (standalone, source-string) program, going through
/// the EXACT SAME [`resolve_extern_signatures`] pass as the multi-file CLI — so there is exactly ONE
/// extern-type resolver in the whole codebase and the backends never re-resolve. Wraps `stmts` in a
/// synthetic one-module [`ModuleGraph`]: id `<main>` (so `module_keys` yields `<main>`, matching the
/// backends' `STANDALONE_MODULE_KEY` struct-identity key), no imports, no native. Width names
/// (`int32`) resolve as reserved leaves; local aliases/structs resolve from the parsed stmts; no
/// qualified / named-import forms exist in single-file source, so nothing else is needed.
///
/// Test-only: the single-file standalone compile/run paths (`compile_module_standalone`,
/// `Interp::execute`) are `#[cfg(test)]`. The production CLI — single- AND multi-file — always goes
/// through `build_graph` → `resolve_extern_signatures`.
#[cfg(test)]
pub fn resolve_extern_signatures_standalone(stmts: &[Stmt]) -> ExternTable {
    let id = crate::resolver::ModuleId(std::path::PathBuf::from("<main>"));
    let graph = ModuleGraph {
        entry: id.clone(),
        modules: vec![crate::resolver::LoadedModule {
            id,
            dotted: Vec::new(),
            ast: crate::ast::Module {
                stmts: stmts.to_vec(),
            },
            imports: Vec::new(),
            native: None,
        }],
    };
    resolve_extern_signatures(&graph)
}

impl Checker {
    /// The shared deps-first module-checking pass behind both [`check_graph`] and
    /// [`resolve_extern_signatures`]. When `harvest_externs` is set, gathers every struct's AST field
    /// types up front and stamps each module's graph index so the extern loop records resolved C
    /// signatures into `self.extern_sigs`.
    fn run_graph_pass(&mut self, graph: &ModuleGraph, harvest_externs: bool) {
        let c = self;
        // ROOT REDESIGN — module-scoped IDENTITY KEYS: scan every non-native module's struct/enum/alias
        // names and key EACH one `<module-key>::Name` (via the shared `resolver::module_keys`, the SAME
        // derivation the compiler + interpreter use), so all three engines agree on every key (parity) and
        // every user type is unique by construction (a cross-module name clash is just two distinct keys).
        {
            let mkeys = crate::resolver::module_keys(graph);
            for (idx, lm) in graph.modules.iter().enumerate() {
                // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (skip the
                // qualified key), so `Ref`/`Iterator`/FFI widths resolve bare, like the synthetic natives.
                if lm.native.is_some() || lm.is_std() {
                    continue;
                }
                for s in &lm.ast.stmts {
                    if let StmtKind::Struct { name, .. }
                    | StmtKind::Enum { name, .. }
                    | StmtKind::NewType { name, .. }
                    | StmtKind::TypeAlias { name, .. } = &s.kind
                    {
                        c.type_keys.insert(
                            (lm.id.clone(), name.clone()),
                            format!("{}::{name}", mkeys[idx]),
                        );
                    }
                }
            }
        }
        // FFI ROOT FIX (fix4): when harvesting extern signatures, gather every user struct's AST field
        // types up front, keyed by IDENTITY KEY (`<module-key>::Name`, the SAME derivation), so
        // `resolve_ctype` can build a by-value extern struct's `CType::Struct` with each field's real C
        // width — even when the struct is declared after the extern block, or in another module.
        if harvest_externs {
            let mkeys = crate::resolver::module_keys(graph);
            for (idx, lm) in graph.modules.iter().enumerate() {
                if lm.native.is_some() {
                    continue;
                }
                let is_std = lm.is_std();
                for s in &lm.ast.stmts {
                    if let StmtKind::Struct { name, fields, .. } = &s.kind {
                        let key = if is_std {
                            name.clone()
                        } else {
                            format!("{}::{name}", mkeys[idx])
                        };
                        c.struct_field_asts.insert(
                            key,
                            fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect(),
                        );
                    }
                }
            }
        }
        // Build the reverse index name → declaring module label(s), in graph (deps-first) order, so a
        // bare unimported-type error can hint "import it from <module>". A non-entry module is labelled
        // by its dotted/file name; the entry module is excluded (its types are local, never imported).
        for lm in &graph.modules {
            if lm.native.is_some() || lm.id == graph.entry {
                continue;
            }
            let label = lm.label();
            for s in &lm.ast.stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::NewType { name, .. }
                | StmtKind::TypeAlias { name, .. } = &s.kind
                {
                    c.types_by_name
                        .entry(name.clone())
                        .or_default()
                        .push(label.clone());
                }
            }
        }
        // The synthetic native-module structs are module-owned too: register their owning module so a
        // bare unimported `m: Match` errors with the "import it from std.regex" hint (the native-module
        // loop above skips them — they have no AST). Pushed after the AST loop so a same-named user
        // struct (declared in some module) is still hinted first.
        for (tn, owner) in [
            ("Match", "std.regex"),
            ("Response", "std.request"),
            ("ProcResult", "std.process"),
        ] {
            c.types_by_name
                .entry(tn.to_string())
                .or_default()
                .push(owner.to_string());
        }
        for (idx, lm) in graph.modules.iter().enumerate() {
            // A native std module (std.math/io/os) has no AST: its public surface is a static table.
            if let Some(name) = lm.native {
                c.module_sigs.insert(lm.id.clone(), native_module_sig(name));
                continue;
            }
            let label = if lm.id == graph.entry {
                None
            } else {
                Some(lm.label())
            };
            c.begin_module(label);
            // Stamp the current module's graph index so the extern loop keys its resolved C signatures
            // the SAME way both backends look them up. `None` outside the harvesting pass.
            c.extern_module_idx = if harvest_externs { Some(idx) } else { None };
            c.current_module_is_stdlib = lm.is_std();
            let sig = c.check_module(&lm.ast.stmts, Some(&lm.id), &lm.imports);
            c.module_sigs.insert(lm.id.clone(), sig);
        }
    }
}

/// The static type signatures of a native std module's members (M6c). This is the **third**
/// lockstep table: it must agree with the runtime members in `src/native/<module>.rs` and the
/// per-engine value lowering. `std.math` params are `float` (the language has no implicit int→float,
/// so callers pass floats); `pi`/`e` are float constants.
fn native_module_sig(name: &str) -> ModuleSig {
    let mut sig = ModuleSig::default();
    let mut func = |n: &str, params: Vec<Ty>, ret: Ty| {
        sig.functions
            .insert(n.to_string(), FnSig::plain(params, ret));
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
            // Float predicates (IEEE-754 classification): `float -> bool`.
            func("is_nan", vec![Ty::Float], Ty::Bool);
            func("is_inf", vec![Ty::Float], Ty::Bool);
            func("is_finite", vec![Ty::Float], Ty::Bool);
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
            // `ProcResult` is the synthetic struct seeded in `seed_stdlib_structs`.
            let proc = || Ty::Struct("ProcResult".to_string(), vec![]);
            func("run", vec![Ty::Str], Ty::result(proc()));
            func(
                "run_args",
                vec![Ty::Str, Ty::list(Ty::Str)],
                Ty::result(proc()),
            );
        }
        "std.rand" => {
            // SplitMix64 PRNG scalars (std.rand). Generic collection helpers (shuffle/choice/sample)
            // live in the pure-Chezzi std.iter and call `int` — the native seam carries only scalars.
            func("seed", vec![Ty::Int], Ty::Nil);
            func("float", vec![], Ty::Float);
            func("int", vec![Ty::Int, Ty::Int], Ty::Int);
            func("bool", vec![], Ty::Bool);
        }
        "std.net" => {
            // D6 — minimal TCP surface. `connect`/`listen` take a `"host:port"` address; the sockets
            // are non-blocking (a would-block op parks the fiber on the netpoller). The `connect`/
            // `listen`/`read`/`write`/`accept` calls are intercepted in the VM (they allocate a
            // `Socket`/`Listener` handle + register poller interest), not run as off-heap natives.
            func("connect", vec![Ty::Str], Ty::result(Ty::Socket));
            func("listen", vec![Ty::Str], Ty::result(Ty::Listener));
            // The two TCP handle TYPE names live in `sig.types` so `import Socket from std.net`
            // validates membership; the checker's `bind_import` records them into the per-module
            // `imported_net` set, and `resolve_type` then accepts the bare `Socket`/`Listener`
            // annotation only in a module that imported it (whole-module or per-name). They carry no
            // runtime module-member value — a `Socket` value comes from `connect`/`listen` and the
            // type resolves directly to `Ty::Socket` — so there are no extra `sig.functions`/`values`.
            for tn in ["Socket", "Listener"] {
                sig.types.insert(tn.to_string());
            }
        }
        "std.fs" => {
            func("list_dir", vec![Ty::Str], Ty::result(Ty::list(Ty::Str)));
            func("exists", vec![Ty::Str], Ty::Bool);
            func("is_file", vec![Ty::Str], Ty::Bool);
            func("is_dir", vec![Ty::Str], Ty::Bool);
            func("size", vec![Ty::Str], Ty::result(Ty::Int));
            func("glob", vec![Ty::Str], Ty::result(Ty::list(Ty::Str)));
            // Mutations (M8+) — all `Result[nil]`. `mkdir` is recursive (mkdir -p); `remove_dir` is
            // non-recursive (faults on a non-empty dir); `append` creates-if-absent + never truncates.
            func("mkdir", vec![Ty::Str], Ty::result(Ty::Nil));
            func("remove_file", vec![Ty::Str], Ty::result(Ty::Nil));
            func("remove_dir", vec![Ty::Str], Ty::result(Ty::Nil));
            func("rename", vec![Ty::Str, Ty::Str], Ty::result(Ty::Nil));
            func("copy", vec![Ty::Str, Ty::Str], Ty::result(Ty::Nil));
            func("append", vec![Ty::Str, Ty::Str], Ty::result(Ty::Nil));
        }
        "std.encoding" => {
            // Reversible text codecs. Every fn takes a `str` (operating on its UTF-8 bytes) and
            // returns `str` (encode) or `Result[str]` (decode — malformed input or non-UTF-8
            // decoded bytes are a recoverable fault). See `src/native/encoding.rs`.
            func("base64_encode", vec![Ty::Str], Ty::Str);
            func("base64_encode_url", vec![Ty::Str], Ty::Str);
            func("base64_decode", vec![Ty::Str], Ty::result(Ty::Str));
            func("base64_decode_url", vec![Ty::Str], Ty::result(Ty::Str));
            func("hex_encode", vec![Ty::Str], Ty::Str);
            func("hex_decode", vec![Ty::Str], Ty::result(Ty::Str));
            func("url_encode", vec![Ty::Str], Ty::Str);
            func("url_decode", vec![Ty::Str], Ty::result(Ty::Str));
            // `query_encode(params: map[str,str]) -> str` — build a `k=v&...` query string with
            // sorted keys + both sides percent-encoded (pure CPU, infallible).
            func(
                "query_encode",
                vec![Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str))],
                Ty::Str,
            );
        }
        "std.crypto" => {
            // Hand-rolled digests: hash the str's UTF-8 bytes → lowercase-hex `str` (infallible).
            func("sha256", vec![Ty::Str], Ty::Str);
            func("md5", vec![Ty::Str], Ty::Str);
        }
        "std.uuid" => {
            // `v4()` → random UUID str; `uuid_seed(n)` → deterministic reseed (golden runs).
            func("v4", vec![], Ty::Str);
            func("uuid_seed", vec![Ty::Int], Ty::Nil);
        }
        "std.concurrency" => {
            // A type-licensing-only native module: NO functions/values/struct shapes — it exports
            // ONLY the four runtime concurrency TYPE/ctor names (`Shared`/`RwShared`/`Atomic`/
            // `Executor`). They live in `sig.types` so `import Shared from std.concurrency` validates
            // membership; the checker's `bind_import` records them into the per-module
            // `imported_concurrency` set, and `resolve_type`/`infer_named_call` then accept the bare
            // ctor/type name only in a module that imported it. The ctors carry no runtime value (they
            // lower via the compiler's name→opcode dispatch), so there are no `sig.functions`/`values`.
            for tn in ["Shared", "RwShared", "Atomic", "Executor"] {
                sig.types.insert(tn.to_string());
            }
        }
        "std.time" => {
            func("now", vec![], Ty::Int);
            func("monotonic", vec![], Ty::Float);
            func("sleep_ms", vec![Ty::Int], Ty::Nil);
            func("format", vec![Ty::Int], Ty::Str);
            // `timer` is an opcode-backed builtin (NOT a callable native member): it carries NO runtime
            // value — it lowers via the compiler's name→opcode dispatch. It lives in `sig.types` ONLY so
            // `import timer from std.time` validates membership and the checker's `bind_import` records
            // it into the per-module `imported_time` set; `infer_named_call` then accepts the bare
            // `timer(ms)` call only in a module that imported it. NOT a `func()` — that would add it to
            // `sig.functions` and bind a (nonexistent) runtime value that faults at runtime.
            sig.types.insert("timer".to_string());
        }
        "std.regex" => {
            // `Match` is the synthetic struct seeded in `seed_stdlib_structs`.
            let m = || Ty::Struct("Match".to_string(), vec![]);
            func("is_match", vec![Ty::Str, Ty::Str], Ty::result(Ty::Bool));
            func("find", vec![Ty::Str, Ty::Str], Ty::result(Ty::option(m())));
            func(
                "find_all",
                vec![Ty::Str, Ty::Str],
                Ty::result(Ty::list(m())),
            );
            func(
                "replace_all",
                vec![Ty::Str, Ty::Str, Ty::Str],
                Ty::result(Ty::Str),
            );
            func(
                "split",
                vec![Ty::Str, Ty::Str],
                Ty::result(Ty::list(Ty::Str)),
            );
        }
        "std.request" => {
            // `Response` is the synthetic struct seeded in `seed_stdlib_structs`. `get`/`post`/
            // `request` carry an OPTIONAL trailing `timeout_ms: int` and are installed AFTER the
            // match (the `func` closure above can only build fixed-arity sigs); the verb wrappers
            // below are plain fixed-arity.
            let resp = || Ty::Struct("Response".to_string(), vec![]);
            func("put", vec![Ty::Str, Ty::Str], Ty::result(resp()));
            func("patch", vec![Ty::Str, Ty::Str], Ty::result(resp()));
            func("delete", vec![Ty::Str], Ty::result(resp()));
            func("head", vec![Ty::Str], Ty::result(resp()));
        }
        "std.ffi" => {
            // The C-ABI vocabulary that pairs with the opaque `ptr` handle type (`extern "lib":`).
            // `null()` is the NULL sentinel; `is_null(p)` tests it. The `ptr` *type* is builtin.
            func("null", vec![], Ty::Ptr);
            func("is_null", vec![Ty::Ptr], Ty::Bool);
            // Memory deref builtins — read/write the C-owned memory behind a `ptr` (load_*/store_*).
            // Each LOAD has a base form `(ptr) -> T` and an `_at(ptr, int) -> T` byte-offset form;
            // each STORE has `(ptr, V) -> nil` and `_at(ptr, int, V) -> nil`. Loads of every int
            // width return `int`, float widths `float`, `bool`/`ptr`/`str` their kind; stores take a
            // value of the matching kind and return `nil`. (See `src/native/ffi.rs` for the runtime;
            // both engines reach these via the engine-neutral Host/NativeFn path — parity by
            // construction.) A NULL base pointer is a recoverable runtime error, NOT a static one.
            for (n, t) in [
                ("load_int", Ty::Int),
                ("load_int8", Ty::Int),
                ("load_int16", Ty::Int),
                ("load_int32", Ty::Int),
                ("load_int64", Ty::Int),
                ("load_uint8", Ty::Int),
                ("load_uint16", Ty::Int),
                ("load_uint32", Ty::Int),
                ("load_uint64", Ty::Int),
                ("load_float", Ty::Float),
                ("load_float32", Ty::Float),
                ("load_bool", Ty::Bool),
                ("load_ptr", Ty::Ptr),
                ("load_str", Ty::Str),
            ] {
                func(n, vec![Ty::Ptr], t.clone());
                func(&format!("{n}_at"), vec![Ty::Ptr, Ty::Int], t);
            }
            for (n, v) in [
                ("store_int", Ty::Int),
                ("store_int8", Ty::Int),
                ("store_int16", Ty::Int),
                ("store_int32", Ty::Int),
                ("store_int64", Ty::Int),
                ("store_uint8", Ty::Int),
                ("store_uint16", Ty::Int),
                ("store_uint32", Ty::Int),
                ("store_uint64", Ty::Int),
                ("store_float", Ty::Float),
                ("store_float32", Ty::Float),
                ("store_bool", Ty::Bool),
                ("store_ptr", Ty::Ptr),
            ] {
                func(n, vec![Ty::Ptr, v.clone()], Ty::Nil);
                func(&format!("{n}_at"), vec![Ty::Ptr, Ty::Int, v], Ty::Nil);
            }
            // C-buffer alloc layer — libc malloc/calloc/free-backed raw buffers for C array/buffer
            // APIs (qsort/bsearch/fread-into-buffer). `alloc`/`alloc_zeroed` take a byte count and
            // return a raw `ptr`; `free` releases it (returns nil). The buffer is MANUALLY freed
            // (never auto-freed) — the idiom is `defer ffi.free(p)`. A negative size or out-of-memory
            // is a recoverable runtime error; `free(ffi.null())` is a no-op.
            func("alloc", vec![Ty::Int], Ty::Ptr);
            func("alloc_zeroed", vec![Ty::Int], Ty::Ptr);
            func("free", vec![Ty::Ptr], Ty::Nil);
            // `std.ffi` ALSO exports the eight fixed-width C-ABI integer TYPE names (Chezzi's first
            // type imports). They live in `sig.types` so `import int32 from std.ffi` validates; the
            // checker's `bind_import` records the import into `imported_ffi_types` and `resolve_type`
            // then resolves the name to `Ty::Int` only in modules that imported it.
            for tn in crate::native::ffi::TYPE_NAMES {
                sig.types.insert((*tn).to_string());
            }
            // The opaque `ptr` handle type is ALSO exported by `std.ffi` (kept out of `TYPE_NAMES`,
            // which routes a name through the ungated C-marshalling path `resolve_ctype_d`). Listing
            // it in `sig.types` lets `import ptr from std.ffi` validate; the checker licenses it into
            // `imported_ffi_types` (whole-module on `import std.ffi`, per-name on the from-import).
            sig.types.insert("ptr".to_string());
        }
        _ => {}
    }
    // Post-match: make `std.request`'s `get`/`post`/`request` accept an OPTIONAL trailing
    // `timeout_ms: int` (a positive value overrides the agent's default timeout caps for that call).
    // Done here (not in the arm) because the `func` closure above borrows `sig.functions` for the
    // whole match; once it is dropped we can install the optional-tail signatures directly.
    if name == "std.request" {
        let resp = || Ty::Struct("Response".to_string(), vec![]);
        sig.functions.insert(
            "get".into(),
            FnSig::optional_tail(vec![Ty::Str, Ty::Int], Ty::result(resp()), 1),
        );
        sig.functions.insert(
            "post".into(),
            FnSig::optional_tail(vec![Ty::Str, Ty::Str, Ty::Int], Ty::result(resp()), 1),
        );
        sig.functions.insert(
            "request".into(),
            FnSig::optional_tail(
                vec![
                    Ty::Str,
                    Ty::Str,
                    Ty::Str,
                    Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
                    Ty::Int,
                ],
                Ty::result(resp()),
                1,
            ),
        );
    }
    // Post-match: EXPORT the synthetic struct LAYOUT a native module returns (`Match`/`Response`/
    // `ProcResult`) as a module-owned type. The SAME field lists `seed_stdlib_structs` keeps globally
    // present for import-free field access live here too as the module's `struct_defs`/`types` so that
    // importing the module (whole-module `import std.regex` → bare `Match`; `from std.regex import
    // Match`; or qualified `regex.Match(...)`) is what licenses the bare type name. `type_params`/
    // `methods` are empty (these are plain data carriers); `origin` is `Builtin`. Done here, after the
    // `func` closure's borrow of `sig` is dropped, so we can write `sig.struct_defs`/`sig.types`.
    let export_struct = |sig: &mut ModuleSig, sname: &str, fields: Vec<(&str, Ty)>| {
        sig.struct_defs.insert(
            sname.to_string(),
            StructInfo {
                type_params: Vec::new(),
                fields: fields
                    .into_iter()
                    .map(|(n, t)| (n.to_string(), t))
                    .collect(),
                methods: HashMap::new(),
                origin: StructOrigin::Builtin,
                doc: None,
            },
        );
        sig.types.insert(sname.to_string());
    };
    match name {
        "std.regex" => export_struct(
            &mut sig,
            "Match",
            vec![
                ("text", Ty::Str),
                ("start", Ty::Int),
                ("end", Ty::Int),
                ("groups", Ty::list(Ty::Str)),
            ],
        ),
        "std.request" => export_struct(
            &mut sig,
            "Response",
            vec![
                ("status", Ty::Int),
                ("body", Ty::Str),
                ("headers", Ty::map(Ty::Str, Ty::Str)),
            ],
        ),
        "std.process" => export_struct(
            &mut sig,
            "ProcResult",
            vec![("stdout", Ty::Str), ("stderr", Ty::Str), ("code", Ty::Int)],
        ),
        _ => {}
    }
    // Editor hover (Tier C): attach a concise one-line doc to each native module function that has an
    // authored blurb (`MODULE_FN_DOCS`). `FnSig.doc` is excluded from `fn_sig_eq`, so this is purely
    // informational — `record_method_hover` already forwards `sig.doc` at the `module.fn` hover site,
    // so no extra threading is needed. Drift-guarded by `module_fn_docs_all_resolve`.
    if let Some((_, docs)) = MODULE_FN_DOCS.iter().find(|(m, _)| *m == name) {
        for (fname, doc) in *docs {
            if let Some(f) = sig.functions.get_mut(*fname) {
                f.doc = Some((*doc).to_string());
            }
        }
    }
    sig
}

/// Editor hover (Tier C): concise one-line `(module, [(fn, doc)])` blurbs for native stdlib module
/// functions, paraphrased from `docs/stdlib.md §4`. Applied to `FnSig.doc` in `native_module_sig`
/// and surfaced verbatim at the `module.fn` hover site. Drift-guarded by `module_fn_docs_all_resolve`
/// (every listed fn must exist). Coverage is `std.math` / `std.io` / `std.os` for v1; other native
/// modules are a follow-up (noted in PROGRESS.md) — their fns simply hover doc-less, as before.
#[allow(clippy::type_complexity)]
const MODULE_FN_DOCS: &[(&str, &[(&str, &str)])] = &[
    (
        "std.math",
        &[
            (
                "abs",
                "absolute value (numeric-polymorphic: int→int, float→float)",
            ),
            ("floor", "round down to the nearest integer (toward -inf)"),
            ("ceil", "round up to the nearest integer (toward +inf)"),
            (
                "round",
                "round to the nearest integer (half away from zero)",
            ),
            ("pow", "pow(base, exp): base raised to exp"),
            ("sqrt", "square root (NaN for a negative argument)"),
            ("sin", "sine of an angle in radians"),
            ("cos", "cosine of an angle in radians"),
            ("tan", "tangent of an angle in radians"),
            ("asin", "arcsine in radians (NaN outside [-1, 1])"),
            ("acos", "arccosine in radians (NaN outside [-1, 1])"),
            ("atan", "arctangent in radians"),
            (
                "atan2",
                "atan2(y, x): angle of the vector (x, y) in radians",
            ),
            ("exp", "e raised to the power x"),
            ("ln", "natural (base-e) logarithm (-inf at 0, NaN below)"),
            ("log2", "base-2 logarithm"),
            ("log10", "base-10 logarithm"),
            (
                "log",
                "log(value, base): logarithm of value in the given base",
            ),
            ("is_nan", "true if x is NaN"),
            ("is_inf", "true if x is +inf or -inf"),
            ("is_finite", "true if x is neither NaN nor infinite"),
        ],
    ),
    (
        "std.io",
        &[
            ("print", "write a line to stdout (with a trailing newline)"),
            ("eprint", "write a line to stderr (with a trailing newline)"),
            (
                "read_line",
                "read one line from stdin, newline stripped (None at EOF)",
            ),
            ("read_file", "read a whole file as text (Result)"),
            (
                "write_file",
                "write/overwrite a file with the given text (Result)",
            ),
        ],
    ),
    (
        "std.os",
        &[
            (
                "args",
                "program arguments (positionals after the script path)",
            ),
            ("env", "look up an environment variable (None if unset)"),
            ("getcwd", "the current working directory (Result)"),
            (
                "exit",
                "halt the program with an exit code (does NOT run defers)",
            ),
        ],
    ),
];

struct Checker {
    errors: Vec<CheckError>,
    scopes: Vec<HashMap<String, Ty>>,
    /// Per-scope set of names bound as `for`-loop variables. Mirrors `scopes` index-for-index (a
    /// loop var is immutable — rebound fresh each iteration — so assigning to it is rejected; this
    /// sidesteps a VM/interp divergence where the VM's counter slot IS the loop var).
    loop_vars: Vec<std::collections::HashSet<String>>,
    /// Per-scope set of names declared as `ref T` bindings/params (mirrors `scopes` index-for-index).
    /// A `ref T` and an explicit first-class `Ref[T]` are the same `Ty::Struct("Ref", _)` after
    /// lowering, so this is the ONLY way to tell them apart for transparency: a diagnostic about a
    /// `ref` binding renders `ref T` (via `ref_display`), but one about an explicit `Ref[T]` keeps
    /// `Ref[T]` (the user wrote `Ref`). Charge-5 transparency without lying in the other direction.
    ref_decls: Vec<std::collections::HashSet<String>>,
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
    /// enum name (runtime key) → its methods (`fn area(self) …`), name-keyed exactly like
    /// `StructInfo.methods`. Resolves `enumval.method(args)` and protocol satisfaction for enums.
    enum_methods: HashMap<String, HashMap<String, FnSig>>,
    /// Keyed by `(enum_name, variant_name)`: variants are scoped under their enum, so two enums may
    /// declare the same variant name. Resolution always carries the enum (a qualified `Enum.Variant`
    /// or, in a pattern slot, the scrutinee's enum) — there is no bare-name lookup.
    variants: HashMap<(String, String), VariantInfo>,
    /// variant name → the enum(s) that declare it. A pure-diagnostic reverse index: lets a bare
    /// variant use (illegal now) report "write it qualified as `Enum.Variant`" and keeps a bare
    /// known-variant from silently becoming a pattern binding (the bare→binding trap).
    variant_owners: HashMap<String, Vec<String>>,
    struct_names: std::collections::HashSet<String>,
    enum_names: std::collections::HashSet<String>,
    /// newtype name (BARE, current-module visibility) — mirrors `enum_names`/`struct_names`.
    newtype_names: std::collections::HashSet<String>,
    /// newtype runtime key (`bare_key`) → its (underlying `Ty`, name-keyed methods). Mirrors
    /// `enum_methods` + the layout tables: drives construct, cast-unwrap, same-type operators,
    /// method dispatch, and protocol satisfaction. NOT cleared per-module (graph-wide, like `enums`).
    newtype_defs: HashMap<String, (Ty, HashMap<String, FnSig>)>,
    /// newtype runtime key → its generic type parameters (empty for a scalar newtype). Mirrors
    /// `enum_type_params`: builds the substitution from a `Ty::NewType(key, args)`'s args onto the
    /// underlying type + method signatures (which may name the params). A non-empty entry marks the
    /// newtype as generic — gating off the scalar-newtype native operator auto-flow (methods-only).
    newtype_type_params: HashMap<String, Vec<TypeParam>>,
    /// Transparent type aliases (`type UserId = int`): name → the aliased AST type, resolved on
    /// demand in `resolve_type`. `alias_resolving` is the active resolution stack (cycle guard).
    aliases: HashMap<String, Type>,
    alias_resolving: Vec<String>,
    /// Alias names whose body is a fixed-width FFI type name (`type Len = int32`) that the alias's
    /// DEFINING module imported per-name from `std.ffi`. Program-global like `aliases` (NOT cleared
    /// in `begin_module`), and the precise opt-in for the width-import gate: a width name resolves
    /// through an alias only if the alias is in this set — i.e. its defining module imported the
    /// width. This closes the gate hole where any `type Len = int32` (even with no module importing
    /// int32 anywhere) laundered the bare width name past the per-module import requirement.
    ffi_alias_ok: std::collections::HashSet<String>,
    /// Declared return type of the function body currently being checked (`Nil` at top level).
    current_ret: Ty,
    /// `Some(T)` while checking a generator function body whose declared return is `Iterator[T]` —
    /// the element type each `yield` must produce. `None` outside a generator, so a stray `yield`
    /// is diagnosed. Saved/restored across nested fn/closure boundaries like `current_ret`.
    yield_ty: Option<Ty>,
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
    /// First segment of each imported DOTTED module path (`std` from `import std.concurrency`) →
    /// `(dotted_path, bound_name)` of the FIRST import that introduced it. Used ONLY to give a
    /// targeted two-level-path hint for a multi-level mistake like `std.concurrency.Shared(0)` (the
    /// head `std` is a path PREFIX, not a bound name) instead of the misleading "unknown name 'std'".
    /// Never a bound value/type itself, so it cannot mask a genuine typo.
    import_path_heads: HashMap<String, (String, String)>,
    /// The first TWO segments of each imported module's dotted path → its bound name (last segment /
    /// alias), or `None` when two imports share those two segments (ambiguous). Keyed on two segments
    /// because a too-deep-path mistake fires from `infer_field` with only the head + the NEXT segment
    /// visible (the call/field already consumed the rest): `(std, net)` → `net`, and
    /// `(std, concurrency)` → `collection` for `import std.concurrency.collection`. Correct for 2- and
    /// 3+-level imports and sibling collisions alike — unlike `import_path_heads` (head-only,
    /// first-wins), which named the wrong sibling / the wrong segment.
    module_prefix2: HashMap<(String, String), Option<(String, String)>>,
    /// Every name an `import`/`import … from` binds in the *current* module → the span of its first
    /// import, across ALL import namespaces (values, functions, modules, type-names). Used to reject
    /// a SECOND import binding the same name (`import f from lib` + `import f from lib2`, or
    /// value-then-fn `import v` + `import v`), which the separate namespace tables otherwise let pass
    /// last-wins (and which makes the checker and runtime disagree on the binding). Per-module:
    /// cleared in `begin_module`.
    import_binds: HashMap<String, Span>,
    /// Type names `from`-imported into the *current* module that resolve to an aliased `Ty`
    /// (`import Len from m` where `m` declares `type Len = int32`). Resolved in the DEFINING module's
    /// scope and injected here so `resolve_type` returns the right underlying type. Per-module.
    imported_alias_tys: HashMap<String, Ty>,
    /// Parallel to [`Self::imported_alias_tys`] but carrying the alias's width-bearing [`CType`]
    /// (computed in the DEFINING module's scope), so a `from`-imported FFI-width alias resolves to
    /// its true width at an extern boundary in THIS module. `Ty` can't carry it (it collapses all
    /// FFI widths to `Ty::Int`), so this is the channel a named-import hop's real width travels.
    /// Per-module: cleared in `begin_module`.
    imported_alias_ctypes: HashMap<String, Option<CType>>,
    /// Extern-fn resolved C signatures harvested during checking, keyed by `(graph module index, fn
    /// name)`. Only populated when [`resolve_extern_signatures`] drives the pass (the error-gate
    /// `check_graph` leaves it empty). The current module's graph index is set per `check_module`.
    extern_sigs: ExternTable,
    /// AST field types of every user struct in the graph, keyed by IDENTITY KEY, gathered once for
    /// [`resolve_ctype`]: a by-value extern struct's `CType::Struct` is built from these (preserving
    /// each field's real C width, which `StructInfo`'s `Ty` fields collapse to `Ty::Int`). Only
    /// populated when [`resolve_extern_signatures`] drives the pass.
    struct_field_asts: HashMap<String, Vec<(String, Type)>>,
    /// The FULLY-RESOLVED, width-bearing by-value [`CType::Struct`] of every user struct in the graph,
    /// keyed by IDENTITY KEY, each computed ONCE in the struct's OWN DEFINING module's import/alias
    /// scope (extending the [`AliasSig::ctype`] precedent to structs). This is the structural fix to
    /// the FFI qualified/aliased-struct-field saga: an importer's extern returning `mod.DivT` reads
    /// this cache VERBATIM, so a field typed via the defining module's local alias (`type Half =
    /// int32`) keeps its true width — the importer's scope (where `Half` is invisible / colliding) is
    /// never consulted. `None` entry = a field is not a scalar leaf (non-marshallable; the marshal
    /// gate is the actual error). Populated deps-first, so a cross-module / qualified / nested struct
    /// is always cached before an importer needs it. NOT cleared per-module (graph-wide). Only built
    /// when [`resolve_extern_signatures`] drives the pass.
    struct_ctypes: HashMap<String, Option<CType>>,
    /// The CURRENT module's graph index (set in `check_module` when harvesting extern sigs), so each
    /// extern fn's resolved C signature is keyed under the SAME index the backends derive. `None`
    /// for a lone `check`.
    extern_module_idx: Option<usize>,
    /// True only while resolving an `extern "lib":` fn's param/return signature. `owned_str` is a
    /// RETURN-ONLY C marshalling form that collapses to `str`; it is legal ONLY inside an extern
    /// signature. This flag licenses `resolve_type`'s `owned_str` arm there and rejects a bare
    /// `owned_str` used as a general type annotation (where it would silently collapse to `str`
    /// with no `import`). Set/reset around the extern fn loop in `hoist_types`.
    in_extern_sig: bool,
    /// Reverse index built once across the whole graph: a user type name → the modules (in graph
    /// load order, deps-first) that declare it. Drives the "import it from <module>" hint when a bare
    /// type name is used without importing its declaring module. NOT cleared per-module.
    types_by_name: HashMap<String, Vec<String>>,
    /// `from`-imported names that are numeric-polymorphic native fns (`abs`/`min`/`max`), so a bare
    /// call resolves their result type by argument type instead of the float-only `FnSig` (gap #12).
    imported_poly: std::collections::HashSet<String>,
    /// Fixed-width C-ABI integer TYPE names (`int8`..`uint64`) imported into the *current* module from
    /// `std.ffi` (`import int32 from std.ffi`). These are NOT callable values — they only gate
    /// `resolve_type`, which maps a width name to `Ty::Int` iff it's in this set (else an unknown-type
    /// error). Per-module: cleared in `begin_module` so module B can't use a name module A imported.
    imported_ffi_types: std::collections::HashSet<String>,
    /// The four runtime concurrency ctor/TYPE names (`Shared`/`RwShared`/`Atomic`/`Executor`) imported
    /// into the *current* module from `std.concurrency` (whole-module `import std.concurrency` licenses
    /// all four; selective `import Shared from std.concurrency` licenses just the named ones). Like
    /// `imported_ffi_types`, these are NOT callable values — they only gate `resolve_type` /
    /// `infer_named_call`, which accept the bare name iff it's in this set (else an unknown-type error
    /// with the `import std.concurrency` hint). Per-module: cleared in `begin_module`. std/* modules
    /// are EXEMPT (they may use the four bare) via `current_module_is_stdlib`.
    imported_concurrency: std::collections::HashSet<String>,
    /// The opcode-backed `timer` builtin licensed into the *current* module from `std.time` (whole-
    /// module `import std.time` OR selective `import timer from std.time`). Like `imported_concurrency`,
    /// this is NOT a callable value — it only gates `infer_named_call`, which accepts the bare
    /// `timer(ms)` call iff `"timer"` is in this set (else an unknown-function error with the
    /// `import std.time` hint). Per-module: cleared in `begin_module`. std/* modules are EXEMPT (they
    /// may call `timer` bare) via `current_module_is_stdlib`.
    imported_time: std::collections::HashSet<String>,
    /// The std.net TCP handle TYPE names (`Socket`/`Listener`) licensed into the *current* module from
    /// `std.net` (whole-module `import std.net` licenses both; selective `import Socket from std.net`
    /// licenses just the named ones). Like `imported_concurrency`, these are NOT callable values — they
    /// only gate `resolve_type`, which accepts the bare `Socket`/`Listener` annotation iff it's in this
    /// set (else an unknown-type error with the `import std.net` hint; the ctors are `connect`/`listen`,
    /// already real native fns). Per-module: cleared in `begin_module`. std/* modules are EXEMPT (they
    /// may use the two bare) via `current_module_is_stdlib`.
    imported_net: std::collections::HashSet<String>,
    /// The bare type names that an `import` licensed into the *current* module as a
    /// `StructOrigin::Builtin` struct layout (the std struct-modeled natives — `Ref`/`Match`/
    /// `Response`/`ProcResult` and every other import-gated std struct like `Token`/`Heap`/`Deque`).
    /// Populated at the two struct-import insert sites (whole-module `import std.regex` and selective
    /// `import Match from std.regex`), keyed on `info.origin == StructOrigin::Builtin`. A same-named
    /// user `struct X` decl IN A MODULE THAT IMPORTED `X` is then rejected as `reserved (builtin)` —
    /// closing the soundness hole where the user layout overwrote the Builtin seed yet the runtime
    /// still returned/constructed the native shape (a check-clean program that trapped at runtime).
    /// NOT reusable from `self.structs` membership: `Match`/`Response`/`ProcResult` are seeded with
    /// Builtin origin GLOBALLY even without any import, and THAT seeded-but-not-imported state is the
    /// legal bare-decl case — only the import EVENT licenses the name, so it's tracked separately.
    /// Per-module: cleared in `begin_module`.
    imported_builtin_types: std::collections::HashSet<String>,
    /// Label of the module currently being checked (`None` = entry); prefixes its error messages.
    current_module_label: Option<String>,
    /// How many enclosing `for`/`while` loops we're inside *within the current function body*.
    /// Reset to 0 when descending into a (nested) function or closure body so an inner `break`
    /// can't escape into an outer loop. `> 0` ⇒ `break`/`continue` are legal here.
    loop_depth: usize,
    /// True while inferring a generic ctor/call/method's args in the unification PREPASS
    /// ([`Checker::infer_generic_arg_tys`]). In that pass an unannotated closure param must stay
    /// `Ty::Unknown` (so unification is driven by the other args / substituted slot type, then the
    /// per-arg [`Checker::check_generic_arg`] re-infers it in checking-mode) — the free-body scan
    /// (sources #2/#3) must NOT pin it here, or a body use like `x.upper()` would force `x: str` and
    /// corrupt unification (e.g. `Mapped(int_iter, fn(x): x.upper())`).
    generic_arg_prepass: bool,
    /// Expected-type HINT (checking-mode) for the OUTERMOST generic ctor / generic fn-call currently
    /// being inferred — pure transport from the three annotation sites (a `let`-binding's declared
    /// type, a `return`'s declared return type, a call argument's declared parameter type) into
    /// [`Checker::infer_call`]. `infer_call` `take()`s it FIRST (before inferring any argument), so a
    /// nested arg call sees `None` and the hint never leaks past the one call it was set for. Consumed
    /// to pre-seed the type-param substitution (via `unify` against the ctor/call's declared return
    /// SHAPE) AFTER arg-unification but BEFORE `report_uninferable_closure_params`, so it fills ONLY
    /// the params the arguments left free — turbofish > args > annotation. This breaks the
    /// `Heap([], fn(x, y): x < y)` deadlock: the annotation pins `T`, which then pins the closure
    /// params. Mirrors the existing closure-vs-fn-annotation checking-mode (`infer_arg`).
    expected_hint: Option<Ty>,
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
    /// Module-scoped types: `(declaring module id, bare type name) → runtime key`. Bare in the
    /// no-collision case; `<dotted>::Name` on a genuine cross-module clash. Built once in
    /// [`check_graph`] with the IDENTICAL graph-order, first-declarer-wins-bare rule as the compiler's
    /// `assign_type_keys`, so the checker and compiler agree on every type's runtime key (and thus on
    /// which declarer is disambiguated). A name absent from this map keys bare. NOT cleared per-module.
    type_keys: HashMap<(ModuleId, String), String>,
    /// The id of the module currently being checked (`None` for a lone `check`), so local type
    /// declarations + bare annotations resolve to THIS module's runtime key. Set per `check_module`.
    current_module_id: Option<ModuleId>,
    /// The CURRENT module's bare-resolvable type names → their runtime key: locally declared +
    /// `from`-imported + std whole-module. Mirrors the compiler's `bare_types`; resolves a bare-written
    /// type name (annotation / constructor) to the module-scoped runtime key. Rebuilt per module.
    bare_types: HashMap<String, String>,
    /// EDITOR HOVER (LSP): when `Some`, a single-position probe — the 1-based `(line, col)` of the
    /// token under the cursor. `None` for an ordinary check (zero overhead). See [`hover_type`].
    hover_probe: Option<(usize, usize)>,
    /// The entry module the probe applies to; recording is gated on `current_module_id == hover_entry`
    /// so a same-named symbol in an imported dependency can't shadow the entry-buffer hit.
    hover_entry: Option<ModuleId>,
    /// First probe hit: the inferred type + its classification + the symbol's doc-comment (if any).
    /// First-write-wins (children infer before parents; only leaves/bindings record), so the smallest
    /// covering symbol's type is kept.
    hover_result: Option<(Ty, HoverKind, Option<String>)>,
    /// EDITOR HOVER: doc-comment for the entry module's non-fn type decls (struct/enum/protocol/
    /// newtype/alias) + top-level lets, keyed by simple name. Populated per module from the AST in
    /// `collect_docs`; consulted by the hover Ident/type-name sites. Keyed by simple name is safe
    /// because hover only fires in the entry module (gated by `current_module_id == hover_entry`),
    /// where this table holds exactly that module's decls (mirrors how `functions` is entry-scoped).
    /// Free fns / methods carry their doc on `FnSig::doc` instead. Runtime-inert (LSP only).
    name_docs: HashMap<String, String>,
}

impl Checker {
    fn new() -> Self {
        let mut c = Checker {
            errors: Vec::new(),
            scopes: Vec::new(),
            ref_decls: Vec::new(),
            loop_vars: Vec::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            protocols: prebuilt_protocols(),
            type_params: HashMap::new(),
            enums: HashMap::new(),
            enum_type_params: HashMap::new(),
            enum_methods: HashMap::new(),
            variants: HashMap::new(),
            variant_owners: HashMap::new(),
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            newtype_names: std::collections::HashSet::new(),
            newtype_defs: HashMap::new(),
            newtype_type_params: HashMap::new(),
            aliases: HashMap::new(),
            alias_resolving: Vec::new(),
            ffi_alias_ok: std::collections::HashSet::new(),
            current_ret: Ty::Nil,
            yield_ty: None,
            recover_depth: 0,
            generic_arg_prepass: false,
            expected_hint: None,
            inferring_ret: false,
            collected_rets: Vec::new(),
            module_sigs: HashMap::new(),
            imported_modules: HashMap::new(),
            import_path_heads: HashMap::new(),
            module_prefix2: HashMap::new(),
            import_binds: HashMap::new(),
            imported_alias_tys: HashMap::new(),
            imported_alias_ctypes: HashMap::new(),
            extern_sigs: ExternTable::new(),
            extern_module_idx: None,
            in_extern_sig: false,
            struct_field_asts: HashMap::new(),
            struct_ctypes: HashMap::new(),
            types_by_name: HashMap::new(),
            imported_poly: std::collections::HashSet::new(),
            imported_ffi_types: std::collections::HashSet::new(),
            imported_concurrency: std::collections::HashSet::new(),
            imported_time: std::collections::HashSet::new(),
            imported_net: std::collections::HashSet::new(),
            imported_builtin_types: std::collections::HashSet::new(),
            current_module_label: None,
            loop_depth: 0,
            capture_floors: Vec::new(),
            defer_floors: Vec::new(),
            current_module_is_stdlib: false,
            type_keys: HashMap::new(),
            current_module_id: None,
            bare_types: HashMap::new(),
            hover_probe: None,
            hover_entry: None,
            hover_result: None,
            name_docs: HashMap::new(),
        };
        c.seed_stdlib_structs();
        c
    }

    /// The runtime key for a bare-written type name in the CURRENT module: its `bare_types` entry when
    /// bare-visible (local / `from`-imported / std), else the name itself — which covers the reserved
    /// built-ins (`Result`/`Option`/`Ref`/…) and a not-bare-visible name (resolution then misses, as
    /// before). Mirrors the compiler's `enum_bare_key` (shared for structs + enums here).
    fn bare_key(&self, name: &str) -> String {
        self.bare_types
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// The module-scoped runtime key for a type `name` declared in module `mid` (bare unless a genuine
    /// cross-module clash disambiguated it in [`check_graph`]). Mirrors the compiler's `type_key`.
    fn type_key(&self, mid: &ModuleId, name: &str) -> String {
        self.type_keys
            .get(&(mid.clone(), name.to_string()))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// Register the synthetic struct shapes that native std modules return (M9): `Match`
    /// (`std.regex`), `Response` (`std.request`), and `ProcResult` (`std.process`). They have no
    /// AST, so their field layouts are seeded here; `infer_field` then types `m.text`, `resp.status`,
    /// `r.code`, etc. — IMPORT-FREE, since the layout lookup is keyed by the name the native return's
    /// `Ty::Struct(...)` carries. These are MODULE-OWNED, NOT program-global: only the LAYOUT is
    /// seeded (so field access on a return works without an import); the BARE TYPE NAME for
    /// annotation/construction is licensed ONLY by importing the owning module (whose
    /// `native_module_sig` exports the same shape via `struct_defs`). So a user `struct Response`
    /// (without `import std.request`) is their OWN type — the `Builtin`-origin seed is overwritten.
    fn seed_stdlib_structs(&mut self) {
        let mk = |fields: Vec<(&str, Ty)>| StructInfo {
            type_params: Vec::new(),
            fields: fields
                .into_iter()
                .map(|(n, t)| (n.to_string(), t))
                .collect(),
            methods: HashMap::new(),
            origin: StructOrigin::Builtin,
            doc: None,
        };
        // The LAYOUT stays globally present (so field access on a native return — `regex.find(...)
        // .text`, `request.get(...).status`, `process.run(...).code` — resolves import-free via
        // `infer_field`'s `self.structs[sname]` lookup keyed by the name the native return's
        // `Ty::Struct("Match",...)` carries). The BARE-NAME reservation (`struct_names`) is NOT
        // seeded: the bare type name (`m: Match` / `Match(...)`) is licensed ONLY by importing the
        // owning module (`import std.regex` / `from std.regex import Match`), whose `struct_defs`
        // flow into `struct_names`/`bare_types` on import. This frees the names for a user `struct
        // Response` — the always-present `Builtin`-origin seed is overwritten by a user `User`-origin
        // declaration (see the already-defined gate in the hoist pass).
        self.structs.insert(
            "Match".into(),
            mk(vec![
                ("text", Ty::Str),
                ("start", Ty::Int),
                ("end", Ty::Int),
                ("groups", Ty::list(Ty::Str)),
            ]),
        );
        self.structs.insert(
            "Response".into(),
            mk(vec![
                ("status", Ty::Int),
                ("body", Ty::Str),
                ("headers", Ty::map(Ty::Str, Ty::Str)),
            ]),
        );
        self.structs.insert(
            "ProcResult".into(),
            mk(vec![
                ("stdout", Ty::Str),
                ("stderr", Ty::Str),
                ("code", Ty::Int),
            ]),
        );
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
        self.name_docs.clear();
        self.type_params.clear();
        self.imported_modules.clear();
        self.import_path_heads.clear();
        self.module_prefix2.clear();
        self.import_binds.clear();
        self.imported_alias_tys.clear();
        self.imported_alias_ctypes.clear();
        self.imported_poly.clear();
        self.imported_ffi_types.clear();
        self.imported_concurrency.clear();
        self.imported_time.clear();
        self.imported_net.clear();
        self.imported_builtin_types.clear();
        // Types are MODULE-SCOPED: a type declared in module A is NOT visible bare in module B (it
        // must be imported). Clear the per-module type tables so a prior module's types don't leak.
        // The synthetic stdlib structs (`Match`/`Response`) and pre-seeded protocols are global, so
        // they're re-seeded after the clear.
        self.structs.clear();
        self.enums.clear();
        self.enum_type_params.clear();
        self.enum_methods.clear();
        self.variants.clear();
        self.variant_owners.clear();
        self.struct_names.clear();
        self.enum_names.clear();
        self.newtype_names.clear();
        self.newtype_defs.clear();
        self.newtype_type_params.clear();
        self.aliases.clear();
        self.bare_types.clear();
        self.seed_stdlib_structs();
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
        id: Option<&ModuleId>,
        imports: &[ResolvedImport],
    ) -> ModuleSig {
        self.push_scope();
        // Module-scoped types: record THIS module's id and seed its locally-declared type names into
        // `bare_types` under their runtime key (bare unless disambiguated), so a bare annotation /
        // constructor resolves to the same key the layout is registered under. `bind_import` then adds
        // `from`-imported + std-whole-module type names. Done before `bind_import`/`hoist` so type
        // resolution during hoisting sees the keys.
        self.current_module_id = id.cloned();
        if let Some(mid) = id {
            for s in stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::NewType { name, .. }
                | StmtKind::TypeAlias { name, .. } = &s.kind
                {
                    let key = self.type_key(mid, name);
                    self.bare_types.insert(name.clone(), key);
                }
            }
        }
        for imp in imports {
            self.bind_import(imp);
        }
        self.collect_names(stmts);
        self.collect_docs(stmts);
        self.hoist(stmts);
        // SINGLE-RESOLVER FFI fix: cache every struct declared in THIS module under its identity key,
        // its by-value `CType::Struct` computed HERE — in this (the DEFINING) module's import/alias
        // scope (extends the `AliasSig::ctype` precedent to structs). Done only when harvesting
        // externs, after `hoist` (all of this module's aliases/`from`-imports are live) and BEFORE the
        // check_stmt loop (so a same-module extern harvested in the loop reads the cache). Modules are
        // checked deps-first, so a downstream importer's extern returning `mod.Struct` reads this
        // cached, defining-scope CType verbatim — its own (colliding/invisible) scope is never used.
        if self.extern_module_idx.is_some() {
            self.populate_struct_ctypes(stmts, id);
        }
        self.infer_returns(stmts);
        for stmt in stmts {
            self.check_stmt(stmt);
        }
        self.check_spawn_global_mutation(stmts);
        let sig = self.capture_sig(stmts);
        self.pop_scope();
        sig
    }

    /// Record that `bind` is bound by an import at `span`. Returns `true` if this name was ALREADY
    /// bound by an earlier import in this module — the caller then emits the duplicate-import error
    /// and skips re-binding. Spans across ALL import namespaces (values/functions/modules/types) so a
    /// value-then-fn or fn-then-fn collision is caught (which the separate tables otherwise miss).
    fn note_import_bind(&mut self, bind: &str, span: Span) -> bool {
        if self.import_binds.contains_key(bind) {
            self.error(span, format!("'{bind}' is already imported"));
            return true;
        }
        self.import_binds.insert(bind.to_string(), span);
        false
    }

    /// Bind an import into the current module: a whole-module import becomes a `Ty::Module` name;
    /// a `from` import injects each member (function/value) into scope, validating it exists.
    fn bind_import(&mut self, imp: &ResolvedImport) {
        match &imp.import {
            Import::Module {
                path,
                alias,
                name_span,
            } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                if self.note_import_bind(&name, imp.span) {
                    return;
                }
                // Editor hover (decl-site): record the bound module name's type at the bound-name
                // token (`math` / the `as` alias). Probe-gated no-op off the probe / outside entry.
                if self.hover_probe.is_some() {
                    self.hover_record_at(
                        *name_span,
                        &Ty::Module(name.clone()),
                        HoverKind::Other,
                        None,
                    );
                }
                self.imported_modules
                    .insert(name.clone(), imp.target.clone());
                // Record the first TWO path segments → bound name (`(std,net)` → `net`,
                // `(std,concurrency)` → `collection` for `import std.concurrency.collection`) so a
                // too-deep-path mistake — which fires with only head + next segment visible — names
                // the EXACT module's bound name. `None` marks a genuine ambiguity (two imports sharing
                // both segments) so we fall back to a generic hint rather than guess.
                if path.len() >= 2 {
                    let key = (path[0].clone(), path[1].clone());
                    match self.module_prefix2.get(&key) {
                        Some(Some((_, prev))) if prev != &name => {
                            self.module_prefix2.insert(key, None);
                        }
                        None => {
                            self.module_prefix2
                                .insert(key, Some((path.join("."), name.clone())));
                        }
                        _ => {}
                    }
                }
                // Record the path HEAD (`std` of `import std.concurrency`) so a multi-level mistake
                // (`std.concurrency.Shared(...)`) gets the two-level hint, not "unknown name 'std'".
                // Only for a genuine dotted path whose head isn't itself the bound name (first wins).
                if path.len() >= 2 {
                    let head = path[0].clone();
                    if head != name {
                        self.import_path_heads
                            .entry(head)
                            .or_insert_with(|| (path.join("."), name.clone()));
                    }
                }
                self.declare(&name, Ty::Module(name.clone()));
                // Register the imported module's struct/enum LAYOUTS into the per-module shape tables
                // (so `geo.Point(1,2).x` and `geo`'s enum methods resolve), but NOT into the bare
                // *_names sets — a bare `Point` must still error. The bare-name gate (`struct_names`/
                // `enum_names`) stays cleared; `infer_field`/`infer_method_call` consult `self.structs`/
                // `self.enums` for the layout, which is what these provide. A same-named layout already
                // present (a local decl or another import) is NOT overwritten (first/local wins; the
                // compiler disambiguates any genuine runtime collision).
                //
                // EXCEPTION — a STDLIB module (`import std.ref`/`std.iter`/…) ALSO exposes its types
                // BARE (`struct_names`/`enum_names`), like the reserved/native surface (`Ref`/`Result`).
                // The `ref T` syntax lowers to a bare `Ref[T]` annotation that has no module prefix, so
                // `Ref` must resolve bare wherever `std.ref` is imported. This keeps the std type
                // surface globally usable on import, as before, without leaking USER module types.
                let is_std = path.first().map(String::as_str) == Some("std");
                if let Some(sig) = self.module_sigs.get(&imp.target).cloned() {
                    for (sname, info) in &sig.struct_defs {
                        // Register the LAYOUT under the DECLARING module's runtime key (bare unless a
                        // genuine cross-module clash). Register BOTH colliding layouts (no first-wins),
                        // so a value of either — whose `Ty` carries the matching key — resolves its
                        // own fields/methods. A std module ALSO exposes its types bare.
                        let key = self.type_key(&imp.target, sname);
                        self.structs.insert(key.clone(), info.clone());
                        if is_std {
                            self.struct_names.insert(sname.clone());
                            self.bare_types.entry(sname.clone()).or_insert(key);
                            // A Builtin-origin std struct (Ref/Match/Response/ProcResult, Token/Heap/
                            // …) licensed bare by THIS import: record the name so a same-named user
                            // `struct` decl below is a clean `reserved (builtin)` error, not an
                            // accept-then-trap (the user layout would shadow the native shape).
                            if info.origin == StructOrigin::Builtin {
                                self.imported_builtin_types.insert(sname.clone());
                            }
                        }
                    }
                    for (ename, edef) in &sig.enum_defs {
                        let key = self.type_key(&imp.target, ename);
                        self.enums.insert(key.clone(), edef.variant_names.clone());
                        self.enum_type_params
                            .insert(key.clone(), edef.type_params.clone());
                        self.enum_methods.insert(key.clone(), edef.methods.clone());
                        if is_std {
                            self.enum_names.insert(ename.clone());
                            self.bare_types.entry(ename.clone()).or_insert(key.clone());
                        }
                        for (vname, vinfo) in edef.variant_names.iter().zip(&edef.variants) {
                            let mut vi = vinfo.clone();
                            vi.enum_name = key.clone();
                            self.variants.insert((key.clone(), vname.clone()), vi);
                            if is_std {
                                self.variant_owners
                                    .entry(vname.clone())
                                    .or_default()
                                    .push(ename.clone());
                            }
                        }
                    }
                    for (ntname, ntdef) in &sig.newtype_defs {
                        // Register the newtype's underlying + methods under the declaring module's
                        // runtime key (so a value whose `Ty::NewType(key)` matches resolves its
                        // methods/construct/cast). A std module also exposes it bare.
                        let key = self.type_key(&imp.target, ntname);
                        self.newtype_defs.insert(
                            key.clone(),
                            (ntdef.underlying.clone(), ntdef.methods.clone()),
                        );
                        self.newtype_type_params
                            .insert(key.clone(), ntdef.type_params.clone());
                        if is_std {
                            self.newtype_names.insert(ntname.clone());
                            self.bare_types.entry(ntname.clone()).or_insert(key);
                        }
                    }
                }
                // A whole-module `import std.ffi` licenses the bare opaque `ptr` type (extern blocks
                // use it pervasively, so whole-module licensing is the ergonomic default — UNLIKE the
                // width types int8..uint64, which stay per-name-only). Keyed on the EXACT path, NOT
                // `is_std`, so `import std.ref`/`std.iter`/… do NOT license `ptr`.
                if path.as_slice() == ["std".to_string(), "ffi".to_string()] {
                    self.imported_ffi_types.insert("ptr".to_string());
                }
                // A whole-module `import std.concurrency` licenses ALL FOUR runtime concurrency ctor/
                // TYPE names (the ergonomic default). Keyed on the EXACT len-2 path, so the real file
                // submodule `import std.concurrency.collection` (len-3) does NOT license them.
                if path.as_slice() == ["std".to_string(), "concurrency".to_string()] {
                    for tn in ["Shared", "RwShared", "Atomic", "Executor"] {
                        self.imported_concurrency.insert(tn.to_string());
                    }
                }
                // A whole-module `import std.time` licenses the opcode-backed `timer(ms)` builtin.
                // Keyed on the EXACT len-2 path. std.time is a REAL native module, so `Import::Plain`
                // already binds the module object (and its now/monotonic/sleep_ms/format members); we
                // only ADD the `timer` licensing insert here.
                if path.as_slice() == ["std".to_string(), "time".to_string()] {
                    self.imported_time.insert("timer".to_string());
                }
                // A whole-module `import std.net` licenses BOTH bare TCP handle TYPE names. Keyed on
                // the EXACT len-2 path. std.net is a REAL native module, so `Import::Plain` already
                // binds the module object (connect/listen members); we only ADD the type licensing.
                if path.as_slice() == ["std".to_string(), "net".to_string()] {
                    for tn in ["Socket", "Listener"] {
                        self.imported_net.insert(tn.to_string());
                    }
                }
            }
            Import::From {
                path,
                names,
                name_spans,
            } => {
                let sig = self
                    .module_sigs
                    .get(&imp.target)
                    .cloned()
                    .unwrap_or_default();
                // `name_spans` is parallel to `names`; zip truncates safely if they ever diverge (a
                // bound name's hover is dropped, never a panic). Diagnostic-only — the per-name span
                // anchors the decl-site hover at the bound import name.
                for ((member, alias), name_span) in names.iter().zip(name_spans.iter()) {
                    let bind = alias.as_ref().unwrap_or(member);
                    // Reject a second import binding the same name (across ALL namespaces), but only
                    // when the member actually exists — a missing member is its own error below, and
                    // shouldn't also claim the name. The bind-name (alias wins) is the collision key,
                    // so `import x as y` + `import z as y` collides while distinct names don't.
                    let member_exists = sig.functions.contains_key(member)
                        || sig.values.contains_key(member)
                        || sig.types.contains(member);
                    if member_exists && self.note_import_bind(bind, imp.span) {
                        continue;
                    }
                    if let Some(fsig) = sig.functions.get(member) {
                        self.functions.insert(bind.clone(), fsig.clone());
                        // Carry the numeric-polymorphism marker onto the imported name (gap #12).
                        if sig.numeric_poly.contains(member) {
                            self.imported_poly.insert(bind.clone());
                        }
                        // Editor hover (decl-site): record the imported function's signature at the
                        // bound-name token (probe-gated no-op off the probe / outside the entry module).
                        if self.hover_probe.is_some() {
                            let fty = Ty::Func {
                                params: fsig.params.clone(),
                                ret: Box::new(fsig.ret.clone()),
                            };
                            self.hover_record_at(
                                *name_span,
                                &fty,
                                HoverKind::Func,
                                fsig.doc.clone(),
                            );
                        }
                    } else if let Some(vty) = sig.values.get(member) {
                        // Editor hover (decl-site): record the imported value's type at the bound name.
                        if self.hover_probe.is_some() {
                            self.hover_record_at(*name_span, vty, HoverKind::Other, None);
                        }
                        self.declare(bind, vty.clone());
                    } else if sig.types.contains(member) {
                        // A type name imported from a module. For `std.ffi`'s exported FFI marshalling
                        // TYPE names — the fixed-width integers (`int32`) AND the opaque `ptr` handle —
                        // this is a special case: record it into the per-module `imported_ffi_types`
                        // set so `resolve_type` accepts the bare name in THIS module (it's a type, not
                        // a callable value). Only `std.ffi` lists these in `sig.types`, so the check is
                        // already scoped to it.
                        if member == "timer" {
                            // A selective `import timer from std.time` licenses the opcode-backed
                            // `timer(ms)` builtin in THIS module. `timer` is a reserved name, so ONLY
                            // `std.time`'s `sig.types` can carry it (no user module can export a member
                            // named `timer`) — matching the member name alone is unambiguous. Like the
                            // concurrency ctors, `timer` carries no runtime value (it lowers via
                            // name→opcode), so an alias would bind nothing usable AND the runtime
                            // `bind_import` skip keys on the original member name — reject the rename.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    "timer cannot be renamed on import — \
                                     write `import timer from std.time`"
                                        .to_string(),
                                );
                            } else {
                                self.imported_time.insert(member.clone());
                                // Editor hover: `timer` is a reserved FUNCTION (`timer(ms) ->
                                // Channel[bool]`), not a type — record a function-style import-line
                                // hover (probe-gated no-op off the probe).
                                if self.hover_probe.is_some() {
                                    let fty = Ty::Func {
                                        params: vec![Ty::Int],
                                        ret: Box::new(Ty::channel(Ty::Bool)),
                                    };
                                    self.hover_record_at(
                                        *name_span,
                                        &fty,
                                        HoverKind::Func,
                                        Some(
                                            "one-shot timeout channel — timer(ms) delivers `true` \
                                             once after ms milliseconds (import std.time)"
                                                .to_string(),
                                        ),
                                    );
                                }
                            }
                        } else if matches!(
                            member.as_str(),
                            "Shared" | "RwShared" | "Atomic" | "Executor"
                        ) {
                            // A selective `import Shared from std.concurrency` licenses just the named
                            // ctor/TYPE in THIS module (mirrors the per-name FFI width imports). Like
                            // the FFI types these carry no runtime value (ctor lowers via name→opcode),
                            // so an alias would bind nothing usable AND the runtime `bind_import` skip
                            // keys on the original member name — reject the rename to keep it honest.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "concurrency type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.concurrency`"
                                    ),
                                );
                            } else {
                                self.imported_concurrency.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if crate::native::ffi::TYPE_NAMES.contains(&member.as_str())
                            || member == "ptr"
                        {
                            // An FFI marshalling type CANNOT be RENAMED on import: the backends'
                            // `ctype_of` keys off the literal surface name (`int32`/`ptr`), so an alias
                            // would resolve to a type the marshaller can't lower. Reject `import int32
                            // as W` / `import ptr as P` (name unusable) and `import int8 as int32`
                            // (silently the wrong width). A redundant identical self-rename (`import
                            // ptr as ptr`) is harmless — the as-name equals the member, no wrong-type
                            // risk — so it falls through to the normal no-op import of `ptr`.
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "FFI type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.ffi`"
                                    ),
                                );
                            } else {
                                self.imported_ffi_types.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if matches!(member.as_str(), "Socket" | "Listener") {
                            // A selective `import Socket from std.net` licenses just the named TCP
                            // handle TYPE in THIS module (mirrors the per-name concurrency imports).
                            // Like those, a net handle carries no runtime value (the type resolves
                            // directly to `Ty::Socket`; a value comes from `connect`/`listen`) AND the
                            // runtime `bind_import` skip keys on the original member name — so an alias
                            // would bind nothing usable: reject the rename. Only `std.net`'s `sig.types`
                            // carries these names (they're reserved, so no user module can export them).
                            if alias.as_ref().is_some_and(|a| a != member) {
                                self.error(
                                    imp.span,
                                    format!(
                                        "net type '{member}' cannot be renamed on import — \
                                         write `import {member} from std.net`"
                                    ),
                                );
                            } else {
                                self.imported_net.insert(member.clone());
                                self.record_native_type_import_hover(member, *name_span, path);
                            }
                        } else if let Some(info) = sig.struct_defs.get(member) {
                            // A user struct imported by name: inject its resolved shape under the
                            // DECLARING module's runtime key (so it unifies with that module's
                            // signatures + a value's `Ty`), and make it BARE-VISIBLE under the bind
                            // name via `struct_names`/`bare_types` so `S(...)`/`x: S` resolve here.
                            let key = self.type_key(&imp.target, member);
                            self.structs.insert(key.clone(), info.clone());
                            self.struct_names.insert(bind.clone());
                            self.bare_types.insert(bind.clone(), key.clone());
                            // Same soundness gate as the whole-module path: a Builtin-origin std
                            // struct imported by name reserves its BIND name against a same-named user
                            // `struct` decl (else accept-then-trap on the native shape mismatch).
                            if info.origin == StructOrigin::Builtin {
                                self.imported_builtin_types.insert(bind.clone());
                            }
                            // Editor hover (Tier C): the imported type's doc — its own decl docstring
                            // carried across the boundary, else a `kind (from module)` fallback. Seed
                            // `name_docs[bind]` so a later bare/annotation/generic-head use surfaces it
                            // (the `Type::Named`/`Type::Generic` hover arms read `name_docs`), AND record
                            // the import-line token hover here. Both are probe-gated no-ops off-probe.
                            self.record_imported_type_hover(
                                bind,
                                *name_span,
                                &Ty::strukt(key),
                                info.doc.as_deref(),
                                "struct",
                                path,
                            );
                        } else if let Some(edef) = sig.enum_defs.get(member) {
                            // A user enum imported by name: inject its variant names, type params, and
                            // each variant's payload under the declaring module's runtime key; expose
                            // it bare under the bind name.
                            let key = self.type_key(&imp.target, member);
                            self.enums.insert(key.clone(), edef.variant_names.clone());
                            self.enum_names.insert(bind.clone());
                            self.bare_types.insert(bind.clone(), key.clone());
                            self.enum_type_params
                                .insert(key.clone(), edef.type_params.clone());
                            self.enum_methods.insert(key.clone(), edef.methods.clone());
                            for (vname, vinfo) in edef.variant_names.iter().zip(&edef.variants) {
                                let mut vi = vinfo.clone();
                                vi.enum_name = key.clone();
                                self.variants.insert((key.clone(), vname.clone()), vi);
                                self.variant_owners
                                    .entry(vname.clone())
                                    .or_default()
                                    .push(bind.clone());
                            }
                            self.record_imported_type_hover(
                                bind,
                                *name_span,
                                &Ty::Enum(key, vec![]),
                                edef.doc.as_deref(),
                                "enum",
                                path,
                            );
                        } else if let Some(ntdef) = sig.newtype_defs.get(member) {
                            // A user newtype imported by name: inject its underlying + methods under
                            // the declaring module's runtime key; expose it bare under the bind name.
                            let key = self.type_key(&imp.target, member);
                            self.newtype_defs.insert(
                                key.clone(),
                                (ntdef.underlying.clone(), ntdef.methods.clone()),
                            );
                            self.newtype_type_params
                                .insert(key.clone(), ntdef.type_params.clone());
                            self.newtype_names.insert(bind.clone());
                            self.bare_types.insert(bind.clone(), key.clone());
                            self.record_imported_type_hover(
                                bind,
                                *name_span,
                                &Ty::NewType(key, vec![]),
                                ntdef.doc.as_deref(),
                                "newtype",
                                path,
                            );
                        } else if let Some(asig) = sig.type_aliases.get(member) {
                            // A user type alias imported by name. An unlicensed alias embedding an
                            // un-imported FFI width cannot be laundered — reject it here, mirroring the
                            // old use-site "unknown type" error.
                            if let Some(w) = &asig.unlicensed_width {
                                self.error(
                                    imp.span,
                                    format!(
                                        "unknown type '{w}' (import it from std.ffi: `import {w} from std.ffi`)"
                                    ),
                                );
                            } else {
                                // Inject the alias's RESOLVED body so bare use (`x: Len`) resolves to
                                // the underlying type. A licensed FFI-width alias re-seeds
                                // `ffi_alias_ok` under the bind name (defensive; the body is already
                                // a concrete `Ty`, so no width re-check is hit).
                                self.imported_alias_tys
                                    .insert(bind.clone(), asig.body.clone());
                                // Carry the alias's width-bearing CType (computed in its DEFINING
                                // module's scope) so an extern boundary in THIS module marshals the
                                // real width through the named-import hop — not the bare flat map.
                                self.imported_alias_ctypes
                                    .insert(bind.clone(), asig.ctype.clone());
                                if asig.licensed {
                                    self.ffi_alias_ok.insert(bind.clone());
                                }
                            }
                        }
                    } else {
                        self.error(
                            imp.span,
                            format!(
                                "module '{}' has no member '{member}'",
                                module_label(&imp.import)
                            ),
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
                StmtKind::Struct { name, .. } => {
                    sig.types.insert(name.clone());
                    // The LAYOUT lives under the runtime key (bare unless disambiguated); the sig is
                    // keyed by the BARE name (importers look up by bare member name + their own
                    // `type_key`).
                    let key = self.bare_key(name);
                    if let Some(info) = self.structs.get(&key) {
                        let mut info = info.clone();
                        // Carry the decl docstring onto the EXPORTED sig (editor hover) so an
                        // importer's `from M import S` / `x: S` hover surfaces it across the boundary.
                        info.doc = self.name_docs.get(name).cloned();
                        sig.struct_defs.insert(name.clone(), info);
                    }
                }
                StmtKind::Enum { name, .. } => {
                    sig.types.insert(name.clone());
                    let key = self.bare_key(name);
                    if let Some(variant_names) = self.enums.get(&key) {
                        let type_params =
                            self.enum_type_params.get(&key).cloned().unwrap_or_default();
                        let variants = variant_names
                            .iter()
                            .filter_map(|v| self.variants.get(&(key.clone(), v.clone())).cloned())
                            .collect();
                        sig.enum_defs.insert(
                            name.clone(),
                            EnumSigInfo {
                                variant_names: variant_names.clone(),
                                type_params,
                                variants,
                                methods: self.enum_methods.get(&key).cloned().unwrap_or_default(),
                                doc: self.name_docs.get(name).cloned(),
                            },
                        );
                    }
                }
                StmtKind::NewType { name, .. } => {
                    sig.types.insert(name.clone());
                    let key = self.bare_key(name);
                    if let Some((underlying, methods)) = self.newtype_defs.get(&key) {
                        sig.newtype_defs.insert(
                            name.clone(),
                            NewTypeSigInfo {
                                underlying: underlying.clone(),
                                type_params: self
                                    .newtype_type_params
                                    .get(&key)
                                    .cloned()
                                    .unwrap_or_default(),
                                methods: methods.clone(),
                                doc: self.name_docs.get(name).cloned(),
                            },
                        );
                    }
                }
                StmtKind::TypeAlias { name, .. } => {
                    sig.types.insert(name.clone());
                    if let Some(body) = self.aliases.get(name) {
                        // Resolve the alias body in THIS (the defining) module's scope so an
                        // importer carries the right underlying type (incl. an FFI width license).
                        let resolved = self.resolve_type_ro_pub(body);
                        // Also resolve the body to a WIDTH-BEARING CType in this defining scope, so a
                        // cross-module `type Len = int32` exports `int32` (not `Ty::Int`). This is the
                        // channel the real width travels through a `from`-import / `module.Alias` hop.
                        let ctype = self.resolve_ctype(body);
                        let licensed = self.ffi_alias_ok.contains(name);
                        // If the alias embeds FFI widths but is NOT licensed, find the first width
                        // the defining module did not import — an importer must reject the alias
                        // rather than launder the un-imported width.
                        let unlicensed_width = if licensed {
                            None
                        } else {
                            let mut widths = Vec::new();
                            Self::collect_width_names(body, &mut widths);
                            widths
                                .into_iter()
                                .find(|w| !self.imported_ffi_types.contains(w))
                        };
                        sig.type_aliases.insert(
                            name.clone(),
                            AliasSig {
                                body: resolved,
                                licensed,
                                unlicensed_width,
                                ctype,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        sig
    }

    /// `resolve_ty_ro` wrapped for use inside `capture_sig` (which holds `&self`). Resolves an alias
    /// body to a `Ty` in the current (defining) module's scope without emitting errors.
    fn resolve_type_ro_pub(&self, t: &Type) -> Ty {
        self.resolve_ty_ro(t)
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
        self.ref_decls.push(std::collections::HashSet::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
        self.loop_vars.pop();
        self.ref_decls.pop();
    }
    /// Record `name` (already declared in the current scope) as a `ref T` binding/param.
    fn declare_ref(&mut self, name: &str) {
        if let Some(set) = self.ref_decls.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name` an in-scope `ref T` binding/param (innermost binding wins, shadowing-aware)?
    fn is_ref_decl(&self, name: &str) -> bool {
        for (vars, refs) in self.scopes.iter().zip(self.ref_decls.iter()).rev() {
            if vars.contains_key(name) {
                return refs.contains(name);
            }
        }
        false
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
    /// Re-pin `name`'s binding to `ty` **in its OWNING scope** (the same scope `lookup` resolves),
    /// not the innermost one. Used by refine-on-first-use to narrow an empty-collection's `Unknown`
    /// element/key/value slot to the concrete type the first mutating op supplies. `declare` always
    /// writes the last scope — wrong for an outer-scope receiver refined inside an `if`/`for` block
    /// (it would shadow-create a bogus inner binding that leaks on pop), so we walk innermost-first
    /// and overwrite the first scope that owns `name`. Returns the scope index written (so the
    /// flow-sensitivity snapshot/restore barrier can revert THIS scope's binding precisely).
    fn repin(&mut self, name: &str, ty: Ty) -> Option<usize> {
        for i in (0..self.scopes.len()).rev() {
            if self.scopes[i].contains_key(name) {
                self.scopes[i].insert(name.to_string(), ty);
                return Some(i);
            }
        }
        None
    }
    /// Snapshot every in-scope binding whose type still carries an `Unknown` in a slot position (a
    /// refinable empty-collection / nullary-variant / None producer), recording its OWNING scope
    /// index, name, and current type. Paired with [`Self::restore_refinable`]. Refine-on-first-use is
    /// now PERSISTENT scope-wide first-use pinning, so the STATEMENT-position sites
    /// (`check_block`/for-loop/`check_match`) no longer snapshot/restore — a pin there persists. These
    /// helpers remain in use by the EXPRESSION-position arms (`infer_if_else`/`infer_match`): a value-
    /// arm produces a VALUE, so a pin in one value-arm must not leak to a sibling value-arm or it
    /// would corrupt branch value inference. We snapshot the OWNING scope index — not the innermost
    /// block scope — so restoring reverts the exact binding `repin` wrote, even when the receiver was
    /// declared in an outer scope.
    fn snapshot_refinable(&self) -> Vec<(usize, String, Ty)> {
        let mut snap = Vec::new();
        for (i, scope) in self.scopes.iter().enumerate() {
            for (name, ty) in scope {
                if contains_unknown_in_slot(ty) {
                    snap.push((i, name.clone(), ty.clone()));
                }
            }
        }
        snap
    }
    /// Restore the bindings captured by [`Self::snapshot_refinable`], reverting any in-arm refinement
    /// so each EXPRESSION-position value-arm refines independently from the pre-arm type (kept only
    /// at `infer_if_else`/`infer_match`; statement-position pins now persist). Writes back by (scope
    /// index, name); a snapshotted scope that was already popped is skipped (binding gone, nothing to
    /// revert).
    fn restore_refinable(&mut self, snap: Vec<(usize, String, Ty)>) {
        for (i, name, ty) in snap {
            if let Some(scope) = self.scopes.get_mut(i)
                && scope.contains_key(&name)
            {
                scope.insert(name, ty);
            }
        }
    }
    /// Is `name` bound *below* the module-global scope (scope 0) — i.e. a local, parameter, or
    /// captured binding? The qualified enum-variant form `Enum.Variant` yields to such a binding but
    /// NOT to a module global or function, mirroring both engines' locals-only precedence gate (VM
    /// `resolve_local`/`captures`, interp `get_local`). Using full [`Self::lookup`] here would let a
    /// top-level global named like the enum shadow in the checker but not the engines — a soundness
    /// hole (the checker would validate a different program than the one that runs).
    fn is_local_binding(&self, name: &str) -> bool {
        self.scopes.iter().skip(1).any(|s| s.contains_key(name))
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
                    if self.enum_names.contains(name) || self.newtype_names.contains(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.struct_names.insert(name.clone());
                }
                StmtKind::Enum { name, .. } => {
                    if self.struct_names.contains(name) || self.newtype_names.contains(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.enum_names.insert(name.clone());
                }
                StmtKind::NewType { name, .. } => {
                    if matches!(
                        name.as_str(),
                        "int" | "float" | "bool" | "str" | "bytes" | "bytearray" | "nil"
                    ) || is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    } else if self.struct_names.contains(name)
                        || self.enum_names.contains(name)
                        || self.newtype_names.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    self.newtype_names.insert(name.clone());
                }
                StmtKind::TypeAlias { name, ty, .. } => {
                    if matches!(
                        name.as_str(),
                        "int" | "float" | "bool" | "str" | "bytes" | "bytearray" | "nil"
                    ) || is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    } else if self.aliases.contains_key(name)
                        || self.struct_names.contains(name)
                        || self.enum_names.contains(name)
                        || self.newtype_names.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    } else {
                        self.aliases.insert(name.clone(), ty.clone());
                        // PRECISE width-alias opt-in: if this alias's body references fixed-width FFI
                        // type names (`int8`..`uint64`) — directly (`type Len = int32`) or embedded in
                        // a composite (`type Pair = (int32, int32)`, `type Buf = list[uint8]`) — and
                        // EVERY such width was imported per-name from `std.ffi` by THIS (the defining)
                        // module, record the alias as licensed. `resolve_type` then lets those widths
                        // resolve through the alias anywhere, including cross-module with no re-import.
                        // A `type Len = int32` whose module never imported int32 is NOT licensed, so it
                        // can't launder the bare width past the import gate. Requiring ALL embedded
                        // widths imported keeps it precise: a `type Mixed = (int32, int64)` that imported
                        // only int32 stays unlicensed, so int64 can't ride in on int32's opt-in.
                        // `collect_names` runs after `bind_import`, so `imported_ffi_types` is populated.
                        let mut widths = Vec::new();
                        Self::collect_width_names(ty, &mut widths);
                        if !widths.is_empty()
                            && widths.iter().all(|w| self.imported_ffi_types.contains(w))
                        {
                            self.ffi_alias_ok.insert(name.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect doc-comments for the module's top-level NON-fn declarations (struct/enum/protocol/
    /// newtype/type-alias names + top-level `let` bindings) into `name_docs`, keyed by simple name.
    /// Free fns and methods carry their doc on `FnSig::doc` instead (handled in `fn_sig`). Purely for
    /// LSP hover — never consulted by checking/codegen, so it is behavior- and parity-neutral.
    fn collect_docs(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Struct {
                    name, doc: Some(d), ..
                }
                | StmtKind::Enum {
                    name, doc: Some(d), ..
                }
                | StmtKind::Protocol {
                    name, doc: Some(d), ..
                }
                | StmtKind::NewType {
                    name, doc: Some(d), ..
                }
                | StmtKind::TypeAlias {
                    name, doc: Some(d), ..
                } => {
                    self.name_docs.insert(name.clone(), d.clone());
                }
                // A top-level `let` binds (possibly multiple) names; surface the doc on each.
                StmtKind::Let {
                    names,
                    doc: Some(d),
                    ..
                } => {
                    for n in names {
                        self.name_docs.insert(n.clone(), d.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect every fixed-width FFI type name (`int8`..`uint64`) referenced anywhere in `ty`,
    /// recursing through composites (`Generic` args, `Func` params/return, `Tuple` elements). Used
    /// to license a width-alias only when its defining module imported all the widths it embeds.
    fn collect_width_names(ty: &Type, out: &mut Vec<String>) {
        match ty {
            Type::Named { name: n, .. } => {
                if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) {
                    out.push(n.clone());
                }
            }
            Type::Generic(_, args, ..) => {
                for a in args {
                    Self::collect_width_names(a, out);
                }
            }
            Type::Func { params, ret } => {
                for p in params {
                    Self::collect_width_names(p, out);
                }
                Self::collect_width_names(ret, out);
            }
            Type::Tuple(elems) => {
                for e in elems {
                    Self::collect_width_names(e, out);
                }
            }
            // A module-qualified type's head is a user type (never a bare width); only its type
            // arguments could carry a width name written in THIS module.
            Type::Qualified { args, .. } => {
                for a in args {
                    Self::collect_width_names(a, out);
                }
            }
        }
    }

    /// Second sub-pass: resolve and register signatures, fields, and variants. Redeclarations
    /// (a name defined twice) are reported here — otherwise "last write wins" would silently
    /// mis-type or, for struct methods, panic in pass 2 on a key that no longer exists.
    fn hoist(&mut self, stmts: &[Stmt]) {
        // Protocols first: function/struct signatures may reference them in type-parameter bounds.
        for s in stmts {
            if let StmtKind::Protocol {
                name,
                type_params,
                methods,
                embeds,
                ..
            } = &s.kind
            {
                self.hoist_protocol(name, type_params, methods, embeds, s.span);
            }
        }
        // M22 — validate embeds in a SECOND pass, now that every protocol is registered, so a forward
        // (or cyclic) embed reference resolves. Collision/cycle detection lives here (declare-time,
        // authoritative — before any satisfaction check can recurse into a cycle).
        for s in stmts {
            if let StmtKind::Protocol {
                name,
                methods,
                embeds,
                ..
            } = &s.kind
            {
                self.validate_protocol_embeds(name, methods, embeds, s.span);
            }
        }
        // extern fn (name, span) pairs, collected during the hoist loop and checked against the
        // fully-built struct/variant/enum registries AFTER the loop (so a `struct S` declared *after*
        // an `extern fn S` still collides — the check is order-independent).
        // `mut` + the post-loop sweep are unix-only (only the `#[cfg(unix)]` arm pushes); on other
        // targets extern is rejected wholesale, leaving this empty.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut extern_names: Vec<(String, Span)> = Vec::new();
        // Extern param/return marshallability is validated AFTER this loop (collected here), so a
        // struct passed/returned BY VALUE may be DECLARED AFTER the extern block: `self.structs`
        // (field info, which `assert_marshallable` inspects for a flat-scalar struct) is only fully
        // populated once every struct in the module has been hoisted. `collect_names` already
        // pre-registered struct *names*, so `resolve_type` accepts the forward reference inline.
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut extern_marshal_checks: Vec<(Ty, String, Span, bool)> = Vec::new();
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    // A user `fn` named after a runtime constructor / builtin op (`timer`, `Channel`,
                    // `range`, …) is silently SHADOWED: `infer_named_call` / the backends resolve the
                    // builtin arm before a plain named call, so the user fn is dead code. Reject the
                    // collision with a clear `reserved` error (mirrors the extern-name guard).
                    if is_reserved_name(&decl.name) {
                        self.error(
                            s.span,
                            format!("function name '{}' is reserved (builtin)", decl.name),
                        );
                    } else if self.functions.contains_key(&decl.name) {
                        self.error(
                            s.span,
                            format!("function '{}' is already defined", decl.name),
                        );
                    }
                    let sig = self.fn_sig(decl, s.span);
                    self.functions.insert(decl.name.clone(), sig);
                }
                StmtKind::Struct {
                    name,
                    type_params,
                    fields,
                    methods,
                    ..
                } => {
                    // `imported_builtin_types`: a name THIS module imported as a Builtin-origin std
                    // struct (Ref/Match/Response/ProcResult, …) is reserved against a same-named user
                    // `struct` decl — declaring it would overwrite the native seed yet the runtime
                    // still constructs/returns the native shape, so the check-clean program would trap
                    // at runtime on a field mismatch. (A bare unimported `struct Match` stays legal —
                    // the name isn't in the set without the import event.)
                    if is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                        || self.imported_builtin_types.contains(name)
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    // A pre-seeded synthetic stdlib struct (`Match`/`Response`/`ProcResult`, always
                    // present in `self.structs` under its bare key tagged `StructOrigin::Builtin` for
                    // import-free field access) is NOT a real prior definition — a user `struct
                    // Response` shadows it (the hoist insert below overwrites the seed with the user's
                    // `User`-origin layout). Only a genuine User-origin entry is "already defined".
                    let already_defined = self
                        .structs
                        .get(&self.bare_key(name))
                        .is_some_and(|i| i.origin != StructOrigin::Builtin);
                    if already_defined {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // A type PARAMETER may not be named after a reserved builtin type (`struct
                    // Box[int]`/`[List]`/`[Result]`) — same one-way-ratchet rule as the decl NAME.
                    self.reject_reserved_type_params(type_params);
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
                    // Register the LAYOUT under this module's runtime key (bare unless a genuine
                    // cross-module clash disambiguated it), so a value of this type — whose `Ty` also
                    // carries the key — resolves its fields/methods here and across the module
                    // boundary. `struct_names` (bare-visibility) stays bare; only the layout is keyed.
                    let key = self.bare_key(name);
                    self.structs.insert(
                        key,
                        StructInfo {
                            type_params: type_params.clone(),
                            fields,
                            methods,
                            origin,
                            // Decl docstring is attached later in `capture_sig` (from `name_docs`),
                            // only for the module's exported sig; the in-checker layout doesn't need it.
                            doc: None,
                        },
                    );
                }
                StmtKind::Enum {
                    name,
                    type_params,
                    variants,
                    methods,
                    ..
                } => {
                    if is_reserved_type(name)
                        || is_reserved_protocol(name)
                        || crate::native::ffi::TYPE_NAMES.contains(&name.as_str())
                    {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    // The LAYOUT tables (`enums`/`variants`/`enum_type_params`) are keyed by this
                    // module's runtime key (bare unless disambiguated), so a value's `Ty::Enum(key)`
                    // resolves its variants here and across module boundaries. `enum_names`/
                    // `variant_owners` (bare-visibility + qualify-hint) stay bare.
                    let key = self.bare_key(name);
                    if self.enums.contains_key(&key) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // A type PARAMETER may not be named after a reserved builtin type (`enum E[int]`).
                    self.reject_reserved_type_params(type_params);
                    // The enum's type parameters are in scope across its variant payloads (so a
                    // `Node(T, Tree[T])` resolves `T`). Validate each bound names a known protocol.
                    let saved = self.enter_type_params(type_params);
                    for tp in type_params {
                        self.check_bounds(&tp.bounds, &tp.name, s.span);
                    }
                    let mut names = Vec::new();
                    for v in variants {
                        // Variants are scoped under their enum, so two *different* enums may share a
                        // variant name. A repeat *within the same* enum is still a collision.
                        if self.variants.contains_key(&(key.clone(), v.name.clone())) {
                            self.error(
                                s.span,
                                format!("variant '{}' is already defined in enum '{name}'", v.name),
                            );
                        }
                        names.push(v.name.clone());
                        let payload = v
                            .payload
                            .iter()
                            .map(|t| self.resolve_type(t, s.span))
                            .collect();
                        self.variants.insert(
                            (key.clone(), v.name.clone()),
                            VariantInfo {
                                enum_name: key.clone(),
                                payload,
                            },
                        );
                        self.variant_owners
                            .entry(v.name.clone())
                            .or_default()
                            .push(name.clone());
                    }
                    // Methods see the enum's type parameters in scope (like the struct path), so a
                    // generic `fn get(self) -> T` resolves `T`. Name-keyed exactly like struct methods.
                    let method_sigs: HashMap<String, FnSig> = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    // Variant names and STATIC-method names must be DISJOINT: `Enum.name` always
                    // resolves the variant first (see `infer_call`), so a static method named after a
                    // variant could never be reached — a collision is an error, not a silent shadow.
                    for m in methods {
                        if names.contains(&m.name) {
                            self.error(
                                s.span,
                                format!("'{}' is already a variant of enum '{name}'", m.name),
                            );
                        }
                    }
                    self.exit_type_params(saved);
                    self.enums.insert(key.clone(), names);
                    self.enum_type_params
                        .insert(key.clone(), type_params.clone());
                    self.enum_methods.insert(key, method_sigs);
                }
                StmtKind::NewType {
                    name,
                    type_params,
                    underlying,
                    methods,
                    ..
                } => {
                    if is_reserved_type(name) || is_reserved_protocol(name) {
                        self.error(s.span, format!("type '{name}' is reserved (builtin)"));
                    }
                    let key = self.bare_key(name);
                    if self.newtype_defs.contains_key(&key) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    // A type PARAMETER may not be named after a reserved builtin type (`newtype
                    // N[List] = int`).
                    self.reject_reserved_type_params(type_params);
                    // A type-parameterized newtype puts its params in scope across the underlying type
                    // + method signatures (so `newtype Stack[T] = list[T]` and `fn push(self, x: T)`
                    // resolve `T`), exactly like the struct/enum generic path. Validate each bound.
                    let saved = self.enter_type_params(type_params);
                    for tp in type_params {
                        self.check_bounds(&tp.bounds, &tp.name, s.span);
                    }
                    let under_ty = self.resolve_type(underlying, s.span);
                    // A newtype cannot wrap itself or another newtype's identity that's still a bare
                    // newtype value — but a newtype OF a newtype is simply nominal nesting (allowed:
                    // construct/unwrap one level at a time). No special rejection needed here.
                    let method_sigs: HashMap<String, FnSig> = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.exit_type_params(saved);
                    self.newtype_type_params
                        .insert(key.clone(), type_params.clone());
                    self.newtype_defs.insert(key, (under_ty, method_sigs));
                }
                StmtKind::Extern { fns, .. } => {
                    // Dynamic C-ABI FFI (`dlopen`/libffi) is unix-only — `int` marshals as C `long`,
                    // which is 64-bit on every supported (LP64) unix target. On a non-unix target
                    // (e.g. LLP64 Windows, where C `long` is 32-bit) `extern` is unavailable; reject
                    // it here so the `MakeCffi`/`dlopen` + `as c_long` truncation path is statically
                    // unreachable off-unix.
                    #[cfg(not(unix))]
                    for ef in fns {
                        self.error(
                            ef.span,
                            format!(
                                "extern FFI is only supported on unix targets ('{}')",
                                ef.name
                            ),
                        );
                    }
                    // Each extern C fn becomes a plain module-global signature, hoisted exactly like
                    // a top-level `fn` so calls type-check through the normal `infer_named_call` path.
                    // v1 marshals scalars only — every resolved param + return type must be
                    // C-marshallable (int/float/bool/str, or void return).
                    #[cfg(unix)]
                    for ef in fns {
                        // An extern fn may not take a builtin/print/constructor name — both backends
                        // resolve those to a special op before a plain call, so the extern would be
                        // dead code (and the compiler's eager `MakeCffi` would `dlsym` a symbol it can
                        // never reach). Struct/variant collisions are checked after the loop.
                        if is_reserved_name(&ef.name) {
                            self.error(
                                ef.span,
                                format!(
                                    "'{}' is a builtin/reserved name and cannot be an extern fn",
                                    ef.name
                                ),
                            );
                        }
                        extern_names.push((ef.name.clone(), ef.span));
                        if self.functions.contains_key(&ef.name) {
                            self.error(
                                ef.span,
                                format!("function '{}' is already defined", ef.name),
                            );
                        }
                        // `owned_str` is a return-only extern marshalling form — license its
                        // `resolve_type` arm only while resolving THIS signature's params + return
                        // (and the harvested C signature below), then reset so a bare non-extern use
                        // is rejected.
                        self.in_extern_sig = true;
                        let params: Vec<Ty> = ef
                            .params
                            .iter()
                            .map(|p| match &p.ty {
                                Some(t) => {
                                    // RETURN-ONLY surface forms (`owned_str`, `str?`/`owned_str?`)
                                    // must be rejected as PARAMS on the SURFACE Type, before
                                    // `resolve_type` collapses `owned_str` to a plain `Str` (which
                                    // would otherwise sail past `assert_marshallable`).
                                    if self.is_return_only_extern_type(t) {
                                        self.error(
                                            ef.span,
                                            format!(
                                                "type '{}' is not C-marshallable in extern fn '{}' \
                                                 (owned_str / str? are return-only)",
                                                describe_extern_type(t),
                                                ef.name
                                            ),
                                        );
                                    }
                                    let ty = self.resolve_type(t, ef.span);
                                    // A parameter must be a real C scalar — `nil` (void) is a
                                    // return-only sentinel and would panic the backend's `ctype_of`.
                                    // Deferred to the post-loop sweep (a by-value struct param may be
                                    // declared after this extern block).
                                    extern_marshal_checks.push((
                                        ty.clone(),
                                        ef.name.clone(),
                                        ef.span,
                                        false,
                                    ));
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
                                // Deferred to the post-loop sweep (a by-value struct return may be
                                // declared after this extern block).
                                extern_marshal_checks.push((
                                    ty.clone(),
                                    ef.name.clone(),
                                    ef.span,
                                    true,
                                ));
                                ty
                            }
                            // A void extern returns nothing observable; model it as `Nil`.
                            None => Ty::Nil,
                        };
                        self.functions
                            .insert(ef.name.clone(), FnSig::plain(params, ret));
                        // ROOT FIX (fix4): harvest the FULLY-RESOLVED, width-bearing C signature for
                        // each extern fn, resolved here in THIS module's import/alias scope (the same
                        // scope `resolve_type` used to accept it). Both backends consume this instead
                        // of re-resolving alias names themselves — closing every spelling at once.
                        // Keyed by `(graph module index, fn name)`, the index both backends derive.
                        // Only built when `resolve_extern_signatures` drives the pass.
                        if let Some(midx) = self.extern_module_idx {
                            let cparams: Vec<Option<CType>> = ef
                                .params
                                .iter()
                                .map(|p| p.ty.as_ref().and_then(|t| self.resolve_ctype(t)))
                                .collect();
                            let cret = ef.ret.as_ref().and_then(|t| self.resolve_ctype(t));
                            self.extern_sigs.insert(
                                (midx, ef.name.clone()),
                                ExternCSig {
                                    params: cparams,
                                    ret: cret,
                                },
                            );
                        }
                        self.in_extern_sig = false;
                    }
                }
                _ => {}
            }
        }
        // Order-independent extern/registry collision sweep: a struct or enum variant registers a
        // same-named constructor the backends resolve before a plain call, so an extern sharing that
        // name is unreachable. Done after the loop so a `struct S`/`enum {Leaf}` declared *after* an
        // `extern fn S`/`fn Leaf` still collides (the maps are fully built by now). `extern_names` is
        // only populated on unix (the `#[cfg(unix)]` arm above); on other targets the extern was
        // already rejected wholesale, so the sweep is a no-op.
        for (name, span) in &extern_names {
            // Struct names and enum *variant* names register backend-resolved constructors; an
            // enum *type* name does not (it is not callable in either engine), so it is NOT a
            // collision — `extern fn Foo` alongside `enum Foo` resolves to the extern, exactly as
            // a plain `fn Foo` alongside `enum Foo` does.
            if self.structs.contains_key(name) || self.variant_owners.contains_key(name) {
                self.error(
                    *span,
                    format!("'{name}' is a builtin/reserved name and cannot be an extern fn"),
                );
            }
        }
        // Now every struct's field info is registered, so a by-value-struct param/return resolves its
        // fields regardless of whether the struct was declared before or after the extern block.
        for (ty, name, span, allow_void) in &extern_marshal_checks {
            self.assert_marshallable(ty, name, *span, *allow_void);
        }
    }

    /// Is this surface extern `Type` a RETURN-ONLY marshalling form (`owned_str`, `str?`, or
    /// `owned_str?`)? Checked on the SURFACE `Type` (pre-`resolve_type`) because `owned_str` collapses
    /// to a plain `Str` once resolved, losing its return-only-ness. (A plain `str?` param is also
    /// caught by `assert_marshallable`, but this gives it a clearer "return-only" message.)
    ///
    /// Transparent type aliases are resolved here, mirroring the backends' alias-resolving `ctype_of`:
    /// `type O = owned_str` makes a param `s: O` whose surface name is `O` (not `owned_str`) yet whose
    /// `ctype_of` is `CType::OwnedStr` — without alias resolution it would slip past this guard,
    /// type-check as a plain `Str`, then hit the return-only `unreachable!` param arm at runtime.
    fn is_return_only_extern_type(&self, t: &Type) -> bool {
        self.is_return_only_extern_type_seen(t, &mut Vec::new())
    }

    /// `is_return_only_extern_type` with a shared `seen` set of alias names that spans the WHOLE
    /// recursion — including the `Named`→`Option`→`Named` re-entry. A single per-loop guard is not
    /// enough: a cyclic alias routed through an `Option`/`?` form (e.g. `type A = A?`) crosses the
    /// arm boundary, and without shared state each frame restarts with an empty set and recurses
    /// forever (stack overflow). The cycle itself is reported separately by `resolve_type`; here we
    /// just terminate cleanly and report "not return-only".
    fn is_return_only_extern_type_seen(&self, t: &Type, seen: &mut Vec<String>) -> bool {
        match t {
            Type::Named { name: n, .. } => {
                if n == "owned_str" {
                    return true;
                }
                if seen.iter().any(|s| s == n) {
                    return false; // cycle — terminate; `resolve_type` diagnoses it
                }
                if let Some(aliased) = self.aliases.get(n) {
                    seen.push(n.clone());
                    return self.is_return_only_extern_type_seen(aliased, seen);
                }
                false
            }
            // `str?` / `owned_str?` parse to `Option[inner]`; the inner may itself be an alias.
            Type::Generic(n, args, ..) if n == "Option" => args.first().is_some_and(|inner| {
                matches!(inner, Type::Named { name: s, .. } if s == "str" || s == "owned_str")
                    || self.is_return_only_extern_type_seen(inner, seen)
            }),
            _ => false,
        }
    }

    /// v1 C-ABI marshallability: an extern fn's param/return types must be C-scalar — `int`, `float`,
    /// `bool`, or `str` (`char*`). `Nil` (void) is accepted ONLY for the return slot (`allow_void`),
    /// never for a parameter: a `nil` param has no `CType` lowering and would panic the backend's
    /// `ctype_of`, while a void-returning extern's `Nil` value would otherwise satisfy it. Everything
    /// else (list/map/set/tuple/struct/enum/func/option/result/protocol/channel/…) is rejected with a
    /// single uniform error. Called on the **resolved** `Ty` (after `resolve_type`), so a transparent
    /// alias to a scalar is accepted. `Unknown` is already-errored and silently allowed (no cascade).
    ///
    /// RETURN-ONLY (`allow_void`) additionally accepts `Option[str]` (surface `str?`): the nullable
    /// opt-in where a NULL `char*` lowers to `None` instead of faulting. (`owned_str` resolves to a
    /// plain `Str` and so needs no special case here — its return-only-ness is guarded on the surface
    /// `Type` in the extern param loop, before `resolve_type` collapses it.)
    fn assert_marshallable(&mut self, ty: &Ty, fn_name: &str, span: Span, allow_void: bool) {
        let scalar = matches!(
            ty,
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Ptr | Ty::Unknown
        );
        let ok = scalar
            || (allow_void
                && (matches!(ty, Ty::Nil)
                    || matches!(ty, Ty::Option(inner) if matches!(**inner, Ty::Str))));
        if ok {
            return;
        }
        // A sync scalar callback (callbacks #4): a function-typed PARAM whose every param and its
        // return is a C scalar (`int`/`float`/`bool`/`ptr`; widths resolve to those). PARAM-ONLY — a
        // function-typed RETURN (`allow_void`) is rejected (no C marshalling for a returned function
        // pointer in v1). A non-scalar part (str/struct/nested callback/void return) falls through to
        // the uniform error below, which names the offending function type.
        if let Ty::Func { params, ret } = ty
            && !allow_void
        {
            let part_ok =
                |t: &Ty| matches!(t, Ty::Int | Ty::Float | Ty::Bool | Ty::Ptr | Ty::Unknown);
            if params.iter().all(part_ok) && part_ok(ret) {
                return;
            }
        }
        // A flat-scalar struct BY VALUE: every field must itself be a marshallable C *scalar* (no
        // nested struct, no str/owned_str). Generic structs (non-empty type args) have no fixed C
        // layout — reject them. `visited` guards a struct cycling back through a field (defensive; a
        // struct field that is itself a struct is already rejected as nested). `Iterator` is a
        // built-in existential `Struct`, not a real POD — never marshallable.
        if let Ty::Struct(name, args) = ty
            && args.is_empty()
            && name != "Iterator"
            && self.structs.contains_key(name)
        {
            let mut visited = std::collections::HashSet::new();
            // The recursion emits field-level errors itself; either way return (no generic error).
            self.struct_fields_marshallable(name, fn_name, span, &mut visited);
            return;
        }
        self.error(
            span,
            format!(
                "type '{ty}' is not C-marshallable in extern fn '{fn_name}' \
                 (v1 supports only int, float, bool, str, ptr, and a flat struct of those)"
            ),
        );
    }

    /// Whether every field of struct `name` is a marshallable C *scalar* — the v1 by-value-struct
    /// rule (flat scalar fields only). On a non-scalar field (str/owned_str, a nested struct, a
    /// generic `Ty::Param`, a list/map/…) emits a clear error naming the struct AND the offending
    /// field, and returns `false`. `visited` breaks a (defensive) field-type cycle without overflow.
    fn struct_fields_marshallable(
        &mut self,
        name: &str,
        fn_name: &str,
        span: Span,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if !visited.insert(name.to_string()) {
            self.error(
                span,
                format!(
                    "struct '{name}' is recursively defined and cannot be C-marshallable in extern \
                     fn '{fn_name}'"
                ),
            );
            return false;
        }
        // Clone the field list to drop the immutable borrow on `self` before emitting errors.
        let fields = match self.structs.get(name) {
            Some(info) => info.fields.clone(),
            None => return false,
        };
        let mut all_ok = true;
        for (fname, fty) in &fields {
            // Only true C *scalars* are valid struct fields (NOT `Str` — str by value is deferred).
            let ok = matches!(fty, Ty::Int | Ty::Float | Ty::Bool | Ty::Ptr | Ty::Unknown);
            if !ok {
                all_ok = false;
                self.error(
                    span,
                    format!(
                        "struct '{name}' field '{fname}' of type '{fty}' is not C-marshallable \
                         (extern structs require flat scalar fields; nested structs and str are not \
                         supported in v1) in extern fn '{fn_name}'"
                    ),
                );
            }
        }
        visited.remove(name);
        all_ok
    }

    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type. The decl's
    /// generic `type_params` are installed (so `T` in annotations resolves to `Ty::Param("T")`) and
    /// each declared bound is validated against the known protocols.
    fn fn_sig(&mut self, decl: &FnDecl, span: Span) -> FnSig {
        // A free fn's or method's own type param `[U]` may not be named after a reserved builtin type
        // (`fn id[int]`). This is the SOLE funnel for free fns AND struct/enum/newtype methods (it is
        // hoist-only, so it fires once per decl — a method's `[U]` is checked here while the struct's
        // `[T]` is checked at the struct hoist, no overlap).
        self.reject_reserved_type_params(&decl.type_params);
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
                // A `ref T` param is a `Ref[T]` box (its reads/writes were lowered to `.get()`/`.set()`
                // by desugar). `check_ref_ty` rejects a non-boxable pointee (e.g. a bare generic).
                Some(t) if p.is_ref => {
                    self.check_ref_ty(t, span);
                    self.resolve_type(
                        &Type::Generic("Ref".to_string(), vec![t.clone()], Span::default()),
                        span,
                    )
                }
                Some(t) => self.resolve_type(t, span),
                None if p.name == "self" => Ty::Unknown, // bound in check_fn_body
                None => {
                    self.error(
                        span,
                        format!("parameter '{}' needs a type annotation", p.name),
                    );
                    Ty::Unknown
                }
            })
            .collect();
        // No `-> T`: leave the return as `Unknown` for now — `infer_returns` (run after `hoist`)
        // walks the body and replaces it with the inferred type. `Unknown` is the safe placeholder
        // any *other* function's inference sees in the meantime (forward refs degrade silently
        // rather than to a confidently-wrong `Nil`).
        let ret = decl
            .ret
            .as_ref()
            .map(|t| self.resolve_type(t, span))
            .unwrap_or(Ty::Unknown);
        self.exit_type_params(saved);
        // STATIC classification (the "no self ⇒ static" rule): a method whose first param is NOT
        // named `self` — or which has no params at all — is a static (associated) method, dispatched
        // `Type.method(args)`. This flag is consulted only when the sig is reached as a struct/enum
        // method; for a free fn it is meaningless (free fns are never reached via the method maps).
        let is_static = decl.params.first().is_none_or(|p| p.name != "self");
        FnSig {
            min_params: params.len(),
            params,
            ret,
            type_params: decl.type_params.clone(),
            is_static,
            doc: decl.doc.clone(),
        }
    }

    /// Pass-1.5: for every function/method that omitted `-> T`, infer its return type from the
    /// body and overwrite the provisional `Unknown` left by `fn_sig`. Runs after `hoist`, so all
    /// type names, variants, and (provisional) function sigs are already visible to the inference.
    ///
    /// Inference is ORDER-INDEPENDENT: a single source-order pass would bail to `Unknown` whenever
    /// the deciding return is a call to a not-yet-inferred function (a forward reference or mutual
    /// recursion), leaking an unsound permissive `Unknown` into a typed slot. Instead this runs the
    /// per-pass walk (`infer_returns_pass`) repeatedly to a FIXPOINT: each pass re-infers every
    /// un-annotated fn/method, and because a callee's resolved `FnSig.ret` is written back
    /// immediately, a later pass sees the earlier pass's resolutions. The iteration is MONOTONE — a
    /// pass only ever turns an `Unknown` ret into a concrete one (or detects a conflict via pass-2),
    /// and a concrete ret is never reverted to `Unknown` — so it converges. The cap
    /// (`un-annotated count + 1`) bounds the longest forward-ref resolution chain and guarantees
    /// termination on genuinely un-inferable cases (pure recursion / mutual recursion with no
    /// concrete base, where the ret stays `Unknown` forever). Such a residual `Unknown` stays
    /// permissive (same as the pre-fixpoint behavior) — it is NOT rejected here: a blanket
    /// "leftover Unknown ⇒ require annotation" check over-reaches, because a bare `Unknown` ret is
    /// also produced by non-recursive paths (e.g. `return x[0]` of an empty-collection literal) and
    /// by already-errored bodies. Rejecting the genuinely-un-inferable recursive case soundly needs
    /// call-graph cycle detection; tracked as a follow-up gap.
    fn infer_returns(&mut self, stmts: &[Stmt]) {
        // Bound: each productive pass resolves at least one more `Unknown`→concrete; `+1` lets the
        // final pass confirm no change (the fixpoint). A non-productive pass breaks the loop early.
        let cap = self.count_uninferred(stmts) + 1;
        for _ in 0..cap {
            if !self.infer_returns_pass(stmts) {
                break;
            }
        }
    }

    /// Count the un-annotated free fns + struct/enum methods that `infer_returns` infers — the
    /// fixpoint iteration bound. (Annotated decls are skipped by the pass, so they cannot extend the
    /// chain.)
    fn count_uninferred(&self, stmts: &[Stmt]) -> usize {
        let mut n = 0;
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() => n += 1,
                StmtKind::Struct { methods, .. } | StmtKind::Enum { methods, .. } => {
                    n += methods.iter().filter(|m| m.ret.is_none()).count();
                }
                _ => {}
            }
        }
        n
    }

    /// One inference pass over every un-annotated fn/method. Re-infers each from the body (idempotent
    /// per the truncate-errors model in `infer_fn_ret`) and writes the result back into the stored
    /// `FnSig.ret` immediately, so a callee resolved earlier in THIS pass is already visible to a
    /// caller later in the pass. Returns `true` iff any stored ret changed (drives the fixpoint).
    fn infer_returns_pass(&mut self, stmts: &[Stmt]) -> bool {
        let mut changed = false;
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) if decl.ret.is_none() => {
                    let Some(sig) = self.functions.get(&decl.name).cloned() else {
                        continue;
                    };
                    let ret = self.infer_fn_ret(decl, None, &sig.params);
                    if let Some(sig) = self.functions.get_mut(&decl.name)
                        && sig.ret != ret
                    {
                        sig.ret = ret;
                        changed = true;
                    }
                }
                StmtKind::Struct {
                    name,
                    type_params,
                    methods,
                    ..
                } => {
                    let self_ty = self.struct_self_ty(name);
                    // The layout is stored under the runtime key (bare unless disambiguated); a bare
                    // `name` lookup misses in the multi-module path, so the inferred ret would be written
                    // to a non-existent slot and never reach call sites / protocol satisfaction.
                    let key = self.bare_key(name);
                    let saved = self.enter_type_params(type_params);
                    for m in methods {
                        if m.ret.is_some() {
                            continue;
                        }
                        let Some(sig) = self
                            .structs
                            .get(&key)
                            .and_then(|s| s.methods.get(&m.name))
                            .cloned()
                        else {
                            continue;
                        };
                        let ret = self.infer_fn_ret(m, Some(self_ty.clone()), &sig.params);
                        if let Some(ms) = self
                            .structs
                            .get_mut(&key)
                            .and_then(|s| s.methods.get_mut(&m.name))
                            && ms.ret != ret
                        {
                            ms.ret = ret;
                            changed = true;
                        }
                    }
                    self.exit_type_params(saved);
                }
                StmtKind::Enum {
                    name,
                    type_params,
                    methods,
                    ..
                } => {
                    // Mirror the struct arm for enum methods (same un-annotated-return inference): read
                    // and write the inferred ret into `enum_methods` under the enum's runtime key.
                    let self_ty = self.enum_self_ty(name);
                    let key = self.bare_key(name);
                    let saved = self.enter_type_params(type_params);
                    for m in methods {
                        if m.ret.is_some() {
                            continue;
                        }
                        let Some(sig) = self
                            .enum_methods
                            .get(&key)
                            .and_then(|ms| ms.get(&m.name))
                            .cloned()
                        else {
                            continue;
                        };
                        let ret = self.infer_fn_ret(m, Some(self_ty.clone()), &sig.params);
                        if let Some(ms) = self
                            .enum_methods
                            .get_mut(&key)
                            .and_then(|ms| ms.get_mut(&m.name))
                            && ms.ret != ret
                        {
                            ms.ret = ret;
                            changed = true;
                        }
                    }
                    self.exit_type_params(saved);
                }
                _ => {}
            }
        }
        changed
    }

    /// Infer one function's return type by walking its body in inference mode: every `return`'s
    /// type is collected by `check_return` (with errors suppressed — pass 2 re-reports for real).
    /// The pick rule, in order:
    /// - first concrete non-`nil` return wins (pass 2 then validates the rest against it);
    /// - else, if any value-return was uncertain (`Unknown` — a forward ref to a not-yet-inferred
    ///   function, or a self-recursive call) → `Unknown` for THIS pass, so the function stays
    ///   permissive instead of producing spurious errors; the enclosing fixpoint (`infer_returns`)
    ///   then re-infers it on a later pass once the callee resolves;
    /// - else (only bare `return`s / no returns at all) → `nil` (void preserved).
    ///
    /// One pass is order-dependent (a call to a not-yet-inferred function yields `Unknown`), but
    /// `infer_returns` iterates this to a FIXPOINT, so the FINAL stored ret is order-independent: a
    /// forward-ref / mutually-recursive callee resolves on a later pass. Only a genuinely
    /// un-inferable function (no concrete base anywhere) stays `Unknown` after convergence — that
    /// residual stays permissive (not rejected; soundly rejecting it needs call-graph cycle
    /// detection — a follow-up).
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
            if param.is_ref {
                self.declare_ref(&param.name);
            }
        }
        // An inline-expr body (`fn a(): <expr>`) implicitly returns its single expression, so its
        // type IS the inferred return (mirroring a closure body) — there is no `return` to collect.
        let inline_ret = if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            Some(self.infer(e))
        } else {
            for stmt in &decl.body {
                self.check_stmt(stmt);
            }
            None
        };
        self.pop_scope();
        let found = std::mem::replace(&mut self.collected_rets, saved_rets);
        self.inferring_ret = saved_flag;
        self.current_ret = saved_ret;
        self.exit_type_params(saved_tps);
        self.errors.truncate(mark); // discard inference-time errors; pass 2 re-reports them for real
        if let Some(t) = inline_ret {
            t
        } else if let Some(t) = found.iter().find(|t| !t.is_unknown() && **t != Ty::Nil) {
            t.clone()
        } else if found.iter().any(|t| t.is_unknown()) {
            // A value-return we couldn't pin (forward ref / recursion): stay permissive, not `nil`.
            Ty::Unknown
        } else {
            Ty::Nil
        }
    }

    /// Validate the pointee type of a `ref T` binding/param. `ref` lowers to a `Ref[T]` box, so the
    /// pointee must be a concrete (monomorphic) type: a bare in-scope generic parameter is rejected
    /// (use a first-class `Ref[T]` field/param for a generic box). Other unboxable shapes do not arise
    /// — the parser already bars `ref` from collection-element / return / field positions.
    fn check_ref_ty(&mut self, t: &Type, span: Span) {
        if let Type::Named { name: n, .. } = t
            && self.type_params.contains_key(n)
        {
            self.error(
                span,
                format!(
                    "`ref {n}` is not allowed: a `ref` binding cannot point at the generic type parameter '{n}' — use a first-class `Ref[{n}]` instead"
                ),
            );
        }
    }

    /// Resolve an AST `Type` annotation into a checker `Ty`, reporting unknown type names.
    /// The "unknown type T" message, with a module-scoped import hint when `T` is declared by some
    /// (un-imported) module: a type is private to its declaring module and must be imported. Picks
    /// the first declaring module in graph (deps-first) order.
    fn unknown_type_msg(&self, n: &str) -> String {
        if let Some(mods) = self.types_by_name.get(n)
            && let Some(m) = mods.first()
        {
            format!("unknown type '{n}'; import it from {m} (`import {n} from {m}`)")
        } else {
            format!("unknown type '{n}'")
        }
    }

    /// True iff the bare runtime concurrency ctor/TYPE name `n` (`Shared`/`RwShared`/`Atomic`/
    /// `Executor`) is usable in the current module: either this module imported it from
    /// `std.concurrency`, or we're inside a privileged stdlib module (std/* may use the four bare —
    /// `std/cancel.chz`, `std/concurrency/collection.chz`). The four ALSO stay reserved names (can't be
    /// shadowed by a user `struct`); this gate is the SEPARATE "must import to USE" requirement.
    fn concurrency_licensed(&self, n: &str) -> bool {
        self.imported_concurrency.contains(n) || self.current_module_is_stdlib
    }

    /// True iff the opcode-backed `timer` builtin name `n` is usable in the current module: either this
    /// module imported it from `std.time` (whole-module or per-name), or we're inside a privileged
    /// stdlib module (std/* may call `timer` bare — e.g. `std/cancel.chz`). `timer` ALSO stays a
    /// reserved name (can't be shadowed by a user `struct`/`fn`); this gate is the SEPARATE "must import
    /// to USE" requirement.
    fn time_licensed(&self, n: &str) -> bool {
        self.imported_time.contains(n) || self.current_module_is_stdlib
    }

    /// True iff the std.net TCP handle TYPE name `n` (`Socket`/`Listener`) is usable in the current
    /// module: either this module imported it from `std.net` (whole-module or per-name), or we're
    /// inside a privileged stdlib module (std/* may use the two bare). The two ALSO stay reserved names
    /// (can't be shadowed by a user `struct`); this gate is the SEPARATE "must import to USE" requirement.
    fn net_licensed(&self, n: &str) -> bool {
        self.imported_net.contains(n) || self.current_module_is_stdlib
    }

    /// Map an opaque/native builtin TYPE name (one that lives ONLY in its owning std module's
    /// `sig.types` — reserved, so no user module can export it) to its builtin `Ty`, given already-
    /// resolved type args. This is the additive "make a Rust type reachable by qualified path"
    /// lever: `concurrency.Shared[int]` -> `Ty::shared(int)`, `net.Socket` -> `Ty::Socket`,
    /// `ffi.int32` -> `Ty::Int`, etc. Returns `None` for a `sig.types` name that is NOT a builtin
    /// TYPE in type position (`timer`, a function — the caller emits the "function, not a type"
    /// error). Arity is NOT validated here — the mutable `resolve_type` caller emits arity
    /// diagnostics; the read-only `resolve_qualified_ro` caller is permissive (no errors). Shared
    /// by both qualified resolvers so the mapping can never drift between them.
    fn qualified_builtin_ty(&self, name: &str, args: &[Ty]) -> Option<Ty> {
        let one = || args.first().cloned().unwrap_or(Ty::Unknown);
        match name {
            "Shared" => Some(Ty::shared(one())),
            "RwShared" => Some(Ty::rwshared(one())),
            "Atomic" => Some(Ty::atomic(one())),
            "Executor" => Some(Ty::Executor),
            "Socket" => Some(Ty::Socket),
            "Listener" => Some(Ty::Listener),
            "ptr" => Some(Ty::Ptr),
            _ if crate::native::ffi::TYPE_NAMES.contains(&name) => Some(Ty::Int),
            _ => None,
        }
    }

    /// Expected type-argument arity for a builtin TYPE reached by qualified path (the generic
    /// concurrency boxes take exactly one; every other in-scope native type is non-generic).
    fn qualified_builtin_arity(name: &str) -> usize {
        match name {
            "Shared" | "RwShared" | "Atomic" => 1,
            _ => 0,
        }
    }

    fn resolve_type(&mut self, t: &Type, span: Span) -> Ty {
        match t {
            Type::Named {
                name: n,
                span: name_span,
            } => {
                let resolved = match n.as_str() {
                    "int" => Ty::Int,
                    "float" => Ty::Float,
                    "bool" => Ty::Bool,
                    "str" => Ty::Str,
                    "bytes" => Ty::Bytes,
                    "bytearray" => Ty::ByteArray,
                    "nil" => Ty::Nil,
                    // A generic type parameter (`T`) or `Self`, in scope while checking a generic fn
                    // signature/body or a protocol method. Resolved BEFORE every reserved/module name
                    // below (ptr / owned_str / Executor / Shared|RwShared|Atomic / Socket / Listener)
                    // and before user structs/aliases, so an in-scope type param uniformly shadows them
                    // (e.g. `fn id[Executor](x: Executor)`), instead of being hijacked by the builtin
                    // arm or mis-erroring with a bogus import hint. Placed below the scalar primitives
                    // (int/float/bool/str/…) so those keep resolving to their scalar even when used as a
                    // type-param name (`fn id[int](x: int)` → x is `int`), preserving existing behavior.
                    _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                    // An opaque C-ABI pointer handle — the marshalling primitive for `extern "lib":`
                    // signatures. Like the fixed-width FFI integer names below, `ptr` is NOT a global
                    // builtin: it resolves only in a module that imported `std.ffi` (whole-module
                    // `import std.ffi` OR selective `import ptr from std.ffi`), OR via a LICENSED
                    // transparent alias body (`ffi_alias_ok`). Since extern blocks use `ptr` pervasively,
                    // the hint points at the whole-module form. See `Ty::Ptr`.
                    "ptr" => {
                        if self.imported_ffi_types.contains("ptr")
                            || self
                                .alias_resolving
                                .last()
                                .is_some_and(|a| self.ffi_alias_ok.contains(a))
                        {
                            Ty::Ptr
                        } else {
                            self.error(
                                span,
                                "unknown type 'ptr' (import it from std.ffi: `import std.ffi`)"
                                    .to_string(),
                            );
                            Ty::Unknown
                        }
                    }
                    // A RETURN-ONLY C-ABI marshalling type name (sibling of `ptr`): an OWNED `char*`
                    // the runtime copies into a `str` and then frees. To the program it IS a plain `str`
                    // (the ownership/free is a runtime-only distinction the backends recover via
                    // `ctype_of`); the return-only-ness is enforced by a surface guard in the extern
                    // param loop (an `owned_str` parameter is rejected before this collapses to `Str`).
                    // Bare use OUTSIDE an extern signature is rejected: it is not a general type — left
                    // ungated it would silently collapse a `fn f(x: owned_str) -> owned_str` to `str`
                    // with no import (mirrors how `ptr` errors when used without `import std.ffi`).
                    "owned_str" => {
                        if self.in_extern_sig {
                            Ty::Str
                        } else {
                            self.error(
                            span,
                            "'owned_str' is a return-only extern marshalling type and cannot be \
                             used as a general type annotation"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // The C5 escape hatch handle, non-generic (a bare `Executor` type annotation). Like
                    // `Shared`/`RwShared`/`Atomic` below, `Executor` is NOT a global builtin: it resolves
                    // only in a module that imported `std.concurrency` (whole-module or per-name). The name
                    // STAYS reserved (no user `struct Executor`); this is the separate import requirement.
                    "Executor" => {
                        if self.concurrency_licensed("Executor") {
                            Ty::Executor
                        } else {
                            self.error(
                            span,
                            "unknown type 'Executor' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // A BARE (no type-arg) `Shared`/`RwShared`/`Atomic` annotation. Like `Executor`
                    // above these names are NOT global builtins and STAY reserved; but they are generic,
                    // so a bare write is either unlicensed (→ same import hint the `Shared[T]` arm below
                    // emits) or licensed-but-missing-its-type-arg (→ the same missing-type-arg message a
                    // bare user-generic struct/enum/newtype gets). Placed before the catch-all so it
                    // can't fall through to a hint-less "unknown type". (An in-scope type param of the
                    // same name, e.g. `fn f[Shared]`, was already resolved by the hoisted `type_params`
                    // arm above this match, so no per-arm guard is needed here.)
                    n @ ("Shared" | "RwShared" | "Atomic") => {
                        if self.concurrency_licensed(n) {
                            self.error(
                                span,
                                format!("type '{n}' expects 1 type argument(s), got 0"),
                            );
                        } else {
                            self.error(
                            span,
                            format!(
                                "unknown type '{n}' (import it from std.concurrency: `import std.concurrency`)"
                            ),
                        );
                        }
                        Ty::Unknown
                    }
                    // D6 — the std.net TCP handles, non-generic (bare `Socket` / `Listener` annotations).
                    // Like `Executor`/`Shared`/`ptr` above, these are NOT global builtins: they resolve
                    // only in a module that imported `std.net` (whole-module or per-name). The names STAY
                    // reserved (no user `struct Socket`); this is the separate import requirement. (An
                    // in-scope type param of the same name was already resolved by the hoisted `type_params`
                    // arm above this match.)
                    n @ ("Socket" | "Listener") => {
                        if self.net_licensed(n) {
                            if n == "Socket" {
                                Ty::Socket
                            } else {
                                Ty::Listener
                            }
                        } else {
                            self.error(
                                span,
                                format!(
                                    "unknown type '{n}' (import it from std.net: `import std.net`)"
                                ),
                            );
                            Ty::Unknown
                        }
                    }
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
                    // A `from`-imported type alias resolves to its pre-resolved body (computed in the
                    // defining module's scope). A licensed FFI-width alias was already re-seeded into
                    // `ffi_alias_ok`, but since the body is already a concrete `Ty` no width re-check is
                    // needed here.
                    _ if self.imported_alias_tys.contains_key(n) => {
                        self.imported_alias_tys[n].clone()
                    }
                    // Fixed-width C-ABI integer marshalling type names (`int8`..`uint64`) — Chezzi's first
                    // type imports. Each resolves to a plain `int` (`Ty::Int`) — the width/signedness is a
                    // runtime-only marshalling distinction the backends recover via `ctype_of`, and they're
                    // BIDIRECTIONAL (valid as both param and return). But they are NOT global builtins: a
                    // width name resolves only in a module that imported it per-name from `std.ffi`
                    // (`import int32 from std.ffi` → `imported_ffi_types`). Otherwise it's an unknown type
                    // with an FFI-specific hint (matches the qualified-variant "write it qualified" style).
                    _ if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) => {
                        // Accept the width name if THIS module imported it, OR if we reached it by
                        // expanding a LICENSED transparent alias body — one whose defining module
                        // imported the width (`ffi_alias_ok`). A `type Len = int32` is a deliberate
                        // opt-in that stays valid wherever the alias is used, including cross-module
                        // (the alias is program-global but the per-module import set is not). A bare
                        // width name in ordinary code still needs the import — and crucially an alias
                        // whose module never imported the width does NOT launder it (the closed gate
                        // hole): only a licensed alias indirection bypasses the per-module requirement.
                        if self.imported_ffi_types.contains(n)
                            || self
                                .alias_resolving
                                .last()
                                .is_some_and(|a| self.ffi_alias_ok.contains(a))
                        {
                            Ty::Int
                        } else {
                            self.error(
                            span,
                            format!(
                                "unknown type '{n}' (import it from std.ffi: `import {n} from std.ffi`)"
                            ),
                        );
                            Ty::Unknown
                        }
                    }
                    _ if self.struct_names.contains(n) => {
                        // The layout is keyed by the runtime key (bare unless disambiguated); the written
                        // name's bare-visibility is the `struct_names` gate above. Carry the key on the Ty.
                        let key = self.bare_key(n);
                        // A generic struct written without type arguments is missing them.
                        let nparams = self.structs.get(&key).map_or(0, |i| i.type_params.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::strukt(key)
                    }
                    _ if self.enum_names.contains(n) => {
                        let key = self.bare_key(n);
                        // A generic enum written without type arguments is missing them.
                        let nparams = self.enum_type_params.get(&key).map_or(0, |tps| tps.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::Enum(key, Vec::new())
                    }
                    _ if self.newtype_names.contains(n) => {
                        let key = self.bare_key(n);
                        // A generic newtype written without type arguments is missing them.
                        let nparams = self
                            .newtype_type_params
                            .get(&key)
                            .map_or(0, |tps| tps.len());
                        if nparams > 0 {
                            self.error(
                                span,
                                format!("type '{n}' expects {nparams} type argument(s), got 0"),
                            );
                        }
                        Ty::NewType(key, Vec::new())
                    }
                    // A protocol name used as a value type (existential), e.g. `Error`.
                    _ if self.protocols.contains_key(n) => Ty::Protocol(n.clone()),
                    _ => {
                        self.error(span, self.unknown_type_msg(n));
                        Ty::Unknown
                    }
                };
                // Editor hover (LSP): record the resolved type at the name token's OWN span (NOT
                // the enclosing-annotation `span` param). Probe-gated so off-probe checks pay
                // nothing; gated on `!generic_arg_prepass` so the generic-arg unification prepass
                // can't first-hit-wins latch an incomplete type. Composite inner names
                // (`int` in `List[int]`) record via this same arm when resolve_type recurses.
                // A builtin/stdlib type name (`str`/`bytes`/bare `Shared`/…) carries its
                // `builtin_type_doc` usage+methods blurb (Tier C); a user type falls back to its
                // `name_docs` docstring — for an imported non-generic type used as `x: Foo`, the
                // import-line binding seeded `name_docs[Foo]` (its decl docstring or a kind+module
                // blurb). A current-module user type's own docstring also surfaces here (additive;
                // the Tier-A decl-name hover already covers the decl site). `builtin_type_doc` stays
                // FIRST so a builtin name is never shadowed by a same-named `name_docs` entry.
                if self.hover_probe.is_some() && !self.generic_arg_prepass {
                    // Suppress the `name_docs` fallback when the name resolved to a type PARAMETER
                    // (a generic `[T]` in scope) that merely SHADOWS a same-named top-level decl —
                    // the param is an unrelated entity and must not borrow that decl's docstring
                    // (mirrors the value-ident hover's scope guard). `builtin_type_doc` is unaffected
                    // (it never names a param). A real generic head is never a `Ty::Param`, so this
                    // is a no-op at the head site.
                    let doc = builtin_type_doc(n).or_else(|| {
                        if matches!(resolved, Ty::Param(_)) {
                            None
                        } else {
                            self.name_docs.get(n).cloned()
                        }
                    });
                    self.hover_record_at(*name_span, &resolved, HoverKind::Type, doc);
                }
                resolved
            }
            Type::Func { params, ret } => Ty::Func {
                params: params.iter().map(|p| self.resolve_type(p, span)).collect(),
                ret: Box::new(self.resolve_type(ret, span)),
            },
            Type::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| self.resolve_type(t, span)).collect()),
            Type::Generic(n, args, head_span) => {
                let resolved = match (n.as_str(), args.as_slice()) {
                    ("List", [inner]) => Ty::list(self.resolve_type(inner, span)),
                    ("Result", [inner]) => Ty::result(self.resolve_type(inner, span)),
                    ("Result", [t, e]) => {
                        Ty::result_e(self.resolve_type(t, span), self.resolve_type(e, span))
                    }
                    ("Option", [inner]) => Ty::option(self.resolve_type(inner, span)),
                    // `Iterator[T]` as a *value* type — the result of calling a generator function.
                    // Represented as `Ty::Struct("Iterator", [T])`, an existential iterator whose element
                    // type `iter_elem` recovers (so `for`-loops and `[S: Iterator[T]]` bounds accept it).
                    // Experimental: only generators produce these; ordinary code still uses adapter
                    // structs / built-in collections.
                    ("Iterator", [elem]) => {
                        Ty::Struct("Iterator".to_string(), vec![self.resolve_type(elem, span)])
                    }
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
                    // through `get`/`set`; the *handle* is what crosses (always sendable). NOT a global
                    // builtin: requires `import std.concurrency` (the inner type is still resolved on the
                    // unlicensed path so a nested error surfaces; the name STAYS reserved). Same for the
                    // RwShared/Atomic siblings below.
                    ("Shared", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if self.concurrency_licensed("Shared") {
                            Ty::shared(elem)
                        } else {
                            self.error(
                            span,
                            "unknown type 'Shared' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // `RwShared[T]` (type annotation): the cross-task read-write box. Like `Shared`, its
                    // element type isn't gated on sendability — the handle is what crosses.
                    ("RwShared", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if self.concurrency_licensed("RwShared") {
                            Ty::rwshared(elem)
                        } else {
                            self.error(
                            span,
                            "unknown type 'RwShared' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    // `Atomic[T]` (type annotation): the cross-task atomic box. Like `Shared`, its element
                    // type isn't gated on sendability — the handle is what crosses.
                    ("Atomic", [inner]) => {
                        let elem = self.resolve_type(inner, span);
                        if self.concurrency_licensed("Atomic") {
                            Ty::atomic(elem)
                        } else {
                            self.error(
                            span,
                            "unknown type 'Atomic' (import it from std.concurrency: `import std.concurrency`)"
                                .to_string(),
                        );
                            Ty::Unknown
                        }
                    }
                    ("Map", [k, v]) => {
                        let key = self.resolve_type(k, span);
                        let value = self.resolve_type(v, span);
                        if !self.is_hashable_key(&key) {
                            self.error(
                            span,
                            format!("Map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {key}"),
                        );
                        }
                        Ty::map(key, value)
                    }
                    ("Set", [t]) => {
                        let elem = self.resolve_type(t, span);
                        if !self.is_hashable_key(&elem) {
                            self.error(
                            span,
                            format!("Set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {elem}"),
                        );
                        }
                        Ty::set(elem)
                    }
                    // A user-defined generic struct instantiated with type arguments: `Pair[int, str]`.
                    _ if self.struct_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        // Clone the param list out so the borrow on `self.structs` is dropped before
                        // the `satisfies`/`error` calls below.
                        let tps = self.structs.get(&key).map(|i| i.type_params.clone());
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
                        Ty::Struct(key, resolved)
                    }
                    // A user-defined generic enum instantiated with type arguments: `Tree[int]`.
                    _ if self.enum_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let tps = self.enum_type_params.get(&key).cloned();
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
                        Ty::Enum(key, resolved)
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
                    // A user-defined generic newtype instantiated with type arguments: `Stack[int]`.
                    _ if self.newtype_names.contains(n) => {
                        let key = self.bare_key(n);
                        let resolved: Vec<Ty> =
                            args.iter().map(|a| self.resolve_type(a, span)).collect();
                        let tps = self.newtype_type_params.get(&key).cloned();
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
                        Ty::NewType(key, resolved)
                    }
                    _ => {
                        self.error(span, format!("unknown generic type '{n}'"));
                        Ty::Unknown
                    }
                };
                // Editor hover (LSP, Tier C): record the resolved type at the GENERIC HEAD-name
                // token's OWN span (`List`/`Heap` in `List[int]`/`Heap[int]`) — the gap the bare
                // `Type::Named` arm already covers for non-generic heads. A builtin/stdlib head
                // (`List`/`Map`/`Set`/`Channel`/…) carries its `builtin_type_doc` usage+methods
                // blurb; a user head (an imported generic struct/enum/newtype like `Heap`) falls back
                // to its `name_docs` docstring (the import-line binding seeds that entry). Probe-gated
                // so off-probe checks pay nothing; gated `!generic_arg_prepass` so the generic-arg
                // unification prepass can't first-hit-wins latch an incomplete type.
                if self.hover_probe.is_some() && !self.generic_arg_prepass {
                    // Suppress the `name_docs` fallback when the name resolved to a type PARAMETER
                    // (a generic `[T]` in scope) that merely SHADOWS a same-named top-level decl —
                    // the param is an unrelated entity and must not borrow that decl's docstring
                    // (mirrors the value-ident hover's scope guard). `builtin_type_doc` is unaffected
                    // (it never names a param). A real generic head is never a `Ty::Param`, so this
                    // is a no-op at the head site.
                    let doc = builtin_type_doc(n).or_else(|| {
                        if matches!(resolved, Ty::Param(_)) {
                            None
                        } else {
                            self.name_docs.get(n).cloned()
                        }
                    });
                    self.hover_record_at(*head_span, &resolved, HoverKind::Type, doc);
                }
                resolved
            }
            // A module-qualified type `module.Type[args]` (mirrors how a function is reached via its
            // bound module name). Resolve `module` in `imported_modules` → the target's `ModuleSig`,
            // confirm the type exists there, and return the matching `Ty`. Enforces arity for generic
            // struct/enum targets.
            Type::Qualified { module, name, args } => {
                let resolved: Vec<Ty> = args.iter().map(|a| self.resolve_type(a, span)).collect();
                let Some(mid) = self.imported_modules.get(module).cloned() else {
                    self.error(
                        span,
                        format!("unknown module '{module}' (import it to use `{module}.{name}`)"),
                    );
                    return Ty::Unknown;
                };
                let Some(sig) = self.module_sigs.get(&mid).cloned() else {
                    self.error(span, format!("module '{module}' has no type '{name}'"));
                    return Ty::Unknown;
                };
                if let Some(info) = sig.struct_defs.get(name) {
                    if info.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                info.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::Struct(self.type_key(&mid, name), resolved)
                } else if let Some(edef) = sig.enum_defs.get(name) {
                    if edef.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                edef.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::Enum(self.type_key(&mid, name), resolved)
                } else if let Some(ntdef) = sig.newtype_defs.get(name) {
                    if ntdef.type_params.len() != resolved.len() {
                        self.error(
                            span,
                            format!(
                                "type '{module}.{name}' expects {} type argument(s), got {}",
                                ntdef.type_params.len(),
                                resolved.len()
                            ),
                        );
                    }
                    Ty::NewType(self.type_key(&mid, name), resolved)
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    asig.body.clone()
                } else if sig.types.contains(name) {
                    // An opaque/native builtin TYPE reached by qualified path (`concurrency.Shared[int]`,
                    // `net.Socket`, `ffi.int32`/`ffi.ptr`). These names live ONLY in their owning std
                    // module's `sig.types` (reserved — no user module can export them), so this branch
                    // fires solely for native builtins; user struct/enum/newtype/alias names were already
                    // consumed by the def-map arms above. The arm required the module be imported (the
                    // `imported_modules.get` above), so the import gate is unchanged: a non-imported
                    // module still hit the `unknown module` error before reaching here. ADDITIVE — the
                    // bare-after-import path (`imported_concurrency`/`_net`/`_ffi_types`) is untouched.
                    if name == "timer" {
                        self.error(
                            span,
                            "'timer' is a function, not a type — call it: `time.timer(ms)`"
                                .to_string(),
                        );
                        Ty::Unknown
                    } else if let Some(ty) = self.qualified_builtin_ty(name, &resolved) {
                        let expected = Self::qualified_builtin_arity(name);
                        if resolved.len() != expected {
                            self.error(
                                span,
                                format!(
                                    "type '{module}.{name}' expects {expected} type argument(s), got {}",
                                    resolved.len()
                                ),
                            );
                        }
                        ty
                    } else {
                        self.error(span, format!("module '{module}' has no type '{name}'"));
                        Ty::Unknown
                    }
                } else {
                    self.error(span, format!("module '{module}' has no type '{name}'"));
                    Ty::Unknown
                }
            }
        }
    }

    // ===== pass 2: check statements =====

    fn check_block(&mut self, block: &Block) {
        // PERSISTENT refine-on-first-use (scope-wide first-use pinning): `check_block` runs every
        // CONDITIONALLY-executed STATEMENT body (an `if`/`else if`/`else` branch, a `while` body, a
        // `defer:` block). A refine-on-first-use narrowing of an OUTER binding performed inside this
        // body PERSISTS — the first mutating op that fixes an empty collection's element/key/value
        // type pins it for the binding's whole scope, even across sibling branches and past the
        // branch. `repin` writes the pin to the binding's OWNING scope, so it survives `pop_scope`
        // (which only removes inner-block-declared bindings, not the outer owner). Building a
        // heterogeneous collection split across branches/arms is therefore now a type error, exactly
        // like the literal `[1, "s"]`. Lexical scoping is intact: a binding DECLARED in this block is
        // still removed by `pop_scope`; only an OUTER binding's first-use pin persists. (Expression-
        // position arms — `infer_if_else`/`infer_match` — keep their snapshot/restore barrier: a pin
        // in one value-arm must not leak to a sibling value-arm, that being the narrow residual.)
        self.push_scope();
        for stmt in block {
            self.check_stmt(stmt);
        }
        self.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        let span = stmt.span;
        match &stmt.kind {
            StmtKind::Let {
                names,
                name_spans,
                ty,
                value,
                is_ref,
                ..
            } => {
                let is_ref = *is_ref;
                // A closure bound to a `fn`-typed annotation is inferred in checking-mode (source #1):
                // resolve the annotation first so its unannotated params bind to the slot's param
                // types. Only the single-name, non-`ref`, `fn`-typed case (a `ref` pointee is never a
                // closure; destructuring never binds one). Otherwise ordinary bottom-up inference.
                let val_ty = match ty {
                    Some(t)
                        if !is_ref
                            && names.len() == 1
                            && matches!(value.kind, ExprKind::Closure { .. }) =>
                    {
                        let expected = self.resolve_type(t, span);
                        if matches!(expected, Ty::Func { .. }) {
                            self.infer_arg(value, Some(&expected))
                        } else {
                            self.infer_value(value)
                        }
                    }
                    // Expected-type checking-mode for a NON-closure value bound to an annotation:
                    // thread the annotation as a hint into the value's inference so a generic
                    // ctor / generic fn-call pre-seeds its type params from it — `a: Heap[int] =
                    // Heap([], fn(x, y): x < y)` pins `T=int`, which then pins the comparator's
                    // params. A `ref` binding is excluded (its init was desugared to a `Ref(...)`
                    // wrapper whose param is the pointee, not this annotation). `infer_call` clears
                    // the hint, but pair the set with an immediate clear so a non-call value never
                    // leaks it into the next statement.
                    Some(t) if !is_ref && names.len() == 1 => {
                        let expected = self.resolve_type(t, span);
                        self.expected_hint = Some(expected);
                        let vt = self.infer_value(value);
                        self.expected_hint = None;
                        vt
                    }
                    _ => self.infer_value(value),
                };
                if names.len() > 1 {
                    // destructuring let `a, b := expr` — `expr` must be a tuple of matching arity.
                    self.check_destructure(names, name_spans, &val_ty, value.span);
                    return;
                }
                let name = &names[0];
                let declared = match ty {
                    Some(t) => {
                        // A `ref T` binding is, at runtime, a `Ref[T]` box (the desugar pass lowered
                        // the init to `Ref(v)`/alias and every read to `.get()`); so the binding's
                        // checker type is `Ref[T]`, not `T`. The pointee element type `T` is rejected
                        // if it is not boxable (e.g. a bare unconstrained generic) by `check_ref_ty`.
                        let expected = if is_ref {
                            self.check_ref_ty(t, span);
                            self.resolve_type(
                                &Type::Generic("Ref".to_string(), vec![t.clone()], Span::default()),
                                span,
                            )
                        } else {
                            self.resolve_type(t, span)
                        };
                        if !self.assignable_w(&expected, &val_ty, true) {
                            // Transparency: a `ref T` binding's mismatch renders `ref T`, not `Ref[T]`.
                            let exp = if is_ref {
                                ref_display(&expected)
                            } else {
                                expected.to_string()
                            };
                            self.error(
                                value.span,
                                format!("cannot assign {val_ty} to variable of type {exp}"),
                            );
                        }
                        expected
                    }
                    None => val_ty,
                };
                // EDITOR HOVER: the let-binding target (`x` in `x := …`) is a NAME, not an `Expr` the
                // probe visits during `infer`; record it here. The statement span starts at the first
                // binding name, so it is that token's position (single-name let — the common case).
                if self.hover_probe.is_some() {
                    // doc surfaces at the binding site too, but ONLY for a top-level `let` (the module
                    // scope is then the lone open scope). A function-local/block let that shadows a
                    // documented global's name must NOT borrow that global's doc (`name_docs` is keyed
                    // by bare name); such a local has no doc of its own.
                    let doc = if self.scopes.len() == 1 {
                        self.name_docs.get(name).cloned()
                    } else {
                        None
                    };
                    self.hover_record_at(span, &declared, HoverKind::Local, doc);
                }
                self.declare(name, declared);
                if is_ref {
                    self.declare_ref(name);
                }
            }
            StmtKind::Assign { target, op, value } => {
                // Checking-mode: a closure assigned to a `fn`-typed lvalue (a struct fn-field or a
                // fn-typed variable) binds its unannotated params from the target's type (source #1).
                let val_ty = if matches!(value.kind, ExprKind::Closure { .. }) {
                    // Discover the target's type for checking-mode WITHOUT diagnosing it: inferring
                    // an lvalue as an rvalue would run read-side gates (non-sendable-captured-binding
                    // read) and double-infer a Field/Index receiver. `check_assign` below is the sole
                    // validator of the target, so snapshot+truncate any errors this probe produces
                    // (mirrors the generic-arg recovery idiom).
                    let mark = self.errors.len();
                    let target_ty = self.infer(target);
                    self.errors.truncate(mark);
                    if matches!(target_ty, Ty::Func { .. }) {
                        self.infer_arg(value, Some(&target_ty))
                    } else {
                        self.infer_value(value)
                    }
                } else {
                    self.infer_value(value)
                };
                self.check_assign(target, *op, val_ty, span);
            }
            StmtKind::Fn(decl) => {
                if decl.is_test {
                    self.validate_test_fn_shape(decl, false);
                }
                // `.get` (not index) is panic-safe even when a redeclaration left a different sig.
                if let Some(sig) = self.functions.get(&decl.name).cloned() {
                    self.check_fn_body(decl, None, sig);
                }
            }
            StmtKind::Struct {
                name,
                name_span,
                type_params,
                fields,
                methods,
                doc,
            } => {
                let self_ty = self.struct_self_ty(name);
                // The struct's type parameters are in scope across its method bodies.
                let saved = self.enter_type_params(type_params);
                // Editor hover (decl-site): record the struct type at its declared-name token, with
                // the struct's doc-comment. Probe-gated no-op off the probe.
                if self.hover_probe.is_some() {
                    self.hover_record_at(*name_span, &self_ty, HoverKind::Struct, doc.clone());
                }
                // Editor hover: record each field's declared type at its DECL-site name span. Reads
                // the already-resolved field types out of `self.structs` (no re-`resolve_type`, so no
                // duplicate errors); gated on the probe so normal checks stay strictly zero-overhead.
                if self.hover_probe.is_some()
                    && let Some(info) = self.structs.get(&self.bare_key(name)).cloned()
                {
                    for field in fields {
                        if let Some((_, fty)) = info.fields.iter().find(|(n, _)| n == &field.name) {
                            self.hover_record_at(field.name_span, fty, HoverKind::Field, None);
                        }
                    }
                }
                // A constant-literal field default must be assignable to the field's type (checked
                // here so a wrong-typed default is caught at the declaration, not only when omitted).
                for field in fields {
                    if let Some(def) = &field.default {
                        let expected = self.resolve_type(&field.ty, def.span);
                        let actual = self.infer(def);
                        if !matches!(expected, Ty::Unknown)
                            && !self.assignable_w(&expected, &actual, true)
                        {
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
                // A struct with ≥1 `test fn` method is a test suite. Its lifecycle hooks
                // (before_all/after_all/before_each/after_each), when present, must be `fn name(self)`
                // returning nothing — validated here so the runner can trust the shape.
                let is_suite = methods.iter().any(|m| m.is_test);
                for m in methods {
                    if m.is_test {
                        self.validate_test_fn_shape(m, true);
                    } else if is_suite && is_lifecycle_hook(&m.name) {
                        self.validate_lifecycle_hook(m);
                    }
                    // Panic-safe: a redeclared struct name means `structs[key]` is a *different*
                    // struct whose method table may not contain `m.name`. Keyed by the runtime key
                    // (mirror the enum arm below) so the layout resolves in the multi-module path —
                    // otherwise a bare-`name` miss silently SKIPS body checking entirely.
                    if let Some(sig) = self
                        .structs
                        .get(&self.bare_key(name))
                        .and_then(|s| s.methods.get(&m.name))
                        .cloned()
                    {
                        self.record_method_decl_hover(m.name_span, &sig);
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
                self.exit_type_params(saved);
            }
            // Enum methods' bodies are checked here (mirroring the struct path); the variant/payload
            // shapes are validated during hoisting.
            StmtKind::Enum {
                name,
                name_span,
                type_params,
                methods,
                doc,
                ..
            } => {
                let self_ty = self.enum_self_ty(name);
                // The enum's type parameters are in scope across its method bodies.
                let saved = self.enter_type_params(type_params);
                // Editor hover (decl-site): record the enum type at its declared-name token + doc.
                if self.hover_probe.is_some() {
                    self.hover_record_at(*name_span, &self_ty, HoverKind::Struct, doc.clone());
                }
                let is_suite = methods.iter().any(|m| m.is_test);
                for m in methods {
                    if m.is_test {
                        self.validate_test_fn_shape(m, true);
                    } else if is_suite && is_lifecycle_hook(&m.name) {
                        self.validate_lifecycle_hook(m);
                    }
                    if let Some(sig) = self
                        .enum_methods
                        .get(&self.bare_key(name))
                        .and_then(|ms| ms.get(&m.name))
                        .cloned()
                    {
                        self.record_method_decl_hover(m.name_span, &sig);
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
                self.exit_type_params(saved);
            }
            // Newtype method bodies are checked here, mirroring the enum path (`self` is the newtype).
            StmtKind::NewType {
                name,
                name_span,
                type_params,
                methods,
                doc,
                ..
            } => {
                let self_ty = self.newtype_self_ty(name);
                let key = self.bare_key(name);
                // The newtype's type parameters are in scope across its method bodies (like the
                // struct/enum path), so a generic `fn peek(self) -> Option[T]` resolves `T`.
                let saved = self.enter_type_params(type_params);
                // Editor hover (decl-site): record the newtype at its declared-name token + doc.
                if self.hover_probe.is_some() {
                    self.hover_record_at(*name_span, &self_ty, HoverKind::Struct, doc.clone());
                }
                for m in methods {
                    if m.is_test {
                        // Parser rejects `test fn` in a newtype body, so this is unreachable; guard
                        // anyway to keep the suite invariants explicit.
                        self.validate_test_fn_shape(m, true);
                    }
                    if let Some(sig) = self
                        .newtype_defs
                        .get(&key)
                        .and_then(|(_, ms)| ms.get(&m.name))
                        .cloned()
                    {
                        self.record_method_decl_hover(m.name_span, &sig);
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
                self.exit_type_params(saved);
            }
            // A protocol's method signatures are validated during hoisting; pass 2 only records its
            // decl-site hover (the protocol existential at the protocol-name token + doc).
            StmtKind::Protocol {
                name,
                name_span,
                doc,
                ..
            } => {
                if self.hover_probe.is_some() {
                    let ty = Ty::Protocol(name.clone());
                    self.hover_record_at(*name_span, &ty, HoverKind::Struct, doc.clone());
                }
            }
            // A type alias is fully resolved during hoisting; pass 2 only records its decl-site hover.
            StmtKind::TypeAlias {
                name,
                name_span,
                doc,
                ..
            } => {
                // Editor hover (decl-site): record the ALIASED type at the alias-name token. Gated
                // strictly on the probe so the extra `resolve_type` never runs in normal checking; on
                // an invalid alias it may add a duplicate error, but hover returns None on ANY error,
                // so observable behavior is unchanged.
                if self.hover_probe.is_some()
                    && let Some(body) = self.aliases.get(name).cloned()
                {
                    let ty = self.resolve_type(&body, *name_span);
                    self.hover_record_at(*name_span, &ty, HoverKind::Struct, doc.clone());
                }
            }
            // Imports and extern blocks carry nothing to check in pass 2.
            StmtKind::Import(_) | StmtKind::Extern { .. } => {}
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    self.expect_bool(cond, "if condition");
                    self.check_block(body);
                }
                if let Some(body) = else_block {
                    self.check_block(body);
                }
            }
            StmtKind::For {
                vars,
                var_spans,
                iter,
                body,
            } => {
                let bindings = self.for_bindings(vars, iter);
                // PERSISTENT refine-on-first-use (see `check_block`): a refine-on-first-use pin of an
                // OUTER empty collection inside the loop body PERSISTS past the loop. We accept the
                // zero-trip / always-runs over-approximation by design — `xs:=[]; for i in []:
                // xs.push(1); xs.push("s")` REJECTS even though the body never runs at runtime; a
                // sound static over-approximation, matching "first statement that fixes the element
                // type records it". (No snapshot/restore here, so the pin written to the binding's
                // OWNING scope by `repin` survives `pop_scope`, which only removes the loop vars.)
                self.push_scope();
                // `bindings` is parallel to `vars` (and thus `var_spans`); zip truncates safely if the
                // lengths ever diverge (a binding's hover is dropped, never a panic).
                for ((name, ty), span) in bindings.into_iter().zip(var_spans.iter()) {
                    // EDITOR HOVER: the loop binding (`i` in `for i in …`) is a NAME, not an `Expr` the
                    // probe visits during `infer`; record its decl-site hover here (no-op unless armed).
                    self.hover_record_at(*span, &ty, HoverKind::Local, None);
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
            StmtKind::Yield(e) => self.check_yield(e, span),
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
                            if self.lookup(name).is_none()
                                && !self.functions.contains_key(name) =>
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
                // A `defer:` block compiles to a fresh child proto with an empty loop stack, so a
                // `break`/`continue` lexically nested in an enclosing loop but placed here is illegal
                // in both engines. Save-zero-restore `loop_depth` (mirroring `check_fn_body`) so the
                // `loop_depth == 0` guard at `StmtKind::Break`/`Continue` fires at check time; a
                // legitimate loop INSIDE the block re-increments from 0, keeping its own break legal.
                let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
                self.check_block(body);
                self.loop_depth = saved_loop_depth;
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
                                        format!(
                                            "cannot spawn on a non-sendable receiver of type {rty}"
                                        ),
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
                        // A `spawn:` block compiles to a fresh child proto with an empty loop stack,
                        // so a `break`/`continue` lexically nested in an enclosing loop but placed
                        // here is illegal in both engines. Save-zero-restore `loop_depth` (mirroring
                        // `infer_closure`) so the `loop_depth == 0` guard at `StmtKind::Break`/
                        // `Continue` fires at check time; a legitimate loop INSIDE the block
                        // re-increments from 0, keeping its own break/continue legal.
                        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
                        self.push_scope();
                        for stmt in body {
                            self.check_stmt(stmt);
                        }
                        self.pop_scope();
                        self.loop_depth = saved_loop_depth;
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
            StmtKind::Assert { cond, msg } => {
                self.expect_bool(cond, "assert condition");
                if let Some(m) = msg {
                    let t = self.infer_value(m);
                    if t != Ty::Str && !t.is_unknown() {
                        self.error(m.span, format!("assert message must be str, found {t}"));
                    }
                }
            }
        }
    }

    /// Best-effort source span for a function declaration (FnDecl has no span of its own): the first
    /// body statement, since a test fn / lifecycle hook always has a non-empty body.
    fn fn_span(decl: &FnDecl) -> Span {
        decl.body
            .first()
            .map(|s| s.span)
            .unwrap_or(Span { line: 0, col: 0 })
    }

    /// A `test fn` takes no parameters (free) or only `self` (method) and returns nothing. Hard
    /// errors here keep the runner's contract simple (it invokes tests with no args / only the
    /// instance). The body is still checked normally by the caller.
    fn validate_test_fn_shape(&mut self, decl: &FnDecl, is_method: bool) {
        let span = Self::fn_span(decl);
        if is_method {
            let ok = decl.params.len() == 1 && decl.params[0].name == "self";
            if !ok {
                self.error(span, "test method must take only self".to_string());
            }
        } else if !decl.params.is_empty() {
            self.error(span, "test function must take no parameters".to_string());
        }
        if decl.ret.is_some() {
            self.error(span, "test function must not return a value".to_string());
        }
    }

    /// A suite lifecycle hook (`before_all`/`after_all`/`before_each`/`after_each`) must be
    /// `fn name(self)` returning nothing — the runner invokes it with only the instance.
    fn validate_lifecycle_hook(&mut self, decl: &FnDecl) {
        let span = Self::fn_span(decl);
        let ok = decl.params.len() == 1 && decl.params[0].name == "self";
        if !ok {
            self.error(
                span,
                format!("lifecycle hook '{}' must take only self", decl.name),
            );
        }
        if decl.ret.is_some() {
            self.error(
                span,
                format!("lifecycle hook '{}' must not return a value", decl.name),
            );
        }
    }

    /// Check a destructuring let `a, b, … := value`. The value's type must be a tuple whose arity
    /// matches the binding count; each name is then declared with its element type. An `Unknown`
    /// value (an already-reported error) declares all names `Unknown` so no cascade follows.
    fn check_destructure(
        &mut self,
        names: &[String],
        name_spans: &[Span],
        val_ty: &Ty,
        span: Span,
    ) {
        match val_ty {
            Ty::Unknown => {
                for name in names {
                    self.declare(name, Ty::Unknown);
                }
            }
            Ty::Tuple(elems) if elems.len() == names.len() => {
                for ((name, ty), name_span) in names.iter().zip(elems).zip(name_spans.iter()) {
                    // EDITOR HOVER: each destructure target (`a`/`b` in `a, b := (1,2)`) is a NAME,
                    // not an `Expr` the probe visits; record its tuple-element type at its own span
                    // (no-op unless a probe is armed → zero overhead on normal checks).
                    self.hover_record_at(*name_span, ty, HoverKind::Local, None);
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
                self.error(
                    span,
                    format!("cannot destructure non-tuple value of type {other}"),
                );
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
                    self.error(
                        span,
                        format!("cannot assign to undeclared variable '{name}'"),
                    );
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
                // Editor hover (decl-site): a reassignment's LHS `i` is an `Ident` lvalue the probe
                // does NOT visit via `infer` (it's the assignment TARGET, not an evaluated expr), so
                // record its type at the target span here. Probe-gated no-op off the probe; mirrors
                // the let-binding/for-binding `Local` hover. Simple-Ident lvalue only (Index/Field
                // targets are handled in their own arms below, where the receiver IS inferred).
                self.hover_record_at(target.span, &var_ty, HoverKind::Local, None);
                self.check_assign_value(&var_ty, op, &val_ty, target.span);
            }
            // `xs[i] = v` — only lists are mutable by index. Strings are immutable; other types
            // aren't indexable. (`infer_index` would green-light a str index — handle it here.)
            ExprKind::Index { obj, index } => {
                // Refine-on-first-use for `m[k]=v` / `xs[i]=v`: when `obj` is a simple variable whose
                // type has an `Unknown` key/value/element slot (an empty `{}`/`[]`), the supplied
                // (idx_ty, val_ty) makes the slot concrete — re-pin the binding so a later conflicting
                // assign is a normal mismatch. The match below then re-reads the refined type from
                // scope. (Same simple-variable-only limitation as `refine_receiver`.)
                self.refine_index_receiver(obj, index, &val_ty);
                match self.infer(obj) {
                    Ty::Map(k, v) => {
                        let idx_ty = self.infer(index);
                        if !compatible(&k, &idx_ty) {
                            self.error(index.span, format!("map key must be {k}, found {idx_ty}"));
                        }
                        // Direct insertion-site Hashable / float-key ban: reject a non-Hashable key
                        // expr even when the map's key type is still `Unknown` (an empty `{}`), so
                        // `m:={}; m[1.5]=..` faults here (mirrors the literal `{1.5:..}` ban) rather
                        // than slipping past check.
                        if !idx_ty.is_unknown() && !self.is_hashable_key(&idx_ty) {
                            self.error(
                                index.span,
                                format!("map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {idx_ty}"),
                            );
                        }
                        self.check_assign_value(&v, op, &val_ty, target.span);
                    }
                    Ty::List(elem) => {
                        self.expect_int(index, "index");
                        self.check_assign_value(&elem, op, &val_ty, target.span);
                    }
                    // `ba[i] = x` — the MUTABLE sibling of bytes. Int index, int value (0–255
                    // validated at runtime). Bytes has NO arm here (immutable); bytearray adds one.
                    Ty::ByteArray => {
                        self.expect_int(index, "index");
                        self.check_assign_value(&Ty::Int, op, &val_ty, target.span);
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
                                self.error(
                                    index.span,
                                    format!("index must be {k}, found {idx_ty}"),
                                );
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
                                self.error(
                                    index.span,
                                    format!("index must be {k}, found {idx_ty}"),
                                );
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
            ExprKind::Field { obj, name, .. } => {
                let obj_ty = self.infer(obj);
                match &obj_ty {
                    Ty::Struct(sname, targs) => {
                        let field_ty = self.structs.get(sname).and_then(|info| {
                            info.fields
                                .iter()
                                .find(|(f, _)| f == name)
                                .map(|(_, ty)| subst(ty, &struct_param_map(info, targs)))
                        });
                        match field_ty {
                            Some(ty) => self.check_assign_value(&ty, op, &val_ty, target.span),
                            None => self.error(
                                target.span,
                                format!(
                                    "cannot assign to '{name}': type {obj_ty} has no field '{name}'"
                                ),
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
            // `a, b = b, a` (and index/field forms) — multi-target tuple assignment. The parser
            // guarantees `op == Eq` here. The value must be a tuple of equal arity; each target is
            // then checked against its positional element type (recursing into the ident/index/field
            // arms above — so vars, list elements, and struct fields all work, identically).
            ExprKind::Tuple(targets) => {
                let Ty::Tuple(elems) = &val_ty else {
                    if !val_ty.is_unknown() {
                        self.error(
                            span,
                            format!("cannot assign {val_ty} to {} targets", targets.len()),
                        );
                    }
                    return;
                };
                if elems.len() != targets.len() {
                    self.error(
                        span,
                        format!(
                            "assignment has {} target(s) but the value has {} element(s)",
                            targets.len(),
                            elems.len()
                        ),
                    );
                    return;
                }
                let elems = elems.clone();
                for (t, ety) in targets.iter().zip(elems) {
                    self.check_assign(t, AssignOp::Eq, ety, span);
                }
            }
            _ => self.error(
                target.span,
                "invalid assignment target (only variables can be assigned)",
            ),
        }
    }

    fn check_assign_value(&mut self, target_ty: &Ty, op: AssignOp, val_ty: &Ty, span: Span) {
        match op {
            AssignOp::Eq => {
                if !self.assignable(target_ty, val_ty) {
                    self.error(span, format!("cannot assign {val_ty} to {target_ty}"));
                }
            }
            // Numeric compound ops `+= -= *= /= %=` (and str+str for `+=`). No implicit widening:
            // `int <op> float` yields a float, which can't flow back into a concrete int slot —
            // reject it (gap #9), mirroring strict `=` (`x = 1.5`). `/=` inherits this rule, so
            // `int /= float` is rejected (true division would widen the slot).
            AssignOp::PlusEq
            | AssignOp::MinusEq
            | AssignOp::StarEq
            | AssignOp::SlashEq
            | AssignOp::PercentEq => {
                let str_ok = op == AssignOp::PlusEq && *target_ty == Ty::Str && *val_ty == Ty::Str;
                let widens = *target_ty == Ty::Int && *val_ty == Ty::Float;
                let num_ok = target_ty.is_numeric() && val_ty.is_numeric() && !widens;
                // Collection forms mirror `infer_binary`: `list += list` (concat), `list *= int`
                // (repeat), `set -= set` (difference). Compound-assign lowers through the same
                // `Op::Add`/`Op::Mul`/`Op::Sub` opcodes the binary form uses, so the runtime already
                // handles these — only the checker had to be taught to accept them.
                let coll_ok = match (op, target_ty, val_ty) {
                    (AssignOp::PlusEq, Ty::List(a), Ty::List(b)) => compatible(a, b),
                    (AssignOp::StarEq, Ty::List(_), Ty::Int) => true,
                    (AssignOp::MinusEq, Ty::Set(a), Ty::Set(b)) => compatible(a, b),
                    _ => false,
                };
                let known = !target_ty.is_unknown() && !val_ty.is_unknown();
                if known && !str_ok && !num_ok && !coll_ok {
                    let sym = match op {
                        AssignOp::PlusEq => "+=",
                        AssignOp::MinusEq => "-=",
                        AssignOp::StarEq => "*=",
                        AssignOp::SlashEq => "/=",
                        _ => "%=",
                    };
                    self.error(
                        span,
                        format!("cannot apply {sym} to {target_ty} and {val_ty}"),
                    );
                }
            }
            // Bitwise/shift compound ops `&= |= ^= <<= >>=` — int-only, EXCEPT `&= |= ^=` also do
            // set algebra on two `set[T]` (mirrors `infer_binary`'s bitwise arm; `<<= >>=` stay
            // strictly int). Lowers through the same `Op::BitOr`/etc opcodes as the binary form.
            AssignOp::AmpEq
            | AssignOp::PipeEq
            | AssignOp::CaretEq
            | AssignOp::ShlEq
            | AssignOp::ShrEq => {
                let int_ok = *target_ty == Ty::Int && *val_ty == Ty::Int;
                let set_ok = matches!(op, AssignOp::AmpEq | AssignOp::PipeEq | AssignOp::CaretEq)
                    && matches!((target_ty, val_ty), (Ty::Set(a), Ty::Set(b)) if compatible(a, b));
                let known = !target_ty.is_unknown() && !val_ty.is_unknown();
                if known && !int_ok && !set_ok {
                    let sym = match op {
                        AssignOp::AmpEq => "&=",
                        AssignOp::PipeEq => "|=",
                        AssignOp::CaretEq => "^=",
                        AssignOp::ShlEq => "<<=",
                        _ => ">>=",
                    };
                    self.error(
                        span,
                        format!("bitwise operator {sym} requires int operands or two sets, found {target_ty} and {val_ty}"),
                    );
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
        // Inside a generator, a `return` may only be bare (stop the iterator early). A returned
        // value is meaningless — the generator's result type is the stream, not a single value.
        if self.yield_ty.is_some() {
            if let Some(e) = value {
                let _ = self.infer(e);
                self.error(
                    e.span,
                    "a generator cannot `return` a value; use a bare `return` to stop early",
                );
            }
            return;
        }
        let ret = self.current_ret.clone();
        match value {
            Some(e) => {
                // Checking-mode: a closure returned into a `fn`-typed return slot binds its
                // unannotated params from the declared return type (source #1). A NON-closure return
                // keeps plain `infer` (NOT `infer_value`): returning `nil` just makes a void fn — it
                // is not "using nil as a value", so it must not get `infer_value`'s nil-rejection on
                // top of check_return's own `function returns nothing` diagnostic.
                let ty = if matches!(e.kind, ExprKind::Closure { .. }) {
                    self.infer_arg(e, Some(&ret))
                } else {
                    // Expected-type checking-mode: thread the declared return type as a hint so a
                    // returned generic ctor / generic fn-call pre-seeds its type params from it —
                    // `fn mk() -> Heap[int]: return Heap([], fn(x, y): x < y)` pins `T=int`. `unify`
                    // no-ops on a `Nil` (void) ret, so setting it unconditionally is safe; pair with
                    // an immediate clear so a non-call return value never leaks the hint.
                    self.expected_hint = Some(ret.clone());
                    let t = self.infer(e);
                    self.expected_hint = None;
                    t
                };
                if ret == Ty::Nil {
                    self.error(e.span, "function returns nothing, cannot return a value");
                } else if !self.assignable_w(&ret, &ty, true) {
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

    /// Experimental generators do not support the structured-concurrency / cleanup statements whose
    /// state (nurseries, frame defers) the suspendable generator context does not manage. Reject them
    /// with a clear message rather than mis-execute. Recurses through nested control-flow blocks but
    /// not into nested `fn` definitions (those have their own generator status).
    fn check_generator_restrictions(&mut self, body: &[Stmt]) {
        for s in body {
            // A restricted statement can also hide in a `recover:` block in expression position
            // (`x := recover: … defer … `), which the statement structure does not reach — descend
            // into those too so the ban can't be bypassed. (Mirrors the parser's yield detection.)
            let mut recover_blocks = Vec::new();
            crate::ast::stmt_expr_recover_blocks(s, &mut recover_blocks);
            for b in recover_blocks {
                self.check_generator_restrictions(b);
            }
            match &s.kind {
                StmtKind::Defer(_) => self.error(
                    s.span,
                    "`defer` is not supported inside a generator (experimental)",
                ),
                StmtKind::Spawn(_) => self.error(
                    s.span,
                    "`spawn` is not supported inside a generator (experimental)",
                ),
                StmtKind::Parallel { .. } => self.error(
                    s.span,
                    "`parallel:` is not supported inside a generator (experimental)",
                ),
                StmtKind::Wait { .. } => self.error(
                    s.span,
                    "`wait:` is not supported inside a generator (experimental)",
                ),
                StmtKind::If {
                    branches,
                    else_block,
                } => {
                    for (_, b) in branches {
                        self.check_generator_restrictions(b);
                    }
                    if let Some(b) = else_block {
                        self.check_generator_restrictions(b);
                    }
                }
                StmtKind::For { body, .. } | StmtKind::While { body, .. } => {
                    self.check_generator_restrictions(body)
                }
                StmtKind::Match { arms, .. } => {
                    for a in arms {
                        self.check_generator_restrictions(&a.body);
                    }
                }
                _ => {}
            }
        }
    }

    /// Sound "this block provably cannot fall off its end" analysis, used to enforce that a function
    /// with a *declared* non-void return type returns a value on every control-flow path (Option B).
    /// Conservative by design: returns `true` only when a path PROVABLY diverges or returns a value,
    /// so it can never false-positive on valid code (which would break the build). A genuine
    /// fall-through that this misses is an acceptable false-negative (misses the error), not a hazard.
    ///
    /// A block terminates iff ANY statement in it terminates (the first terminator dominates; no
    /// dead-code diagnosis — out of scope).
    fn block_terminates(body: &[Stmt]) -> bool {
        body.iter().any(Self::stmt_terminates)
    }

    fn stmt_terminates(s: &Stmt) -> bool {
        match &s.kind {
            // `return <expr>` and bare `return` both leave the function (a bare `return` under a
            // non-nil signature is already its own error in `check_return`; don't double-report).
            StmtKind::Return(_) => true,
            // An `if` terminates only with an `else` AND every branch body + the else body terminate.
            StmtKind::If {
                branches,
                else_block: Some(eb),
            } => {
                branches.iter().all(|(_, b)| Self::block_terminates(b))
                    && Self::block_terminates(eb)
            }
            // No `else` -> the all-conditions-false path falls through.
            StmtKind::If {
                else_block: None, ..
            } => false,
            // A `match` terminates iff every arm body terminates. Exhaustiveness (coverage by the
            // unguarded arms) is enforced separately by the match checker, so once every arm
            // terminates the eventually-chosen arm terminates too.
            StmtKind::Match { arms, .. } => arms.iter().all(|a| Self::block_terminates(&a.body)),
            // `while true:` with no reachable `break` loops forever (never falls through).
            StmtKind::While { cond, body } => {
                matches!(cond.kind, ExprKind::Bool(true)) && !Self::block_has_break(body)
            }
            // A trailing `exit(...)` / `panic(...)` diverges (neither returns to the caller). A
            // narrow, syntactic special-case on the callee name; a user shadowing the name only
            // causes an acceptable false-negative (missed error), never a false-positive.
            StmtKind::Expr(e) => Self::expr_is_diverging_call(e),
            _ => false,
        }
    }

    /// Whether `e` is a call to a diverging builtin — `exit` (`std.os.exit`, typed `nil`, never
    /// returns) or `panic` (raises a recoverable `RuntimeError`, bottom-typed, never returns
    /// normally). Matches both a bare `exit(...)`/`panic(...)` and the module-qualified
    /// `os.exit(...)` form. A narrow, syntactic special-case: a user shadowing the name only causes
    /// an acceptable false-negative (a missed error), never a false-positive that breaks a valid build.
    fn expr_is_diverging_call(e: &Expr) -> bool {
        if let ExprKind::Call { callee, .. } = &e.kind {
            match &callee.kind {
                ExprKind::Ident(name) => name == "exit" || name == "panic",
                // Only `exit` has a module-qualified form (`os.exit`); `panic` is bare-call only.
                // A user method named `panic` (`obj.panic()`) compiles to CallMethod and RETURNS
                // normally, so treating it as divergence would suppress missing-return and let a
                // typed body fall through to nil. Keep the Field arm to `exit`.
                ExprKind::Field { name, .. } => name == "exit",
                _ => false,
            }
        } else {
            false
        }
    }

    /// Whether `body` contains a `break` that targets THIS loop level — descends into `if`/`match`
    /// arms (a `break` there exits the enclosing loop) but NOT into nested `while`/`for` loops (their
    /// `break` is theirs) nor into closures/nested fns (those open a fresh loop context).
    fn block_has_break(body: &[Stmt]) -> bool {
        body.iter().any(Self::stmt_has_break)
    }

    fn stmt_has_break(s: &Stmt) -> bool {
        match &s.kind {
            StmtKind::Break => true,
            StmtKind::If {
                branches,
                else_block,
            } => {
                branches.iter().any(|(_, b)| Self::block_has_break(b))
                    || else_block.as_deref().is_some_and(Self::block_has_break)
            }
            StmtKind::Match { arms, .. } => arms.iter().any(|a| Self::block_has_break(&a.body)),
            // A nested `while`/`for` owns its own `break`; do not descend.
            _ => false,
        }
    }

    /// `yield <expr>` — legal only inside a generator function (one whose return type is
    /// `Iterator[T]`); the operand must be assignable to the element type `T`.
    fn check_yield(&mut self, e: &Expr, span: Span) {
        let ty = self.infer(e);
        match self.yield_ty.clone() {
            Some(elem) => {
                if !self.assignable(&elem, &ty) {
                    self.error(e.span, format!("expected yield type {elem}, found {ty}"));
                }
            }
            None => self.error(span, "`yield` can only appear inside a generator function"),
        }
    }

    fn check_fn_body(&mut self, decl: &FnDecl, self_ty: Option<Ty>, sig: FnSig) {
        let saved_tps = self.enter_type_params(&decl.type_params);
        let saved_ret = std::mem::replace(&mut self.current_ret, sig.ret.clone());
        // A generator (`is_generator`, i.e. its body contains `yield`) must declare `-> Iterator[T]`.
        // Recover `T` as the per-yield element type; a wrong/missing return type is an error here.
        let new_yield_ty = if decl.is_generator {
            match &sig.ret {
                Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                    Some(args[0].clone())
                }
                _ => {
                    let span = decl.body.first().map(|s| s.span);
                    if let Some(span) = span {
                        self.error(
                            span,
                            "a generator function (one that uses `yield`) must declare a return type of `Iterator[T]`",
                        );
                    }
                    None
                }
            }
        } else {
            None
        };
        if decl.is_generator {
            self.check_generator_restrictions(&decl.body);
        }
        let saved_yield = std::mem::replace(&mut self.yield_ty, new_yield_ty);
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
                // One-way int→float widening (scalar sink): a `float` param accepts an int default,
                // coerced to f64 at the callee prologue (the default is desugar-spliced into the call
                // when omitted). Mirrors the typed-`let`/arg/return/struct-field sinks.
                if !matches!(ty, Ty::Unknown) && !self.assignable_w(&ty, &actual, true) {
                    self.error(
                        def.span,
                        format!(
                            "default value for parameter '{}': expected {ty}, found {actual}",
                            param.name
                        ),
                    );
                }
            }
            // Editor hover: record the param's declared type at its DECL-site name span (no-op
            // off-probe; covers free fns AND methods, both routed through check_fn_body).
            self.hover_record_at(param.name_span, &ty, HoverKind::Param, None);
            self.declare(&param.name, ty);
        }
        // An inline-expr body (`fn a() -> T: <expr>`) implicitly returns its single expression,
        // exactly as a `return <expr>` would. We infer that expr ONCE here and validate it against
        // the declared return type with the same diagnostics `check_return` uses — so we must NOT
        // also run the statement-position `check_stmt` on it (that would infer it a second time and
        // double every error inside the expression). Any other body is checked statement-by-
        // statement as usual.
        if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            let ret = sig.ret.clone();
            let ty = self.infer(e);
            if ret == Ty::Nil {
                // A NON-nil expr against `-> nil` is a void fn that actually returns a value —
                // reject it, mirroring the multiline `return <expr>` path. A nil-typed inline expr
                // (e.g. a bare void call) implicitly returns nil and stays legal.
                if ty != Ty::Nil && !ty.is_unknown() {
                    self.error(e.span, "function returns nothing, cannot return a value");
                }
            } else if !self.assignable_w(&ret, &ty, true) {
                self.error(e.span, format!("expected return type {ret}, found {ty}"));
            }
        } else {
            for stmt in &decl.body {
                self.check_stmt(stmt);
            }
        }
        // Option B: a function with a *declared* non-void return type must return a value on every
        // control-flow path. The gate is the user's *annotation* (`decl.ret.is_some()`), NOT the
        // resolved `sig.ret`: an UN-annotated fn that returns a value on some path (the common
        // early-return / `find` idiom) infers a non-nil `sig.ret`, but with no `-> T` it stays
        // legal — gating on `sig.ret` alone would wrongly reject it. A bare `fn a(): 10` (no
        // annotation) is exempt; generators (`-> Iterator[T]`, value-produced via `yield`) too. If
        // the body can fall off the end, that silently yields nil at runtime — turn it into a loud
        // static error.
        if !decl.is_generator
            && !decl.inline_expr_body
            && decl.ret.is_some()
            && sig.ret != Ty::Nil
            && !Self::block_terminates(&decl.body)
            && let Some(span) = decl.body.first().map(|s| s.span)
        {
            let ret = &sig.ret;
            self.error(
                span,
                format!(
                    "function '{}' has return type {ret} but can fall off the end without returning a value; add an explicit `return`, or use a closure `fn() -> {ret}: <expr>` which implicitly returns its expression body",
                    decl.name
                ),
            );
        }
        self.pop_scope();
        self.current_ret = saved_ret;
        self.yield_ty = saved_yield;
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
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
        let info = self.structs.get(name)?;
        let sig = info.methods.get("next")?;
        if sig.params.len() != 1 {
            return None; // (self) only — no extra args
        }
        let Ty::Option(inner) = &sig.ret else {
            return None;
        };
        let map = struct_param_map(info, targs);
        Some(subst(inner, &map))
    }

    /// The element type a user struct's structural `iter(self) -> Iterator[E]` produces, or `None`.
    /// Sibling of [`struct_iter_elem`](Self::struct_iter_elem); used so a struct with `iter` but no
    /// `next` is recognised as `Iterable` and bound in `for`. `Iterator` is not a registered struct,
    /// so this only matches real user structs.
    fn struct_iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
        if name == "Iterator" {
            return None; // the existential cursor — handled by `iter_elem`, not as a user struct
        }
        let info = self.structs.get(name)?;
        let sig = info.methods.get("iter")?;
        if sig.params.len() != 1 {
            return None; // (self) only
        }
        let Ty::Struct(rname, rargs) = &sig.ret else {
            return None;
        };
        if rname != "Iterator" || rargs.len() != 1 {
            return None; // must declare `-> Iterator[E]`
        }
        let map = struct_param_map(info, targs);
        Some(subst(&rargs[0], &map))
    }

    /// The element type of ANY `Iterable` value — the single source of truth for `Iterable`
    /// conformance and the `Iterable`-driven `for`. A built-in collection, an `Iterator[T]`
    /// existential, or a struct with structural `next` all flow through [`iter_elem`](Self::iter_elem)
    /// (every `Iterator` is `Iterable` via `iter() == self`); a struct with only `iter` flows through
    /// [`struct_iterable_elem`](Self::struct_iterable_elem). `None` ⇒ not iterable.
    fn iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        self.iter_elem(ty).or_else(|| self.struct_iterable_elem(ty))
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
            // `bytes`/`bytearray` iterate to `int` (0–255), like Python.
            Ty::Bytes | Ty::ByteArray => Some(Ty::Int),
            Ty::Map(k, _) => Some((**k).clone()),
            // `Iterator[T]` value (a generator result): element type is its single type argument.
            Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                Some(args[0].clone())
            }
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
            // `bytes[i]`/`bytearray[i]` yield an `int` (0–255).
            Ty::Bytes | Ty::ByteArray => Some((Ty::Int, Ty::Int)),
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
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
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
            // `bytes[a:b:c]` yields a new `bytes`; `bytearray` slices to a new `bytearray`;
            // `list`/`str` slice to themselves.
            Ty::List(_) | Ty::Str | Ty::Bytes | Ty::ByteArray => Some(ty.clone()),
            Ty::Struct(name, targs) => {
                let info = self.structs.get(name)?;
                let sig = info.methods.get("slice")?;
                // The `Slice` protocol fixes the bounds: `slice(self, int? , int?, int?) -> R`.
                // Both engines always pass three `Option[int]` components (start/end/step, each
                // `None` when omitted), so a non-conforming signature (wrong arity or non-`int?`
                // bounds) is not a valid `Slice` impl — reject rather than green-light a crash.
                let opt_int = Ty::option(Ty::Int);
                if sig.params.len() != 4 || sig.params[1..=3].iter().any(|p| *p != opt_int) {
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
                self.error(
                    iter.span,
                    "a range binds a single loop variable; `for k, v` needs a map",
                );
                return unknowns(vars);
            }
            return vec![(vars[0].clone(), Ty::Int)];
        }
        let it = self.infer(iter);
        match &it {
            Ty::Map(k, v) => match vars.len() {
                1 => vec![(vars[0].clone(), (**k).clone())],
                2 => vec![
                    (vars[0].clone(), (**k).clone()),
                    (vars[1].clone(), (**v).clone()),
                ],
                _ => {
                    self.error(
                        iter.span,
                        "a `for` over a map binds one (key) or two (key, value) names",
                    );
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
                    self.error(
                        iter.span,
                        format!("`for k, v` requires a map or a list of tuples, found {it}"),
                    );
                    unknowns(vars)
                }
            },
            Ty::Str | Ty::Bytes | Ty::ByteArray | Ty::Set(_) | Ty::Channel(_)
                if vars.len() != 1 =>
            {
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
            // `for x in bytes:`/`for x in bytearray:` bind a single `int` (0–255).
            Ty::Bytes | Ty::ByteArray => vec![(vars[0].clone(), Ty::Int)],
            // `for v in ch:` over a `Channel[T]` blocks for each value and ends when the channel is
            // closed-and-drained (Go's `for v := range ch`). Binds a single element of type `T`.
            Ty::Channel(elem) => vec![(vars[0].clone(), (**elem).clone())],
            Ty::Unknown => unknowns(vars),
            Ty::Param(name) => {
                // A type parameter bounded `S: Iterator[T]` is iterable; bind the loop var to its
                // declared element type `T` (resolved with the surrounding params in scope).
                let arg = self.type_params.get(name).and_then(|bs| {
                    // `S: Iterator[T]` OR `S: Iterable[T]` is for-iterable; both carry the element as
                    // their single bound arg (an `Iterable` is driven through a one-time `.iter()`).
                    bs.iter()
                        .find(|b| b.name == "Iterator" || b.name == "Iterable")
                        .and_then(|b| b.args.first().cloned())
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
            // A generator result `Iterator[T]` (experimental, VM-only) binds a single element of T.
            Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                if vars.len() != 1 {
                    self.error(
                        iter.span,
                        "a generator iterator binds a single loop variable",
                    );
                    return unknowns(vars);
                }
                vec![(vars[0].clone(), args[0].clone())]
            }
            _ if self.struct_iter_elem(&it).is_some() => {
                // A user struct with `next(self) -> Option[E]` is iterable; it binds a single element.
                // Checked FIRST so a struct with BOTH `next` and `iter` keeps the existing `next()`
                // fast path (back-compat precedence).
                let elem = self
                    .struct_iter_elem(&it)
                    .expect("guarded by the match arm");
                if vars.len() != 1 {
                    self.error(iter.span, "a struct iterator binds a single loop variable");
                    return unknowns(vars);
                }
                vec![(vars[0].clone(), elem)]
            }
            _ if self.struct_iterable_elem(&it).is_some() => {
                // A pure-`Iterable` struct: `iter(self) -> Iterator[E]` but NO `next`. Driven by a
                // one-time `.iter()` then the cursor's `next()` (the additive `Iterable` for-case).
                let elem = self
                    .struct_iterable_elem(&it)
                    .expect("guarded by the match arm");
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
    ///
    /// `patterns` are the arms' top-level patterns — used ONLY to recover exhaustiveness when the
    /// scrutinee is un-inferable (`Ty::Unknown`, e.g. an unannotated closure param): a `Skip` there
    /// would bypass coverage entirely (soundness hole — `match x: E.A: ..` over `enum E{A,B}` checked
    /// ok then trapped at runtime). See `reconstruct_unknown_kind`.
    fn match_kind(&mut self, scrutinee: &Expr, patterns: &[&Pattern]) -> MatchKind {
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
                        let payload = self.variants[&(name.clone(), v.clone())]
                            .payload
                            .iter()
                            .map(|p| subst(p, &map))
                            .collect();
                        (v, payload)
                    })
                    .collect();
                MatchKind::Variants {
                    label: name.clone(),
                    variants,
                }
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
            // Un-inferable scrutinee: rather than skip exhaustiveness outright (a soundness hole),
            // reconstruct a concrete kind from the arm patterns when they unambiguously name a single
            // known enum or are literals — so the normal coverage check applies. Genuinely unknowable
            // cases still fall back to `Skip`.
            Ty::Unknown => self.reconstruct_unknown_kind(patterns, scrutinee.span),
            other => {
                self.error(
                    scrutinee.span,
                    format!("cannot match on non-enum type {other}"),
                );
                MatchKind::Skip
            }
        }
    }

    /// Classify a match over a residual un-inferable (`Ty::Unknown`) scrutinee — one that §3 closure
    /// inference did NOT pin (e.g. a tuple-element binding `a` from `(a, b)`, or an `Unknown`-typed
    /// expression). The top-level scrutinee arm goes through this path (not `bind_subpattern`), so it
    /// is the OTHER half of the §4.1 structural-over-`Unknown` reject (the nested half is in
    /// `bind_subpattern`):
    /// - **Any STRUCTURAL arm** (a tuple, or a real variant — qualified, builtin `Ok`/`Err`/`Some`/
    ///   `None`, an owned bare variant, or a `Name(..)` with a payload) tests a shape/tag against a
    ///   value whose type we can't prove → it traps at runtime on a wrong shape (a trailing `_` cannot
    ///   rescue it). Reject; annotate the enclosing param. Returns `Skip` afterwards so the arm bodies
    ///   still bind permissively (no cascade).
    /// - **Only literal/range arms** → `Literal(first-arm scalar)`: the existing literal-domain rule
    ///   then makes a non-`_` match non-exhaustive AND a heterogeneous arm (`1` + `"b"`) a literal
    ///   mismatch — restoring the pre-`OpenScrutinee` behaviour.
    /// - **Only bindings / `_`** → `Skip` (a bare-ident catch-all closes it; nothing to enforce).
    ///
    /// Top-level `Or` alternatives are flattened first. `&mut self` (it may emit the §4.1 reject).
    fn reconstruct_unknown_kind(&mut self, patterns: &[&Pattern], span: Span) -> MatchKind {
        // Flatten top-level or-alternatives into a flat head list.
        let mut heads: Vec<&Pattern> = Vec::new();
        for p in patterns {
            match p {
                Pattern::Or(alts) => heads.extend(alts.iter()),
                other => heads.push(other),
            }
        }
        let mut lit_ty: Option<Ty> = None;
        // The kind word of the FIRST structural arm seen (`"tuple"` / `"variant"`), if any.
        let mut structural: Option<&'static str> = None;
        for h in &heads {
            match h {
                Pattern::Tuple(_) => {
                    structural.get_or_insert("tuple");
                }
                Pattern::Variant {
                    name,
                    enum_name,
                    module_name,
                    bindings,
                } => {
                    // A real variant pattern (qualified, builtin, owned, or carrying a payload) is
                    // structural; a bare name that is NOT a known variant and binds nothing is a
                    // catch-all binding → not structural.
                    let is_variant = module_name.is_some()
                        || enum_name.is_some()
                        || matches!(name.as_str(), "Ok" | "Err" | "Some" | "None")
                        || self.variant_owners.contains_key(name)
                        || !bindings.is_empty();
                    if is_variant {
                        structural.get_or_insert("variant");
                    }
                }
                Pattern::Literal(lit) => {
                    if lit_ty.is_none() {
                        lit_ty = Some(lit_pattern_ty(lit));
                    }
                }
                Pattern::Range { .. } => {
                    if lit_ty.is_none() {
                        lit_ty = Some(Ty::Int);
                    }
                }
                // Ident/Wildcard/(nested Or) — no structural or literal signal.
                _ => {}
            }
        }
        if let Some(kind) = structural {
            self.error(
                span,
                format!(
                    "cannot match a {kind} pattern on a value of un-inferable type; annotate it"
                ),
            );
            return MatchKind::Skip;
        }
        if let Some(t) = lit_ty {
            return MatchKind::Literal(t);
        }
        MatchKind::Skip
    }

    /// The substitution from a generic enum's type parameters to a concrete instantiation's type
    /// arguments (`Tree[int]` ⇒ `{T: int}`). Empty for a non-generic enum.
    fn enum_param_map(&self, name: &str, targs: &[Ty]) -> HashMap<String, Ty> {
        self.enum_type_params
            .get(name)
            .map(|tps| {
                tps.iter()
                    .map(|tp| tp.name.clone())
                    .zip(targs.iter().cloned())
                    .collect()
            })
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
                            let payload = self.variants[&(name.clone(), v.clone())]
                                .payload
                                .iter()
                                .map(|p| subst(p, &map))
                                .collect();
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
            Pattern::Ident(name, bind_span) => {
                // A nested bare identifier names a *built-in* nullary variant of the matched type (a
                // refutable variant match — `Some(None)`, `Ok(Err(e))`), or a fresh binding. User
                // variants must be written qualified (handled below), never resolved bare here.
                let is_builtin_variant = matches!(name.as_str(), "Ok" | "Err" | "Some" | "None");
                if is_builtin_variant {
                    if let Some(vmap) = self.variants_of(ty)
                        && let Some(payload) = vmap.get(name)
                    {
                        if payload.is_empty() {
                            // A nullary built-in variant of `ty`: a refutable match, binds nothing.
                            return false;
                        }
                        // A non-nullary variant used without its payload — needs `Name(...)`.
                        self.error(
                            span,
                            format!("variant '{name}' of {ty} requires its payload — write '{name}(...)'"),
                        );
                        return false;
                    }
                    // A built-in variant name that ISN'T a variant of `ty` cannot be a binding: the
                    // compiler routes it by the variant registry (a `MatchArm` test), so it would trap
                    // on the VM while the interp binds. Reject it so all engines agree.
                    if !ty.is_unknown() {
                        self.error(span, format!("'{name}' is not a variant of {ty}"));
                        return false;
                    }
                }
                // A *user* variant must be written qualified — never resolved bare, never silently a
                // binding (the bare→binding trap). Reject with a hint to the qualified form.
                if self.variant_owners.contains_key(name) {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                    return false;
                }
                // EDITOR HOVER: a pattern binding (`n` in `Col.Val(n)`, `a`/`b` in `(a, b)`) is a
                // NAME, not an `Expr` the probe visits — record its decl-site hover at the binding
                // token's OWN span (`bind_span`, not the arm-level `span`), exactly as the for-loop
                // uses `var_spans`. No-op unless a probe is armed → zero overhead on normal checks.
                self.hover_record_at(*bind_span, ty, HoverKind::Local, None);
                self.declare(name, ty.clone());
                true
            }
            Pattern::Or(alts) => self.bind_or_alternatives(alts, ty, span),
            Pattern::Literal(lit) => {
                let lit_ty = lit_pattern_ty(lit);
                if !ty.is_unknown() && &lit_ty != ty {
                    self.error(
                        span,
                        format!("literal of type {lit_ty} cannot match a value of type {ty}"),
                    );
                }
                false
            }
            Pattern::Range { .. } => {
                // A range sub-pattern is int-only and always refutable.
                if !ty.is_unknown() && ty != &Ty::Int {
                    self.error(
                        span,
                        format!("range pattern cannot match a value of type {ty}"),
                    );
                }
                false
            }
            Pattern::Tuple(subs) => match ty {
                Ty::Tuple(tys) => {
                    if tys.len() != subs.len() {
                        self.error(
                            span,
                            format!(
                                "tuple pattern has {} element(s), but the value has {}",
                                subs.len(),
                                tys.len()
                            ),
                        );
                    }
                    let mut irref = true;
                    for (sub, t) in subs.iter().zip(tys.iter()) {
                        irref &= self.bind_subpattern(sub, t, span);
                    }
                    irref
                }
                Ty::Unknown => {
                    // §4.1 — a STRUCTURAL sub-pattern (here a tuple) over an un-inferable element/
                    // payload would destructure a value whose shape we can't prove → it traps at
                    // runtime on a wrong shape (a trailing `_` cannot rescue it). Reject; annotate the
                    // enclosing param. Still bind the sub-patterns (as Unknown) so the arm body does
                    // not cascade into spurious "unknown name" errors.
                    self.error(
                        span,
                        "cannot match a tuple pattern on a value of un-inferable type; annotate it"
                            .to_string(),
                    );
                    for sub in subs {
                        self.bind_subpattern(sub, &Ty::Unknown, span);
                    }
                    false
                }
                other => {
                    self.error(
                        span,
                        format!("tuple pattern cannot match a value of type {other}"),
                    );
                    for sub in subs {
                        self.bind_subpattern(sub, &Ty::Unknown, span);
                    }
                    false
                }
            },
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                module_name,
            } => {
                self.check_pattern_qualifier(
                    module_name,
                    enum_name,
                    name,
                    Self::scrutinee_enum(ty),
                    span,
                );
                match self.variants_of(ty) {
                    Some(vmap) => {
                        // A nested variant sub-pattern is irrefutable ONLY when its enum has exactly
                        // one variant (so naming it covers the whole domain) AND every payload
                        // sub-pattern is itself irrefutable. `Some(Some(v))` (2-variant Option) or
                        // `Some(0)` (literal payload) stays refutable; `Outer.Wrap(Inner.Only(x))`
                        // over single-variant enums is irrefutable and may close its parent variant.
                        let single_variant = vmap.len() == 1;
                        match vmap.get(name) {
                            Some(payload) => {
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
                                let mut sub_irref = true;
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    sub_irref &= self.bind_subpattern(b, t, span);
                                }
                                single_variant && sub_irref
                            }
                            None => {
                                self.error(span, format!("'{name}' is not a variant of {ty}"));
                                for b in bindings {
                                    self.bind_subpattern(b, &Ty::Unknown, span);
                                }
                                false
                            }
                        }
                    }
                    None if ty.is_unknown() => {
                        // §4.1 — a STRUCTURAL sub-pattern (here an enum/variant) over an un-inferable
                        // element/payload tests a variant tag against a value whose type we can't
                        // prove → it traps at runtime on a wrong shape (a trailing `_` cannot rescue
                        // it). Reject; annotate the enclosing param. Still bind the payload (as
                        // Unknown) so the arm body does not cascade into "unknown name" errors.
                        self.error(
                            span,
                            "cannot match a variant pattern on a value of un-inferable type; annotate it"
                                .to_string(),
                        );
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                        false
                    }
                    None => {
                        self.error(
                            span,
                            format!("variant pattern '{name}' cannot match a value of type {ty}"),
                        );
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                        false
                    }
                }
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
            let snap: std::collections::BTreeMap<String, Ty> = self
                .scopes
                .last()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect();
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
        guarded: bool,
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
                // Thread `guarded` unchanged: a top-level guard makes EVERY alternative refutable,
                // so a guarded `E.A(0) | E.B` closes nothing; an unguarded one lets each alternative
                // decide its own payload-irrefutability independently.
                let alt_irref = self.bind_match_arm(alt, kind, span, covered, guarded);
                let snap: std::collections::BTreeMap<String, Ty> = self
                    .scopes
                    .last()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                self.pop_scope(); // discard the scratch scope (we re-declare into the arm scope)
                irref |= alt_irref;
                binders.push((i, snap));
            }
            self.enforce_or_consistency(&binders, span);
            return irref;
        }
        // Reject a name bound more than once within this (non-Or, non-Wildcard) pattern, e.g. `(x, x)`
        // or `E.V(a, a)` — Rust's rule. Emitted (not early-returned) so the arm body still checks on
        // the last binding, avoiding cascade errors. Or-alternatives are checked when this fn recurses
        // on each alt above, so a duplicate inside one alt is still caught.
        // A bare ident is a real binder UNLESS it names a (refutable) nullary variant — the built-in
        // `Ok`/`Err`/`Some`/`None` or a user enum variant — which binds nothing (see `bind_subpattern`).
        // Mirror that registry here so `(None, None, None)` isn't falsely flagged as a duplicate binding.
        let is_binder = |name: &str| {
            !(self.variant_owners.contains_key(name)
                || matches!(name, "Ok" | "Err" | "Some" | "None"))
        };
        if let Some(dup) = first_duplicate_binder(pattern, &is_binder) {
            self.error(
                span,
                format!("identifier '{dup}' is bound more than once in this pattern"),
            );
        }
        match kind {
            MatchKind::Skip => {
                // Un-inferable scrutinee with only binding/`_` arms: accept the pattern shape
                // permissively, binding everything as `Unknown`. Still scope so the caller can
                // `pop_scope` uniformly. (A structural arm here was already rejected upstream by §4.1.)
                self.push_scope();
                match pattern {
                    Pattern::Variant {
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } => {
                        // Un-inferable scrutinee (Skip): no enum to validate the qualifier against.
                        self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                        // A bare name (no qualifier, no payload) that is NOT a known variant is an
                        // irrefutable binding catch-all — `n:` binds the scrutinee like `_` and
                        // closes the match, exactly as a concretely-typed scrutinee does (the parser
                        // models every bare pattern name as a nullary `Variant`). Declaring it +
                        // returning irrefutable keeps the un-inferable path consistent with the
                        // typed-`Literal` path; treating it as a refutable variant instead would both
                        // leave the binding undeclared (`unknown name`) and wrongly report the match
                        // non-exhaustive.
                        let is_known_variant = self.variant_owners.contains_key(name)
                            || matches!(name.as_str(), "Ok" | "Err" | "Some" | "None");
                        if enum_name.is_none()
                            && module_name.is_none()
                            && bindings.is_empty()
                            && !is_known_variant
                        {
                            self.declare(name, Ty::Unknown);
                            return true;
                        }
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
                    Pattern::Variant {
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } => {
                        self.check_pattern_qualifier(
                            module_name,
                            enum_name,
                            name,
                            Some(label.as_str()),
                            span,
                        );
                        let payload = variants.get(name).cloned();
                        if payload.is_none() {
                            self.error(
                                span,
                                format!(
                                    "'{name}' is not a variant of {}",
                                    crate::compiler::bare_display(label.as_str())
                                ),
                            );
                        }
                        // Bind the payload FIRST, accumulating whether every sub-pattern is
                        // irrefutable (a wildcard or plain binding). A literal/range/nested-variant
                        // sub-pattern (e.g. `Some(0)`, `P.Pair(0, y)`) makes the payload refutable,
                        // so the arm covers only part of the variant's domain — it must NOT close it.
                        let mut payload_irref = true;
                        match &payload {
                            Some(payload) => {
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
                                    payload_irref &= self.bind_subpattern(b, t, span);
                                }
                            }
                            None => {
                                for b in bindings {
                                    payload_irref &= self.bind_subpattern(b, &Ty::Unknown, span);
                                }
                            }
                        }
                        // A variant is `covered` ONLY when an arm for it is BOTH unguarded AND has an
                        // all-irrefutable payload (docs/syntax.md §8: a guarded arm is never
                        // irrefutable). Duplicate-arm detection fires only against a PRIOR fully
                        // closing arm, so a guard-then-fallback on the same variant is legal.
                        if covered.contains(name) {
                            self.error(span, format!("duplicate match arm '{name}'"));
                        } else if !guarded && payload_irref {
                            covered.insert(name.clone());
                        }
                    }
                    Pattern::Literal(_) => self.error(
                        span,
                        format!(
                            "cannot match a literal against {}",
                            crate::compiler::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Range { .. } => self.error(
                        span,
                        format!(
                            "cannot match a range against {}",
                            crate::compiler::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Tuple(_) => self.error(
                        span,
                        format!(
                            "cannot match a tuple against {}",
                            crate::compiler::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Ident(..) | Pattern::Wildcard | Pattern::Or(_) => {
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
                                format!(
                                    "literal of type {lit_ty} cannot match scrutinee of type {ty}"
                                ),
                            );
                        }
                    }
                    Pattern::Range { .. } => {
                        // A range pattern is int-only; reject against str/bool scrutinees.
                        if ty != &Ty::Int {
                            self.error(
                                span,
                                format!("range pattern cannot match scrutinee of type {ty}"),
                            );
                        }
                    }
                    // int/str/bool have no nullary variants, so a bare top-level identifier here is a
                    // binding capturing the whole scrutinee value (irrefutable catch-all). The parser
                    // emits it as `Variant { bindings: [] }`; reinterpret it as a binding — UNLESS the
                    // name is a registered variant (e.g. `None`). The compiler routes by the variant
                    // registry, so a colliding name would bind in the interp but trap on the VM; reject
                    // it here so all engines agree. (Rename the binding to fix.)
                    Pattern::Variant {
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } if bindings.is_empty() => {
                        // A *qualified* `Enum.Variant` is unambiguously a variant, never a binding —
                        // validate the qualifier and reject it against an int/str/bool scrutinee (a
                        // variant cannot match a literal-typed value). Falls through the bare path
                        // below otherwise.
                        if enum_name.is_some() {
                            // int/str/bool scrutinee: no enum to validate against (the variant is
                            // rejected below regardless).
                            self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                            self.error(span, format!("cannot match a variant against {ty}"));
                            return false;
                        }
                        // Match the compiler's variant registry: user enums PLUS the built-in
                        // Result/Option variants (which the checker special-cases elsewhere).
                        if self.variant_owners.contains_key(name)
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
                    Pattern::Variant {
                        bindings,
                        enum_name,
                        name,
                        module_name,
                    } => {
                        self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                        self.error(span, format!("cannot match a variant against {ty}"));
                        // Still bind the payload sub-patterns (as Unknown) so the arm body doesn't
                        // cascade into spurious "unknown name" errors — notably the desugared `?.`
                        // case, where the payload binding is an internal `__opt` temp the user can't
                        // see. (The `cannot match` error already flags the real problem.)
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                    Pattern::Tuple(_) => {
                        self.error(span, format!("cannot match a tuple against {ty}"))
                    }
                    Pattern::Ident(..) | Pattern::Wildcard | Pattern::Or(_) => {
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
                            format!(
                                "tuple pattern has {} element(s), but the value has {}",
                                subs.len(),
                                tys.len()
                            ),
                        );
                    }
                    let mut irref = true;
                    for (sub, t) in subs.iter().zip(tys.iter()) {
                        irref &= self.bind_subpattern(sub, t, span);
                    }
                    return irref;
                }
                self.error(
                    span,
                    "a tuple scrutinee requires a tuple pattern (or `_`)".to_string(),
                );
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
                let mut missing: Vec<String> = variants
                    .keys()
                    .filter(|v| !covered.contains(*v))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    missing.sort();
                    self.error(
                        span,
                        format!(
                            "non-exhaustive match on {}: missing {}",
                            crate::compiler::bare_display(label.as_str()),
                            missing.join(", ")
                        ),
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
                WaitTarget::Assign(target) => {
                    self.check_assign(target, AssignOp::Eq, elem, arm.span)
                }
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
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        for arm in arms {
            // PERSISTENT refine-on-first-use (see `check_block`): a STATEMENT-`match` arm mirrors an
            // if/else statement body — a refine-on-first-use pin of an OUTER empty collection inside
            // one arm PERSISTS across sibling arms and past the match (Option B: a cross-arm element-
            // type conflict is a hard error). No snapshot/restore here, so the pin `repin` wrote to
            // the binding's OWNING scope survives `pop_scope` (which only removes the arm's binders).
            // The EXPRESSION-position matcher `infer_match` keeps its barrier — value-arms stay
            // independent.
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                scrutinee.span,
                &mut covered,
                arm.guard.is_some(),
            );
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
        // Capture + clear the expected-type hint before the scrutinee/guards (the hint is for the
        // arm BODIES, the tail values). It is re-installed before each arm body below — every arm
        // is equally the value, and `infer_call` drains the single take()-once slot, so without
        // re-installing per arm only the first-inferred arm would get the hint (branch-order bug).
        let hint = self.expected_hint.take();
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut result: Option<Ty> = None;
        for arm in arms {
            // Flow-sensitivity barrier (see `check_block`): expression-`match` arms run
            // conditionally too — refinement inside one arm must not leak across arms or past it.
            let snap = self.snapshot_refinable();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                scrutinee.span,
                &mut covered,
                arm.guard.is_some(),
            );
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            self.expected_hint = hint.clone();
            let t = self.infer(&arm.body);
            self.pop_scope();
            self.restore_refinable(snap);
            result = Some(self.unify_branch(result, t, arm.body.span));
        }
        self.expected_hint = None;
        self.check_exhaustive(&kind, &covered, has_wildcard, scrutinee.span);
        result.unwrap_or(Ty::Unknown)
    }

    /// Infer an expression-position `if c: a else: b`: condition is bool, the two branches unify.
    fn infer_if_else(&mut self, cond: &Expr, then: &Expr, els: &Expr) -> Ty {
        // Capture + clear the expected-type hint before the condition: the hint is for the branch
        // VALUES (tail position), not the bool condition. Re-install it for EACH branch — both are
        // equally the tail value, and `infer_call` drains the single slot via `take()`, so without
        // re-installing it the second-inferred branch would lose the hint and a generic ctor there
        // would deadlock (acceptance would depend on branch order).
        let hint = self.expected_hint.take();
        self.expect_bool(cond, "if condition");
        // Flow-sensitivity barrier (see `check_block`): the two branch expressions run
        // conditionally — refinement inside one must not leak into the other or past the `if`.
        let snap = self.snapshot_refinable();
        self.expected_hint = hint.clone();
        let t_then = self.infer(then);
        self.restore_refinable(snap.clone());
        self.expected_hint = hint;
        let t_els = self.infer(els);
        self.expected_hint = None;
        self.restore_refinable(snap);
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
                    if prev.is_unknown() { t } else { prev }
                } else {
                    self.error(
                        span,
                        format!("branches have incompatible types: {prev} and {t}"),
                    );
                    Ty::Unknown
                }
            }
        }
    }

    // ===== expression inference =====

    /// Type-check an interpolated string literal's `{...}` fragment expressions. The string is
    /// parsed into chunks by the SHARED `crate::interpolation` parser (the very one the compiler
    /// emits from — so the checker and the compiler can never disagree on how a string is chunked),
    /// and every fragment `Expr` is run through the normal `infer_value` path: undefined names,
    /// type/method/arity mismatches, and void-call fragments all surface here as compile errors
    /// instead of slipping past `check` to panic the compiler (`global_slot`) or fault at runtime.
    ///
    /// A malformed interpolation (unterminated `{`, bad format spec) is reported as an error; we
    /// then stop (the compiler treats the same malformed string as fatal). Format-spec *validation*
    /// stays the compiler's job — we discard the parsed spec and only infer the expression.
    ///
    /// Span: a fragment expr is parsed from the `{…}` substring with fragment-relative spans (its
    /// root is `line 1, col 1`), so an error keyed on the fragment ROOT node — the nil-in-value-position
    /// ban, the only diagnostic anchored there — would otherwise report the bogus `(1,1)` fallback.
    /// We stamp the whole-string-literal `span` onto each fragment's root before inferring, matching
    /// the compiler's existing emit site (it uses the string span for fragment bytecode too) so the
    /// nil error points at the real string literal. Errors raised DEEPER inside a fragment (an
    /// undefined name in a nested call) keep their fragment-relative child spans; narrowing those is
    /// out of scope and fragments are short. Always returns `Ty::Str`.
    fn check_interpolation(&mut self, raw: &str, span: Span) -> Ty {
        match crate::interpolation::parse_interpolation(raw, span) {
            Ok(chunks) => {
                for chunk in chunks {
                    if let crate::interpolation::Chunk::Expr(mut e, _spec) = chunk {
                        // Anchor the fragment root at the string literal so a nil-fragment error
                        // points at the literal, not the `(1,1)` fragment-relative fallback.
                        e.span = span;
                        // Discard the inferred type; we only want the side-effecting checks
                        // (name resolution, arity/type/method validation, nil ban).
                        let _ = self.infer_value(&e);
                    }
                }
            }
            Err(e) => self.error(span, e.message),
        }
        Ty::Str
    }

    /// Infer an expression that is used in **value position** (assignment RHS, a call/collection
    /// argument, a binary/unary operand, an index/range bound, …). `nil` is a return-only / void
    /// type, never a writable value: a void call's result must not silently propagate into a binding
    /// or another expression. So if the expr is exactly `Ty::Nil`, report it and degrade to `Unknown`
    /// (suppressing the cascade). A bare void call AS A STATEMENT keeps using plain `infer` (legal),
    /// as does a fn/closure RETURN expr (returning nil just makes a void fn — not "using nil").
    fn infer_value(&mut self, expr: &Expr) -> Ty {
        let ty = self.infer(expr);
        if ty == Ty::Nil {
            self.error(
                expr.span,
                "expression returns no value (nil) and cannot be used as a value".to_string(),
            );
            return Ty::Unknown;
        }
        ty
    }

    fn infer(&mut self, expr: &Expr) -> Ty {
        let ty = self.infer_kind(expr);
        // EDITOR HOVER probe: record this expr's type if its leaf/field anchor is the cursor token.
        // No-op (one `Option` check) unless a probe is armed. Children infer before parents and only
        // LEAF kinds record, so a parent expression never overwrites the smaller symbol's type.
        if self.hover_probe.is_some() {
            self.hover_record_expr(expr, &ty);
        }
        ty
    }

    fn infer_kind(&mut self, expr: &Expr) -> Ty {
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(raw) => self.check_interpolation(raw, expr.span),
            ExprKind::RawStr(_) => Ty::Str, // verbatim `str`, no interpolation to check
            ExprKind::Bytes(_) => Ty::Bytes,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Ident(name) => self.infer_ident(name, expr.span),
            ExprKind::List(items) => self.infer_list(items),
            ExprKind::Tuple(items) => {
                Ty::Tuple(items.iter().map(|e| self.infer_value(e)).collect())
            }
            ExprKind::Map(entries) => self.infer_map(entries),
            ExprKind::Set(elems) => self.infer_set(elems),
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.infer_comprehension(*kind, key.as_deref(), elem, clauses),
            ExprKind::Unary { op, expr: inner } => self.infer_unary(*op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs),
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => self.infer_slice(
                obj,
                start.as_deref(),
                end.as_deref(),
                step.as_deref(),
                expr.span,
            ),
            ExprKind::Range { start, end } => {
                self.expect_int(start, "range bound");
                self.expect_int(end, "range bound");
                Ty::list(Ty::Int)
            }
            ExprKind::Call {
                callee,
                args,
                named,
                type_args,
            } => self.infer_call(callee, args, named, type_args, expr.span),
            ExprKind::Field { obj, name, .. } => self.infer_field(obj, name),
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
            ExprKind::Closure { params, ret, body } => {
                // No expected type at the generic `infer` seam — free-closure inference (sources
                // #2/#3) and the ambiguity check happen inside `infer_closure`.
                self.infer_closure(params, ret.as_ref(), body, None)
            }
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms),
            ExprKind::IfElse { cond, then, els } => self.infer_if_else(cond, then, els),
            ExprKind::Recover(block) => self.infer_recover(block),
            // `Type[T1, T2]` is a type-application HEAD — only valid as the receiver of a member
            // access / call (`Result[int, str].Ok(5)`, nullary `Box[int].Empty`). The `infer_call`
            // and `infer_field` paths consume it before it reaches here; a bare one in value
            // position is a use of a type as a value.
            ExprKind::TypeApply { name, .. } => {
                self.error(
                    expr.span,
                    format!(
                        "'{name}' is a type, not a value; access a member (`{name}[…].member`)"
                    ),
                );
                Ty::Unknown
            }
        }
    }

    /// Record `(ty, kind)` as the hover result if `span` is the armed probe position (entry module,
    /// first hit wins). The single place a probe hit is committed; both the expr-leaf path and the
    /// let-binding path funnel through here. A no-op when no probe is armed.
    fn hover_record_at(&mut self, span: Span, ty: &Ty, kind: HoverKind, doc: Option<String>) {
        let Some((pl, pc)) = self.hover_probe else {
            return;
        };
        if self.hover_result.is_some() || self.current_module_id != self.hover_entry {
            return;
        }
        if span.line == pl && span.col == pc {
            self.hover_result = Some((ty.clone(), kind, doc));
        }
    }

    /// Editor hover for a `from M import T` user type (struct/enum/newtype). Computes the effective
    /// doc — the type's own decl docstring carried across the module boundary, else a `kind (from
    /// module)` fallback — then (1) seeds `name_docs[bind]` so a later bare (`x: T`) / generic-head
    /// (`x: T[..]`) annotation use surfaces the same doc (those arms read `name_docs`), and (2) records
    /// the import-line token hover at `name_span`. Both halves are probe-gated no-ops off the hover
    /// probe (`name_docs` is editor-tooling-only and entry-module-scoped), so this is parity-neutral.
    fn record_imported_type_hover(
        &mut self,
        bind: &str,
        name_span: Span,
        ty: &Ty,
        own_doc: Option<&str>,
        kind_word: &str,
        path: &[String],
    ) {
        if self.hover_probe.is_none() {
            return;
        }
        let doc = own_doc
            .map(str::to_string)
            .unwrap_or_else(|| format!("{kind_word} (from {})", path.join(".")));
        self.name_docs.insert(bind.to_string(), doc.clone());
        self.hover_record_at(name_span, ty, HoverKind::Type, Some(doc));
    }

    /// Editor hover for a per-name import of a native/reserved TYPE (`import Shared from
    /// std.concurrency`, `import Socket from std.net`, `import ptr from std.ffi`, …). These branches
    /// license the name via the per-module sets and short-circuit BEFORE the user-struct import arm
    /// that records a hover, so the import-line token would otherwise show nothing. Records that
    /// token hover with the type's `builtin_type_doc` blurb (else a `(from <module>)` fallback) and
    /// its resolved native `Ty` for display. Probe-gated no-op off the hover probe; unlike a user
    /// `.chz` type NO `name_docs` seeding is needed — the bare/annotation use already resolves its
    /// doc through `builtin_type_doc` in the `Type::Named`/`Type::Generic` hover arms.
    fn record_native_type_import_hover(&mut self, member: &str, name_span: Span, path: &[String]) {
        if self.hover_probe.is_none() {
            return;
        }
        let ty = self
            .qualified_builtin_ty(member, &[])
            .unwrap_or(Ty::Unknown);
        let doc = builtin_type_doc(member)
            .unwrap_or_else(|| format!("{member} (from {})", path.join(".")));
        self.hover_record_at(name_span, &ty, HoverKind::Type, Some(doc));
    }

    /// Hover-record a LEAF expression (identifier / literal) or a field-name access. Non-leaf kinds
    /// (Binary/Index/Call/…) are skipped so hovering `a` in `a[0]` reports `a`'s type, not the element
    /// type — the parent never overwrites the child. The field-name access anchors on `name_span` (the
    /// field-name token), not the receiver-start `expr.span`, so `a.b.c` resolves the hovered segment.
    fn hover_record_expr(&mut self, expr: &Expr, ty: &Ty) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bytes(_)
            | ExprKind::Bool(_) => self.hover_record_at(expr.span, ty, HoverKind::Literal, None),
            ExprKind::Ident(name) => {
                // doc source mirrors the resolution: a `let`-bound local/global → `name_docs`; a free
                // fn → its `FnSig::doc`; a bare type/ctor name used as a value → `name_docs`. All keyed
                // by simple name, entry-module-scoped (safe — hover only fires in the entry module).
                let (kind, doc) = if self.lookup(name).is_some() {
                    // Only a TRUE module-top-level binding (resolves at scope 0) owns its `name_docs`
                    // entry; a shadowing param/local of the same name has no doc of its own and must
                    // NOT borrow the global's (`name_docs` is keyed by bare name).
                    let at_top_level = (0..self.scopes.len())
                        .rev()
                        .find(|&i| self.scopes[i].contains_key(name))
                        == Some(0);
                    let doc = if at_top_level {
                        self.name_docs.get(name).cloned()
                    } else {
                        None
                    };
                    (HoverKind::Local, doc)
                } else if let Some(sig) = self.functions.get(name) {
                    (HoverKind::Func, sig.doc.clone())
                } else {
                    (HoverKind::Other, self.name_docs.get(name).cloned())
                };
                self.hover_record_at(expr.span, ty, kind, doc);
            }
            ExprKind::Field { name_span, .. } => {
                self.hover_record_at(*name_span, ty, HoverKind::Field, None);
            }
            _ => {}
        }
    }

    /// Build a DISPLAY-only `Ty::Func` for a by-name call callee (free fn, struct constructor, or a
    /// reserved builtin via [`builtin_sig`]), for editor hover. `None` only for bare enum variants —
    /// they carry no recordable signature, so hover stays `None`. Pure read of the fn/struct/builtin
    /// tables: emits no error and changes no checking decision (it is only ever called under the hover
    /// probe). The free-fn branch displays a generic fn's declared signature verbatim (`FnSig`
    /// params/ret stay `Ty::Param(T)` → "fn(T, T) -> T"); the struct branch mirrors `name_is_generic`'s
    /// module-keyed `bare_key` lookup and renders fields → `Struct` ("fn(int, int) -> Vec2"); the
    /// builtin branch returns a canonical display sig ("fn(int) -> List[int]" for `range`).
    fn callee_display_ty(&self, name: &str) -> Option<Ty> {
        if let Some(sig) = self.functions.get(name) {
            return Some(Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
            });
        }
        if let Some(info) = self.structs.get(&self.bare_key(name)) {
            let params: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
            let targs: Vec<Ty> = info
                .type_params
                .iter()
                .map(|tp| Ty::Param(tp.name.clone()))
                .collect();
            return Some(Ty::Func {
                params,
                ret: Box::new(Ty::Struct(name.to_string(), targs)),
            });
        }
        // A newtype constructor (`UserId(10)`): one arg of the underlying type → the newtype. Mirrors
        // the struct branch (module-keyed `bare_key` lookup); a generic newtype keeps its declared
        // `Ty::Param`s so it Displays "fn(list[T]) -> Stack[T]".
        if self.newtype_names.contains(name) {
            let key = self.bare_key(name);
            if let Some((under, _)) = self.newtype_defs.get(&key) {
                let targs: Vec<Ty> = self
                    .newtype_type_params
                    .get(&key)
                    .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
                    .unwrap_or_default();
                return Some(Ty::Func {
                    params: vec![under.clone()],
                    ret: Box::new(Ty::NewType(name.to_string(), targs)),
                });
            }
        }
        // A free / constructor builtin (`print`/`range`/`List`/`Channel`/…): a DISPLAY-only signature
        // from `builtin_sig` (the inference arms aren't a single queryable sig). Reserved names can't
        // be user-shadowed, so this never collides with the fn/struct tables above.
        if let Some(sig) = builtin_sig(name) {
            return Some(Ty::Func {
                params: sig.params,
                ret: Box::new(sig.ret),
            });
        }
        None
    }

    /// `recover: <block>` yields `Result[T, Error]` where `T` is the type of the block's trailing
    /// expression (or `nil`). Non-final statements are checked for their effects.
    fn infer_recover(&mut self, block: &Block) -> Ty {
        // A `recover:` block is a value, not a control-flow target: `return`/`break`/`continue` that
        // would escape it are rejected (both engines agree). `?` is fine — it propagates normally.
        if let Some((span, kw)) = recover_escaping_flow(block, false) {
            self.error(
                span,
                format!("'{kw}' is not allowed inside a recover block"),
            );
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
            // A `recover:` whose tail provably diverges (a statement-form `match` whose every arm
            // `panic`s, `while true:`, all-branch-returning `if/else`, a trailing `exit`/`panic`)
            // yields no normal value, so its `Ok` payload is bottom (`Unknown`), not `nil` — exactly
            // like the direct `recover: panic(...)` form, which `infer`s `panic` to `Unknown` via the
            // `Expr` arm above. Without this, a diverging *statement* tail leaves `value_ty = Nil` and
            // its `Ok(v)` is wrongly nil-banned in value position. Guarding on `== Ty::Nil` keeps every
            // concrete-tail recover (`recover: 5` -> `int`) and non-diverging statement tail
            // (`recover: x := 5` -> `Result[nil]`) untouched. Reuses the sound, conservative
            // `stmt_terminates` divergence predicate.
            if value_ty == Ty::Nil && Self::stmt_terminates(last) {
                value_ty = Ty::Unknown;
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
                        "cannot use non-sendable captured binding '{name}' of type {disp} inside a \
                         spawned task (captures cross the airlock — communicate via a Channel or Shared)",
                        disp = if self.is_ref_decl(name) { ref_display(&ty) } else { ty.to_string() }
                    ),
                );
            }
            return ty;
        }
        if let Some(sig) = self.functions.get(name) {
            return Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
            };
        }
        if name == "None" {
            return Ty::option(Ty::Unknown);
        }
        // A bare user-variant name used as a value (`Red`, `Leaf`) is no longer allowed — variants are
        // scoped under their enum and must be written qualified (`Color.Red`, `Tree.Leaf`).
        if self.variant_owners.contains_key(name) {
            let hint = self.qualify_hint(name);
            self.error(span, hint);
            return Ty::Unknown;
        }
        // A bare use of a name that is a type declared in some (un-imported) module — typically a
        // constructor like `Point(1)` whose module wasn't `from`-imported. Hint how to import it.
        if self.types_by_name.contains_key(name) {
            self.error(span, self.unknown_type_msg(name));
            return Ty::Unknown;
        }
        // A multi-level path mistake (`std.concurrency.Shared(0)`): the head `std` is the first
        // segment of a real imported dotted module path, NOT a bound name. Steer to the two-level form
        // instead of the misleading bare "unknown name 'std'". Narrow: fires ONLY for a literal import
        // path head, never a genuine typo (which has no `import_path_heads` entry).
        if let Some((dotted, bound)) = self.import_path_heads.get(name).cloned() {
            self.error(
                span,
                format!(
                    "Chezzi uses two-level paths — write `{bound}.<Name>` (the imported module's \
                     bound name) or alias with `import {dotted} as {bound}` then `{bound}.<Name>`; \
                     multi-level paths like `{name}.….<Name>` are not supported"
                ),
            );
            return Ty::Unknown;
        }
        self.error(span, format!("unknown name '{name}'"));
        Ty::Unknown
    }

    fn infer_list(&mut self, items: &[Expr]) -> Ty {
        let mut elem = Ty::Unknown;
        for item in items {
            let t = self.infer_value(item);
            if elem.is_unknown() {
                elem = t;
            } else if numeric_mix(&elem, &t) {
                // One-way int→float widening: a mixed int/float list literal infers `list[float]`.
                elem = Ty::Float;
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
            let et = self.infer_value(e);
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
            let kt = self.infer_value(k_expr);
            let vt = self.infer_value(v_expr);
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
            } else if numeric_mix(&value, &vt) {
                // One-way int→float widening on the VALUE position (keys stay strict — float keys are
                // banned anyway): a mixed int/float map value infers `map[K, float]`.
                value = Ty::Float;
            } else if !vt.is_unknown() && !compatible(&value, &vt) {
                self.error(v_expr.span, format!("map values differ: {value} vs {vt}"));
            }
        }
        Ty::map(key, value)
    }

    /// Infer a comprehension's type. Walks each `for` clause in order (first outermost): binds the
    /// clause's loop variable(s) to the iterand's element type(s) via `for_bindings` (the exact path
    /// a `for` loop uses, so every iterable behaves the same) — inferred in the scope of the earlier
    /// clauses so a later clause can reference an earlier binding — and checks each guard is `Bool`.
    /// Then it infers the element (and key) in the cumulative scope. The result mirrors
    /// `infer_list`/`infer_set`/`infer_map`, including the Hashable check on set elements and map keys.
    fn infer_comprehension(
        &mut self,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        clauses: &[CompClause],
    ) -> Ty {
        self.push_scope();
        for clause in clauses {
            // `for_bindings` infers the iter IN the current scope, so later clauses see earlier
            // bindings (the whole point of nesting). Compute before declaring this clause's vars.
            let bindings = self.for_bindings(&clause.vars, &clause.iter);
            // A comprehension materializes eagerly, but a `Channel` is a blocking iteration form whose
            // termination depends on `close()`. Draining it into a list/set/map is out of scope and would
            // DIVERGE between engines (the VM's `compile_comprehension` reuses the channel-aware
            // `compile_for`, but the interp oracle's comprehension path can't iterate a channel). Reject
            // on both engines instead — the `for v in ch:` statement form is the way to drain a channel.
            // Checked per clause so a channel in ANY clause is rejected.
            if matches!(self.infer(&clause.iter), Ty::Channel(_)) {
                self.error(
                    clause.iter.span,
                    "a channel cannot be drained in a comprehension; use the `for v in ch:` statement form",
                );
            }
            for (name, ty) in bindings {
                // Intentionally NOT `mark_loop_var`: a comprehension body is an expression, so its
                // binding can't be assigned to — no divergence to guard against. If a statement-bearing
                // comprehension is ever added, mark these too (see `check_assign` / for-loop handling).
                self.declare(&name, ty);
            }
            for g in &clause.guards {
                self.expect_bool(g, "comprehension guard");
            }
        }
        let result = match kind {
            CompKind::List => Ty::list(self.infer_value(elem)),
            CompKind::Set => {
                let et = self.infer_value(elem);
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
                let kt = self.infer_value(key);
                let vt = self.infer_value(elem);
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
        let t = self.infer_value(inner);
        match op {
            UnaryOp::Neg => {
                // int/float negate natively; a struct/newtype/type-param negates via the `Neg`
                // protocol (method `neg(self) -> Self`) — the unary mirror of how `+` consults `Add`.
                if t.is_numeric() || t.is_unknown() || self.satisfies(&t, "Neg").is_ok() {
                    t
                } else {
                    self.error(inner.span, format!("cannot negate {t}"));
                    Ty::Unknown
                }
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
        let l = self.infer_value(lhs);
        let r = self.infer_value(rhs);
        let either_unknown = l.is_unknown() || r.is_unknown();
        match op {
            And | Or => {
                if l != Ty::Bool && !l.is_unknown() {
                    self.error(
                        lhs.span,
                        format!("logical operator expects bool, found {l}"),
                    );
                }
                if r != Ty::Bool && !r.is_unknown() {
                    self.error(
                        rhs.span,
                        format!("logical operator expects bool, found {r}"),
                    );
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
                } else if let (Ty::List(le), Ty::List(re)) = (&l, &r) {
                    // List concat (gap #3): `[1,2] + [3,4]` → `list[T]`, identical to `.concat`.
                    // Element types must be compatible; an empty `[]` side (Unknown elem) is
                    // joined by `merge_unknown` so `[] + [1]` infers `list[int]`.
                    if compatible(le, re) {
                        Ty::List(Box::new(merge_unknown(le, re)))
                    } else {
                        self.error(lhs.span, format!("cannot apply + to {l} and {r}"));
                        Ty::Unknown
                    }
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
                } else if op == Mul && matches!((&l, &r), (Ty::List(_), Ty::Int)) {
                    // List repeat (gap #3): `[0] * 3` → `list[T]`. Result keeps the list's element.
                    l.clone()
                } else if op == Mul && matches!((&l, &r), (Ty::Int, Ty::List(_))) {
                    // Commutative, Python-style: `3 * [0]` → `list[T]`.
                    r.clone()
                } else if op == Sub
                    && let (Ty::Set(le), Ty::Set(re)) = (&l, &r)
                {
                    // Set difference (gap #3): `a - b` → `set[T]`, identical to `.difference`.
                    if compatible(le, re) {
                        Ty::Set(Box::new(merge_unknown(le, re)))
                    } else {
                        self.error(
                            lhs.span,
                            format!("cannot apply {} to {l} and {r}", op_sym(op)),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!("cannot apply {} to {l} and {r}", op_sym(op)),
                    );
                    Ty::Unknown
                }
            }
            // `/`/`%` overload via the `Div`/`Mod` protocols on same-typed structs/enums/type-params,
            // exactly like `-`/`*` use `Sub`/`Mul` (M22). `op_overload_result` also covers the same
            // SCALAR numeric newtype auto-flow (`Meters / Meters`), so no hand-rolled newtype branch.
            Div | Mod => {
                let proto = if op == Div { "Div" } else { "Mod" };
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) = self.op_overload_result(&l, &r, proto) {
                    t
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!("cannot apply {} to {l} and {r}", op_sym(op)),
                    );
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
            // Bitwise/shift ops are int-only (gap #13), EXCEPT `| & ^` also do set algebra
            // (gap #3): union / intersection / symmetric-difference on two `set[T]`. Shifts
            // (`<< >>`) stay strictly int-only.
            BitAnd | BitOr | BitXor | Shl | Shr => {
                if l == Ty::Int && r == Ty::Int {
                    Ty::Int
                } else if matches!(op, BitAnd | BitOr | BitXor)
                    && let (Ty::Set(le), Ty::Set(re)) = (&l, &r)
                {
                    // Set `|`→union, `&`→intersection, `^`→symmetric-difference → `set[T]`,
                    // identical to the `.union`/`.intersection` methods (`^` has no method form).
                    if compatible(le, re) {
                        Ty::Set(Box::new(merge_unknown(le, re)))
                    } else {
                        self.error(
                            lhs.span,
                            format!(
                                "bitwise operator {} requires int operands or two sets, found {l} and {r}",
                                op_sym(op)
                            ),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!(
                            "bitwise operator {} requires int operands or two sets, found {l} and {r}",
                            op_sym(op)
                        ),
                    );
                    Ty::Unknown
                }
            }
            Eq | NotEq => Ty::Bool, // equality is permissive (matches the interpreter)
            // `x in xs` — membership, type-directed on the RHS container. List/Set test element
            // membership, Map tests KEY membership (Python-style), Str tests substring. Always
            // yields `bool`. No user-`Contains` overload (reject anything else). The element/key
            // type must be compatible with the LHS.
            In => {
                // A bare range has no runtime value (only valid as a `for` iterable) yet types as
                // `list[int]` — reject a range RHS here or the engines diverge: the VM rejects the
                // bare range at compile time, the interpreter dies at runtime. Mirrors the
                // compiler's bare-range rejection.
                if matches!(rhs.kind, ExprKind::Range { .. }) {
                    self.error(rhs.span, "cannot use `in` on a range (a range is only valid as the iterable of a `for` loop)".to_string());
                    return Ty::Bool;
                }
                match &r {
                    Ty::List(elem) | Ty::Set(elem) => {
                        if !either_unknown && !compatible(elem, &l) {
                            self.error(lhs.span, format!("cannot test membership of {l} in {r}"));
                        }
                    }
                    Ty::Map(key, _) => {
                        if !either_unknown && !compatible(key, &l) {
                            self.error(
                                lhs.span,
                                format!(
                                    "cannot test membership of {l} in {r} (map `in` tests keys)"
                                ),
                            );
                        }
                    }
                    Ty::Str => {
                        if l != Ty::Str && !either_unknown {
                            self.error(
                                lhs.span,
                                format!("substring `in` requires a str on the left, found {l}"),
                            );
                        }
                    }
                    Ty::Unknown => {}
                    other => {
                        self.error(
                            rhs.span,
                            format!(
                                "cannot use `in` on {other} (expected a list, set, map, or str)"
                            ),
                        );
                    }
                }
                Ty::Bool
            }
        }
    }

    /// A "this name is a variant — write it qualified" diagnostic, naming the owning enum(s).
    /// Falls back to "unknown name" if the name isn't a known variant (shouldn't normally happen at
    /// the call sites, which guard on `variant_owners` first).
    fn qualify_hint(&self, name: &str) -> String {
        match self.variant_owners.get(name).map(Vec::as_slice) {
            Some([en]) => {
                format!("'{name}' is a variant of enum '{en}'; write it qualified as '{en}.{name}'")
            }
            Some(ens @ [_, _, ..]) => {
                let opts = ens
                    .iter()
                    .map(|e| format!("'{e}.{name}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "'{name}' is a variant of several enums; write it qualified (one of {opts})"
                )
            }
            _ => format!("unknown name '{name}'"),
        }
    }

    /// The enum a scrutinee/slot type belongs to (`Color`, or `Result`/`Option` for the built-ins),
    /// or `None` for a non-enum / un-inferable type. Used to validate a pattern's `Enum.` qualifier
    /// against the value being matched.
    fn scrutinee_enum(ty: &Ty) -> Option<&str> {
        match ty {
            Ty::Enum(name, _) => Some(name),
            Ty::Result(..) => Some("Result"),
            Ty::Option(_) => Some("Option"),
            _ => None,
        }
    }

    /// Validate the `Enum.` qualifier on a `case Enum.Variant:` pattern. The named variant must (a)
    /// belong to `enum_name`, and (b) — since variant names may now be shared across enums — name the
    /// **scrutinee's** enum (`scrut_enum`): owning the name isn't enough, because a foreign qualifier
    /// resolves to a different `variant_id` (a dead arm that would still be miscounted toward
    /// exhaustiveness → a "checked-OK" match that traps at runtime). When *unqualified*, a user variant
    /// name is an error — variants must be written qualified (built-in Ok/Err/Some/None stay bare).
    fn check_pattern_qualifier(
        &mut self,
        module_name: &Option<String>,
        enum_name: &Option<String>,
        name: &str,
        scrut_enum: Option<&str>,
        span: Span,
    ) {
        // A leading module binder (`module.Enum.Variant`) is validated here then dropped: the module
        // must be bound and must own the named enum. Resolution mirrors construction
        // (`infer_field`'s module.Enum.Variant path) — `imported_modules` → `ModuleSig` → `enum_defs`.
        // Errors render BARE names only (never the qualified identity key). On success we fall through
        // to the existing `enum_name` validation, which is scrutinee-driven and keeps everything else
        // (variant-exists, scrutinee-agrees, exhaustiveness-by-identity) unchanged.
        // When a module binder is present and resolves, this holds the enum's true IDENTITY KEY
        // (`module::Enum`), used below instead of the bare/scrutinee fallback so variant-lookup and
        // scrutinee-agreement key on the SAME identity as construction.
        let mut module_ekey: Option<String> = None;
        if let Some(m) = module_name {
            let Some(en) = enum_name else {
                // A module binder always comes with an enum name from the parser (3-part form); a
                // None here would be a parser bug. Defensive: nothing to validate.
                return;
            };
            let Some(mid) = self.imported_modules.get(m).cloned() else {
                self.error(span, format!("unknown module '{m}'"));
                return;
            };
            match self.module_sigs.get(&mid) {
                Some(sig) if sig.enum_defs.contains_key(en) => {
                    module_ekey = Some(self.type_key(&mid, en));
                }
                _ => {
                    self.error(span, format!("module '{m}' has no enum '{en}'"));
                    return;
                }
            }
        }
        match enum_name {
            Some(en) => {
                // ROOT REDESIGN — the pattern carries the BARE written enum name. Resolve it to its
                // qualified IDENTITY KEY for the layout lookup. A module binder (`module.Enum.Variant`)
                // resolves the key directly (above). Otherwise a bare-visible enum (local / from-import
                // / std) resolves via `bare_types`; a WHOLE-module-imported enum (`Color` from
                // `import geo`) is NOT bare-visible, so fall back to the SCRUTINEE's own enum key when
                // its bare display name equals `en` (the pattern `Color.Red` matching a `geo::Color`
                // value). Error messages keep the bare `en`.
                let ekey = match module_ekey {
                    Some(k) => k,
                    None => match self.bare_types.get(en) {
                        Some(k) => k.clone(),
                        None => match scrut_enum {
                            Some(s) if crate::compiler::bare_display(s) == *en => s.to_string(),
                            _ => en.to_string(),
                        },
                    },
                };
                // User variants live in `self.variants` keyed by `(enum, variant)`; the built-in
                // Result/Option variants don't, so accept their canonical enums explicitly.
                let builtin_ok = matches!(
                    (en.as_str(), name),
                    ("Result", "Ok") | ("Result", "Err") | ("Option", "Some") | ("Option", "None")
                );
                if !builtin_ok
                    && !self
                        .variants
                        .contains_key(&(ekey.clone(), name.to_string()))
                {
                    self.error(span, format!("enum '{en}' has no variant '{name}'"));
                    return;
                }
                // The qualifier must name the scrutinee's own enum. (Skipped when the scrutinee enum
                // is unknown — an int/str/bool or un-inferable scrutinee, handled by the caller.) The
                // scrutinee carries the runtime key, so compare against the resolved `ekey`.
                if let Some(s) = scrut_enum
                    && ekey != s
                {
                    self.error(
                        span,
                        format!(
                            "variant '{en}.{name}' cannot match a value of enum '{}'",
                            crate::compiler::bare_display(s)
                        ),
                    );
                }
            }
            None => {
                // A bare user-variant name in a pattern must be qualified. (Built-ins are not in
                // `variant_owners`, so they pass through untouched.)
                if self.variant_owners.contains_key(name) {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                }
            }
        }
    }

    fn infer_field(&mut self, obj: &Expr, name: &str) -> Ty {
        // A too-deep qualified-path mistake (`std.net.Socket(0)`, `std.concurrency.Shared(0)`,
        // `std.concurrency.collection.Counter(...)`): the receiver `obj` is the BARE first segment of
        // an imported dotted module path (`std`) — never a bound name — and `name` is the NEXT segment.
        // The enclosing call/field already consumed the rest, so we identify the module by its
        // (head, next) prefix and name its EXACT bound name (correct for 2- and 3+-level imports and
        // sibling collisions; the trailing type isn't visible here, hence the `<Name>` placeholder).
        if let ExprKind::Ident(head) = &obj.kind
            && self.lookup(head).is_none()
            && !self.imported_modules.contains_key(head)
            && let Some(slot) = self
                .module_prefix2
                .get(&(head.clone(), name.to_string()))
                .cloned()
        {
            let hint = match slot {
                Some((dotted, bound)) => format!(
                    "Chezzi uses two-level paths — write `{bound}.<Name>` (the imported module's \
                     bound name) or alias with `import {dotted} as {bound}` then `{bound}.<Name>`; \
                     multi-level paths like `{head}.{name}.<Name>` are not supported"
                ),
                None => format!(
                    "Chezzi uses two-level paths — reference an imported module by its bound name \
                     (its last path segment, or an alias); multi-level paths like \
                     `{head}.{name}.<Name>` are not supported"
                ),
            };
            self.error(obj.span, hint);
            return Ty::Unknown;
        }
        // `module.Enum.Variant` used as a value: a bound module dotted with one of its enums dotted
        // with a nullary variant — the qualified analogue of the bare `Enum.Variant` value form.
        if let ExprKind::Field {
            obj: inner_obj,
            name: ename,
            ..
        } = &obj.kind
            && let ExprKind::Ident(mname) = &inner_obj.kind
            && !self.is_local_binding(mname)
            && let Some(mid) = self.imported_modules.get(mname).cloned()
            && let Some(sig) = self.module_sigs.get(&mid).cloned()
            && let Some(edef) = sig.enum_defs.get(ename)
        {
            match edef.variant_names.iter().position(|v| v == name) {
                Some(i) if edef.variants[i].payload.is_empty() => {
                    return Ty::Enum(
                        self.type_key(&mid, ename),
                        vec![Ty::Unknown; edef.type_params.len()],
                    );
                }
                Some(_) => {
                    self.error(
                        obj.span,
                        format!("variant '{name}' of enum '{ename}' carries a payload; construct it as {mname}.{ename}.{name}(…)"),
                    );
                    return Ty::Unknown;
                }
                None => {
                    self.error(obj.span, format!("enum '{ename}' has no variant '{name}'"));
                    return Ty::Unknown;
                }
            }
        }
        // `Enum.Variant` used as a value: a bare *unbound* name that is an enum, dotted with one of
        // its nullary variants — sugar for the bare `Variant`. A real binding (struct/tuple/local
        // named like the enum) wins, so only when `lookup` finds nothing. The bare enum name is gated
        // by `enum_names` (visibility) and resolved to its runtime key for the layout lookup.
        if let ExprKind::Ident(ename) = &obj.kind
            && !self.is_local_binding(ename)
            && self.enum_names.contains(ename)
        {
            let ekey = self.bare_key(ename);
            let resolved = self
                .variants
                .get(&(ekey.clone(), name.to_string()))
                .cloned();
            match resolved {
                Some(v) if v.payload.is_empty() => {
                    let nparams = self.enum_type_params.get(&ekey).map_or(0, |t| t.len());
                    return Ty::Enum(ekey, vec![Ty::Unknown; nparams]);
                }
                Some(_) => {
                    self.error(
                        obj.span,
                        format!("variant '{name}' of enum '{ename}' carries a payload; construct it as {ename}.{name}(…)"),
                    );
                    return Ty::Unknown;
                }
                None => {
                    self.error(obj.span, format!("enum '{ename}' has no variant '{name}'"));
                    return Ty::Unknown;
                }
            }
        }
        // `Type[T…].Variant` used as a VALUE (no call) — the declaration-site turbofish on a nullary
        // variant: `Box[int].Empty`. Mirrors the bare `Enum.Variant` value form above, but returns
        // the EXPLICIT type args (resolved), not `Unknown`. Both carriers (single-arg `Index`,
        // multi-arg `TypeApply`) converge through `type_apply_head`.
        if let Some((tname, type_exprs)) = self.type_apply_head(obj)
            && self.enum_names.contains(&tname)
        {
            let ekey = self.bare_key(&tname);
            let resolved: Vec<Ty> = type_exprs
                .iter()
                .map(|t| self.resolve_type(t, obj.span))
                .collect();
            // Arity-check the explicit args against the enum's params (reuse `seed_targs`).
            let tps = self
                .enum_type_params
                .get(&ekey)
                .cloned()
                .unwrap_or_default();
            self.seed_targs(&tname, &tps, &resolved, obj.span);
            match self
                .variants
                .get(&(ekey.clone(), name.to_string()))
                .cloned()
            {
                Some(v) if v.payload.is_empty() => {
                    return Ty::Enum(ekey, resolved);
                }
                Some(_) => {
                    self.error(
                        obj.span,
                        format!("variant '{name}' of enum '{tname}' carries a payload; construct it as {tname}[…].{name}(…)"),
                    );
                    return Ty::Unknown;
                }
                None => {
                    self.error(obj.span, format!("enum '{tname}' has no variant '{name}'"));
                    return Ty::Unknown;
                }
            }
        }
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
                        return Ty::Func {
                            params,
                            ret: Box::new(subst(&sig.ret, &map)),
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
                            // A first-class function VALUE is fixed-arity (Ty::Func carries no
                            // optional tail), so expose only the REQUIRED params — `f := request.get`
                            // then `f(url)` keeps working. The optional tail (e.g. timeout_ms) is
                            // reachable via a direct `request.get(url, ms)` call.
                            Some(Ty::Func {
                                params: fsig.params[..fsig.min_params].to_vec(),
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
        match self.infer_value(obj) {
            Ty::Map(k, v) => {
                let idx_ty = self.infer_value(index);
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
                    let idx_ty = self.infer_value(index);
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
                    let idx_ty = self.infer_value(index);
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
        let k = bound
            .args
            .first()
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        let v = bound
            .args
            .get(1)
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// The `(K, V)` of a bounded type parameter's `IndexSet` bound (write requires `IndexSet`
    /// specifically — a read-only `Index` bound is not assignable). `None` ⇒ no `IndexSet` bound.
    fn param_indexset_kv(&mut self, name: &str, span: Span) -> Option<(Ty, Ty)> {
        let bound = self
            .type_params
            .get(name)?
            .iter()
            .find(|b| b.name == "IndexSet")
            .cloned()?;
        let k = bound
            .args
            .first()
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        let v = bound
            .args
            .get(1)
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// Type `obj[start:end:step]`. Each *present* component must be `int`; the result type follows the
    /// `Slice` protocol — `list[T] → list[T]`, `str → str`, or a struct's
    /// `slice(self, int?, int?, int?) -> R`.
    fn infer_slice(
        &mut self,
        obj: &Expr,
        start: Option<&Expr>,
        end: Option<&Expr>,
        step: Option<&Expr>,
        span: Span,
    ) -> Ty {
        // Only the *present* components are constrained to int; an omitted bound/step is `None`.
        for comp in [start, end, step].into_iter().flatten() {
            self.expect_int(comp, "slice bound");
        }
        let obj_ty = self.infer_value(obj);
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
            return bound
                .args
                .first()
                .map(|a| self.resolve_type(a, span))
                .unwrap_or(Ty::Unknown);
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
        // The enclosing function must be able to early-return the Err/None. The operand's sum-type
        // KIND must match the enclosing return's KIND — a Result-`?` early-returns an `Err`, so the
        // function must itself return `Result`; an Option-`?` early-returns a `None`, so it must
        // return `Option`. `Nil` (top-level / `fn main()` — the interpreter unwinds at the boundary)
        // accepts either. Mixing kinds would make the function return the wrong sum-type and fault a
        // downstream exhaustive `match`/`??` at runtime even though `check` passed.
        match t {
            Ty::Result(ok, err) => {
                match self.current_ret.clone() {
                    // Propagating an `Err` early-returns it as the enclosing function's error, so the
                    // inner error type must fit the enclosing one (Rust-like).
                    Ty::Result(_, re) => {
                        if !self.assignable(&re, &err) {
                            self.error(
                                span,
                                format!("'?' propagates error {err}, but the enclosing function's error type is {re}"),
                            );
                        }
                    }
                    // Top-level / `fn main()` — the interpreter unwinds the Err at the boundary.
                    Ty::Nil => {}
                    Ty::Option(_) => {
                        self.error(
                            span,
                            "'?' propagates a Result error, but the enclosing function returns Option, not Result".to_string(),
                        );
                    }
                    other => {
                        self.error(
                            span,
                            format!(
                                "'?' used in a function that returns {other}, not Result or Option"
                            ),
                        );
                    }
                }
                *ok
            }
            Ty::Option(inner) => {
                match self.current_ret.clone() {
                    Ty::Option(_) | Ty::Nil => {}
                    Ty::Result(..) => {
                        self.error(
                            span,
                            "'?' propagates a None, but the enclosing function returns Result, not Option".to_string(),
                        );
                    }
                    other => {
                        self.error(
                            span,
                            format!(
                                "'?' used in a function that returns {other}, not Result or Option"
                            ),
                        );
                    }
                }
                *inner
            }
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
        let arg_ty = self.infer_value(arg);
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

    /// A free closure's param type, inferred from how its body USES the param (sources #2/#3 — only
    /// when there is no expected/slot type). **Shallow + precise** (closes the bare-param structural
    /// trap without over-pinning): source #2 — a `match` whose scrutinee is the BARE param identifier
    /// (`match x:`, not `match x.f:` / `match g(x):`), pinned from its first concrete arm; source #3 —
    /// a member access on the bare param (`x.f` / `x.m()`) whose name is declared by exactly one struct.
    /// Source #2 wins. Does NOT descend into nested closures (an inner param shadowing the name is
    /// unrelated). Read-only; returns `None` when nothing pins the param.
    fn scan_free_closure_param(&self, name: &str, body: &Expr) -> Option<Ty> {
        let mut match_pin = None;
        let mut member_pin = None;
        self.scan_expr_for_pin(name, body, &mut match_pin, &mut member_pin);
        match_pin.or(member_pin)
    }

    /// Walk `e` (skipping nested closures) accumulating the source-#2 (`match_pin`) and source-#3
    /// (`member_pin`) candidates for a free closure's param `name`. Stops descending once a source-#2
    /// pin is found (highest priority). See [`Checker::scan_free_closure_param`].
    fn scan_expr_for_pin(
        &self,
        name: &str,
        e: &Expr,
        match_pin: &mut Option<Ty>,
        member_pin: &mut Option<Ty>,
    ) {
        if match_pin.is_some() {
            return;
        }
        match &e.kind {
            // Source #2: a match whose scrutinee is the BARE param — pin from the first concrete arm.
            ExprKind::Match { scrutinee, arms } => {
                if let ExprKind::Ident(s) = &scrutinee.kind
                    && s == name
                {
                    for arm in arms {
                        if let Some(t) = self.pin_ty_of_pattern(&arm.pattern) {
                            *match_pin = Some(t);
                            return;
                        }
                    }
                }
                self.scan_expr_for_pin(name, scrutinee, match_pin, member_pin);
                for arm in arms {
                    // Scope-awareness: an arm whose pattern BINDS `name` (a tuple/variant sub-position
                    // or a bare catch-all of the same spelling) shadows the closure param inside the
                    // guard + body — a `match <name>:` there reads that binding, not the param, so it
                    // must NOT pin. Skip the shadowed arm's guard/body.
                    if pattern_binds(&arm.pattern, name) {
                        continue;
                    }
                    if let Some(g) = &arm.guard {
                        self.scan_expr_for_pin(name, g, match_pin, member_pin);
                    }
                    self.scan_expr_for_pin(name, &arm.body, match_pin, member_pin);
                }
            }
            // Source #3: a member access (field or method receiver) on the bare param.
            ExprKind::Field {
                obj, name: member, ..
            } => {
                if member_pin.is_none()
                    && let ExprKind::Ident(r) = &obj.kind
                    && r == name
                    && let Some(t) = self.unique_member_owner(member)
                {
                    *member_pin = Some(t);
                }
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
            }
            // A nested closure is its own scope — never descend (it may shadow `name`).
            ExprKind::Closure { .. } => {}
            // Every other expression: recurse into its child expressions.
            ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
                for c in es {
                    self.scan_expr_for_pin(name, c, match_pin, member_pin);
                }
            }
            ExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    self.scan_expr_for_pin(name, k, match_pin, member_pin);
                    self.scan_expr_for_pin(name, v, match_pin, member_pin);
                }
            }
            ExprKind::Comprehension {
                key, elem, clauses, ..
            } => {
                // Scope-awareness: a clause's `vars` shadow `name` for every LATER clause's
                // iter/guards and for the key/elem. Scan each clause's iter (evaluated before this
                // clause binds), then stop once a clause binds `name` — its own guards and everything
                // downstream read the shadowing binding, not the param.
                let mut shadowed = false;
                for c in clauses {
                    if !shadowed {
                        self.scan_expr_for_pin(name, &c.iter, match_pin, member_pin);
                    }
                    if c.vars.iter().any(|v| v == name) {
                        shadowed = true;
                    }
                    if !shadowed {
                        for g in &c.guards {
                            self.scan_expr_for_pin(name, g, match_pin, member_pin);
                        }
                    }
                }
                if !shadowed {
                    if let Some(k) = key {
                        self.scan_expr_for_pin(name, k, match_pin, member_pin);
                    }
                    self.scan_expr_for_pin(name, elem, match_pin, member_pin);
                }
            }
            ExprKind::Unary { expr, .. } => {
                self.scan_expr_for_pin(name, expr, match_pin, member_pin)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.scan_expr_for_pin(name, lhs, match_pin, member_pin);
                self.scan_expr_for_pin(name, rhs, match_pin, member_pin);
            }
            ExprKind::Range { start, end } => {
                self.scan_expr_for_pin(name, start, match_pin, member_pin);
                self.scan_expr_for_pin(name, end, match_pin, member_pin);
            }
            ExprKind::Call { callee, args, .. } => {
                self.scan_expr_for_pin(name, callee, match_pin, member_pin);
                for a in args {
                    self.scan_expr_for_pin(name, a, match_pin, member_pin);
                }
            }
            ExprKind::Index { obj, index } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                self.scan_expr_for_pin(name, index, match_pin, member_pin);
            }
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                for c in [start, end, step].into_iter().flatten() {
                    self.scan_expr_for_pin(name, c, match_pin, member_pin);
                }
            }
            ExprKind::Try(inner) => self.scan_expr_for_pin(name, inner, match_pin, member_pin),
            ExprKind::DecodeCall { obj, arg, .. } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                self.scan_expr_for_pin(name, arg, match_pin, member_pin);
            }
            ExprKind::IfElse { cond, then, els } => {
                self.scan_expr_for_pin(name, cond, match_pin, member_pin);
                self.scan_expr_for_pin(name, then, match_pin, member_pin);
                self.scan_expr_for_pin(name, els, match_pin, member_pin);
            }
            // String interpolation: the `{…}` fragment expressions are not stored as child `Expr`s —
            // they live inside the raw text and are produced on demand by the shared interpolation
            // parser (the same one `check_interpolation` uses). Parse + scan them so a param pinned
            // ONLY by a member access inside an interpolation (`"{x.f}"`) resolves via source #3. A
            // malformed interpolation is ignored here (it is diagnosed by `check_interpolation`).
            ExprKind::Str(raw) => {
                if let Ok(chunks) = crate::interpolation::parse_interpolation(raw, e.span) {
                    for chunk in &chunks {
                        if let crate::interpolation::Chunk::Expr(frag, _) = chunk {
                            self.scan_expr_for_pin(name, frag, match_pin, member_pin);
                        }
                    }
                }
            }
            // `?.`/`??` carriers are lowered before checking. `recover:` carries a statement block
            // (which can introduce its own bindings); it is NOT scanned — a param pinnable only from
            // inside a `recover:` body stays un-inferable and requires an annotation (sound: this is
            // the conservative v1 fallback, never a mis-pin). Leaves (`Ident`/literals/`TypeApply`/
            // `RawStr`) have no child to scan.
            _ => {}
        }
    }

    /// The scrutinee type a top-level match arm pattern implies (source #2 classification). Mirrors
    /// [`Checker::reconstruct_unknown_kind`]'s arm classification: a qualified/unique enum variant or
    /// builtin `Ok`/`Err`/`Some`/`None` → that enum/Result/Option (type args `Unknown`); a tuple → an
    /// all-`Unknown` tuple of that arity; a literal/range → its scalar; a binding/wildcard/ambiguous →
    /// `None` (no pin).
    fn pin_ty_of_pattern(&self, p: &Pattern) -> Option<Ty> {
        match p {
            Pattern::Or(alts) => alts.first().and_then(|a| self.pin_ty_of_pattern(a)),
            Pattern::Tuple(subs) => Some(Ty::Tuple(vec![Ty::Unknown; subs.len()])),
            Pattern::Literal(lit) => Some(lit_pattern_ty(lit)),
            Pattern::Range { .. } => Some(Ty::Int),
            Pattern::Variant {
                name,
                enum_name,
                module_name,
                ..
            } => {
                // A module-qualified variant can't be resolved through the bare-name table — no pin.
                if module_name.is_some() {
                    return None;
                }
                if let Some(en) = enum_name {
                    let key = self.bare_key(en);
                    return self
                        .enums
                        .contains_key(&key)
                        .then(|| self.enum_ty_unknown_args(&key));
                }
                match name.as_str() {
                    "Ok" | "Err" => Some(Ty::Result(Box::new(Ty::Unknown), Box::new(Ty::Unknown))),
                    "Some" | "None" => Some(Ty::Option(Box::new(Ty::Unknown))),
                    other => {
                        // A bare variant uniquely owned by one enum pins it; an ambiguous one, or a
                        // bare binding name (not a known variant), does not.
                        let owners = self.variant_owners.get(other)?;
                        if owners.len() != 1 {
                            return None;
                        }
                        let key = self.bare_key(&owners[0]);
                        self.enums
                            .contains_key(&key)
                            .then(|| self.enum_ty_unknown_args(&key))
                    }
                }
            }
            Pattern::Ident(..) | Pattern::Wildcard => None,
        }
    }

    /// A user enum's `Ty::Enum` with its type arguments filled as `Unknown` (the scrutinee shape an
    /// arm pattern pins — element types are unknown, the enum identity is what matters for call-site
    /// checking).
    fn enum_ty_unknown_args(&self, key: &str) -> Ty {
        let n = self
            .enum_type_params
            .get(key)
            .map(|tps| tps.len())
            .unwrap_or(0);
        Ty::Enum(key.to_string(), vec![Ty::Unknown; n])
    }

    /// If exactly one struct declares a field OR method `member`, return that struct's type (type args
    /// `Unknown`); else `None` (source #3 only fires for a UNIQUELY-owned member — a name shared by
    /// >1 type, or none, never pins). Read-only.
    fn unique_member_owner(&self, member: &str) -> Option<Ty> {
        // A member shared by any PARAMETERIZED collection (`len`/`map`/`get`/`push`/… on
        // `list`/`map`/`set`) is never a unique pin: it is shared across types AND would only pin a
        // weak `list[Unknown]`-style type (design §3 — "methods/fields shared by >1 type … never
        // pin"). Bail before collecting owners so such members fall through to the annotation rule.
        if list_method_sig(member, &Ty::Unknown).is_some()
            || map_method_sig(member, &Ty::Unknown, &Ty::Unknown).is_some()
            || set_method_sig(member, &Ty::Unknown).is_some()
        {
            return None;
        }
        // Collect every PINNABLE owner of `member`: user structs (by field or method) plus the
        // concrete scalar builtins `str`/`bytes` (the design's `x.upper()` → `str` case). The pin
        // fires only when exactly one type owns it.
        let mut owners: Vec<Ty> = Vec::new();
        for (key, info) in &self.structs {
            // Source #3 pins only from a USER type. The `Builtin`-origin native structs
            // (Match/Response/ProcResult/…) are seeded into `self.structs` unconditionally at init
            // regardless of imports, so scanning them would mis-pin a param to an unimported,
            // unreferenced builtin (their fields like `end`/`code`/`status`). Skip them.
            if info.origin == StructOrigin::Builtin {
                continue;
            }
            let has =
                info.fields.iter().any(|(n, _)| n == member) || info.methods.contains_key(member);
            if has {
                let n = info.type_params.len();
                owners.push(Ty::Struct(key.clone(), vec![Ty::Unknown; n]));
            }
        }
        if str_method_sig(member).is_some() {
            owners.push(Ty::Str);
        }
        if bytes_method_sig(member).is_some() {
            owners.push(Ty::Bytes);
        }
        if owners.len() == 1 {
            owners.pop()
        } else {
            None
        }
    }

    fn infer_closure(
        &mut self,
        params: &[Param],
        ret: Option<&Type>,
        body: &Expr,
        expected: Option<&Ty>,
    ) -> Ty {
        // Source #1 — the *expected* type of the slot the closure literal sits in. When it is a
        // `fn(..)` whose arity matches, an UNANNOTATED param binds to the expected param type
        // (checking-mode), and a non-`Unknown` expected return becomes the body's return context.
        // On an arity mismatch the params stay `Unknown` here and the call site's `assignable` check
        // reports the mismatch (single diagnostic).
        let (exp_params, exp_ret): (Option<Vec<Ty>>, Option<Ty>) = match expected {
            Some(Ty::Func { params: p, ret: r }) if p.len() == params.len() => {
                (Some(p.clone()), Some((**r).clone()))
            }
            _ => (None, None),
        };
        // A `fn`-typed slot whose arity does NOT match: keep unannotated params silently `Unknown`
        // here and let the call site's `assignable` check report the single arity diagnostic — do
        // NOT route them through the free-closure scan (which would emit a spurious, misdirecting
        // "cannot infer type of parameter").
        let expected_arity_mismatch =
            matches!(expected, Some(Ty::Func { params: p, .. }) if p.len() != params.len());
        // A closure body opens a fresh loop context (same rule as `check_fn_body`): a loop around
        // the closure's definition must not make a `break`/`continue` inside it legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
        // `?` inside the body targets THIS closure's return, not the enclosing function's. With no
        // annotation there is no Result/Option context, so `?` is rejected (`Unknown` → `infer_try`
        // errors). Mirrors `check_fn_body`'s `current_ret` handling. An expected (slot) return type
        // supplies that context when the closure is unannotated.
        let declared_ret = ret
            .map(|t| self.resolve_type(t, body.span))
            .or_else(|| exp_ret.clone().filter(|r| !r.is_unknown()))
            .unwrap_or(Ty::Unknown);
        let saved_ret = std::mem::replace(&mut self.current_ret, declared_ret);
        // A closure inside a generator is NOT itself a generator: clear the yield context so a stray
        // `yield` in the closure is diagnosed as "outside a generator", not bound to the enclosing
        // one. (Closure bodies are single expressions today, so this is a latent-invariant guard.)
        let saved_yield = self.yield_ty.take();
        self.push_scope();
        let param_tys: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // A closure `ref T` param is a `Ref[T]` box, exactly like a named-fn `ref` param
                // (charge 3): the body's reads/writes were lowered to `.get()`/`.set()` by desugar,
                // and a `ref` arg aliases at the call site. `check_ref_ty` rejects a non-boxable
                // pointee. Without a `ref`, the param keeps its by-value type, or — for an
                // unannotated param — the expected (slot) param type (source #1), else `Unknown`.
                let ty = match &p.ty {
                    Some(t) if p.is_ref => {
                        self.check_ref_ty(t, body.span);
                        self.resolve_type(
                            &Type::Generic("Ref".to_string(), vec![t.clone()], Span::default()),
                            body.span,
                        )
                    }
                    Some(t) => self.resolve_type(t, body.span),
                    None => {
                        // An unannotated param: prefer the expected (slot) param type (source #1);
                        // else infer it from the body — source #2 (a match whose scrutinee is the
                        // bare param) / source #3 (a uniquely-owned member access).
                        //
                        // An `Unknown` expected param type is NOT a pin: it arises when a generic
                        // slot's type param was unified ONLY from this closure (`store(fn(a): …)` →
                        // `T = fn(Unknown) -> Unknown`), so binding the param to it silently would
                        // leave the call site unchecked → check-passes-then-traps. Filter it out and
                        // fall through to the body scan / annotation requirement (soundness).
                        if let Some(t) = exp_params
                            .as_ref()
                            .and_then(|ps| ps.get(i))
                            .filter(|t| !t.is_unknown())
                            .cloned()
                        {
                            t
                        } else if expected_arity_mismatch {
                            // Arity mismatch against a `fn`-typed slot — stay `Unknown`; the call
                            // site reports the mismatch (single diagnostic).
                            Ty::Unknown
                        } else if self.generic_arg_prepass {
                            // Generic unification prepass: keep the param `Unknown` so the other
                            // args / substituted slot type drive unification; `check_generic_arg`
                            // re-infers it in checking-mode afterwards. Running the free scan here
                            // would corrupt unification (see `generic_arg_prepass` doc).
                            Ty::Unknown
                        } else if let Some(t) = self.scan_free_closure_param(&p.name, body) {
                            t
                        } else {
                            // Genuinely unresolved: no expected/slot type and nothing in the body
                            // pins it. Require an annotation rather than degrade the param to a
                            // runtime `Unknown` value (the one place `Unknown` could reach a value).
                            // Bind `Unknown` after erroring so the body still checks (no cascade).
                            self.error(
                                p.name_span,
                                format!(
                                    "cannot infer type of parameter '{}'; add a type annotation",
                                    p.name
                                ),
                            );
                            Ty::Unknown
                        }
                    }
                };
                // Editor hover: record the closure param's type at its DECL-site name span (no-op
                // off-probe; first-hit-wins, so a body-use span records separately). SKIP during the
                // generic-arg unification prepass: there an unannotated param is forced `Unknown`
                // (see the `generic_arg_prepass` arm above), and first-hit-wins would latch that `?`
                // over the real type the later per-arg check (run with the substituted slot type and
                // `generic_arg_prepass=false`) infers — so `xs.map(fn(a): a + 1)` would hover `?`.
                if !self.generic_arg_prepass {
                    self.hover_record_at(p.name_span, &ty, HoverKind::Param, None);
                }
                self.declare(&p.name, ty.clone());
                if p.is_ref {
                    self.declare_ref(&p.name);
                }
                ty
            })
            .collect();
        let body_ty = self.infer(body);
        self.pop_scope();
        self.loop_depth = saved_loop_depth;
        self.recover_depth = saved_recover;
        self.current_ret = saved_ret;
        self.yield_ty = saved_yield;
        let ret_ty = match ret {
            Some(t) => {
                let declared = self.resolve_type(t, body.span);
                if !self.assignable(&declared, &body_ty) {
                    self.error(
                        body.span,
                        format!(
                            "closure body has type {body_ty}, but its return type is {declared}"
                        ),
                    );
                }
                declared
            }
            None => body_ty,
        };
        Ty::Func {
            params: param_tys,
            ret: Box::new(ret_ty),
        }
    }

    // ===== calls =====

    fn infer_call(
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
        // `print(..., sep=, end=)` is the only call whose named args survive desugar. Type-check the
        // `sep`/`end` value(s) as `str` here (desugar already validated the key names). Any other
        // call should have an empty `named` post-desugar.
        if !named.is_empty()
            && let ExprKind::Ident(name) = &callee.kind
            && name == "print"
            && self.lookup(name).is_none()
        {
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
        // Explicit call-site type arguments `name[T, …](…)`. Resolved once here; only generic
        // by-name calls (fn / struct / variant constructors) can consume them.
        let targs: Vec<Ty> = type_args
            .iter()
            .map(|t| self.resolve_type(t, span))
            .collect();
        // Method call: `obj.method(args)`. The parser never attaches type args to a method callee.
        if let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &callee.kind
        {
            // `module.Struct(args)` — qualified struct constructor. `module` is a bound module name
            // whose sig declares struct `name`. Inject nothing: resolve the constructor through the
            // sig's struct shape (mirrors `infer_named_call`'s struct path, with type args).
            if let ExprKind::Ident(mname) = &obj.kind
                && !self.is_local_binding(mname)
                && let Some(mid) = self.imported_modules.get(mname).cloned()
                && let Some(sig) = self.module_sigs.get(&mid).cloned()
                && let Some(info) = sig.struct_defs.get(name)
            {
                let key = self.type_key(&mid, name);
                return self
                    .infer_qualified_struct_call(info, name, &key, args, &targs, span, expected);
            }
            // `module.NewType(args)` — qualified newtype constructor: one arg of the underlying
            // type, returns the newtype keyed to the declaring module (mirrors the bare newtype
            // ctor in `infer_named_call`; the struct arm above already consumed any struct name).
            if let ExprKind::Ident(mname) = &obj.kind
                && !self.is_local_binding(mname)
                && let Some(mid) = self.imported_modules.get(mname).cloned()
                && let Some(sig) = self.module_sigs.get(&mid).cloned()
                && let Some(info) = sig.newtype_defs.get(name)
            {
                let key = self.type_key(&mid, name);
                let under = info.underlying.clone();
                let tps = info.type_params.clone();
                return self
                    .infer_newtype_call(name, &key, &under, &tps, args, &targs, span, expected);
            }
            // `module.Enum.Variant(args)` — qualified payload-variant constructor.
            if let ExprKind::Field {
                obj: inner_obj,
                name: ename,
                ..
            } = &obj.kind
                && let ExprKind::Ident(mname) = &inner_obj.kind
                && !self.is_local_binding(mname)
                && let Some(mid) = self.imported_modules.get(mname).cloned()
                && let Some(sig) = self.module_sigs.get(&mid).cloned()
                && let Some(edef) = sig.enum_defs.get(ename)
            {
                if let Some(vinfo) = edef
                    .variant_names
                    .iter()
                    .position(|v| v == name)
                    .map(|i| edef.variants[i].clone())
                {
                    // The OLD gliding form `module.Enum.Variant[T](args)` (type args on the VARIANT)
                    // is removed — explicit type args go on the TYPE: `module.Enum[T].Variant(args)`.
                    if !targs.is_empty() {
                        self.infer_all(args);
                        self.error(
                            span,
                            format!(
                                "put the type arguments on the type: {}.{ename}[{}].{name}(...)",
                                mname,
                                render_targs(&targs)
                            ),
                        );
                        return Ty::Unknown;
                    }
                    let mut vi = vinfo;
                    // The result `Ty::Enum` carries the DECLARING module's runtime key (bare unless a
                    // genuine clash), matching the layout tables + the declaring module's signatures.
                    vi.enum_name = self.type_key(&mid, ename);
                    return self
                        .infer_variant_call(&vi, name, args, &targs, *name_span, span, expected);
                }
                // Not a variant — a QUALIFIED enum STATIC method `module.Enum.method(args)`. Mirror the
                // bare enum-static path: variant-first ran above (a variant always wins, disjointness
                // enforced at decl), so delegate to `infer_static_call` keyed by the declaring module's
                // runtime key. Emits "type 'Enum' has no static method 'm'" for a genuine miss.
                let key = self.type_key(&mid, ename);
                return self.infer_static_call(
                    &key,
                    ename,
                    name,
                    args,
                    &[],
                    &targs,
                    *name_span,
                    span,
                );
            }
            // `module.Struct.method(args)` — a QUALIFIED struct STATIC method. The enum arm above
            // consumed enum names; this covers structs declared in a bound (non-local) module. Resolve
            // the type's module-scoped key and delegate to `infer_static_call` (the SAME path the bare
            // `Type.static_method()` form uses). Placed before the bare-type / native-ctor arms so a
            // qualified static call no longer falls through to "module has no member 'Struct'".
            if let ExprKind::Field {
                obj: inner_obj,
                name: tname,
                ..
            } = &obj.kind
                && let ExprKind::Ident(mname) = &inner_obj.kind
                && !self.is_local_binding(mname)
                && let Some(mid) = self.imported_modules.get(mname).cloned()
                && let Some(sig) = self.module_sigs.get(&mid).cloned()
                && sig.struct_defs.contains_key(tname)
            {
                let key = self.type_key(&mid, tname);
                return self.infer_static_call(
                    &key,
                    tname,
                    name,
                    args,
                    &[],
                    &targs,
                    *name_span,
                    span,
                );
            }
            // `Enum.Variant(args)` — qualified payload-variant constructor. Same gate as the nullary
            // value form in `infer_field`: an unbound enum name dotted with one of its variants. The
            // bare-written enum name is gated by `enum_names` (bare visibility) and resolved to its
            // runtime key (`bare_key`) for the layout lookup.
            if let ExprKind::Ident(ename) = &obj.kind
                && !self.is_local_binding(ename)
                && self.enum_names.contains(ename)
            {
                let ekey = self.bare_key(ename);
                // Editor hover (probe-gated no-op): record the receiver `Col` of `Col.Val(3)` /
                // `Col.method()` as its enum type. Covers both the variant-ctor and enum-static paths.
                if self.hover_probe.is_some() {
                    self.hover_record_at(
                        obj.span,
                        &Ty::Enum(ekey.clone(), Vec::new()),
                        HoverKind::Other,
                        None,
                    );
                }
                if self
                    .variants
                    .contains_key(&(ekey.clone(), name.to_string()))
                {
                    // The OLD gliding form `Enum.Variant[T](args)` (type args on the VARIANT) is
                    // removed — explicit type args now go on the TYPE: `Enum[T].Variant(args)`.
                    if !targs.is_empty() {
                        self.infer_all(args);
                        self.error(
                            span,
                            format!(
                                "put the type arguments on the type: {ename}[{}].{name}(...)",
                                render_targs(&targs)
                            ),
                        );
                        return Ty::Unknown;
                    }
                    if let Some(ty) = self.infer_named_call(
                        name,
                        args,
                        &targs,
                        *name_span,
                        span,
                        Some(&ekey),
                        expected,
                    ) {
                        return ty;
                    }
                } else {
                    // Not a variant — try a STATIC method `Enum.method(args)` (variant check ran
                    // first, so a variant always wins; disjointness is enforced at decl time). The
                    // member-level turbofish (`Enum.method[U](...)`) is the bare carrier of the
                    // method's OWN `[U]` args (PART 2): pass them as `mtargs` (no enclosing turbofish).
                    return self.infer_static_call(
                        &ekey,
                        ename,
                        name,
                        args,
                        &[],
                        &targs,
                        *name_span,
                        span,
                    );
                }
            }
            // `Type.method(args)` — STATIC (associated) method on a bare struct/enum type name. The
            // enum branch above already handled enums; this covers structs. The type name must be a
            // known (unbound) struct; a static method is one whose first param is not `self`.
            if let ExprKind::Ident(tname) = &obj.kind
                && !self.is_local_binding(tname)
                && self.struct_names.contains(tname)
            {
                let key = self.bare_key(tname);
                // Editor hover (probe-gated no-op): record the receiver `Foo` of `Foo.default()` as
                // its struct type.
                if self.hover_probe.is_some() {
                    self.hover_record_at(
                        obj.span,
                        &Ty::Struct(key.clone(), Vec::new()),
                        HoverKind::Other,
                        None,
                    );
                }
                // The member-level turbofish (`Type.method[U](...)`) is the bare carrier of the
                // method's OWN `[U]` args (PART 2): pass them as `mtargs` (no enclosing turbofish).
                return self.infer_static_call(
                    &key,
                    tname,
                    name,
                    args,
                    &[],
                    &targs,
                    *name_span,
                    span,
                );
            }
            // `Type[T…].member(args)` — declaration-site turbofish for a generic TYPE: a VARIANT
            // constructor (`Box[int].Has(5)`, `E[int, str].Pair(…)`) or a generic STATIC method
            // (`Box[int].empty()`). Two carriers converge here:
            //   • SINGLE type arg — `Field{obj: Index{Ident(Type), idx}, name}` (the `[..]` is
            //     followed by `.` not `(`, so the turbofish-call steal never fires; the parser can't
            //     tell `Type[int].x` from `arr[i].field`, so the checker reinterprets the index).
            //   • MULTI type arg — `Field{obj: TypeApply{name, args}, name}` (the parser committed a
            //     real type list because of the disambiguating comma).
            // VARIANT-FIRST (a same-named static method is barred at decl time by disjointness); if
            // no variant matches the member name, fall to the static-method path.
            if let Some((tname, type_exprs)) = self.type_apply_head(obj) {
                let key = self.bare_key(&tname);
                let resolved: Vec<Ty> = type_exprs
                    .iter()
                    .map(|t| self.resolve_type(t, span))
                    .collect();
                if let Some(v) = self.variants.get(&(key.clone(), name.to_string())).cloned() {
                    // A variant ctor takes NO method-level type args. Under the broadened parser steal
                    // the combined `Box[int].Has[str](5)` now arrives here as a Field callee carrying
                    // `targs=[str]` (it used to ride the Index-over-Field block below, which errored);
                    // preserve that error rather than silently dropping the targs.
                    if !targs.is_empty() {
                        self.error(
                            span,
                            format!("variant '{name}' of '{tname}' takes no method type arguments"),
                        );
                    }
                    return self
                        .infer_variant_call(&v, name, args, &resolved, *name_span, span, expected);
                }
                // `targs` is the member-level (method) turbofish — `Box[int].make[str](x)` arrives here
                // as a Field callee under the broadened steal, with the enclosing `[int]` in `resolved`
                // and the method `[str]` in `targs`. Thread `targs` as the static method's `mtargs` so
                // the combined form composes (was `&[]`, which dropped the method turbofish).
                return self.infer_static_call(
                    &key, &tname, name, args, &resolved, &targs, *name_span, span,
                );
            }
            // `module.Ctor(args)` — a qualified native builtin CONSTRUCTOR (`concurrency.Shared(0)`,
            // aliased `c.Shared(0)`, `time.timer(100)`). `module` is a bound (non-local) module name
            // whose sig declares `name` in `sig.types` — and those reserved names live ONLY in the
            // owning native module's sig, so this fires solely for native builtins. Concurrency
            // ctors + `timer` delegate to `infer_named_call` (the SAME value-first inference + license
            // check the bare name uses). The type-only handles (Socket/Listener) and FFI widths/ptr
            // have NO from-nothing constructor — reject with a clear message. Placed AFTER the
            // user-type qualified arms above and BEFORE the method-call fallthrough (so a genuine
            // module method like `time.now()` still reaches `infer_method_call`).
            if let ExprKind::Ident(mname) = &obj.kind
                && !self.is_local_binding(mname)
                && let Some(mid) = self.imported_modules.get(mname).cloned()
                && let Some(sig) = self.module_sigs.get(&mid).cloned()
                && sig.types.contains(name)
            {
                if matches!(
                    name.as_str(),
                    "Shared" | "RwShared" | "Atomic" | "Executor" | "timer"
                ) {
                    return self
                        .infer_named_call(name, args, &targs, *name_span, span, None, expected)
                        .unwrap_or(Ty::Unknown);
                }
                // A type-only native name (Socket/Listener/FFI width/ptr) — no from-nothing ctor.
                // Gated on `qualified_builtin_ty` so this fires ONLY for genuine native types; a
                // (non-builtin) user `sig.types` name — e.g. an exported type alias used as a bogus
                // `mod.Alias(x)` ctor — falls through to `infer_method_call` (its original error),
                // not this native-specific message.
                if self.qualified_builtin_ty(name, &[]).is_some() {
                    self.infer_all(args);
                    self.error(
                        span,
                        format!(
                            "'{mname}.{name}' has no constructor — it is a type-only native type (a value is obtained from the module's functions, e.g. net.connect/net.listen for a Socket)"
                        ),
                    );
                    return Ty::Unknown;
                }
            }
            return self.infer_method_call(obj, name, *name_span, args, &targs, span);
        }
        // Combined member-side turbofish — DEFENSIVE FALLBACK. Since the parser steal was broadened to
        // ANY `Field` receiver, the combined `Type[T].member[U](args)` now parses as a `Call{callee:
        // Field, type_args:[U]}` and is handled by the `Field`-callee dispatch above (the `type_apply_head`
        // branch). This `Index`-over-`Field`-callee block stays in place for the residual shapes that do
        // NOT get stolen — e.g. a head that is a value (`arr[i].field[k](x)`, where `resolved_head` is
        // `None` and we fall through to ordinary index-then-call). Reinterpret: the inner `Field`'s `obj`
        // is the enclosing-type head (a type-applied `Box[int]` or a bare `Ident(Box)`), the trailing
        // index is the single method type argument. Gate on the head being a KNOWN, NON-local struct/enum.
        if let ExprKind::Index {
            obj: callee_obj,
            index: mt,
        } = &callee.kind
            && let ExprKind::Field {
                obj: head,
                name,
                name_span,
            } = &callee_obj.kind
        {
            // Resolve the enclosing-type head + its type args. A bare `Ident(Box).member[U]` head has
            // NO enclosing type args; a `Box[int].member[U]` head carries them via `type_apply_head`.
            let resolved_head = self.type_apply_head(head).or_else(|| match &head.kind {
                ExprKind::Ident(tn)
                    if !self.is_local_binding(tn)
                        && (self.struct_names.contains(tn) || self.enum_names.contains(tn)) =>
                {
                    Some((tn.clone(), Vec::new()))
                }
                _ => None,
            });
            if let Some((tname, type_exprs)) = resolved_head
                && let Some(mt_ty) = self.index_as_type(mt).map(|t| self.resolve_type(&t, span))
            {
                let key = self.bare_key(&tname);
                let enclosing: Vec<Ty> = type_exprs
                    .iter()
                    .map(|t| self.resolve_type(t, span))
                    .collect();
                // VARIANT-FIRST (a same-named static is barred at decl time); a variant takes no
                // method-level type args, so a method turbofish on a variant is an error.
                if let Some(v) = self.variants.get(&(key.clone(), name.to_string())).cloned() {
                    self.error(
                        span,
                        format!("variant '{name}' of '{tname}' takes no method type arguments"),
                    );
                    return self.infer_variant_call(
                        &v, name, args, &enclosing, *name_span, span, expected,
                    );
                }
                return self.infer_static_call(
                    &key,
                    &tname,
                    name,
                    args,
                    &enclosing,
                    &[mt_ty],
                    *name_span,
                    span,
                );
            }
        }
        if let ExprKind::Ident(name) = &callee.kind {
            // Shadowing local (e.g. a closure bound to a variable) wins over a global of the same name.
            if self.lookup(name).is_none() {
                // Editor hover (probe-gated no-op): record a DISPLAY function type at the callee
                // token so hovering a CALL's callee yields its signature — the callee never reaches
                // `infer()`/`hover_record_expr`, so without this it returns None. We build the display
                // `Ty::Func` WITHOUT emitting any error and never touch normal checking results.
                // A free fn → its declared `FnSig` (a generic fn's params/ret stay `Ty::Param(T)`, so
                // it Displays "fn(T, T) -> T"); a struct ctor → fields-to-`Struct`; a reserved builtin
                // (print/range/List) → its `builtin_sig` display sig. Only bare enum variants record
                // nothing → hover stays None.
                if self.hover_probe.is_some()
                    && let Some(fty) = self.callee_display_ty(name)
                {
                    // doc: a user-defined free fn owns its `FnSig::doc` and NOTHING else — an
                    // undocumented user fn must NOT fall through to a builtin blurb (a user fn whose
                    // name shadows a builtin, e.g. `fn range(...)`, would otherwise show the builtin's
                    // usage text). Only a NON-user-fn callee (a struct/type-decl ctor via `name_docs`,
                    // or a reserved builtin ctor via `builtin_type_doc`) consults those fallbacks.
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
                if let Some(ty) =
                    self.infer_named_call(name, args, &targs, callee.span, span, None, expected)
                {
                    return ty;
                }
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
                // A closure/fn value coerces its float params at the prologue.
                self.check_args_w("closure", &params, args, span);
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

    /// Resolve a `Type[T…]` member-access head — the receiver of `Type[T…].member(args)` /
    /// nullary `Type[T…].member` — into `(type-name, type-arg-exprs)` when `obj` is a declaration-site
    /// turbofish on a KNOWN struct/enum name. Two carriers converge:
    ///   • `Index{obj: Ident(Type), index}` — the SINGLE-arg form (`Box[int].x`); reinterpret the
    ///     index expression as one type via `index_as_type`.
    ///   • `TypeApply{name, args}` — the MULTI-arg form (`Result[int, str].x`); the parser already
    ///     parsed real `Type`s.
    /// Returns `None` when `obj` is not such a head (a real index-then-member, a local binding, an
    /// unknown name, or a non-type index), so the caller falls back to the ordinary method path.
    fn type_apply_head(&self, obj: &Expr) -> Option<(String, Vec<Type>)> {
        match &obj.kind {
            ExprKind::TypeApply { name, args } => {
                if !self.is_local_binding(name)
                    && (self.struct_names.contains(name) || self.enum_names.contains(name))
                {
                    Some((name.clone(), args.clone()))
                } else {
                    None
                }
            }
            ExprKind::Index { obj: tobj, index } => {
                if let ExprKind::Ident(tname) = &tobj.kind
                    && !self.is_local_binding(tname)
                    && (self.struct_names.contains(tname) || self.enum_names.contains(tname))
                {
                    Some((tname.clone(), vec![self.index_as_type(index)?]))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Reinterpret the index of a `Type[..]` subscript (in a generic-static turbofish like
    /// `Box[int].empty()`) as a single type argument. The parser produced an EXPRESSION (the index
    /// of an `Index` node); only the expression shapes that name a type are convertible: a bare
    /// ident (`int`/`T`), a generic application (`list[int]`), and a module-qualified name
    /// (`geo.Point`). Anything else (a literal, an arithmetic expr) is not a type → `None`, and the
    /// caller falls back to the ordinary index-then-method path so a real index error still surfaces.
    fn index_as_type(&self, index: &Expr) -> Option<Type> {
        match &index.kind {
            ExprKind::Ident(n) => Some(Type::named(n.clone())),
            ExprKind::Index { obj, index } => {
                if let ExprKind::Ident(n) = &obj.kind {
                    Some(Type::Generic(
                        n.clone(),
                        vec![self.index_as_type(index)?],
                        Span::default(),
                    ))
                } else {
                    None
                }
            }
            ExprKind::Field { obj, name, .. } => {
                if let ExprKind::Ident(m) = &obj.kind {
                    Some(Type::Qualified {
                        module: m.clone(),
                        name: name.clone(),
                        args: Vec::new(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Type-check a STATIC (associated) method call `Type.method(args)` (the "no self ⇒ static"
    /// rule). `key` is the type's resolved runtime key; `tname` its display name. Looks the method up
    /// in the struct/enum method maps; rejects calling an INSTANCE method this way, or an unknown
    /// method. The enclosing type's params (`targs`, from `Box[int].empty()`) AND the method's OWN
    /// `[U]` params (`mtargs`, from the combined `Box[int].make[U](x)`) compose into ONE by-name
    /// substitution map: enclosing seeded from `targs`, method from `mtargs`, the rest inferred by
    /// unifying the declared param types against the argument types (like a generic free fn). Every
    /// un-inferred param — enclosing OR method — degrades to `Ty::Unknown` (a free `Ty::Param` must
    /// never leak). Mirrors the instance-method arms minus the receiver.
    #[allow(clippy::too_many_arguments)] // enclosing key/name + method + args + enclosing & method targs + span
    fn infer_static_call(
        &mut self,
        key: &str,
        tname: &str,
        method: &str,
        args: &[Expr],
        targs: &[Ty],
        mtargs: &[Ty],
        name_span: Span,
        span: Span,
    ) -> Ty {
        // Resolve the method sig + the enclosing type's params, from either map.
        let resolved = self
            .structs
            .get(key)
            .and_then(|info| {
                info.methods
                    .get(method)
                    .cloned()
                    .map(|s| (s, info.type_params.clone()))
            })
            .or_else(|| {
                self.enum_methods.get(key).and_then(|ms| {
                    ms.get(method).cloned().map(|s| {
                        (
                            s,
                            self.enum_type_params.get(key).cloned().unwrap_or_default(),
                        )
                    })
                })
            });
        let Some((sig, tps)) = resolved else {
            self.infer_all(args);
            self.error(
                span,
                format!("type '{tname}' has no static method '{method}'"),
            );
            return Ty::Unknown;
        };
        if !sig.is_static {
            self.infer_all(args);
            self.error(
                span,
                format!(
                    "'{method}' is an instance method of '{tname}'; call it on a value (`value.{method}(...)`)"
                ),
            );
            return Ty::Unknown;
        }
        // Editor hover (probe-gated no-op): record the static method's declared call signature at the
        // method-name token (`Foo.default()` → "fn() -> Foo"), mirroring the free-fn convention
        // (declared `FnSig` params/ret verbatim). Emits no error, changes no checking result.
        if self.hover_probe.is_some() {
            let fty = Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
        // A method-level turbofish (`Box.empty[int]()`) on a static method that declares NO own
        // `[U]` is an arity error: it takes no method type arguments.
        if !mtargs.is_empty() && sig.type_params.is_empty() {
            self.infer_all(args);
            self.error(
                span,
                format!(
                    "method '{method}' takes no type argument(s) (it declares no own type parameters)"
                ),
            );
            return Ty::Unknown;
        }
        // Non-generic static (no enclosing AND no method params): the simple substitution-free path.
        if tps.is_empty() && sig.type_params.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{tname}' takes no type arguments"));
            }
            self.check_args_w(method, &sig.params, args, span);
            return sig.ret;
        }
        // Generic: ONE by-name substitution map over BOTH the enclosing type's params and the
        // method's own `[U]` params. Seed each from its respective turbofish, then infer the rest by
        // unifying the declared param types (which may carry either set of `Ty::Param`s) against the
        // argument types — exactly like the struct/newtype ctor + a generic free fn.
        let arg_tys = self.infer_generic_arg_tys(args);
        if arg_tys.len() != sig.params.len() {
            self.check_arity(method, sig.params.len(), args, span);
        }
        let mut sub = self.seed_targs(tname, &tps, targs, span);
        let msub = self.seed_targs(method, &sig.type_params, mtargs, span);
        sub.extend(msub);
        for (decl, actual) in sig.params.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        self.recover_iter_elems(&sig.type_params, &mut sub, span);
        self.recover_index_args(&sig.type_params, &mut sub, span);
        for (decl, (actual, arg)) in sig.params.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(method, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &sub, span);
        self.enforce_bounds(&sig.type_params, &sub, span);
        // Any param still un-inferred — no turbofish and not bound by an argument, e.g.
        // `Box.empty() -> Box[T]` or a method `[U]` unbindable from args — degrades to the refinable
        // `Ty::Unknown`; a free `Ty::Param` must never leak into the value's type (mirrors the ctor's
        // `unwrap_or(Ty::Unknown)` targs_out).
        for tp in tps.iter().chain(sig.type_params.iter()) {
            sub.entry(tp.name.clone()).or_insert(Ty::Unknown);
        }
        subst(&sig.ret, &sub)
    }

    /// Resolve a by-name call (builtin / constructor / variant / global fn). Returns `None` if
    /// `name` is none of those, so the caller can treat it as a value-call.
    /// Type-check an enum-variant constructor `Enum.Variant(args)` given its resolved `VariantInfo`.
    /// Handles both non-generic and generic enums (type args from explicit `[T]` or inferred from the
    /// payload). Shared by the qualified-call fast path and reachable only once the `(enum, variant)`
    /// pair is known.
    #[allow(clippy::too_many_arguments)] // variant shape + call args + span + expected-type hint
    fn infer_variant_call(
        &mut self,
        v: &VariantInfo,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        name_span: Span,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let tps = self
            .enum_type_params
            .get(&v.enum_name)
            .cloned()
            .unwrap_or_default();
        // Editor hover (probe-gated no-op): record the variant's ctor signature at the variant-name
        // token (`Col.Val(3)` → "fn(int) -> Col"). The enum's declared `Ty::Param`s are preserved in
        // the return type, so a generic variant Displays "fn(T) -> Box[T]". Emits no error.
        if self.hover_probe.is_some() {
            let targs_disp: Vec<Ty> = tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect();
            let fty = Ty::Func {
                params: v.payload.clone(),
                ret: Box::new(Ty::Enum(v.enum_name.clone(), targs_disp)),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, None);
        }
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            self.check_args(name, &v.payload, args, span);
            return Ty::Enum(v.enum_name.clone(), Vec::new());
        }
        // Generic enum: type arguments come from explicit call-site args (`Tree.Node[int](…)`) when
        // given, else are inferred by unifying the variant's declared payload types (which contain
        // the enum's `Ty::Param`s) against the argument types, then check each argument against the
        // substituted payload.
        let arg_tys = self.infer_generic_arg_tys(args);
        if arg_tys.len() != v.payload.len() {
            self.check_arity(name, v.payload.len(), args, span);
        }
        let mut sub = self.seed_targs(name, &tps, targs, span);
        for (decl, actual) in v.payload.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: an annotation (`let`/return/param `Enum[int]`) seeds any type
        // param the args left FREE — unify the declared enum SHAPE (Param-bearing) against the hint
        // AFTER arg-unification, so turbofish/args win and the seed only breaks a genuine deadlock.
        seed_from_hint(
            hint,
            &Ty::Enum(v.enum_name.clone(), param_shape(&tps)),
            &mut sub,
        );
        for (decl, (actual, arg)) in v.payload.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Enum(v.enum_name.clone(), targs_out)
    }

    /// Type-check a module-qualified struct constructor `module.Struct(args)` from the struct's
    /// resolved `StructInfo` (held in the defining module's `ModuleSig`), mirroring the bare struct
    /// path in `infer_named_call`. Returns a `Ty::Struct` keyed by the DECLARING module's runtime key
    /// (`key`, bare in the common case, `<dotted>::Name` on a genuine cross-module clash) so the value
    /// resolves its fields/methods against the right module's layout — and so it unifies with the
    /// declaring module's own signatures (which carry the same key).
    #[allow(clippy::too_many_arguments)] // struct shape + call args + span + expected-type hint
    fn infer_qualified_struct_call(
        &mut self,
        info: &StructInfo,
        name: &str,
        key: &str,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        let tps = info.type_params.clone();
        let field_tys: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            // Struct ctor float fields are coerced per-field by the backend's `NewStruct` site.
            self.check_args_w(name, &field_tys, args, span);
            return Ty::strukt(key.to_string());
        }
        let arg_tys = self.infer_generic_arg_tys(args);
        if arg_tys.len() != field_tys.len() {
            self.check_arity(name, field_tys.len(), args, span);
        }
        let mut sub = self.seed_targs(name, &tps, targs, span);
        for (decl, actual) in field_tys.iter().zip(&arg_tys) {
            unify(decl, actual, &mut sub);
        }
        self.recover_iter_elems(&tps, &mut sub, span);
        // Expected-type checking-mode: a `let`/return/param annotation (`Heap[int]`) seeds any type
        // param the args left FREE, BEFORE the deadlock probe — so the annotation breaks the
        // `Heap([], fn(a, b): a < b)` deadlock (it pins `T`, which then pins the closure params).
        seed_from_hint(
            hint,
            &Ty::Struct(key.to_string(), param_shape(&tps)),
            &mut sub,
        );
        // Same un-inferable closure-param deadlock guard as the bare struct-ctor / free-fn paths
        // (`Heap([], fn(a, b): a < b)` via a module-qualified ctor): report the cause and bind the
        // params to Unknown BEFORE the per-arg closure body is checked, so it doesn't leak a
        // misleading "cannot compare T and T" from inside the lambda.
        self.report_uninferable_closure_params(name, &tps, &field_tys, args, &mut sub, span);
        for (decl, (actual, arg)) in field_tys.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &sub);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        self.enforce_bounds(&tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::Struct(key.to_string(), targs_out)
    }

    /// Type-check a newtype constructor `Name(arg)` (bare or `module.Name`) given its resolved
    /// underlying `Ty`, generic type params, and runtime `key`. Mirrors the struct ctor path: a
    /// scalar newtype checks the single arg against the underlying and returns `NewType(key, [])`; a
    /// generic newtype infers (or takes via turbofish) its type args by unifying the underlying
    /// (which contains the `Ty::Param`s) against the arg type, then returns `NewType(key, targs)`.
    /// Turbofish (`Stack[int]([])`) supplies args the empty `[]` can't bind — the documented
    /// inference gap shared with `ConcurrentMap(RwShared({}))`.
    #[allow(clippy::too_many_arguments)] // the newtype's resolved shape pieces + call args + span
    fn infer_newtype_call(
        &mut self,
        name: &str,
        key: &str,
        underlying: &Ty,
        tps: &[TypeParam],
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        if tps.is_empty() {
            if !targs.is_empty() {
                self.error(span, format!("'{name}' takes no type arguments"));
            }
            self.check_args(name, std::slice::from_ref(underlying), args, span);
            return Ty::NewType(key.to_string(), Vec::new());
        }
        let arg_tys = self.infer_generic_arg_tys(args);
        if arg_tys.len() != 1 {
            self.check_arity(name, 1, args, span);
        }
        let mut sub = self.seed_targs(name, tps, targs, span);
        if let Some(actual) = arg_tys.first() {
            unify(underlying, actual, &mut sub);
        }
        self.recover_iter_elems(tps, &mut sub, span);
        // Expected-type checking-mode: a `let`/return/param annotation (`Stack[int]`) seeds any type
        // param the single arg left FREE (e.g. `Stack[int] = Stack([])`).
        seed_from_hint(
            hint,
            &Ty::NewType(key.to_string(), param_shape(tps)),
            &mut sub,
        );
        if let (Some(actual), Some(arg)) = (arg_tys.first(), args.first()) {
            let expected = subst(underlying, &sub);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        self.enforce_bounds(tps, &sub, span);
        let targs_out = tps
            .iter()
            .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
            .collect();
        Ty::NewType(key.to_string(), targs_out)
    }

    /// For a numeric/scalar cast builtin (`int`/`float`/`bool`), if the single arg is a NEWTYPE,
    /// require its underlying to be exactly the cast target — `int(uid)` unwraps a `newtype X=int`
    /// but `int(meters)` (underlying float) is rejected. A generic newtype's underlying is
    /// substituted with its instantiated type args first (concrete for a scalar newtype — trivial).
    /// A non-newtype arg is left to the normal permissive cast. (`str` is handled separately — it is
    /// dual cast+display, never rejected.)
    fn check_newtype_cast_unwrap(&mut self, cast: &str, arg: &Expr, target: Ty) {
        let aty = self.infer_value(arg);
        if matches!(aty, Ty::NewType(..))
            && let Some(under) = self.newtype_unwrap_target(&aty)
            && !matches!(under, Ty::Unknown)
            && !compatible(&target, &under)
        {
            self.error(
                arg.span,
                format!(
                    "{cast}() cannot unwrap newtype {aty} (its underlying type is {under}, not {target})"
                ),
            );
        }
    }

    /// The substituted underlying `Ty` of a `Ty::NewType(key, targs)` — the type a cast-unwrap
    /// (`int(uid)` / `list(s)`) yields. Builds the param→targs map from the newtype's declared type
    /// params and substitutes it into the stored underlying, so `list(s)` for `s: Stack[int]` yields
    /// `list[int]` (not bare `list[T]`). `None` if `ty` is not a known newtype.
    fn newtype_unwrap_target(&self, ty: &Ty) -> Option<Ty> {
        let Ty::NewType(key, targs) = ty else {
            return None;
        };
        let under = self.newtype_defs.get(key).map(|(u, _)| u.clone())?;
        let map: HashMap<String, Ty> = self
            .newtype_type_params
            .get(key)
            .map(|tps| {
                tps.iter()
                    .map(|tp| tp.name.clone())
                    .zip(targs.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        Some(subst(&under, &map))
    }

    /// Aggregate cast-unwrap (`list(s)`/`set(s)`/`map(s)`) of a newtype: if `it` is a `Ty::NewType`,
    /// unwrap to its substituted underlying and require that to be the matching aggregate kind
    /// (`list`→list, `set`→set, `map`→map) — `list(s)` for `s: Stack[int]` yields exactly `list[int]`,
    /// and `list(b)` for a non-list `Box[T]` errors. Returns `Some(result_ty)` when `it` is a newtype
    /// (handled here); `None` lets the caller fall through to the ordinary iterable cast.
    fn newtype_aggregate_cast(&mut self, cast: &str, it: &Ty, span: Span) -> Option<Ty> {
        if !matches!(it, Ty::NewType(..)) {
            return None;
        }
        let under = self.newtype_unwrap_target(it).unwrap_or(Ty::Unknown);
        let ok = match cast {
            "List" => matches!(under, Ty::List(_) | Ty::Unknown),
            "Set" => matches!(under, Ty::Set(_) | Ty::Unknown),
            "Map" => matches!(under, Ty::Map(..) | Ty::Unknown),
            _ => false,
        };
        if ok {
            Some(if under.is_unknown() {
                match cast {
                    "List" => Ty::list(Ty::Unknown),
                    "Set" => Ty::set(Ty::Unknown),
                    _ => Ty::map(Ty::Unknown, Ty::Unknown),
                }
            } else {
                under
            })
        } else {
            self.error(
                span,
                format!("{cast}() cannot unwrap newtype {it} (its underlying type is {under})"),
            );
            Some(match cast {
                "List" => Ty::list(Ty::Unknown),
                "Set" => Ty::set(Ty::Unknown),
                _ => Ty::map(Ty::Unknown, Ty::Unknown),
            })
        }
    }

    #[allow(clippy::too_many_arguments)] // call name/args/targs/spans + enum-qual + expected-type hint
    fn infer_named_call(
        &mut self,
        name: &str,
        args: &[Expr],
        targs: &[Ty],
        name_span: Span,
        span: Span,
        enum_qual: Option<&str>,
        hint: Option<&Ty>,
    ) -> Option<Ty> {
        // Qualified `Enum.Variant(args)`: resolve strictly within the named enum, bypassing the bare
        // dispatch below — so a variant named like a built-in (`enum E: Ok(int)`) or a struct can't be
        // hijacked by that branch. The caller has already verified `(enum, variant)` exists.
        if let Some(en) = enum_qual {
            let v = self
                .variants
                .get(&(en.to_string(), name.to_string()))
                .cloned()?;
            return Some(self.infer_variant_call(&v, name, args, targs, name_span, span, hint));
        }
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
                    self.infer_value(a);
                }
                Some(Ty::Nil)
            }
            // `panic(msg)` raises the same recoverable `RuntimeError` the runtime uses for overflow/
            // OOB/decode faults (caught by the nearest `recover:` as `Err`, else aborts the program).
            // It never returns, so it is bottom-typed (`Ty::Unknown`): in value position it absorbs
            // into the other branch's concrete type via `unify_branch`, and in tail position
            // `stmt_terminates` (via `expr_is_diverging_call`) treats it as a divergence.
            "panic" => {
                self.check_arity("panic", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("panic() expects a str, got {other}")),
                    }
                }
                Some(Ty::Unknown)
            }
            "range" => {
                for a in args {
                    self.expect_int_val(a);
                }
                if args.is_empty() || args.len() > 3 {
                    self.error(
                        span,
                        "range() expects range(end), range(start, end), or range(start, end, step)",
                    );
                }
                Some(Ty::list(Ty::Int))
            }
            "int" => {
                self.check_arity("int", 1, args, span);
                if let Some(a) = args.first() {
                    self.check_newtype_cast_unwrap("int", a, Ty::Int);
                }
                self.infer_all(args);
                Some(Ty::Int)
            }
            "float" => {
                self.check_arity("float", 1, args, span);
                if let Some(a) = args.first() {
                    self.check_newtype_cast_unwrap("float", a, Ty::Float);
                }
                self.infer_all(args);
                Some(Ty::Float)
            }
            "str" => {
                self.check_arity("str", 1, args, span);
                // `str` is dual: for `newtype N = str` it UNWRAPS the inner str; for any other
                // underlying it is the normal Stringable display cast (accepts anything). So no
                // newtype-mismatch check here — `str(meters)` is a legal display, not an error.
                self.infer_all(args);
                Some(Ty::Str)
            }
            "ord" => {
                self.check_arity("ord", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("ord() expects a str, got {other}")),
                    }
                }
                Some(Ty::Int)
            }
            "chr" => {
                self.check_arity("chr", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Int | Ty::Unknown => {}
                        other => self.error(a.span, format!("chr() expects an int, got {other}")),
                    }
                }
                Some(Ty::Str)
            }
            // `list(it)` → a list from ANY for-iterable (list/set/str/bytes/bytearray/map-keys/range/
            // Iterator). The element type flows through `iter_elem` — the single source of truth for
            // "what `for x in X` accepts". The argument is REQUIRED: an empty list is the `[]` literal
            // (zero args can't infer T).
            "List" => {
                // `List[T]()` — explicit element type (turbofish), bare `List()` — empty list whose
                // element type is refined from the expected type / first use (mirrors `Set()`), and
                // `List(it)` builds from any for-iterable. With a turbofish AND an iterable, the
                // iterable's elements are checked against `T`.
                let targ_elem = match targs {
                    [] => None,
                    [t] => Some(t.clone()),
                    _ => {
                        self.error(span, "List[T]() takes exactly one type argument");
                        Some(Ty::Unknown)
                    }
                };
                match args.len() {
                    0 => Some(Ty::list(targ_elem.unwrap_or(Ty::Unknown))),
                    1 => {
                        let it = self.infer_value(&args[0]);
                        if let Some(result) = self.newtype_aggregate_cast("List", &it, args[0].span)
                        {
                            return Some(result);
                        }
                        let elem = match self.iter_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!("List() expects an iterable, got {it}"),
                                );
                                Ty::Unknown
                            }
                        };
                        match targ_elem {
                            Some(t) => {
                                if !t.is_unknown()
                                    && !elem.is_unknown()
                                    && !self.assignable(&t, &elem)
                                {
                                    self.error(
                                        args[0].span,
                                        format!(
                                            "List[{t}]() expected elements of type {t}, found {elem}"
                                        ),
                                    );
                                }
                                Some(Ty::list(t))
                            }
                            None => Some(Ty::list(elem)),
                        }
                    }
                    _ => {
                        self.error(
                            span,
                            "List() takes at most one iterable argument — use [] for an empty list",
                        );
                        Some(Ty::list(targ_elem.unwrap_or(Ty::Unknown)))
                    }
                }
            }
            "Set" => {
                // `Set()`/`Set[T]()` → empty set (element from the turbofish, else inferred from
                // later use, like `{}` for maps); `Set(it)` → a set from ANY for-iterable (broadened
                // from list-only), deduped. The element type flows through `iter_elem`; it must be
                // Hashable. With a turbofish AND an iterable, elements are checked against `T`.
                let targ_elem = match targs {
                    [] => None,
                    [t] => Some(t.clone()),
                    _ => {
                        self.error(span, "Set[T]() takes exactly one type argument");
                        Some(Ty::Unknown)
                    }
                };
                let elem = match args.len() {
                    0 => targ_elem.unwrap_or(Ty::Unknown),
                    1 => {
                        let it = self.infer_value(&args[0]);
                        if let Some(result) = self.newtype_aggregate_cast("Set", &it, args[0].span)
                        {
                            return Some(result);
                        }
                        let elem = match self.iter_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!("Set() expects an iterable, got {it}"),
                                );
                                Ty::Unknown
                            }
                        };
                        match targ_elem {
                            Some(t) => {
                                if !t.is_unknown()
                                    && !elem.is_unknown()
                                    && !self.assignable(&t, &elem)
                                {
                                    self.error(
                                        args[0].span,
                                        format!(
                                            "Set[{t}]() expected elements of type {t}, found {elem}"
                                        ),
                                    );
                                }
                                t
                            }
                            None => elem,
                        }
                    }
                    _ => {
                        self.error(span, "Set() expects Set() or Set(iterable)");
                        targ_elem.unwrap_or(Ty::Unknown)
                    }
                };
                if !elem.is_unknown() && !self.is_hashable_key(&elem) {
                    self.error(
                        span,
                        format!("Set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {elem}"),
                    );
                }
                Some(Ty::set(elem))
            }
            // `map(it)` → a map from an iterable of EXACTLY 2-tuples `(K, V)`. A non-2-tuple element is
            // a STATIC error here (not a runtime surprise). K must be Hashable. Last-wins on duplicate
            // keys (like the `{k: v}` literal). The argument is REQUIRED: an empty map is the `{}`
            // literal. (Free-call `map(it)` is a distinct namespace from the `xs.map(f)` list HOF.)
            "Map" => {
                // `Map[K, V]()` → typed empty map (turbofish); bare `Map()` → empty map refined from
                // the expected type / first use (mirrors the `{}` literal and `Set()`); `Map(it)` →
                // a map from an iterable of EXACTLY 2-tuples. With a turbofish AND an iterable, the
                // tuple parts are checked against `[K, V]`.
                let targ_kv = match targs {
                    [] => None,
                    [k, v] => Some((k.clone(), v.clone())),
                    _ => {
                        self.error(span, "Map[K, V]() takes exactly two type arguments");
                        Some((Ty::Unknown, Ty::Unknown))
                    }
                };
                let (k, v) = match args.len() {
                    0 => targ_kv.clone().unwrap_or((Ty::Unknown, Ty::Unknown)),
                    1 => {
                        let it = self.infer_value(&args[0]);
                        if let Some(result) = self.newtype_aggregate_cast("Map", &it, args[0].span)
                        {
                            return Some(result);
                        }
                        let elem = match self.iter_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!("Map() expects an iterable, got {it}"),
                                );
                                Ty::Unknown
                            }
                        };
                        let (k, v) = match elem {
                            Ty::Tuple(ref parts) if parts.len() == 2 => {
                                (parts[0].clone(), parts[1].clone())
                            }
                            Ty::Unknown => (Ty::Unknown, Ty::Unknown),
                            other => {
                                self.error(
                                    args[0].span,
                                    format!("Map() expects an iterable of (key, value) 2-tuples, found element {other}"),
                                );
                                (Ty::Unknown, Ty::Unknown)
                            }
                        };
                        if let Some((tk, tv)) = &targ_kv {
                            if !tk.is_unknown() && !k.is_unknown() && !self.assignable(tk, &k) {
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map[{tk}, {tv}]() expected keys of type {tk}, found {k}"
                                    ),
                                );
                            }
                            if !tv.is_unknown() && !v.is_unknown() && !self.assignable(tv, &v) {
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map[{tk}, {tv}]() expected values of type {tv}, found {v}"
                                    ),
                                );
                            }
                            (tk.clone(), tv.clone())
                        } else {
                            (k, v)
                        }
                    }
                    _ => {
                        self.error(
                            span,
                            "Map() takes at most one iterable argument — use {} for an empty map",
                        );
                        targ_kv.clone().unwrap_or((Ty::Unknown, Ty::Unknown))
                    }
                };
                if !k.is_unknown() && !self.is_hashable_key(&k) {
                    self.error(
                        span,
                        format!("Map key type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {k}"),
                    );
                }
                Some(Ty::map(k, v))
            }
            // `bytearray(...)` — the MUTABLE byte buffer (constructor-only, no literal). Four forms:
            // `bytearray()` (empty), `bytearray(N)` (N zero bytes), `bytearray(b)` (from a `bytes`,
            // mutable copy), `bytearray([ints])` (from a `list[int]`, each 0–255 validated at runtime),
            // and `bytearray(ba)` (copy). Always infers `bytearray`.
            "bytearray" => {
                match args.len() {
                    0 => {}
                    1 => match self.infer_value(&args[0]) {
                        Ty::Int | Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                        Ty::List(elem) if matches!(*elem, Ty::Int | Ty::Unknown) => {}
                        other => self.error(
                            args[0].span,
                            format!("bytearray() expects an int size, a bytes, a bytearray, or a List[int], got {other}"),
                        ),
                    },
                    _ => self.error(span, "bytearray() expects bytearray(), bytearray(int), bytearray(bytes|bytearray), or bytearray(List[int])"),
                }
                Some(Ty::ByteArray)
            }
            // `bytes(...)` — the conversion bridge to the IMMUTABLE form (also constructor-only; the
            // `b"..."` literal is the other way to make a `bytes`). `bytes(ba)` snapshots a `bytearray`,
            // `bytes(b)` copies a `bytes`, `bytes([ints])` builds from a `list[int]`. Infers `bytes`.
            "bytes" => {
                match args.len() {
                    1 => match self.infer_value(&args[0]) {
                        Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                        Ty::List(elem) if matches!(*elem, Ty::Int | Ty::Unknown) => {}
                        other => self.error(
                            args[0].span,
                            format!(
                                "bytes() expects a bytes, a bytearray, or a List[int], got {other}"
                            ),
                        ),
                    },
                    _ => self.error(
                        span,
                        "bytes() expects bytes(bytes|bytearray) or bytes(List[int])",
                    ),
                }
                Some(Ty::Bytes)
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
                    self.error(
                        span,
                        format!("Channel element type must be sendable, found {elem}"),
                    );
                }
                Some(Ty::channel(elem))
            }
            "Shared" => {
                // `Shared(v)` — a fresh cross-task box initialised with `v`. The element type is
                // inferred from the value (value-first, unlike `Channel[T]()`); a `[T]` type arg is
                // rejected upstream by the `name_is_generic` gate. NOT a global builtin: requires
                // `import std.concurrency` (the arg is still inferred on the unlicensed path so a nested
                // error surfaces; the name STAYS reserved). Same for RwShared/Atomic/Executor below.
                let elem = self.one_arg("Shared", args, span);
                if self.concurrency_licensed("Shared") {
                    Some(Ty::shared(elem))
                } else {
                    self.error(
                        span,
                        "unknown type 'Shared' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "RwShared" => {
                // `RwShared(v)` — a fresh cross-task read-write box initialised with `v`. The element
                // type is inferred from the value (value-first, like `Shared`); a `[T]` type arg is
                // rejected upstream by the `name_is_generic` gate.
                let elem = self.one_arg("RwShared", args, span);
                if self.concurrency_licensed("RwShared") {
                    Some(Ty::rwshared(elem))
                } else {
                    self.error(
                        span,
                        "unknown type 'RwShared' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "Atomic" => {
                // `Atomic(v)` — a fresh cross-task atomic box initialised with `v`. Value-first like
                // `Shared`; a `[T]` type arg is rejected upstream by the `name_is_generic` gate.
                let elem = self.one_arg("Atomic", args, span);
                if self.concurrency_licensed("Atomic") {
                    Some(Ty::atomic(elem))
                } else {
                    self.error(
                        span,
                        "unknown type 'Atomic' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            "timer" => {
                // `timer(ms)` — a one-shot timeout channel: a `Channel[bool]` that delivers `true`
                // once, `ms` milliseconds after creation. The composable timeout primitive (recv it in
                // a `wait` arm). Takes an int; a `[T]` type arg is rejected upstream. NOT a global
                // builtin: requires `import std.time` (the arg is still checked on the unlicensed path
                // so a nested error surfaces; the name STAYS reserved). Returns `Channel[bool]` even on
                // the unlicensed path so a chained `.recv()` doesn't emit a confusing secondary error.
                self.check_args("timer", &[Ty::Int], args, span);
                if self.time_licensed("timer") {
                    Some(Ty::channel(Ty::Bool))
                } else {
                    self.error(
                        span,
                        "unknown function 'timer' (import it from std.time: `import std.time`)"
                            .to_string(),
                    );
                    Some(Ty::channel(Ty::Bool))
                }
            }
            "Executor" => {
                // `Executor()` — a fresh, empty, explicitly-owned work queue (C5 escape hatch).
                // Non-generic and zero-arg; a `[T]` type arg is rejected upstream. NOT a global
                // builtin: requires `import std.concurrency` (the name STAYS reserved).
                self.check_arity("Executor", 0, args, span);
                if self.concurrency_licensed("Executor") {
                    Some(Ty::Executor)
                } else {
                    self.error(
                        span,
                        "unknown type 'Executor' (import it from std.concurrency: `import std.concurrency`)"
                            .to_string(),
                    );
                    Some(Ty::Unknown)
                }
            }
            // Generic built-in constructors for Result / Option.
            // `Ok(x)`: success type known, error type open (unifies with the declared `E`).
            "Ok" => Some(Ty::result_e(self.one_arg(name, args, span), Ty::Unknown)),
            "Some" => Some(Ty::option(self.one_arg(name, args, span))),
            // `Err(x)`: error type known (`typeof x`), success type open.
            "Err" => Some(Ty::result_e(Ty::Unknown, self.one_arg(name, args, span))),
            _ => {
                // Newtype constructor? `UserId(x)` — one arg of the underlying type, returns the
                // newtype. Mirrors the single-field struct ctor; only a BARE-resolvable newtype. A
                // generic newtype (`Stack([1,2])` / turbofish `Stack[int]([])`) infers/takes its type
                // args via `infer_newtype_call`.
                if self.newtype_names.contains(name) {
                    let key = self.bare_key(name);
                    let under = self
                        .newtype_defs
                        .get(&key)
                        .map(|(u, _)| u.clone())
                        .unwrap_or(Ty::Unknown);
                    let tps = self
                        .newtype_type_params
                        .get(&key)
                        .cloned()
                        .unwrap_or_default();
                    return Some(
                        self.infer_newtype_call(name, &key, &under, &tps, args, targs, span, hint),
                    );
                }
                // Struct constructor? Only a BARE-resolvable struct (`struct_names`): a locally
                // declared, `from`-imported, or std type. A whole-module-imported USER struct's layout
                // lives in `self.structs` for `m.S(...)`/field access, but its name is NOT in
                // `struct_names`, so bare `S(...)` is not a constructor — it falls through to the
                // unknown-name path (with an import hint).
                if self.struct_names.contains(name)
                    && let key = self.bare_key(name)
                    && let Some((tps, fields)) = self
                        .structs
                        .get(&key)
                        .map(|i| (i.type_params.clone(), i.fields.clone()))
                {
                    let field_tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
                    if tps.is_empty() {
                        // Struct ctor float fields are coerced per-field by the `NewStruct` site.
                        self.check_args_w(name, &field_tys, args, span);
                        return Some(Ty::strukt(key));
                    }
                    // Generic struct: type arguments come from explicit call-site args (`S[int](…)`)
                    // when given, else are inferred by unifying the declared field types (which
                    // contain the struct's `Ty::Param`s) against the argument types.
                    let arg_tys = self.infer_generic_arg_tys(args);
                    if arg_tys.len() != field_tys.len() {
                        self.check_arity(name, field_tys.len(), args, span);
                    }
                    let mut sub = self.seed_targs(name, &tps, targs, span);
                    for (decl, actual) in field_tys.iter().zip(&arg_tys) {
                        unify(decl, actual, &mut sub);
                    }
                    self.recover_iter_elems(&tps, &mut sub, span);
                    // Expected-type checking-mode: a `let`/return/param annotation (`Heap[int]`) seeds
                    // any type param the args left FREE, BEFORE the deadlock probe — so the annotation
                    // breaks the `Heap([], fn(a, b): a < b)` deadlock (it pins `T`, which in turn pins
                    // the comparator's closure params via the per-arg checking-mode re-infer below).
                    seed_from_hint(hint, &Ty::Struct(key.clone(), param_shape(&tps)), &mut sub);
                    // Detect the un-inferable closure-param deadlock (e.g. `Heap([], fn(a,b): a<b)`)
                    // BEFORE the per-arg check, so it reports the cause instead of leaking a
                    // "cannot compare T and T" from inside the lambda. Binds the params to Unknown.
                    self.report_uninferable_closure_params(
                        name, &tps, &field_tys, args, &mut sub, span,
                    );
                    for (decl, (actual, arg)) in field_tys.iter().zip(arg_tys.iter().zip(args)) {
                        let expected = subst(decl, &sub);
                        self.check_generic_arg(name, &expected, actual, arg);
                    }
                    self.enforce_bounds(&tps, &sub, span);
                    let targs = tps
                        .iter()
                        .map(|tp| sub.get(&tp.name).cloned().unwrap_or(Ty::Unknown))
                        .collect();
                    return Some(Ty::Struct(key, targs));
                }
                // A bare user-variant constructor (`Circle(5)`) is no longer allowed — variants are
                // scoped under their enum and must be written qualified (`Shape.Circle(5)`).
                if self.variant_owners.contains_key(name) {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                    for a in args {
                        self.infer_value(a);
                    }
                    return Some(Ty::Unknown);
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
                        return Some(self.infer_generic_call(name, &sig, args, targs, span, hint));
                    }
                    // Float params are coerced at the callee's prologue (compile_fn / extern).
                    // Honor an optional trailing tail (`min_params < params.len()`, e.g. a native
                    // `from`-imported fn with an optional arg); for plain sigs `min_params ==
                    // params.len()`, so this is identical to the old exact-arity check.
                    self.check_args_range_w(name, &sig.params, sig.min_params, args, span, true);
                    return Some(sig.ret);
                }
                None
            }
        }
    }

    /// Does the method `method` on receiver type `recv_ty` declare its OWN `[U]` type params? Only a
    /// user struct/enum/newtype method can; a builtin (`str`/`list`/…) member never does. Used to gate
    /// the member-level turbofish (`obj.method[A](x)`): a turbofish on anything else is an arity error.
    fn method_has_own_type_params(&self, recv_ty: &Ty, method: &str) -> bool {
        match recv_ty {
            Ty::Struct(sname, _) => self
                .structs
                .get(sname)
                .and_then(|info| info.methods.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            Ty::Enum(ename, _) => self
                .enum_methods
                .get(ename)
                .and_then(|ms| ms.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            Ty::NewType(nkey, _) => self
                .newtype_defs
                .get(nkey)
                .and_then(|(_under, methods)| methods.get(method))
                .is_some_and(|sig| !sig.type_params.is_empty()),
            _ => false,
        }
    }

    /// Editor hover (probe-gated no-op): record a builtin method's CALL signature at the method-name
    /// token. The builtin `*_method_sig` helpers carry NO receiver param (unlike user methods), so the
    /// sig is recorded as-is — `str.upper()`'s `[]→str` renders "fn() -> str". Pure side effect: only
    /// runs under the hover probe and emits no error / changes no checking. Mirrors the user-struct
    /// method-arm probe block (which strips the receiver first).
    fn record_method_hover(&mut self, name_span: Span, sig: &FnSig) {
        if self.hover_probe.is_some() {
            let fty = Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
    }

    /// Record a DECL-site method-name hover (the `dbl` in `fn dbl(self) -> int:`). Mirrors the
    /// CALL-site method hover ([`infer_method_call`]): the receiver `self` is stripped for an
    /// instance method (so `fn dbl(self) -> int` displays "fn() -> int"), but a STATIC method (no
    /// receiver) keeps all its params. Probe-gated no-op like [`record_method_hover`].
    fn record_method_decl_hover(&mut self, name_span: Span, sig: &FnSig) {
        if self.hover_probe.is_some() {
            let params: Vec<Ty> = if !sig.is_static && !sig.params.is_empty() {
                sig.params[1..].to_vec()
            } else {
                sig.params.clone()
            };
            let fty = Ty::Func {
                params,
                ret: Box::new(sig.ret.clone()),
            };
            self.hover_record_at(name_span, &fty, HoverKind::Func, sig.doc.clone());
        }
    }

    /// Type-check an instance method call `obj.method(args)`. `type_args` are the explicit
    /// member-level turbofish (`obj.method[A, B](x, y)`) — non-empty only when the parser stole a
    /// type list after the `.method`; they seed a generic method's own `[U]` params. A method-level
    /// turbofish on a BUILTIN or non-generic member (no own type params) is rejected — including the
    /// `.iter` fast-path (`xs.iter[int]()`), guarded BEFORE that fast-path runs.
    fn infer_method_call(
        &mut self,
        obj: &Expr,
        method: &str,
        // Source span of the method-NAME token (the `Field` callee's `name_span`), used ONLY to
        // anchor the editor hover record at the method name; never affects checking.
        name_span: Span,
        args: &[Expr],
        type_args: &[Ty],
        span: Span,
    ) -> Ty {
        let obj_ty = self.infer(obj);
        // Refine-on-first-use: if `obj` is a simple variable whose type has an `Unknown` element/
        // key/value/type-arg slot (an empty literal / nullary variant / native `None`), and this is
        // a slot-supplying mutator (`push`/`add`/`insert`/`extend`), re-pin the binding to the
        // concrete shape the arg supplies — so a later conflicting op is a normal `check_args`
        // mismatch and the set-element Hashable ban runs at concrete-ification. Then re-read the
        // (possibly refined) receiver type from scope so dispatch sees the narrowed element.
        self.refine_receiver(obj, &obj_ty, method, args);
        let obj_ty = match &obj.kind {
            ExprKind::Ident(name) => self.lookup(name).unwrap_or(obj_ty),
            _ => obj_ty,
        };
        // A member-level turbofish (`obj.method[A, B](...)`) is only valid on a USER method that
        // declares its OWN `[U]` type params. On a builtin (`xs.len[int]()`, `xs.iter[int]()`) or a
        // non-generic user method it is an arity error — checked BEFORE the `.iter` fast-path below
        // so `xs.iter[int]()` is rejected like `xs.len[int]()` (it was silently swallowed otherwise).
        if !type_args.is_empty() && !self.method_has_own_type_params(&obj_ty, method) {
            self.infer_all(args);
            self.error(
                span,
                format!("method '{method}' takes no type argument(s) (it declares no own type parameters)"),
            );
            return Ty::Unknown;
        }
        // `.iter()` — the formal `Iterable[T]` entry point. Returns a fresh cursor typed as the
        // existing `Iterator[T]` existential (no new `Ty`), `T = iter_elem`. Handled here, BEFORE the
        // per-type dispatch, for every built-in iterable AND for an `Iterator[T]` value (a generator
        // result or another cursor) where `iter()` is idempotent (returns self). A user STRUCT is
        // excluded so a struct that declares its own `iter` (the pure-`Iterable` producer) resolves
        // through the normal struct-method path below — its `iter` return type IS `Iterator[E]`.
        if method == "iter"
            && args.is_empty()
            && !matches!(&obj_ty, Ty::Struct(n, _) if n != "Iterator")
            && let Some(elem) = self.iter_elem(&obj_ty)
        {
            return Ty::Struct("Iterator".to_string(), vec![elem]);
        }
        match &obj_ty {
            // `module.fn(args)` is a plain call on the member — no `self`.
            Ty::Module(mname) => {
                let sig = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id));
                let is_poly = sig.is_some_and(|s| s.numeric_poly.contains(method));
                let fsig = sig.and_then(|s| s.functions.get(method).cloned());
                // Editor hover (CASE 2): record `module.fn`'s native signature at the method name —
                // covers plain, numeric-poly (`abs` has an arity fsig), and generic module fns.
                if let Some(f) = &fsig {
                    self.record_method_hover(name_span, f);
                }
                // Numeric-polymorphic native fns (gap #12): result type follows the argument type.
                if is_poly {
                    let arity = fsig.as_ref().map_or(2, |f| f.params.len());
                    return self.infer_numeric_poly(method, arity, args, span);
                }
                if let Some(fsig) = fsig {
                    // A generic module function (`cmp.max`): infer its type parameters from the
                    // arguments, enforce bounds, and substitute into the return type.
                    if !fsig.type_params.is_empty() {
                        // A module-qualified generic fn call (`m.f(...)`) does not thread the
                        // expected-type hint (only the 3 primary annotation sites do); pass None.
                        return self.infer_generic_call(method, &fsig, args, &[], span, None);
                    }
                    // Float params are coerced at the callee's prologue. Honor an optional trailing
                    // tail (`min_params < params.len()`, e.g. `request.get(url, timeout_ms?)`); for
                    // plain sigs `min_params == params.len()`, identical to the old exact check.
                    self.check_args_range_w(
                        method,
                        &fsig.params,
                        fsig.min_params,
                        args,
                        span,
                        true,
                    );
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
            // An `Iterator[T]` value (a generator result) exposes the protocol's one method,
            // `next(self) -> Option[T]`, so it is drivable by explicit `.next()` as well as `for`.
            // (There is no registered struct named `Iterator`, so this must be handled here.)
            Ty::Struct(sname, targs) if sname == "Iterator" && targs.len() == 1 => {
                if method == "next" {
                    self.check_args(method, &[], args, span);
                    return Ty::option(targs[0].clone());
                }
                self.infer_all(args);
                self.error(
                    span,
                    format!(
                        "type {obj_ty} has no method '{method}' (an iterator only has `next()`)"
                    ),
                );
                Ty::Unknown
            }
            Ty::Struct(sname, targs) => {
                // Substitute the struct's type arguments into the method signature, so calling
                // `Stack[int].push(x)` checks `x` against `int`, not the parameter `T`.
                let resolved = self.structs.get(sname).and_then(|info| {
                    info.methods.get(method).map(|sig| {
                        let map = struct_param_map(info, targs);
                        let params: Vec<Ty> = sig.params.iter().map(|t| subst(t, &map)).collect();
                        (
                            params,
                            subst(&sig.ret, &map),
                            sig.type_params.clone(),
                            sig.is_static,
                            sig.doc.clone(),
                        )
                    })
                });
                if let Some((params, ret, mtps, is_static, mdoc)) = resolved {
                    // A STATIC method (no `self`) is NOT callable on a value — it is reached only as
                    // `Type.method(...)`. Reject the instance call with a pointer at the right form.
                    if is_static {
                        self.infer_all(args);
                        let disp = crate::compiler::bare_display(sname);
                        self.error(
                            span,
                            format!("'{method}' is a static method of '{disp}'; call it as `{disp}.{method}(...)`"),
                        );
                        return ret;
                    }
                    // A generic method introduces its own type params `[U]` (beyond the struct's
                    // `[T]`, already substituted above). Infer them from the call arguments —
                    // mirrors the free generic-fn path (`infer_generic_call`).
                    if !mtps.is_empty() {
                        return self.infer_generic_method(
                            method, &params, &ret, &mtps, &obj_ty, type_args, args, span,
                        );
                    }
                    // The first param is the receiver (bound implicitly from `obj`), so the call's
                    // explicit args correspond to params[1..]. A method with NO params has no
                    // receiver slot — both engines prepend the receiver and would error at runtime,
                    // so reject the call here instead. (A zero-param method is classified static
                    // above, so this `None` arm is now defensive — kept for the FnSig that omits the
                    // static flag, e.g. a protocol-derived sig.)
                    match params.split_first() {
                        Some((_receiver, expected)) => {
                            // Editor hover (probe-gated no-op): record the method's CALL signature
                            // (receiver stripped → "fn(int) -> int") at the method-name token, so
                            // hovering `c.foo(2)`'s `foo` yields the signature. Pure side effect.
                            if self.hover_probe.is_some() {
                                let fty = Ty::Func {
                                    params: expected.to_vec(),
                                    ret: Box::new(ret.clone()),
                                };
                                self.hover_record_at(
                                    name_span,
                                    &fty,
                                    HoverKind::Func,
                                    mdoc.clone(),
                                );
                            }
                            self.check_args_w(method, expected, args, span)
                        }
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
                    // A fn-typed field is a closure/fn value; its prologue coerces float params.
                    self.check_args_w(method, &params, args, span);
                    return *ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            // Enum methods (name-resolved exactly like struct methods). Substitute the enum's type
            // arguments into the method signature, so `Box[int].get()` returns `int`, not `T`.
            // A newtype dispatches its own (non-generic) methods by name, like an enum. The
            // underlying's methods are NOT inherited (an aggregate underlying's `.push`/index/iter
            // never resolve here — that is the v1 distinct-type contract).
            Ty::NewType(ntkey, targs) => {
                // Substitute the newtype's own type arguments into the method signature, so
                // `Stack[int].peek()` returns `Option[int]`, not `Option[T]` (mirrors the enum arm).
                let resolved = self.newtype_defs.get(ntkey).and_then(|(_, ms)| {
                    ms.get(method).map(|sig| {
                        let map: HashMap<String, Ty> = self
                            .newtype_type_params
                            .get(ntkey)
                            .map(|tps| {
                                tps.iter()
                                    .map(|tp| tp.name.clone())
                                    .zip(targs.iter().cloned())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let params: Vec<Ty> = sig.params.iter().map(|t| subst(t, &map)).collect();
                        (
                            params,
                            subst(&sig.ret, &map),
                            sig.type_params.clone(),
                            sig.is_static,
                        )
                    })
                });
                if let Some((params, ret, mtps, is_static)) = resolved {
                    // Static methods on a newtype are DEFERRED (v1 covers struct + enum only). A
                    // no-self newtype method is still classified static, so reject the instance call
                    // with the static-method diagnostic (it is not reachable as `Type.method` yet —
                    // the static-dispatch branches in `infer_call` gate on struct/enum names only).
                    if is_static {
                        self.infer_all(args);
                        self.error(
                            span,
                            format!("'{method}' is a static method (static methods on newtypes are not supported yet)"),
                        );
                        return ret;
                    }
                    if !mtps.is_empty() {
                        return self.infer_generic_method(
                            method, &params, &ret, &mtps, &obj_ty, type_args, args, span,
                        );
                    }
                    match params.split_first() {
                        Some((_receiver, expected)) => {
                            self.check_args_w(method, expected, args, span)
                        }
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
            Ty::Enum(ename, targs) => {
                let resolved = self.enum_methods.get(ename).and_then(|ms| {
                    ms.get(method).map(|sig| {
                        let map: HashMap<String, Ty> = self
                            .enum_type_params
                            .get(ename)
                            .map(|tps| {
                                tps.iter()
                                    .map(|tp| tp.name.clone())
                                    .zip(targs.iter().cloned())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let params: Vec<Ty> = sig.params.iter().map(|t| subst(t, &map)).collect();
                        (
                            params,
                            subst(&sig.ret, &map),
                            sig.type_params.clone(),
                            sig.is_static,
                        )
                    })
                });
                if let Some((params, ret, mtps, is_static)) = resolved {
                    // A STATIC enum method is reached only as `Enum.method(...)`; reject the call on a
                    // value with a pointer at the right form (mirrors the struct arm).
                    if is_static {
                        self.infer_all(args);
                        let disp = crate::compiler::bare_display(ename);
                        self.error(
                            span,
                            format!("'{method}' is a static method of '{disp}'; call it as `{disp}.{method}(...)`"),
                        );
                        return ret;
                    }
                    if !mtps.is_empty() {
                        return self.infer_generic_method(
                            method, &params, &ret, &mtps, &obj_ty, type_args, args, span,
                        );
                    }
                    match params.split_first() {
                        Some((_receiver, expected)) => {
                            self.check_args_w(method, expected, args, span)
                        }
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
                    self.record_method_hover(name_span, &sig);
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
                if matches!(
                    method,
                    "map" | "filter" | "fold" | "sort_by" | "sort_by_key"
                ) {
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
                    if is_orderable(&elem)
                        || elem.is_unknown()
                        || self.satisfies(&elem, "Comparable").is_ok()
                    {
                        return Ty::Nil;
                    }
                    self.error(
                        span,
                        format!("sort() requires a list of Comparable values (int, float, str, or a struct with a `compare` method), found List[{elem}]"),
                    );
                    return Ty::Nil;
                }
                if let Some(sig) = list_method_sig(method, elem) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                if method == "sum" {
                    self.error(
                        span,
                        format!("sum() requires a numeric list, found List[{elem}]"),
                    );
                } else {
                    self.error(span, format!("type {obj_ty} has no method '{method}'"));
                }
                Ty::Unknown
            }
            // `bytes` core methods (immutable byte sequence): only `decode() -> str` (UTF-8).
            Ty::Bytes => {
                if let Some(sig) = bytes_method_sig(method) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type bytes has no method '{method}'"));
                Ty::Unknown
            }
            // `bytearray` core methods (mutable buffer): `len`, `push(int)`, `pop() -> Option[int]`,
            // `extend(bytes|bytearray|list[int])`. `extend` is handled here (not the fixed sig table)
            // because its argument may be any of the three byte-sequence shapes.
            Ty::ByteArray => {
                if method == "extend" {
                    self.check_arity("extend", 1, args, span);
                    if let Some(a) = args.first() {
                        match self.infer_value(a) {
                            Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                            Ty::List(elem) if matches!(*elem, Ty::Int | Ty::Unknown) => {}
                            other => self.error(
                                a.span,
                                format!("extend() expects a bytes, a bytearray, or a List[int], got {other}"),
                            ),
                        }
                    }
                    return Ty::Nil;
                }
                if let Some(sig) = bytearray_method_sig(method) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type bytearray has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Map(k, v) => {
                if let Some(sig) = map_method_sig(method, k, v) {
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Set(elem) => {
                if let Some(sig) = set_method_sig(method, elem) {
                    self.record_method_hover(name_span, &sig);
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
                    self.record_method_hover(name_span, &sig);
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
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::RwShared(elem) => {
                // `get()->T`, `set(T)->nil`, `write(fn(T)->T)->nil` mirror `Shared`. `read` is the
                // read-only twin: `read(fn(T)->R)->R` — R-polymorphic in the closure's return type.
                // `rwshared_method_sig` types `read`'s param as `fn(T)->Unknown` (any closure return
                // is accepted) and ret `Unknown`; here we recover R from the supplied closure so the
                // call site sees the real return type (not `Unknown`).
                if let Some(sig) = rwshared_method_sig(method, elem) {
                    // `read`'s sig ret is the placeholder `Unknown` (the real R is recovered below) —
                    // hover shows the declared `fn(fn(T) -> ?) -> ?` shape, which is the sig of record.
                    self.record_method_hover(name_span, &sig);
                    self.check_args(method, &sig.params, args, span);
                    if method == "read" {
                        // R = the closure argument's actual return type (else `Unknown` on arity
                        // error). `check_args` already inferred the closure (emitting any body
                        // errors); this is a RECOVERY-ONLY re-inference, so snapshot + truncate to
                        // avoid double-reporting those same body errors.
                        let mark = self.errors.len();
                        let recovered = args.first().map(|arg| self.infer_value(arg));
                        self.errors.truncate(mark);
                        if let Some(Ty::Func { ret, .. }) = recovered {
                            return *ret;
                        }
                        return Ty::Unknown;
                    }
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
                    self.record_method_hover(name_span, &sig);
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
                    self.record_method_hover(name_span, &sig);
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
                    self.record_method_hover(name_span, &sig);
                    self.check_args_range(method, &sig.params, sig.min_params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Listener => {
                if let Some(sig) = listener_method_sig(method) {
                    self.record_method_hover(name_span, &sig);
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
                    // `Iterable[T].iter()` yields the existential cursor `Iterator[T]` — the bound's
                    // element arg, not `Iterator[Self]` (the registered placeholder return).
                    if proto.name == "Iterable"
                        && method == "iter"
                        && let Some(arg) = proto.args.first()
                    {
                        return Ty::Struct(
                            "Iterator".to_string(),
                            vec![self.resolve_type(arg, span)],
                        );
                    }
                    return subst(&msig.ret, &map);
                }
                self.infer_all(args);
                self.error(
                    span,
                    format!("type parameter {pname} has no method '{method}'"),
                );
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
                    self.error(
                        span,
                        format!("'map' expects 1 argument(s), got {}", args.len()),
                    );
                    self.infer_all(args);
                    return Ty::Unknown;
                }
                let expected = Ty::Func {
                    params: vec![elem.clone()],
                    ret: Box::new(Ty::Unknown),
                };
                let ft = self.infer_arg(&args[0], Some(&expected));
                match ft {
                    Ty::Unknown => Ty::Unknown,
                    Ty::Func { params, ret }
                        if params.len() == 1 && compatible(&params[0], elem) =>
                    {
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
                    self.error(
                        span,
                        format!("'filter' expects 1 argument(s), got {}", args.len()),
                    );
                    self.infer_all(args);
                    return Ty::Unknown;
                }
                let expected = Ty::Func {
                    params: vec![elem.clone()],
                    ret: Box::new(Ty::Bool),
                };
                let pt = self.infer_arg(&args[0], Some(&expected));
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
                    self.error(
                        span,
                        format!("'fold' expects 2 argument(s), got {}", args.len()),
                    );
                    self.infer_all(args);
                    return Ty::Unknown;
                }
                let init = self.infer_value(&args[0]);
                let expected = Ty::Func {
                    params: vec![init.clone(), elem.clone()],
                    ret: Box::new(init.clone()),
                };
                let ft = self.infer_arg(&args[1], Some(&expected));
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
                let expected = Ty::Func {
                    params: vec![elem.clone(), elem.clone()],
                    ret: Box::new(Ty::Int),
                };
                let ft = self.infer_arg(&args[0], Some(&expected));
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
                let expected = Ty::Func {
                    params: vec![elem.clone()],
                    ret: Box::new(Ty::Unknown),
                };
                let ft = self.infer_arg(&args[0], Some(&expected));
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
                    self.error(
                        a.span,
                        format!("argument of '{method}': expected int or float, found {other}"),
                    );
                    bad = true;
                }
            }
        }
        if saw_int && saw_float {
            self.error(
                span,
                format!(
                    "'{method}' arguments must be the same numeric type (no implicit int/float mix)"
                ),
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
        args.first()
            .map(|a| self.infer_value(a))
            .unwrap_or(Ty::Unknown)
    }

    fn infer_all(&mut self, args: &[Expr]) {
        for a in args {
            self.infer_value(a);
        }
    }

    /// Check argument count and each argument's type against a known parameter list. STRICT — no
    /// int→float widening. Used for type-blind / collection-mutator paths (`push`/`add`/`insert`,
    /// `send`, builtin methods) where the backend cannot coerce the argument.
    fn check_args(&mut self, name: &str, params: &[Ty], args: &[Expr], span: Span) {
        self.check_args_range_w(name, params, params.len(), args, span, false);
    }

    /// Like [`Checker::check_args`] but accepting C-like one-way int→float widening. Used ONLY where
    /// the COMPILER coerces the argument at the callee boundary from a static annotation: a call into
    /// a user/extern function or method's float param, and a struct constructor's float field. The
    /// backend's prologue / per-field coercion makes the stored value a genuine `f64` (no hole).
    fn check_args_w(&mut self, name: &str, params: &[Ty], args: &[Expr], span: Span) {
        self.check_args_range_w(name, params, params.len(), args, span, true);
    }

    /// D6c — `check_args` generalized to an optional trailing tail: the arg count must fall in
    /// `min_params..=params.len()`, and each supplied arg must match its positional param. Used for the
    /// net socket ops whose `timeout_ms` is optional. `min_params == params.len()` reproduces the
    /// exact-arity behavior of [`Checker::check_args`]. STRICT (no widening).
    fn check_args_range(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
    ) {
        self.check_args_range_w(name, params, min_params, args, span, false);
    }

    /// Infer a single call argument in *checking mode*: if the argument is a closure literal and the
    /// expected slot type is a `fn(..)`, infer the closure WITH that expected type (source #1 — so its
    /// unannotated params bind to the expected param types and call sites are checked); otherwise it is
    /// the ordinary bottom-up [`Checker::infer_value`]. The single seam every `fn`-typed slot routes
    /// through, so closure-detection lives in one place.
    fn infer_arg(&mut self, arg: &Expr, expected: Option<&Ty>) -> Ty {
        if let ExprKind::Closure { params, ret, body } = &arg.kind {
            if matches!(expected, Some(Ty::Func { .. })) {
                return self.infer_closure(params, ret.as_ref(), body, expected);
            }
            return self.infer_value(arg);
        }
        // Non-closure arg: thread the declared parameter type as an expected-type hint so a generic
        // ctor / generic fn-call passed directly as a call argument pre-seeds its type params —
        // `take(Heap([], fn(x, y): x < y))` with `fn take(h: Heap[int])` pins `T=int`. `infer_call`
        // consumes the hint; pair set+clear so a non-call arg never leaks it into a sibling arg.
        if let Some(e) = expected {
            self.expected_hint = Some(e.clone());
            let t = self.infer_value(arg);
            self.expected_hint = None;
            return t;
        }
        self.infer_value(arg)
    }

    /// First-pass bottom-up inference of a generic ctor/call's args (the pass that drives unification).
    /// A closure arg is inferred WITHOUT an expected type here (its params would be `Unknown`); its body
    /// errors — and the phase-5 "cannot infer parameter" error — are SUPPRESSED, because the per-arg
    /// check ([`Checker::check_generic_arg`]) re-infers each closure against its SUBSTITUTED expected
    /// type and reports cleanly there (mirrors the `RwShared.read` recovery-reinfer idiom). Every
    /// generic ctor/variant/fn/method path uses this pair so closure params are pinned by the field/
    /// param type, not left `Unknown`.
    fn infer_generic_arg_tys(&mut self, args: &[Expr]) -> Vec<Ty> {
        args.iter()
            .map(|a| {
                if matches!(a.kind, ExprKind::Closure { .. }) {
                    let mark = self.errors.len();
                    // Keep the closure's unannotated params `Unknown` in the unification prepass —
                    // the free-body scan (sources #2/#3) must not pin them here (see the field doc).
                    let saved = std::mem::replace(&mut self.generic_arg_prepass, true);
                    let t = self.infer_value(a);
                    self.generic_arg_prepass = saved;
                    self.errors.truncate(mark);
                    t
                } else {
                    self.infer_value(a)
                }
            })
            .collect()
    }

    /// Per-argument check for a generic ctor/call/method. `expected` is the arg's SUBSTITUTED declared
    /// type; `fallback` is its first-pass inferred type. A closure arg is re-inferred in checking-mode
    /// against `expected` (binding its unannotated params + reporting body errors here, since
    /// [`Checker::infer_generic_arg_tys`] suppressed them); other args keep `fallback`. Then assert
    /// assignability with the uniform "argument to '<name>'" diagnostic.
    fn check_generic_arg(&mut self, name: &str, expected: &Ty, fallback: &Ty, arg: &Expr) {
        if matches!(arg.kind, ExprKind::Closure { .. }) {
            // Re-infer the closure in checking-mode against the substituted expected type: this binds
            // its unannotated params and re-reports its body errors (which `infer_generic_arg_tys`
            // suppressed). The assignability check below still uses the first-pass `fallback`
            // (Unknown-bearing) type — its params/return are leniently assignable, so a type param
            // bound ONLY from this closure's body (e.g. `Mapped`'s `U`, recovered from the closure's
            // return) doesn't spuriously fail against an unbound `Ty::Param`. The param binding + body
            // re-check is the real enforcement; the `fallback` check still catches an arity or
            // annotated-return mismatch.
            self.infer_arg(arg, Some(expected));
        }
        if !self.assignable(expected, fallback) {
            self.error(
                arg.span,
                format!("argument to '{name}' has type {fallback}, expected {expected}"),
            );
        }
    }

    /// Trial-check the given closure args (by index into `args`/`decl_tys`) against their declared
    /// slot type substituted with `sub`, reporting whether ANY error was produced. Errors emitted
    /// during the probe are rolled back (`truncate`) so the trial never leaks diagnostics — it only
    /// answers "would these bodies error under this substitution?". Used by
    /// [`Checker::report_uninferable_closure_params`] to discriminate a genuine type-param deadlock
    /// from a harmless body. (Hover side effects are first-hit-wins and idempotent, so the eventual
    /// real per-arg check records the same result.)
    fn trial_check_closure_args(
        &mut self,
        idxs: &[usize],
        decl_tys: &[Ty],
        args: &[Expr],
        sub: &HashMap<String, Ty>,
        prepass: bool,
    ) -> bool {
        // `prepass` routes an unannotated closure param whose substituted slot type is `Unknown`
        // through the silent-`Unknown` binding (see `infer_closure`) instead of the
        // free-scan/annotation-required path — so the "params bound to `Unknown`" trial actually
        // checks the body with `Unknown` params (no spurious "cannot infer parameter").
        let saved = std::mem::replace(&mut self.generic_arg_prepass, prepass);
        let mark = self.errors.len();
        for &i in idxs {
            if let (Some(decl), Some(arg)) = (decl_tys.get(i), args.get(i)) {
                let expected = subst(decl, sub);
                self.infer_arg(arg, Some(&expected));
            }
        }
        let errored = self.errors.len() > mark;
        self.errors.truncate(mark);
        self.generic_arg_prepass = saved;
        errored
    }

    /// Detect (and clearly report) the un-inferable type-parameter DEADLOCK that arises when a
    /// generic ctor/fn is given an **unannotated closure** for a closure-typed slot AND no other
    /// argument pins the type parameter that slot mentions (e.g. `Heap([], fn(a, b): a < b)` — the
    /// empty `[]` gives no element type and the bare comparator params give none either). Without
    /// this, the leftover `Ty::Param(T)` flows into the closure body and surfaces as a misleading
    /// "cannot compare T and T" inside the user's lambda.
    ///
    /// Fires ONLY on the genuine deadlock: a still-unbound param (`!sub.contains_key`) that is
    /// mentioned by an unannotated closure's PARAMETER slot AND whose body actually *constrains* it.
    /// The mention is a NECESSARY condition (forms that pin the param — turbofish, a concrete
    /// element, annotated closure params — leave `sub` populated, so the guard never reaches here);
    /// the body PROBE ([`Checker::trial_check_closure_args`], two trials) is the SUFFICIENT one: it
    /// fires only when leaving the param as the unbound `Ty::Param` errors the body but binding it to
    /// `Unknown` does not. That keeps a harmless body that never uses the param (`fn(x): print(x)`,
    /// `fn(x): 42`) type-checking — those ran on `main` and must not be newly rejected — and leaves an
    /// unrelated body error (errors under BOTH trials) for the normal per-arg check to report.
    /// On firing it binds the offending params to `Unknown` in `sub` so the downstream substituted
    /// closure check sees `Unknown` params (no cascade) — the resulting type is identical to the
    /// existing `unwrap_or(Ty::Unknown)` fallback, so this is behavior-preserving apart from the
    /// message. Returns `true` if it fired.
    fn report_uninferable_closure_params(
        &mut self,
        name: &str,
        tps: &[TypeParam],
        decl_tys: &[Ty],
        args: &[Expr],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) -> bool {
        let unbound: std::collections::HashSet<String> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .filter(|n| !sub.contains_key(n))
            .collect();
        if unbound.is_empty() {
            return false;
        }
        // Only an unbound param appearing in a closure's PARAMETER position is a deadlock: such a
        // param can't be recovered from the closure body (the params would have to be typed first).
        // A param that appears ONLY in the closure's RETURN slot (e.g. `Mapped`'s `f: fn(T) -> U`)
        // is still inferable from the body, so it must NOT trigger — scan declared param slots only,
        // aligned to the UNANNOTATED closure parameters.
        let mut mentioned: Vec<String> = Vec::new();
        let mut candidate_idxs: Vec<usize> = Vec::new();
        for (ai, (decl, arg)) in decl_tys.iter().zip(args).enumerate() {
            if let ExprKind::Closure {
                params: cparams, ..
            } = &arg.kind
                && let Ty::Func {
                    params: dparams, ..
                } = decl
            {
                let before = mentioned.len();
                for (i, cp) in cparams.iter().enumerate() {
                    if cp.ty.is_none()
                        && let Some(dp) = dparams.get(i)
                    {
                        ty_collect_params(dp, &unbound, &mut mentioned);
                    }
                }
                // This closure arg has an unannotated param slot mentioning an unbound param — it is
                // a candidate whose BODY we must probe before deciding the deadlock actually bites.
                if mentioned.len() > before {
                    candidate_idxs.push(ai);
                }
            }
        }
        if mentioned.is_empty() {
            return false;
        }
        // Preserve declaration order of the type params in the message.
        let names: Vec<String> = tps
            .iter()
            .map(|tp| tp.name.clone())
            .filter(|n| mentioned.contains(n))
            .collect();
        // PROBE the candidate closure bodies before firing. A still-unbound param in an unannotated
        // closure slot is a *potential* deadlock, but only a GENUINE one when the body actually
        // constrains that param (e.g. `a < b` — ordering on an unconstrained `Ty::Param` is
        // rejected). A harmless body (`print(x)`, a constant) imposes no constraint, type-checks on
        // every engine, and ran on `main`; firing there would reject previously-valid code. The
        // discriminator: check the candidate bodies twice — once leaving the params as the unbound
        // `Ty::Param` (the current `sub`), once with them bound to `Unknown`. Fire ONLY when the
        // unbound form errors AND the `Unknown` form is clean. That isolates a true "needs `T`"
        // body from a harmless one (clean under BOTH) and from an unrelated body error (errors under
        // BOTH — left for the normal per-arg check to report as itself).
        let errored_unbound =
            self.trial_check_closure_args(&candidate_idxs, decl_tys, args, sub, false);
        if !errored_unbound {
            return false;
        }
        let mut sub_unknown = sub.clone();
        for n in &names {
            sub_unknown.insert(n.clone(), Ty::Unknown);
        }
        let errored_unknown =
            self.trial_check_closure_args(&candidate_idxs, decl_tys, args, &sub_unknown, true);
        if errored_unknown {
            // The body errors even with the params known (Unknown) — the failure is unrelated to
            // `T`. Don't mask it; let the normal per-arg check surface the real diagnostic.
            return false;
        }
        let list = names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let turbo = names.join(", ");
        let word = if names.len() == 1 {
            "type parameter"
        } else {
            "type parameters"
        };
        self.error(
            span,
            format!(
                "cannot infer {word} {list} of `{name}`; annotate `{name}[{turbo}](…)` or the closure parameters"
            ),
        );
        for n in names {
            sub.insert(n, Ty::Unknown);
        }
        true
    }

    /// [`Checker::check_args_range`] with an explicit `widen` flag — see [`Checker::assignable_w`].
    fn check_args_range_w(
        &mut self,
        name: &str,
        params: &[Ty],
        min_params: usize,
        args: &[Expr],
        span: Span,
        widen: bool,
    ) {
        if !(min_params..=params.len()).contains(&args.len()) {
            let want = if min_params == params.len() {
                format!("{}", params.len())
            } else {
                format!("{min_params}–{}", params.len())
            };
            self.error(
                span,
                format!("'{name}' expects {want} argument(s), got {}", args.len()),
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let at = self.infer_arg(arg, params.get(i));
            if let Some(pt) = params.get(i)
                && !self.assignable_w(pt, &at, widen)
            {
                // Transparency: render `ref T` (not the lowered `Ref[T]`) ONLY when the argument is
                // a `ref` binding — an explicit first-class `Ref[T]` arg keeps its `Ref[T]` spelling.
                let is_ref_arg = matches!(&arg.kind, ExprKind::Ident(n) if self.is_ref_decl(n));
                let (expected, actual) = if is_ref_arg {
                    (ref_display(pt), ref_display(&at))
                } else {
                    (pt.to_string(), at.to_string())
                };
                // Annotation hint for a collection mutator whose element slot was PINNED by an
                // earlier push/add/insert (refine-on-first-use). An un-annotated `xs := []` reads as
                // `list[<first element>]`; a later element of a different (e.g. protocol-sibling) type
                // is a real mismatch — point the user at the explicit annotation that makes a
                // mixed/protocol collection legal.
                let hint = if i == 0 && matches!(name, "push" | "add" | "insert") {
                    // Only an UN-BOUND/leaked type param (not in scope here) means "un-inferred": a
                    // return-only `T` from `empty[T]()` called with nothing to bind it from. A
                    // `Ty::Param` that IS in scope (`self.type_params`) is a legitimately-bound
                    // generic param genuinely pinned by an earlier push — keep the original
                    // narrative for it (and for every concrete element type).
                    if matches!(pt, Ty::Param(p) if !self.type_params.contains_key(p)) {
                        // The expected element type is an un-inferred type parameter (e.g. a
                        // return-only `T` from `empty[T]()` with nothing to bind it from), NOT a
                        // type pinned by an earlier push. The "earlier push" narrative is wrong
                        // here (this may be the FIRST push) and `List[<protocol>] = []` would not
                        // help — the fix is to bind the parameter at the construction site.
                        format!(
                            " (the collection's element type is the un-inferred type parameter {expected}; bind it at the construction site with a turbofish or annotation, e.g. `empty[int]()` or `xs: List[int] = ...`)"
                        )
                    } else {
                        format!(
                            " (the collection's element type was pinned to {expected} by an earlier {name}; annotate the binding, e.g. `List[<protocol>] = []`, for a mixed/protocol collection)"
                        )
                    }
                } else {
                    String::new()
                };
                self.error(
                    arg.span,
                    format!(
                        "argument {} of '{name}': expected {expected}, found {actual}{hint}",
                        i + 1
                    ),
                );
            }
        }
    }

    fn check_arity(&mut self, name: &str, n: usize, args: &[Expr], span: Span) {
        if args.len() != n {
            self.error(
                span,
                format!("{name}() expects {n} argument(s), got {}", args.len()),
            );
        }
    }

    fn expect_bool(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer_value(e);
        if t != Ty::Bool && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be bool, found {t}"));
        }
    }

    fn expect_int(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer_value(e);
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
        // Key by the struct's runtime key (bare unless a cross-module clash disambiguated it), exactly
        // like `enum_self_ty`/`newtype_self_ty`. In the multi-module (`build_graph`) path the layout is
        // stored under `<module-key>::Name`, so a bare-`name` lookup here would miss — leaving `self`'s
        // type wrong during both return inference and pass-2 body checking.
        let key = self.bare_key(name);
        let args = self
            .structs
            .get(&key)
            .map(|i| {
                i.type_params
                    .iter()
                    .map(|tp| Ty::Param(tp.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Ty::Struct(key, args)
    }

    /// The `Ty::Enum` of an enum's own `self`: keyed by its runtime key, parameterized by its own
    /// generic type params as `Ty::Param`s (so `fn get(self) -> T` inside `enum Box[T]` resolves).
    fn enum_self_ty(&self, name: &str) -> Ty {
        let key = self.bare_key(name);
        let args = self
            .enum_type_params
            .get(&key)
            .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
            .unwrap_or_default();
        Ty::Enum(key, args)
    }

    /// The `Ty::NewType` of a newtype's own `self`: keyed by its runtime key, parameterized by its
    /// own generic type params as `Ty::Param`s (so `fn peek(self) -> Option[T]` inside
    /// `newtype Stack[T]` resolves `T`). Mirrors `enum_self_ty`.
    fn newtype_self_ty(&self, name: &str) -> Ty {
        let key = self.bare_key(name);
        let args = self
            .newtype_type_params
            .get(&key)
            .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
            .unwrap_or_default();
        Ty::NewType(key, args)
    }

    /// Reject a reserved builtin type name used as a generic type-PARAMETER identifier (`struct
    /// Box[int]` / `[List]` / `[Result]`, a method's own `[U]`, a `protocol P[int]`). A reserved name
    /// as a param is a one-way-ratchet violation that otherwise type-checks clean and then shadows
    /// kind-dependently — a scalar param (`int`/`str`/…) is dead/unreferenceable (the scalar wins in
    /// `resolve_type`), while a container/enum-builtin param (`List`/`Result`/…) silently SHADOWS the
    /// builtin and acts as a real generic. Mirror the decl-NAME guards (`struct int` →
    /// `reserved (builtin)`) so the param form errors identically. Predicate = `is_reserved_type` +
    /// the fixed-width FFI integer names (`int32`/`int64`/…, reserved TYPE names via
    /// `native::ffi::TYPE_NAMES`). Deliberately NOT `is_reserved_protocol`: a param named after a
    /// prebuilt protocol (`fn id[Comparable]`) is a protocol-name shadow, not a builtin-TYPE shadow,
    /// and is kept legal by design (guarded by `protocol_bound_and_typeparam_named_protocol_still_ok`,
    /// commit b2aa8ac). A param BOUND `[T: Comparable]` is likewise untouched (the bound is a separate
    /// `Bound` list, not the param name). Called once per decl at the hoist sites (struct/enum/newtype/
    /// fn_sig/protocol), NOT inside `enter_type_params` (which is re-entered during body checking and
    /// would double-report).
    fn reject_reserved_type_params(&mut self, tps: &[TypeParam]) {
        for tp in tps {
            if is_reserved_type(&tp.name)
                || crate::native::ffi::TYPE_NAMES.contains(&tp.name.as_str())
            {
                self.error(
                    tp.name_span,
                    format!("type '{}' is reserved (builtin)", tp.name),
                );
            }
        }
    }

    /// Install `tps` as the in-scope generic type parameters, returning the previous map to restore.
    fn enter_type_params(&mut self, tps: &[TypeParam]) -> HashMap<String, Vec<Bound>> {
        let saved = self.type_params.clone();
        for tp in tps {
            // Editor hover (decl-site): record the bound generic param `T` at its DECLARATION token
            // (`fn id[T]`, `struct Box[T]`, a method `[U]`). This is the single funnel for entering
            // type params, so every generic decl form is covered. The hover renders the bare param
            // name (`T`); a bound suffix (`T: Comparable`) is not representable through the `Ty`-only
            // hover channel. GUARD on the probe so the `Ty::Param(..clone())` argument is not built on
            // every generic check (enter_type_params is hot — runs for every generic fn/struct/method).
            if self.hover_probe.is_some() {
                self.hover_record_at(
                    tp.name_span,
                    &Ty::Param(tp.name.clone()),
                    HoverKind::Struct,
                    None,
                );
            }
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
                self.error(
                    span,
                    format!("unknown protocol '{}' in bound on '{param}'", b.name),
                );
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
        embeds: &[Bound],
        span: Span,
    ) {
        if is_reserved_protocol(name) {
            self.error(span, format!("protocol '{name}' is reserved (builtin)"));
            return;
        }
        if self.protocols.contains_key(name) {
            self.error(span, format!("protocol '{name}' is already defined"));
        }
        // A protocol's own type param may not be named after a reserved builtin type (`protocol
        // P[int]`) — same rule as struct/enum/newtype/fn params.
        self.reject_reserved_type_params(type_params);
        let mut saved = self.type_params.clone();
        std::mem::swap(&mut self.type_params, &mut saved); // start clean, with only Self visible
        self.type_params.insert("Self".to_string(), Vec::new());
        // The protocol's own type params are in scope while resolving its method signatures, so
        // `fn get(self, i: int) -> T` resolves `T` to `Ty::Param("T")`.
        for tp in type_params {
            if tp.name == "Self" {
                self.error(
                    span,
                    "protocol type parameter cannot be named 'Self'".to_string(),
                );
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
                        // A `ref T` protocol-method param is a `Ref[T]` box, exactly like a `ref`
                        // param on a concrete fn/method (charge 4) — so a conforming struct's `ref`
                        // method matches it, and a `ref` arg through the existential aliases.
                        Some(t) if p.is_ref => {
                            self.check_ref_ty(t, span);
                            self.resolve_type(
                                &Type::Generic("Ref".to_string(), vec![t.clone()], Span::default()),
                                span,
                            )
                        }
                        Some(t) => self.resolve_type(t, span),
                        None if p.name == "self" => Ty::Unknown,
                        None => {
                            self.error(
                                span,
                                format!("protocol method parameter '{}' needs a type", p.name),
                            );
                            Ty::Unknown
                        }
                    })
                    .collect();
                let ret = m
                    .ret
                    .as_ref()
                    .map(|t| self.resolve_type(t, span))
                    .unwrap_or(Ty::Nil);
                (m.name.clone(), FnSig::plain(params, ret))
            })
            .collect();
        self.type_params = saved; // restore
        self.protocols.insert(
            name.to_string(),
            ProtocolInfo {
                type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                methods: sigs,
                embeds: embeds.to_vec(),
            },
        );
    }

    /// M22 — validate a protocol's embeds AFTER every protocol is hoisted (so forward/cyclic refs
    /// resolve). Three LOCKED rules: (1) an OWN `fn` whose name collides with any transitively-embedded
    /// required method → error, regardless of order or sig; (2) two embeds pulling the same method name
    /// with DIFFERING signatures → error (identical sigs dedup silently — the legal diamond); (3) a
    /// cyclic embed (A embeds B, B embeds A) → error. Built-in protocols (registered via
    /// [`prebuilt_protocols`], not here) are trusted and skipped.
    fn validate_protocol_embeds(
        &mut self,
        name: &str,
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_reserved_protocol(name) {
            return; // builtin (or already errored as a redeclaration / reserved name)
        }
        // Each embed must name a real protocol.
        for emb in embeds {
            if !self.protocols.contains_key(&emb.name) {
                self.error(
                    span,
                    format!("unknown protocol '{}' embedded in '{name}'", emb.name),
                );
            }
        }
        // Flatten the transitive embed method set, detecting cycles + cross-embed signature conflicts.
        let mut path = vec![name.to_string()];
        let (required, cyclic, conflict) = self.flatten_embed_methods(embeds, &mut path);
        if cyclic {
            self.error(
                span,
                format!("cyclic protocol embedding involving '{name}'"),
            );
            return;
        }
        if let Some(m) = conflict {
            self.error(
                span,
                format!("conflicting signature for method '{m}' from embedded protocols"),
            );
        }
        // Rule 1: an own `fn` colliding with an embedded-required method name.
        for m in methods {
            if required.contains_key(&m.name) {
                self.error(
                    span,
                    format!(
                        "method '{}' conflicts with embedded protocol requirement",
                        m.name
                    ),
                );
            }
        }
    }

    /// Recursively collect the method signatures required by a list of embeds. Returns
    /// `(required, cyclic, conflict)`: `required` maps method name → signature (identical sigs deduped),
    /// `cyclic` is set if any embed revisits a protocol on the current `path`, and `conflict` names a
    /// method seen with two DIFFERING signatures (rule 2). Read-only over `self.protocols`.
    fn flatten_embed_methods(
        &self,
        embeds: &[Bound],
        path: &mut Vec<String>,
    ) -> (HashMap<String, FnSig>, bool, Option<String>) {
        let mut required: HashMap<String, FnSig> = HashMap::new();
        let mut conflict: Option<String> = None;
        let mut merge = |mn: &str, ms: &FnSig, conflict: &mut Option<String>| {
            match required.get(mn) {
                Some(existing) if !fn_sig_eq(existing, ms) => {
                    if conflict.is_none() {
                        *conflict = Some(mn.to_string());
                    }
                }
                Some(_) => {} // identical sig — dedup silently (legal diamond)
                None => {
                    required.insert(mn.to_string(), ms.clone());
                }
            }
        };
        for emb in embeds {
            if path.iter().any(|p| p == &emb.name) {
                return (required, true, conflict);
            }
            let Some(pinfo) = self.protocols.get(&emb.name).cloned() else {
                continue; // unknown embed already errored in validate_protocol_embeds
            };
            for (mn, ms) in &pinfo.methods {
                merge(mn, ms, &mut conflict);
            }
            path.push(emb.name.clone());
            let (sub, cyclic, sub_conf) = self.flatten_embed_methods(&pinfo.embeds, path);
            path.pop();
            if cyclic {
                return (required, true, conflict);
            }
            for (mn, ms) in &sub {
                merge(mn, ms, &mut conflict);
            }
            if conflict.is_none() {
                conflict = sub_conf;
            }
        }
        (required, false, conflict)
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

    /// Refine-on-first-use (empty-slot half of the `Ty::Unknown` soundness family). A bare empty
    /// collection literal (`[]`/`{}`/`set()`), a nullary user-enum variant (`Box.Empty`), or the
    /// native nullary `None` types its element/key/value/type-arg slot as `Ty::Unknown`, which is
    /// permissive in both directions — so junk would flow into a check-blessed program and fault at
    /// runtime, and the float-key/Hashable ban would be bypassed. This hook fires at the top of
    /// `infer_method_call`, when a mutating method (`push`/`add`/`insert`/`extend`) on a
    /// **simple-variable** receiver supplies a CONCRETE type at an `Unknown` slot: it structurally
    /// merges the supplied shape into the binding, re-pins it in its owning scope, and runs the
    /// Hashable check on a newly-concrete set element. A later op supplying an incompatible concrete
    /// type then fails as a normal `check_args` mismatch against the now-pinned element — and the
    /// mismatch diagnostic is enriched (in `check_args`) to hint at annotating for a mixed/protocol
    /// collection.
    ///
    /// RESIDUAL HOLE (documented, not fixed here): refine only fires when the receiver is a simple
    /// `Ident` in scope. `obj.field.push(...)` / `f().push(...)` / `xss[0].push(...)` (non-Ident
    /// receivers) stay unrefined — struct fields are explicitly typed anyway, so the impact is low.
    fn refine_receiver(&mut self, obj: &Expr, obj_ty: &Ty, method: &str, args: &[Expr]) {
        // (a) simple-variable receiver only (the documented limitation).
        let ExprKind::Ident(name) = &obj.kind else {
            return;
        };
        // Must be a real in-scope binding (not a function/global-type name).
        if self.lookup(name).is_none() {
            return;
        }
        // Skip captured bindings: mirror the airlock reassignment ban — refine is a checker-side
        // narrowing, but skipping it here keeps behavior aligned and avoids a confusing diagnostic.
        if self.is_captured(name) {
            return;
        }
        // (b) the binding must have an Unknown in a SLOT position (not a bare top-level Unknown —
        // that's the cascade-suppression sentinel and must stay permissive).
        if !contains_unknown_in_slot(obj_ty) {
            return;
        }
        // (c) determine the supplied ELEMENT type from a slot-supplying mutator's args.
        // `push(x)`/`add(x)`/`insert(x)` supply the element directly; `extend(xs)` supplies a
        // list/set whose element refines ours.
        let mark = self.errors.len();
        let elem = match method {
            "push" | "add" | "insert" => args.first().map(|a| self.infer_value(a)),
            "extend" => args.first().map(|a| match self.infer_value(a) {
                Ty::List(e) | Ty::Set(e) => *e,
                other => other,
            }),
            _ => return,
        };
        let Some(elem) = elem else { return };
        // Wrap the element into a RECEIVER-SHAPED value so the structural merge lines up the slot:
        // a list receiver merges with `list[elem]`, a set receiver with `set[elem]`. Any other
        // receiver kind isn't a push/add/extend target, so nothing to refine.
        let shape = match obj_ty {
            Ty::List(_) => Ty::list(elem),
            Ty::Set(_) => Ty::set(elem),
            _ => return,
        };
        // (d) cascade invariant: if inferring the arg itself reported an error, don't refine — and
        // roll back the speculative diagnostics so the real dispatch path (check_args) reports them
        // exactly once. Leaving them here double-reports an erroring arg (e.g. `xs.push(undefined)`).
        if self.errors.len() != mark {
            self.errors.truncate(mark);
            return;
        }
        // A shape that is itself Unknown supplies nothing concrete; merge is a no-op, bail early.
        if shape.is_unknown() {
            return;
        }
        let merged = merge_unknown(obj_ty, &shape);
        if merged == *obj_ty {
            return; // nothing newly concrete
        }
        // Run the Hashable / float-key ban at the moment a SET element becomes concrete (the sig
        // tables don't). Map keys are handled in the `m[k]=v` index-assign refine path.
        if let Ty::Set(e) = &merged
            && !e.is_unknown()
            && !self.is_hashable_key(e)
        {
            self.error(
                obj.span,
                format!("set element type must implement Hashable (int, str, bool, or a struct with hash(self) -> int), found {e}"),
            );
        }
        self.repin(name, merged);
    }

    /// Refine-on-first-use for an index-assign `m[k]=v` / `xs[i]=v` (the assignment-statement
    /// sibling of [`Self::refine_receiver`]). When the receiver is a simple variable whose type has
    /// an `Unknown` key/value/element slot, merge the supplied (index type, value type) shape into
    /// the binding, re-pin it, and run the Hashable / float-key ban on a newly-concrete MAP key.
    /// `val_ty` is already inferred by the caller; we infer the index type here only when the
    /// receiver is actually refinable (so we don't double-report on the common already-typed path).
    fn refine_index_receiver(&mut self, obj: &Expr, index: &Expr, val_ty: &Ty) {
        let ExprKind::Ident(name) = &obj.kind else {
            return;
        };
        let Some(obj_ty) = self.lookup(name) else {
            return;
        };
        if self.is_captured(name) || !contains_unknown_in_slot(&obj_ty) {
            return;
        }
        if val_ty.is_unknown() {
            return;
        }
        // The supplied shape mirrors the receiver kind: `Map(idx, val)` for a map, `List(val)` for a
        // list (index type is the int position, irrelevant to the element slot).
        let mark = self.errors.len();
        let shape = match &obj_ty {
            Ty::Map(..) => Ty::map(self.infer(index), val_ty.clone()),
            Ty::List(..) => Ty::list(val_ty.clone()),
            _ => return,
        };
        if self.errors.len() != mark {
            self.errors.truncate(mark); // roll back the speculative index-infer diagnostics; the
            return; // real index-assign path re-infers + reports them once (no double-report)
        }
        let merged = merge_unknown(&obj_ty, &shape);
        if merged == obj_ty {
            return;
        }
        // NOTE: the map-key Hashable / float-key ban is NOT run here — it is the direct
        // insertion-site check in `check_assign`'s Index branch (so it fires even while the key type
        // is still `Unknown`, e.g. `m:={}; m[1.5]=..`), keeping a single owner and no double-report.
        self.repin(name, merged);
    }

    /// Assignability with protocol-existential awareness. Like the free [`compatible`], but a
    /// concrete type is assignable to a `Protocol(P)` slot iff it satisfies `P` — which needs the
    /// protocol/struct registry, so it can't live in the context-free `compatible`. Recurses through
    /// compound types so a nested existential (the `E` in `Result[T, Error]`) is checked structurally.
    /// Strict assignability — NO int→float widening (the reverse `float`→`int` is always rejected).
    fn assignable(&self, expected: &Ty, actual: &Ty) -> bool {
        use Ty::*;
        match (expected, actual) {
            (Unknown, _) | (_, Unknown) => true,
            (Protocol(p), a) => self.satisfies(a, p).is_ok(),
            (List(e), List(a)) | (Option(e), Option(a)) | (Set(e), Set(a)) => self.assignable(e, a),
            (Result(et, ee), Result(at, ae)) => self.assignable(et, at) && self.assignable(ee, ae),
            (Map(ek, ev), Map(ak, av)) => self.assignable(ek, ak) && self.assignable(ev, av),
            (Struct(n, ea), Struct(m, aa)) | (Enum(n, ea), Enum(m, aa)) => {
                n == m
                    && ea.len() == aa.len()
                    && ea.iter().zip(aa).all(|(x, y)| self.assignable(x, y))
            }
            (Tuple(e), Tuple(a)) => {
                e.len() == a.len() && e.iter().zip(a).all(|(x, y)| self.assignable(x, y))
            }
            (
                Func {
                    params: p1,
                    ret: r1,
                },
                Func {
                    params: p2,
                    ret: r2,
                },
            ) => {
                p1.len() == p2.len()
                    && p1.iter().zip(p2).all(|(a, b)| self.assignable(a, b))
                    && self.assignable(r1, r2)
            }
            _ => compatible(expected, actual),
        }
    }

    /// Like [`Checker::assignable`], but accepts C-like **one-way int→float widening** at a SCALAR
    /// value-DEFINITION sink (typed `let`, function/struct/method arg, return, struct-field default):
    /// `(Float, Int)` only. Widening is deliberately NOT propagated into ANY compound position
    /// (list/set/option element, map/result value, struct/tuple/func) — only a scalar `float` sink is
    /// coerced by the compiler (`Op::CoerceFloat`), so widening a compound would accept an int-bearing
    /// value the runtime never converts (an `Int` left in a `float` slot — the exact hole this design
    /// avoids; the checker cannot tell a safe literal `[1, 2]` from an unsafe non-literal `f()`).
    /// Collection floats come instead from mixed-literal element inference (`[1, 2.3]` infers
    /// `list[float]`) + literal element coercion, which is independently sound. `widen=false` ⇒
    /// identical to [`Checker::assignable`].
    fn assignable_w(&self, expected: &Ty, actual: &Ty, widen: bool) -> bool {
        if widen && matches!((expected, actual), (Ty::Float, Ty::Int)) {
            return true;
        }
        self.assignable(expected, actual)
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

    /// M22 — does a declared bound `bound_name[bound_args]` PROVIDE `protocol[required]`, directly or
    /// transitively through embedded (super-)protocols? This is what makes `a + b` / `a / b` legal
    /// inside a `[T: Arithmetic]` body (the bound `Arithmetic` flattens to Add/Sub/Mul/Div) and lets a
    /// `[T: Arithmetic]` value forward into a `[U: Div]` call. Preserves the `Iterator`→`Iterable`
    /// subsumption. Depth-capped against a (declare-time-rejected, but still-checked) cyclic embed.
    fn bound_provides(
        &self,
        bound_name: &str,
        bound_args: &[Type],
        protocol: &str,
        required: &[Ty],
        depth: usize,
    ) -> bool {
        if depth > 64 {
            return false;
        }
        // Direct: the bound names the required protocol with matching args.
        if bound_name == protocol && self.bound_args_match(bound_args, required) {
            return true;
        }
        // Subsumption: every `Iterator[T]` IS `Iterable[T]` (its `iter()` returns self).
        if protocol == "Iterable"
            && bound_name == "Iterator"
            && self.bound_args_match(bound_args, required)
        {
            return true;
        }
        // Transitive: any embed of the bound's protocol provides it.
        if let Some(pinfo) = self.protocols.get(bound_name) {
            return pinfo
                .embeds
                .iter()
                .any(|e| self.bound_provides(&e.name, &e.args, protocol, required, depth + 1));
        }
        false
    }

    /// Read-only type resolution (no error emission), for contexts that only hold `&self`. Returns
    /// `Ty::Unknown` for anything it can't resolve, which callers treat permissively.
    fn resolve_ty_ro(&self, t: &Type) -> Ty {
        self.resolve_ty_ro_d(t, 0)
    }

    /// Depth-bounded read-only type resolution. `depth` guards against a recursive alias body
    /// (`type A = B; type B = A`): without `alias_resolving` (this is `&self`), a hard cap of 64
    /// expansions returns `Ty::Unknown` instead of overflowing the stack. 64 is far beyond any real
    /// alias chain.
    fn resolve_ty_ro_d(&self, t: &Type, depth: usize) -> Ty {
        if depth > 64 {
            return Ty::Unknown;
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "bytes" => Ty::Bytes,
                "bytearray" => Ty::ByteArray,
                "nil" => Ty::Nil,
                "Executor" => Ty::Executor,
                "Socket" => Ty::Socket,
                "Listener" => Ty::Listener,
                "ptr" => Ty::Ptr,
                "owned_str" => Ty::Str,
                _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                // A fixed-width FFI integer name resolves to plain `int` (the width is a marshalling
                // detail). Needed so an exported alias body `type Len = int32` captures `Ty::Int`.
                _ if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) => Ty::Int,
                // A bare alias name resolves to its (recursively-resolved) body.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ty_ro_d(&body, depth + 1)
                }
                _ if self.imported_alias_tys.contains_key(n) => self.imported_alias_tys[n].clone(),
                _ if self.struct_names.contains(n) => Ty::strukt(self.bare_key(n)),
                _ if self.enum_names.contains(n) => Ty::Enum(self.bare_key(n), Vec::new()),
                _ if self.newtype_names.contains(n) => Ty::NewType(self.bare_key(n), Vec::new()),
                _ if self.protocols.contains_key(n) => Ty::Protocol(n.clone()),
                _ => Ty::Unknown,
            },
            Type::Generic(n, args, ..) => match (n.as_str(), args.as_slice()) {
                ("List", [x]) => Ty::list(self.resolve_ty_ro_d(x, depth + 1)),
                ("Set", [x]) => Ty::set(self.resolve_ty_ro_d(x, depth + 1)),
                ("Option", [x]) => Ty::option(self.resolve_ty_ro_d(x, depth + 1)),
                ("Channel", [x]) => Ty::channel(self.resolve_ty_ro_d(x, depth + 1)),
                ("Shared", [x]) => Ty::shared(self.resolve_ty_ro_d(x, depth + 1)),
                ("RwShared", [x]) => Ty::rwshared(self.resolve_ty_ro_d(x, depth + 1)),
                ("Atomic", [x]) => Ty::atomic(self.resolve_ty_ro_d(x, depth + 1)),
                ("Result", [x]) => Ty::result(self.resolve_ty_ro_d(x, depth + 1)),
                ("Result", [x, e]) => Ty::result_e(
                    self.resolve_ty_ro_d(x, depth + 1),
                    self.resolve_ty_ro_d(e, depth + 1),
                ),
                ("Map", [k, v]) => Ty::map(
                    self.resolve_ty_ro_d(k, depth + 1),
                    self.resolve_ty_ro_d(v, depth + 1),
                ),
                _ if self.struct_names.contains(n) => Ty::Struct(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if self.enum_names.contains(n) => Ty::Enum(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if self.newtype_names.contains(n) => Ty::NewType(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ => Ty::Unknown,
            },
            Type::Func { params, ret } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.resolve_ty_ro_d(p, depth + 1))
                    .collect(),
                ret: Box::new(self.resolve_ty_ro_d(ret, depth + 1)),
            },
            Type::Tuple(ts) => Ty::Tuple(
                ts.iter()
                    .map(|t| self.resolve_ty_ro_d(t, depth + 1))
                    .collect(),
            ),
            Type::Qualified { module, name, args } => {
                let resolved_args: Vec<Ty> = args
                    .iter()
                    .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                    .collect();
                self.resolve_qualified_ro(module, name, &resolved_args)
            }
        }
    }

    /// Read-only resolution of a module-qualified type `module.name[args]` to a `Ty`. Looks the bound
    /// module up in `imported_modules`, finds the type in its `ModuleSig`, and returns the matching
    /// `Ty` (struct / enum / alias body). `Ty::Unknown` if anything is missing (callers permissive).
    fn resolve_qualified_ro(&self, module: &str, name: &str, args: &[Ty]) -> Ty {
        let Some(mid) = self.imported_modules.get(module) else {
            return Ty::Unknown;
        };
        let Some(sig) = self.module_sigs.get(mid) else {
            return Ty::Unknown;
        };
        if sig.struct_defs.contains_key(name) {
            Ty::Struct(self.type_key(mid, name), args.to_vec())
        } else if sig.enum_defs.contains_key(name) {
            Ty::Enum(self.type_key(mid, name), args.to_vec())
        } else if sig.newtype_defs.contains_key(name) {
            Ty::NewType(self.type_key(mid, name), args.to_vec())
        } else if let Some(asig) = sig.type_aliases.get(name) {
            asig.body.clone()
        } else if sig.types.contains(name) {
            // Mirror `resolve_type`'s qualified builtin branch on the READ-ONLY export path (so an
            // EXPORTED `type S = concurrency.Shared[int]` / `newtype MyS[T] = concurrency.Shared[T]`
            // resolved via `resolve_ty_ro_d` carries the right builtin `Ty`). Permissive: no errors,
            // and a non-type `sig.types` name (`timer`) returns `Ty::Unknown`.
            self.qualified_builtin_ty(name, args).unwrap_or(Ty::Unknown)
        } else {
            Ty::Unknown
        }
    }

    /// Resolve a surface extern `Type` to its WIDTH-BEARING [`CType`] in the CURRENT module's
    /// import/alias scope — the SINGLE resolver both backends consume (the FFI collision-fix root).
    /// Mirrors `resolve_ty_ro_d`'s alias / `from`-import / `Qualified` walk EXACTLY, but stops at the
    /// width-bearing leaf instead of collapsing every FFI integer to `Ty::Int`. Crucially:
    ///   * a LOCAL alias (`self.aliases`) recurses on its body — resolving each hop in THIS module's
    ///     scope (a colliding same-named alias in another module can never be reached);
    ///   * a `from`-imported alias reads `self.imported_alias_ctypes` (the alias's CType computed in
    ///     its DEFINING module's scope) — closing the named-import-hop hole;
    ///   * a `module.Alias` reads the TARGET module's `AliasSig.ctype` (likewise defining-scope).
    ///
    /// `depth > 64` returns `None` (cycle guard, matching `resolve_ty_ro_d`). `None` means "not
    /// C-marshallable here" — the marshallability gate (`assert_marshallable`) is the actual error,
    /// this is only the width carrier.
    fn resolve_ctype(&self, t: &Type) -> Option<CType> {
        self.resolve_ctype_d(t, 0)
    }

    fn resolve_ctype_d(&self, t: &Type, depth: usize) -> Option<CType> {
        if depth > 64 {
            return None; // cyclic alias — defended (the marshal gate rejects it cleanly).
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                "int" => Some(CType::Int),
                "float" => Some(CType::Float),
                "bool" => Some(CType::Bool),
                "str" => Some(CType::Str),
                "ptr" => Some(CType::Ptr),
                "owned_str" => Some(CType::OwnedStr),
                "int8" => Some(CType::Int8),
                "int16" => Some(CType::Int16),
                "int32" => Some(CType::Int32),
                "int64" => Some(CType::Int64),
                "uint8" => Some(CType::UInt8),
                "uint16" => Some(CType::UInt16),
                "uint32" => Some(CType::UInt32),
                "uint64" => Some(CType::UInt64),
                // A LOCAL transparent alias: recurse on its body in THIS module's scope.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ctype_d(&body, depth + 1)
                }
                // A `from`-imported alias: its CType was computed in the DEFINING module's scope.
                _ if self.imported_alias_ctypes.contains_key(n) => {
                    self.imported_alias_ctypes[n].clone()
                }
                // A bare-visible struct (local or `from`-imported): a by-value flat-scalar struct.
                // SAME-MODULE path (current scope == defining scope): a cache hit (already populated)
                // OR a field-walk in THIS scope for a not-yet-populated forward-reference nested struct
                // — both correct here because this is the struct's own defining scope.
                _ if self.struct_names.contains(n) => {
                    let key = self.bare_key(n);
                    self.resolve_struct_ctype(&key)
                        .or_else(|| self.struct_ctype_from_asts(&key, depth))
                }
                _ => None,
            },
            // A sync scalar callback param (callbacks #4): `fn(scalars...) -> scalar` lowers to
            // `CType::Callback`. Every part must lower to a C SCALAR (`is_scalar`) — a non-scalar
            // part (str/struct/nested callback) yields `None`, so the marshal gate rejects it cleanly.
            // (Param-only; a function-typed RETURN is rejected by `assert_marshallable`, never lowered.)
            Type::Func { params, ret } => {
                let mut cparams = Vec::with_capacity(params.len());
                for p in params {
                    let cp = self.resolve_ctype_d(p, depth + 1)?;
                    if !cp.is_scalar() {
                        return None;
                    }
                    cparams.push(cp);
                }
                let cret = self.resolve_ctype_d(ret, depth + 1)?;
                if !cret.is_scalar() {
                    return None;
                }
                Some(CType::Callback {
                    params: cparams,
                    ret: Box::new(cret),
                })
            }
            // RETURN-ONLY nullable `char*` (`str?` / `owned_str?`): the inner type decides
            // borrowed (`str` → OptStr) vs owned (`owned_str` → OptOwnedStr).
            Type::Generic(n, args, ..) if n == "Option" && args.len() == 1 => {
                match self.resolve_ctype_d(&args[0], depth + 1) {
                    Some(CType::Str) => Some(CType::OptStr),
                    Some(CType::OwnedStr) => Some(CType::OptOwnedStr),
                    _ => None,
                }
            }
            // A module-qualified type `mod.Name` — resolved in the TARGET module's scope, so a width
            // alias carries its DEFINING module's width and a struct its identity key (collision-proof
            // by construction: never the bare flat alias map).
            Type::Qualified { module, name, .. } => {
                let mid = self.imported_modules.get(module)?;
                let sig = self.module_sigs.get(mid)?;
                // A qualified FFI marshalling TYPE name (`ffi.int32` / `ffi.ptr`) in an extern
                // signature: map to its WIDTH-bearing CType, exactly like the bare `Type::Named`
                // arm above. The surface name is unchanged (the backends' `ctype_of` reads the
                // resulting CType, never re-derives from a module prefix), so it marshals identically
                // to the bare width. Gated on `sig.types` membership — only `std.ffi`'s sig carries
                // these reserved names — so this fires solely for genuine FFI types.
                if sig.types.contains(name) {
                    let c = match name.as_str() {
                        "ptr" => Some(CType::Ptr),
                        "int8" => Some(CType::Int8),
                        "int16" => Some(CType::Int16),
                        "int32" => Some(CType::Int32),
                        "int64" => Some(CType::Int64),
                        "uint8" => Some(CType::UInt8),
                        "uint16" => Some(CType::UInt16),
                        "uint32" => Some(CType::UInt32),
                        "uint64" => Some(CType::UInt64),
                        _ => None,
                    };
                    if c.is_some() {
                        return c;
                    }
                }
                if sig.struct_defs.contains_key(name) {
                    // CROSS-MODULE: read the qualified struct's CType from the cache VERBATIM (computed
                    // in ITS defining module's scope, deps-first). NEVER field-walk here — the current
                    // scope is the IMPORTER's, where the struct's field aliases are invisible/colliding.
                    self.resolve_struct_ctype(&self.type_key(mid, name))
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    asig.ctype.clone()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// PURE CACHE READ of the by-value `CType::Struct` for the struct under IDENTITY `key`. The CType
    /// was pre-computed in the struct's OWN DEFINING module's scope (by `populate_struct_ctypes`, run
    /// after `hoist` in each module's `check_module` — deps-first AND before this module's own extern
    /// harvest, so a cross-module OR same-module struct is always cached before an extern needs it).
    /// This is the SINGLE-RESOLVER invariant that kills the FFI drift: `resolve_struct_ctype` NEVER
    /// re-resolves a struct's fields in the (wrong, importing) current scope — it only reads the cache,
    /// so a field typed via the defining module's local alias keeps its true width.
    fn resolve_struct_ctype(&self, key: &str) -> Option<CType> {
        self.struct_ctypes.get(key).cloned().flatten()
    }

    /// Cache every struct DECLARED IN THIS MODULE under its identity key, its by-value `CType::Struct`
    /// computed HERE — in this (the DEFINING) module's import/alias scope (extending the
    /// `AliasSig::ctype` precedent to structs). Called once per module from `check_module` after
    /// `hoist` (so all of this module's aliases/`from`-imports are live) and BEFORE the check_stmt loop
    /// (so a same-module extern harvested in the loop reads the cache). Modules are checked deps-first,
    /// so a downstream importer's extern returning `mod.Struct` reads this cached, defining-scope CType
    /// verbatim. Gated to the extern-harvesting pass; a lone `check` never builds these.
    fn populate_struct_ctypes(&mut self, stmts: &[Stmt], id: Option<&ModuleId>) {
        let Some(mid) = id else { return };
        let mid = mid.clone();
        for stmt in stmts {
            if let StmtKind::Struct { name, .. } = &stmt.kind {
                let key = self.type_key(&mid, name);
                if self.struct_ctypes.contains_key(&key) {
                    continue; // already cached (a cross-module forward-ref resolved it earlier).
                }
                let c = self.struct_ctype_from_asts(&key, 0);
                self.struct_ctypes.insert(key, c);
            }
        }
    }

    /// Build a by-value `CType::Struct` for the struct under IDENTITY `key` from its raw field ASTs,
    /// mapping each AST field type to its own width-bearing `CType` (so an `int32` field stays 4 bytes
    /// — the layout the C ABI expects) IN THE CURRENT SCOPE. `key` is the identity key (the tag a
    /// returned struct carries, so field lookup hits). `None` if a field isn't a scalar leaf (a
    /// non-marshallable struct — the marshal gate rejects it). The ONLY field-resolving path — only
    /// `populate_struct_ctypes` calls it, in the defining scope, so there is exactly one resolver.
    fn struct_ctype_from_asts(&self, key: &str, depth: usize) -> Option<CType> {
        let fields = self.struct_field_asts.get(key)?;
        let mut cfields = Vec::with_capacity(fields.len());
        let mut field_names = Vec::with_capacity(fields.len());
        for (fname, fty) in fields {
            cfields.push(self.resolve_ctype_d(fty, depth + 1)?);
            field_names.push(fname.clone());
        }
        Some(CType::Struct {
            name: key.to_string(),
            field_names,
            fields: cfields,
        })
    }

    /// Does concrete `ty` satisfy `protocol` instantiated with `args` (the bound's type arguments,
    /// e.g. `[int]` for `Container[int]`)? `args` is empty for a bare protocol. For a parameterized
    /// protocol the structural check substitutes the protocol's type params with `args` before
    /// matching method signatures.
    fn satisfies_args(&self, ty: &Ty, protocol: &str, args: &[Ty]) -> Result<(), String> {
        self.satisfies_args_d(ty, protocol, args, 0)
    }

    /// Depth-bounded core of [`satisfies_args`]. `depth` guards the embed-flattening recursion (M22):
    /// cycles are rejected at declare time, but a malformed cyclic program still runs the rest of the
    /// checker, so a hard cap (mirroring `resolve_ty_ro_d`) breaks the recursion with a plain failure
    /// instead of overflowing the stack.
    fn satisfies_args_d(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        depth: usize,
    ) -> Result<(), String> {
        let Some(pinfo) = self.protocols.get(protocol) else {
            return Err(format!("unknown protocol '{protocol}'"));
        };
        if let Ty::Unknown = ty {
            return Ok(()); // don't cascade
        }
        // M22 — embedded (super-)protocols: a type satisfies `protocol` iff it satisfies every embed
        // (transitively) AND has every OWN method below. A PURE bundle (`Arithmetic` = embeds only,
        // no own methods) short-circuits once its embeds pass — this is what lets int/float/struct
        // satisfy `Arithmetic` (each embed recurses into the intrinsic/structural arms). A `Ty::Param`
        // is NOT flattened here — it forwards through its declared bounds in the `Ty::Param` arm below
        // (which knows, via `bound_provides`, that an `Arithmetic`-bound param provides Add/Sub/…).
        if !pinfo.embeds.is_empty() && !matches!(ty, Ty::Param(_)) {
            if depth <= 64 {
                for emb in &pinfo.embeds {
                    let eargs: Vec<Ty> = emb.args.iter().map(|a| self.resolve_ty_ro(a)).collect();
                    self.satisfies_args_d(ty, &emb.name, &eargs, depth + 1)?;
                }
            }
            if pinfo.methods.is_empty() {
                return Ok(()); // pure bundle — all embeds satisfied, no own methods to check
            }
        }
        if protocol == "Comparable" && matches!(ty, Ty::Int | Ty::Float | Ty::Str) {
            return Ok(());
        }
        // `Hashable` is satisfied intrinsically by the scalar key types (mirrors the map/set key
        // restriction; float is excluded — its equality is a hazard). Struct conformance falls
        // through to the structural check (needs a `hash(self) -> int` method).
        if protocol == "Hashable" && matches!(ty, Ty::Int | Ty::Str | Ty::Bytes | Ty::Bool) {
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
        // `Iterable` conformance is "can produce a fresh cursor". Built-in collections satisfy it
        // intrinsically; ANY `Iterator[T]`-satisfying type satisfies it too (every Iterator IS
        // Iterable — `iter()` returns self), so `iter_elem` (which already covers both) is reused as
        // the predicate. A user struct with a structural `iter(self) -> Iterator[E]` (but no `next`)
        // is caught by the `iterable_elem` helper. The bound's `[T]` arg, if supplied and concrete,
        // must match the element type (mirrors the parameterized-`Index` arg check). A `Ty::Param`
        // falls through to the declared-bounds check below (so `[S: Iterable[T]]` forwards).
        if protocol == "Iterable" && !matches!(ty, Ty::Param(_)) {
            let Some(elem) = self.iterable_elem(ty) else {
                return Err(format!("type {ty} does not satisfy Iterable"));
            };
            if let Some(want) = args.first()
                && !want.is_unknown()
                && !elem.is_unknown()
                && !compatible(want, &elem)
            {
                return Err(format!("type {ty} does not satisfy Iterable"));
            }
            return Ok(());
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
                    if protocol == "IndexSet"
                        && !matches!(ty, Ty::List(_) | Ty::Map(_, _) | Ty::ByteArray)
                    {
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
        // The numeric operator protocols are satisfied intrinsically by int/float (their `+ - * / %`
        // and unary `-` are the primitive ops), so a `[T: Add + Mul]` / `[T: Div]` / `[T: Neg]`
        // generic works over numbers as well as structs.
        if matches!(protocol, "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg")
            && matches!(ty, Ty::Int | Ty::Float)
        {
            return Ok(());
        }
        // A bound type parameter satisfies a protocol if that protocol is among its declared bounds —
        // this is what lets a generic forward its `T: P` value into another `[U: P]` call. For a
        // parameterized protocol the bound's type args must also match the required ones, so a
        // `Container[str]` value is NOT accepted where `Container[int]` is required (forwarding hole).
        if let Ty::Param(name) = ty {
            let matched = self.type_params.get(name).is_some_and(|bs| {
                bs.iter()
                    .any(|b| self.bound_provides(&b.name, &b.args, protocol, args, 0))
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
                self.satisfies_methods(ty, protocol, args, pinfo, &info.methods)
            }
            // Enum conformance is structural exactly like a struct's: the enum satisfies `protocol`
            // iff its `methods` map carries every protocol method with a matching signature. This
            // unlocks Stringable/Hashable/Add/Sub/Mul/Comparable for enums and protocol-bound generics.
            Ty::Enum(ename, _) => {
                let Some(methods) = self.enum_methods.get(ename) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
            }
            // A newtype satisfies a protocol structurally via its OWN methods (like struct/enum).
            // PLUS, when its underlying is numeric, it intrinsically satisfies the operator protocols
            // (`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Comparable`) — its same-type `+`/`<` use the native op
            // (unwrap→op→rewrap), so a `newtype Meters = float` flows into a `[T: Add]` generic with
            // no user `add` method. Hashable/Stringable stay strictly opt-in (the user's own method).
            Ty::NewType(ntkey, _) => {
                // The intrinsic numeric-operator satisfaction is for SCALAR newtypes only. A generic
                // newtype is methods-only — even `newtype Box[T] = T` gets no native Add/Sub/Mul/
                // Comparable; operators come strictly from its own methods (checked below).
                let numeric = !self.newtype_is_generic(ntkey)
                    && self
                        .newtype_underlying(ntkey)
                        .is_some_and(|u| u.is_numeric());
                if numeric
                    && matches!(
                        protocol,
                        "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Comparable"
                    )
                {
                    return Ok(());
                }
                // SOUNDNESS: a newtype operator overload defined as a METHOD is NEVER dispatched at
                // runtime — the same-newtype operator arm (vm `newtype_arith` / `compare_op`, interp
                // `eval_binop`) always auto-flows to the UNDERLYING's native op, and unary `-` has no
                // newtype path at all. So an operator protocol on a newtype is satisfiable ONLY via the
                // numeric auto-flow above; admitting it structurally here would type-check a call that
                // diverges on every engine (`check` ok / `run` silently using the native op, or
                // "cannot apply <Op> to <underlying>"). A non-numeric (or generic) newtype therefore
                // does NOT satisfy these — its own `add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare` method
                // is intentionally unreachable as an operator. `Comparable` is in this list for the
                // same reason: same-newtype `<`/`<=`/`>`/`>=` always uses the underlying's NATIVE
                // ordering (`compare_op`'s `same_newtype_keys` fast path), never the user `compare`, so
                // a generic newtype (the only non-numeric case that reaches here after the numeric
                // short-circuit) must NOT claim `Comparable` via a method — that would be check-ok /
                // run-divergent. (Hashable/Stringable/Iterable/etc. still resolve structurally below.)
                if matches!(
                    protocol,
                    "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg" | "Comparable"
                ) {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                }
                let Some((_, methods)) = self.newtype_defs.get(ntkey) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
            }
            _ => Err(format!("type {ty} does not satisfy {protocol}")),
        }
    }

    /// Structural conformance check shared by the struct and enum arms of [`satisfies_args`]: a type
    /// satisfies `protocol` iff `methods` carries every protocol method with a matching signature
    /// (the protocol's own type params substituted from the bound's `args`; `Self` handled inside
    /// `method_matches`).
    fn satisfies_methods(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        methods: &HashMap<String, FnSig>,
    ) -> Result<(), String> {
        let pmap: HashMap<String, Ty> = pinfo
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        // The RECEIVING type's own param→arg substitution (e.g. `T→int` from `Box[int]`). The user's
        // stored methods carry the struct/enum/newtype's type params UNsubstituted (a method on
        // `Box[T]` is stored as `add(self, o: Box[T]) -> Box[T]`), so for a generic instantiation we
        // must bind those params to the instantiation's args before comparing against the (Self-bound)
        // protocol signature — otherwise `compatible(Box[int], Box[T])` fails and a perfectly good
        // operator method is rejected as "wrong signature". Non-generic types yield an empty map (no
        // params), and checking inside the generic's own def (`ty = Box[T]`) yields an identity map
        // `{T→T}` — both are no-op substitutions, so this only ever binds concrete args.
        let tymap: HashMap<String, Ty> = match ty {
            Ty::Struct(name, targs) => self
                .structs
                .get(name)
                .map(|info| struct_param_map(info, targs))
                .unwrap_or_default(),
            Ty::Enum(name, targs) => self.enum_param_map(name, targs),
            Ty::NewType(key, targs) => self
                .newtype_type_params
                .get(key)
                .map(|tps| {
                    tps.iter()
                        .map(|tp| tp.name.clone())
                        .zip(targs.iter().cloned())
                        .collect()
                })
                .unwrap_or_default(),
            _ => HashMap::new(),
        };
        for (mname, msig) in &pinfo.methods {
            let subst_params: Vec<Ty> = msig.params.iter().map(|t| subst(t, &pmap)).collect();
            let min_params = subst_params.len();
            let want = FnSig {
                params: subst_params,
                ret: subst(&msig.ret, &pmap),
                type_params: Vec::new(),
                min_params,
                is_static: false,
                doc: None,
            };
            let msig = &want;
            // Pre-substitute the receiving type's params into the ACTUAL (user) method signature so
            // its `Box[T]` becomes `Box[int]` before the comparison. Only the actual side is bound —
            // a genuinely wrong sig (`add(self, o: int) -> int`) stays a mismatch, no laundering.
            let actual_owned = methods.get(mname).map(|actual| {
                if tymap.is_empty() {
                    actual.clone()
                } else {
                    FnSig {
                        params: actual.params.iter().map(|t| subst(t, &tymap)).collect(),
                        ret: subst(&actual.ret, &tymap),
                        ..actual.clone()
                    }
                }
            });
            match actual_owned.as_ref() {
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

    /// Result type of an overloaded arithmetic operator (`+`/`-`/`*`) on two operands of the *same*
    /// struct or type-parameter that satisfies `protocol` (`Add`/`Sub`/`Mul`). The runtime dispatches
    /// to the `add`/`sub`/`mul` method; the result type is that same type. `None` ⇒ not overloadable.
    fn op_overload_result(&self, l: &Ty, r: &Ty, protocol: &str) -> Option<Ty> {
        // A SAME newtype with a NUMERIC underlying auto-applies the underlying's NATIVE arithmetic op
        // (unwrap→op→rewrap, NOT a user `add`) and returns the newtype. `Meters + float` /
        // `Meters + Seconds` don't match (different/non-newtype operands) → the caller's "cannot
        // apply" error. (A user-defined `add` method also works via the satisfies() path below, but
        // the native numeric op is the no-method common case.)
        if let (Ty::NewType(a, _), Ty::NewType(b, _)) = (l, r)
            && a == b
            && !self.newtype_is_generic(a)
            && self.newtype_underlying(a).is_some_and(|u| u.is_numeric())
        {
            return Some(l.clone());
        }
        // SAME generic type REQUIRES matching type ARGS, not just the same name. The user's operator
        // method `add(self, o: Box[T]) -> Box[T]` on a `Box[int]` receiver needs `o: Box[int]`, so a
        // heterogeneous pair like `Box[int] + Box[str]` must NOT overload — admitting it would infer
        // the result `Box[int]` for a value carrying a `Box[str]` (the returned type the checker can't
        // honor → runtime type confusion). `compatible` checks name + pairwise-compatible targs; an
        // `Unknown` targ on a partially-inferred side still unifies (no false rejection there).
        let same = match (l, r) {
            (Ty::Struct(..), Ty::Struct(..))
            | (Ty::Enum(..), Ty::Enum(..))
            | (Ty::NewType(..), Ty::NewType(..)) => compatible(l, r),
            (Ty::Param(a), Ty::Param(b)) => a == b,
            _ => false,
        };
        if same && self.satisfies(l, protocol).is_ok() {
            Some(l.clone())
        } else {
            None
        }
    }

    /// The resolved underlying `Ty` of a newtype (by runtime key), if known.
    fn newtype_underlying(&self, key: &str) -> Option<Ty> {
        self.newtype_defs.get(key).map(|(u, _)| u.clone())
    }

    /// Is the newtype (by runtime key) type-parameterized? A generic newtype is METHODS-ONLY — it
    /// gets no native operator auto-flow (even over a numeric/`T` underlying); operators come strictly
    /// from its own methods + protocol satisfaction. Gates the scalar-newtype auto-flow paths.
    fn newtype_is_generic(&self, key: &str) -> bool {
        self.newtype_type_params
            .get(key)
            .is_some_and(|t| !t.is_empty())
    }

    /// Are `l < r` etc. allowed? True for same-named comparable type params, or same-named structs
    /// that satisfy `Comparable` (operator overloading dispatches to their `compare` at runtime).
    fn ordering_allowed(&self, l: &Ty, r: &Ty) -> bool {
        match (l, r) {
            (Ty::Param(a), Ty::Param(b)) if a == b => self.type_params.get(a).is_some_and(|bs| {
                bs.iter()
                    .any(|proto| self.protocol_has_method(&proto.name, "compare"))
            }),
            // Same generic struct/enum REQUIRES matching type ARGS (`compatible` = name + targs), not
            // just the same name — `Box[int] < Box[str]` must not overload `compare` (same
            // heterogeneous laundering as `+`; see `op_overload_result`).
            (Ty::Struct(..), Ty::Struct(..)) if compatible(l, r) => {
                self.satisfies(l, "Comparable").is_ok()
            }
            (Ty::Enum(..), Ty::Enum(..)) if compatible(l, r) => {
                self.satisfies(l, "Comparable").is_ok()
            }
            // Same SCALAR newtype with a numeric underlying: `Meters < Meters` uses the underlying's
            // native ordering (returns bool). A user `compare` method also enables it via satisfies()
            // (the only path for a generic newtype — methods-only, no native ordering auto-flow).
            (Ty::NewType(a, _), Ty::NewType(b, _)) if a == b => {
                (!self.newtype_is_generic(a)
                    && self.newtype_underlying(a).is_some_and(|u| u.is_numeric()))
                    || self.satisfies(l, "Comparable").is_ok()
            }
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
            // `bytearray` crosses by deep copy (a fresh independent buffer on the other side, like
            // `list`) — always sendable (its elements are always `int`).
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Unknown
            | Ty::Param(_) => true,
            // A `Shared[T]` handle always crosses — that's its whole point (one box, many tasks);
            // its element type is *not* a constraint (the value never crosses, only the handle).
            Ty::Shared(_) => true,
            // A `RwShared[T]` handle crosses for the same reason as `Shared` — one box, many tasks;
            // the element type is not a constraint (only the handle crosses).
            Ty::RwShared(_) => true,
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
            // An opaque `ptr` is a plain raw address (a `usize`) — it crosses the airlock by value,
            // so it is always sendable (its referent, if any, is the foreign library's concern).
            Ty::Ptr => true,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Channel(t) => {
                self.sendable_rec(t, stack)
            }
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
            // A newtype is sendable iff its underlying type is (it crosses by deep-copy of the inner
            // value, like a 1-field struct). Cycle-guarded by the newtype key.
            Ty::NewType(name, _) => {
                if stack.contains(name) {
                    return true;
                }
                // Substitute the newtype's instantiated type args into its underlying, so a generic
                // `Stack[int]` checks `list[int]` (not bare `list[T]`) for sendability.
                match self.newtype_unwrap_target(ty) {
                    Some(under) => {
                        stack.push(name.clone());
                        let ok = self.sendable_rec(&under, stack);
                        stack.pop();
                        ok
                    }
                    None => true,
                }
            }
        }
    }

    fn name_is_generic(&self, name: &str) -> bool {
        // The built-in `Channel[T]()` constructor takes its element type as an explicit type arg.
        // The container constructors `List[T]()` / `Set[T]()` / `Map[K, V]()` likewise accept a
        // turbofish that pins the (otherwise un-inferable, for an empty container) element type.
        if matches!(name, "Channel" | "List" | "Set" | "Map") {
            return true;
        }
        // Look the struct up by its module-scoped runtime key (`bare_key`), NOT the bare name: under
        // the real `check_graph` path `structs` is keyed `<module>::Name`, so a bare-name lookup
        // always misses and wrongly reports user generic structs as non-generic — which made the
        // `infer_named_call` gate reject explicit call-site type args (`Pair[int, str](…)`) that the
        // struct-ctor branch fully supports. Mirrors the newtype arm below + the ctor branch itself.
        if let Some(i) = self.structs.get(&self.bare_key(name)) {
            return !i.type_params.is_empty();
        }
        // A generic newtype constructor takes turbofish type args (`Stack[int]([])`) — report it so
        // the args aren't pre-rejected by `infer_named_call`'s non-generic gate.
        if self.newtype_names.contains(name) && self.newtype_is_generic(&self.bare_key(name)) {
            return true;
        }
        // Bare-name query (the qualified path resolves genericity directly). A variant name may now
        // belong to several enums; treat it as generic if any owner enum is generic. `variant_owners`
        // stores BARE enum names but `enum_type_params` is module-keyed, so go through `bare_key`
        // (same keying fix as the struct/newtype arms above — else a bare generic-enum variant like
        // `Full[int](5)` reports the misleading "takes no type arguments" instead of the
        // "write it qualified as 'Box.Full'" hint under the real `check_graph` path).
        if let Some(owners) = self.variant_owners.get(name) {
            return owners.iter().any(|en| {
                self.enum_type_params
                    .get(&self.bare_key(en))
                    .is_some_and(|t| !t.is_empty())
            });
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
                    format!(
                        "'{name}' expects {} type argument(s), found {}",
                        tps.len(),
                        targs.len()
                    ),
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
                    if !pinned.is_unknown() && !elem.is_unknown() && !self.assignable(&pinned, elem)
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
            let Some(concrete) = sub.get(&tp.name).cloned() else {
                continue;
            };
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
                            format!(
                                "index type {recovered} does not match the declared type {pinned}"
                            ),
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
        hint: Option<&Ty>,
    ) -> Ty {
        if args.len() != sig.params.len() {
            self.check_arity(name, sig.params.len(), args, span);
        }
        let arg_tys = self.infer_generic_arg_tys(args);
        // Explicit call-site type arguments (`max[int](…)`) seed the substitution; remaining (or
        // all, when none given) parameters are inferred from positional arguments. `unify` only
        // binds a parameter that isn't already in the map, so explicit args take precedence and a
        // conflicting argument is caught by the per-argument check below.
        let mut subst_map: HashMap<String, Ty> =
            self.seed_targs(name, &sig.type_params, targs, span);
        for (decl, actual) in sig.params.iter().zip(&arg_tys) {
            unify(decl, actual, &mut subst_map);
        }
        // Recover element types from parameterized `Iterator[T]` bounds (bind `T` to the iterand's
        // element), then enforce every declared bound against its inferred binding.
        self.recover_iter_elems(&sig.type_params, &mut subst_map, span);
        self.recover_index_args(&sig.type_params, &mut subst_map, span);
        // Expected-type checking-mode: a `let`/return/param annotation seeds any type param the args
        // left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing) against the
        // hint — so `xs: List[int] = empty()` pins a return-only `T`, and the deadlock probe below
        // sees it bound. After arg-unification ⇒ turbofish/args win.
        seed_from_hint(hint, &sig.ret, &mut subst_map);
        // Same un-inferable closure-param deadlock guard as the struct-ctor path: report the cause
        // (and bind the params to Unknown) before the per-arg closure body is checked.
        self.report_uninferable_closure_params(
            name,
            &sig.type_params,
            &sig.params,
            args,
            &mut subst_map,
            span,
        );
        self.enforce_bounds(&sig.type_params, &subst_map, span);
        // Each argument must match its parameter's substituted type (catches a type param used in
        // two positions with conflicting types, e.g. `max(1, "x")`).
        for (decl, (actual, arg)) in sig.params.iter().zip(arg_tys.iter().zip(args)) {
            let expected = subst(decl, &subst_map);
            self.check_generic_arg(name, &expected, actual, arg);
        }
        subst(&sig.ret, &subst_map)
    }

    /// Infer a generic *method*'s own type parameters from the call arguments. `params`/`ret` are the
    /// method signature already substituted with the receiver struct's type arguments, so only the
    /// method's own `[U]` params remain free; `params[0]` is the receiver (bound from `obj`, not an
    /// explicit arg). `targs` are the EXPLICIT member-level turbofish (`obj.method[A, B](...)`): they
    /// seed the `[U]` params first; the rest are inferred positionally. Mirrors `infer_generic_call`.
    #[allow(clippy::too_many_arguments)] // the method's resolved signature pieces + receiver + targs + call
    fn infer_generic_method(
        &mut self,
        method: &str,
        params: &[Ty],
        ret: &Ty,
        mtps: &[TypeParam],
        recv_ty: &Ty,
        targs: &[Ty],
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
                format!(
                    "'{method}' expects {} argument(s), got {}",
                    expected.len(),
                    args.len()
                ),
            );
        }
        let arg_tys = self.infer_generic_arg_tys(args);
        // Explicit member-level turbofish seeds the `[U]` params (arity-checked); `unify` only binds
        // a param not already in the map, so an explicit targ wins and a conflicting arg is caught by
        // the per-argument check below.
        let mut mmap: HashMap<String, Ty> = self.seed_targs(method, mtps, targs, span);
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
        // The receiver must still match its declared type AFTER substitution. Without this, a method
        // type param in receiver position (`fn m[U](self: U)`) turbofished to a contradicting type
        // (`b.m[str]()` on a `Box[int]`) is unchecked — `unify` silently drops the conflict once `[str]`
        // is seeded — and a wrong static type escapes onto the value (a soundness hole).
        let want_recv = subst(receiver, &mmap);
        if !self.assignable(&want_recv, recv_ty) {
            self.error(
                span,
                format!("receiver of '{method}' has type {recv_ty}, expected {want_recv}"),
            );
        }
        for (decl, (actual, arg)) in expected.iter().zip(arg_tys.iter().zip(args)) {
            let want = subst(decl, &mmap);
            self.check_generic_arg(method, &want, actual, arg);
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

/// Render a resolved type-arg list for a user-facing redirect hint (`int, str`), used by the
/// removed-gliding-form error to suggest the type-side form `Enum[int, str].Variant(...)`.
fn render_targs(targs: &[Ty]) -> String {
    targs
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ")
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
            StmtKind::If {
                branches,
                else_block,
            } => {
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
            | StmtKind::Defer(DeferTarget::Call(e)) => collect_free_calls_expr(e, fns, scopes, out),
            StmtKind::Defer(DeferTarget::Block(body)) => {
                collect_free_calls_block(body, fns, scopes, out)
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, b) in branches {
                    collect_free_calls_expr(c, fns, scopes, out);
                    collect_free_calls_block(b, fns, scopes, out);
                }
                if let Some(b) = else_block {
                    collect_free_calls_block(b, fns, scopes, out);
                }
            }
            StmtKind::For {
                vars, iter, body, ..
            } => {
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
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
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
        ExprKind::List(xs) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => xs
            .iter()
            .for_each(|x| collect_free_calls_expr(x, fns, scopes, out)),
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            collect_free_calls_expr(k, fns, scopes, out);
            collect_free_calls_expr(v, fns, scopes, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            // Clauses nest: each clause's iter is in the scope of earlier clauses; its vars then
            // bind for everything after. Push one scope per clause and pop them all at the end.
            for clause in clauses {
                collect_free_calls_expr(&clause.iter, fns, scopes, out);
                scopes.push(clause.vars.iter().cloned().collect());
                for g in &clause.guards {
                    collect_free_calls_expr(g, fns, scopes, out);
                }
            }
            if let Some(k) = key {
                collect_free_calls_expr(k, fns, scopes, out);
            }
            collect_free_calls_expr(elem, fns, scopes, out);
            for _ in clauses {
                scopes.pop();
            }
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
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            collect_free_calls_expr(obj, fns, scopes, out);
            for c in [start, end, step].into_iter().flatten() {
                collect_free_calls_expr(c, fns, scopes, out);
            }
        }
        ExprKind::Try(inner) => collect_free_calls_expr(inner, fns, scopes, out),
        ExprKind::OptChain { obj, call, .. } => {
            collect_free_calls_expr(obj, fns, scopes, out);
            if let Some(c) = call {
                c.args
                    .iter()
                    .for_each(|a| collect_free_calls_expr(a, fns, scopes, out));
                c.named
                    .iter()
                    .for_each(|(_, a)| collect_free_calls_expr(a, fns, scopes, out));
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
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        // A type-application head names a type and holds only `Type`s — no free-fn calls.
        | ExprKind::TypeApply { .. }
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
            StmtKind::Assign {
                target,
                value,
                op: _,
            } => {
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
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, b) in branches {
                    find_mutations_in_expr(c, globals, scopes, out);
                    find_global_mutations(b, globals, scopes, out);
                }
                if let Some(b) = else_block {
                    find_global_mutations(b, globals, scopes, out);
                }
            }
            StmtKind::For {
                vars, iter, body, ..
            } => {
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
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            // Clauses nest: each clause's iter is in the scope of earlier clauses; its vars then
            // bind for everything after. Push one scope per clause and pop them all at the end.
            for clause in clauses {
                find_mutations_in_expr(&clause.iter, globals, scopes, out);
                scopes.push(clause.vars.iter().cloned().collect());
                for g in &clause.guards {
                    find_mutations_in_expr(g, globals, scopes, out);
                }
            }
            if let Some(k) = key {
                find_mutations_in_expr(k, globals, scopes, out);
            }
            find_mutations_in_expr(elem, globals, scopes, out);
            for _ in clauses {
                scopes.pop();
            }
        }
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
            find_mutations_in_expr(callee, globals, scopes, out);
            for a in args {
                find_mutations_in_expr(a, globals, scopes, out);
            }
            for (_, a) in named {
                find_mutations_in_expr(a, globals, scopes, out);
            }
        }
        ExprKind::List(xs) | ExprKind::Tuple(xs) | ExprKind::Set(xs) => xs
            .iter()
            .for_each(|x| find_mutations_in_expr(x, globals, scopes, out)),
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
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            find_mutations_in_expr(obj, globals, scopes, out);
            for c in [start, end, step].into_iter().flatten() {
                find_mutations_in_expr(c, globals, scopes, out);
            }
        }
        ExprKind::Try(inner) => find_mutations_in_expr(inner, globals, scopes, out),
        ExprKind::OptChain { obj, call, .. } => {
            find_mutations_in_expr(obj, globals, scopes, out);
            if let Some(c) = call {
                c.args
                    .iter()
                    .for_each(|a| find_mutations_in_expr(a, globals, scopes, out));
                c.named
                    .iter()
                    .for_each(|(_, a)| find_mutations_in_expr(a, globals, scopes, out));
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
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        // A type-application head holds only `Type`s — no global mutations.
        | ExprKind::TypeApply { .. }
        | ExprKind::Ident(_) => {}
    }
}

/// The names a match pattern binds (variant payload slots, tuple elements, sub-bindings).
fn pattern_bindings(p: &Pattern) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn go(p: &Pattern, out: &mut std::collections::HashSet<String>) {
        match p {
            Pattern::Ident(n, _) => {
                out.insert(n.clone());
            }
            Pattern::Variant { bindings, .. }
            | Pattern::Tuple(bindings)
            | Pattern::Or(bindings) => bindings.iter().for_each(|b| go(b, out)),
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
        }
    }
    go(p, &mut out);
    out
}

/// The first identifier bound more than once WITHIN a single pattern (Rust's rule), or `None`. Walks
/// like [`pattern_bindings`] but COUNTS instead of dedup'ing: `_` (wildcard) binds nothing and is
/// skipped; literals/ranges bind nothing. Or-alternatives are NOT descended — each `A(x) | B(x)`
/// alternative is its own binding context (consistency across alts is governed elsewhere); a
/// duplicate inside one alternative is caught when `bind_match_arm` recurses on that alternative.
/// `is_binder(name)` returns `true` iff a bare `Pattern::Ident(name)` actually BINDS a fresh variable
/// here, rather than naming a (refutable) nullary variant. A bare `None`/`Ok`/`Err`/`Some` or a user
/// variant name binds nothing (it's a variant test — see `bind_subpattern`), so two of them in one
/// pattern (e.g. `(None, None, None)`) is NOT a duplicate binding. Mirrors `bind_subpattern`'s
/// Ident-as-variant recognition so this pre-pass doesn't falsely reject correct code.
fn first_duplicate_binder(p: &Pattern, is_binder: &impl Fn(&str) -> bool) -> Option<String> {
    fn go(
        p: &Pattern,
        seen: &mut std::collections::HashSet<String>,
        is_binder: &impl Fn(&str) -> bool,
    ) -> Option<String> {
        match p {
            Pattern::Ident(n, _) => {
                if !is_binder(n) {
                    return None;
                }
                if !seen.insert(n.clone()) {
                    return Some(n.clone());
                }
                None
            }
            Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) => {
                for b in bindings {
                    if let Some(dup) = go(b, seen, is_binder) {
                        return Some(dup);
                    }
                }
                None
            }
            // An or-pattern's alternatives are separate binding contexts, so a name reused ACROSS
            // alternatives (`A(x) | B(x)`) is NOT a duplicate. But (a) each alternative must itself
            // be duplicate-free, and (b) the names an or-pattern binds STILL collide with binders
            // OUTSIDE the or (`(x, A(x) | B(x))` binds `x` twice on a matching path). So check each
            // alternative on its own, then merge the or's binder set ONCE into the outer `seen`.
            Pattern::Or(alts) => {
                let mut or_binds = std::collections::BTreeSet::new();
                for alt in alts {
                    let mut alt_seen = std::collections::HashSet::new();
                    if let Some(dup) = go(alt, &mut alt_seen, is_binder) {
                        return Some(dup);
                    }
                    or_binds.extend(alt_seen);
                }
                for n in or_binds {
                    if !seen.insert(n.clone()) {
                        return Some(n);
                    }
                }
                None
            }
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => None,
        }
    }
    let mut seen = std::collections::HashSet::new();
    go(p, &mut seen, is_binder)
}

/// The prebuilt protocols every program starts with. `Comparable` requires
/// `compare(self, other: Self) -> int`; primitives (int/float/str) satisfy it intrinsically.
/// M22 — structural equality of two protocol method signatures (params + return + arity), used to
/// dedup identical embed-pulled methods (the legal diamond) vs. flag a conflicting-signature embed.
fn fn_sig_eq(a: &FnSig, b: &FnSig) -> bool {
    a.params == b.params && a.ret == b.ret && a.min_params == b.min_params
}

fn prebuilt_protocols() -> HashMap<String, ProtocolInfo> {
    let mut m = HashMap::new();
    m.insert(
        "Comparable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
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
            embeds: Vec::new(),
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
            embeds: Vec::new(),
            methods: vec![(
                "message".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Str),
            )],
        },
    );
    m.insert(
        "Hashable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
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
    // Per-operator numeric protocols (M10-G3, M22): a struct satisfying `Add`/`Sub`/`Mul`/`Div`/`Mod`
    // (method `add`/`sub`/`mul`/`div`/`mod`(self, other: Self) -> Self) overloads `+`/`-`/`*`/`/`/`%`.
    // `Self` for `other` and the return makes them binary same-type operators (mirrors `Comparable`'s
    // `compare`).
    for (proto, method) in [
        ("Add", "add"),
        ("Sub", "sub"),
        ("Mul", "mul"),
        ("Div", "div"),
        ("Mod", "mod"),
    ] {
        m.insert(
            proto.to_string(),
            ProtocolInfo {
                type_params: Vec::new(),
                embeds: Vec::new(),
                methods: vec![(
                    method.to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("Self".into())],
                        Ty::Param("Self".into()),
                    ),
                )],
            },
        );
    }
    // `Neg` (M22) — the UNARY `-` protocol: a struct/type-param satisfying `Neg` (method
    // `neg(self) -> Self`, one param `self` only, no `other`) overloads unary `-`. int/float satisfy
    // it intrinsically (their native negation). Mirrors `Stringable`'s single-`self`-param shape.
    m.insert(
        "Neg".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "neg".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Param("Self".into())),
            )],
        },
    );
    // `Arithmetic` (M22) — a builtin protocol BUNDLE: embeds `Add + Sub + Mul + Div` and adds no own
    // methods. `[T: Arithmetic]` flattens to "has add/sub/mul/div", so int/float and any 4-op struct
    // satisfy it. Built with the SAME `embeds` field user bundles use — no special-casing of builtins.
    m.insert(
        "Arithmetic".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: vec![
                Bound {
                    name: "Add".to_string(),
                    args: Vec::new(),
                },
                Bound {
                    name: "Sub".to_string(),
                    args: Vec::new(),
                },
                Bound {
                    name: "Mul".to_string(),
                    args: Vec::new(),
                },
                Bound {
                    name: "Div".to_string(),
                    args: Vec::new(),
                },
            ],
            methods: Vec::new(),
        },
    );
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
            embeds: Vec::new(),
            methods: vec![(
                "next".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown],
                    Ty::Option(Box::new(Ty::Param("Self".into()))),
                ),
            )],
        },
    );
    // `Iterable[T]` — "can produce a fresh cursor". The method shape mirrors the structural detection
    // (`iter(self) -> Iterator[T]`); conformance is decided in `satisfies_args` (built-in collections
    // + any `Iterator[T]` intrinsically — every Iterator IS Iterable via `iter() == self` — plus a
    // user struct with a structural `iter`), NOT the generic structural loop, and the bound's `[T]`
    // recovers the element type at call sites (`infer_method_call`'s `Iterable.iter` special-case).
    // Distinct from `Iterator[T]`: an `Iterable` only promises a cursor; an `Iterator` also has `next`.
    m.insert(
        "Iterable".to_string(),
        ProtocolInfo {
            type_params: vec!["Elem".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "iter".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown],
                    Ty::Struct("Iterator".to_string(), vec![Ty::Param("Elem".into())]),
                ),
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
            embeds: Vec::new(),
            methods: vec![(
                "index".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown, Ty::Param("K".into())],
                    Ty::Param("V".into()),
                ),
            )],
        },
    );
    m.insert(
        "IndexSet".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            embeds: Vec::new(),
            methods: vec![
                (
                    "index".to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("K".into())],
                        Ty::Param("V".into()),
                    ),
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
            embeds: Vec::new(),
            methods: vec![(
                "slice".to_string(),
                // Python-style: three `Option[int]` components (start/end/step), each `None` when
                // omitted. Both engines always pass all three explicitly.
                FnSig::plain(
                    vec![
                        Ty::Unknown,
                        Ty::option(Ty::Int),
                        Ty::option(Ty::Int),
                        Ty::option(Ty::Int),
                    ],
                    Ty::Param("R".into()),
                ),
            )],
        },
    );
    m
}

/// The substitution from a struct's type parameters to a concrete instantiation's type arguments
/// (`Stack[int]` ⇒ `{T: int}`). Empty for a non-generic struct.
fn struct_param_map(info: &StructInfo, targs: &[Ty]) -> HashMap<String, Ty> {
    info.type_params
        .iter()
        .map(|tp| tp.name.clone())
        .zip(targs.iter().cloned())
        .collect()
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

/// True iff `t` is a COMPOUND type whose recursive structure contains a `Ty::Unknown` anywhere in a
/// type-argument / element / key / value position. A bare top-level `Ty::Unknown` returns FALSE —
/// that is the cascade-suppression sentinel (a real type error already happened, or a permissive
/// receiver); refining it would fight cascade-suppression. Drives the refine-on-first-use gate: we
/// only narrow a binding whose empty-slot Unknown is reachable, never the bare sentinel.
fn contains_unknown_in_slot(t: &Ty) -> bool {
    fn has_unknown(t: &Ty) -> bool {
        match t {
            Ty::Unknown => true,
            Ty::List(x)
            | Ty::Option(x)
            | Ty::Set(x)
            | Ty::Channel(x)
            | Ty::Shared(x)
            | Ty::RwShared(x)
            | Ty::Atomic(x) => has_unknown(x),
            Ty::Map(k, v) => has_unknown(k) || has_unknown(v),
            Ty::Result(a, b) => has_unknown(a) || has_unknown(b),
            Ty::Struct(_, a) | Ty::Enum(_, a) => a.iter().any(has_unknown),
            Ty::Tuple(ts) => ts.iter().any(has_unknown),
            _ => false,
        }
    }
    match t {
        Ty::Unknown => false, // bare sentinel — never refine
        _ => has_unknown(t),
    }
}

/// Structural merge for refine-on-first-use: fill `Ty::Unknown` slots in `a` with the corresponding
/// concrete slot from `shape`, recursing to arbitrary depth (so `list[Option[Box[int]]]` fills in a
/// single merge). A bare `Unknown` in `a` becomes `shape` (when `shape` is concrete). For matching
/// compounds (List/Set/Option/Channel/Shared/Atomic ×1, Map/Result ×2, Tuple ×n, Struct/Enum by
/// NAME + arity) it recurses pairwise. On a shape-NAME or arity mismatch (e.g. pushing a different
/// generic enum) it leaves `a` unchanged — no refine — so the normal `check_args` mismatch fires.
fn merge_unknown(a: &Ty, shape: &Ty) -> Ty {
    use Ty::*;
    if shape.is_unknown() {
        return a.clone();
    }
    if a.is_unknown() {
        return shape.clone();
    }
    match (a, shape) {
        (List(ae), List(se)) => List(Box::new(merge_unknown(ae, se))),
        (Set(ae), Set(se)) => Set(Box::new(merge_unknown(ae, se))),
        (Option(ae), Option(se)) => Option(Box::new(merge_unknown(ae, se))),
        (Channel(ae), Channel(se)) => Channel(Box::new(merge_unknown(ae, se))),
        (Shared(ae), Shared(se)) => Shared(Box::new(merge_unknown(ae, se))),
        (RwShared(ae), RwShared(se)) => RwShared(Box::new(merge_unknown(ae, se))),
        (Atomic(ae), Atomic(se)) => Atomic(Box::new(merge_unknown(ae, se))),
        (Map(ak, av), Map(sk, sv)) => Map(
            Box::new(merge_unknown(ak, sk)),
            Box::new(merge_unknown(av, sv)),
        ),
        (Result(at, ae), Result(st, se)) => Result(
            Box::new(merge_unknown(at, st)),
            Box::new(merge_unknown(ae, se)),
        ),
        (Tuple(ats), Tuple(sts)) if ats.len() == sts.len() => Tuple(
            ats.iter()
                .zip(sts)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (Struct(an, aa), Struct(sn, sa)) if an == sn && aa.len() == sa.len() => Struct(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (Enum(an, aa), Enum(sn, sa)) if an == sn && aa.len() == sa.len() => Enum(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (NewType(an, aa), NewType(sn, sa)) if an == sn && aa.len() == sa.len() => NewType(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        // Shape/name/arity mismatch: leave `a` unchanged (no refine — normal mismatch fires later).
        _ => a.clone(),
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
        Ty::NewType(n, args) => {
            Ty::NewType(n.clone(), args.iter().map(|t| subst(t, map)).collect())
        }
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
/// The `Ty::Param` vector for a list of type parameters — the type-arg slots of a generic type's
/// declared SHAPE (`Struct(key, [Param(T), …])`). Used to build the expected-type-hint unify target.
fn param_shape(tps: &[TypeParam]) -> Vec<Ty> {
    tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect()
}

/// Pre-seed a generic ctor/call's type-param substitution `sub` from an expected-type `hint` (a
/// `let`/return/parameter annotation). `shape` is the ctor/call's declared return type in terms of its
/// OWN params (e.g. `Struct(key, [Param(T)])`, or a generic fn's `sig.ret`). Unify is a no-op on a
/// key/shape mismatch and only binds a param still FREE in `sub`, so this runs AFTER arg-unification
/// to give precedence turbofish > args > annotation — it fills only the params the arguments left
/// unbound (the genuine deadlock, e.g. `Heap([], fn(x, y): x < y)` annotated `Heap[int]`).
fn seed_from_hint(hint: Option<&Ty>, shape: &Ty, sub: &mut HashMap<String, Ty>) {
    if let Some(e) = hint {
        unify(shape, e, sub);
    }
}

fn unify(decl: &Ty, actual: &Ty, map: &mut HashMap<String, Ty>) {
    match (decl, actual) {
        (Ty::Param(n), a) => {
            if !a.is_unknown() && !map.contains_key(n) {
                map.insert(n.clone(), a.clone());
            }
        }
        (Ty::List(d), Ty::List(a)) | (Ty::Set(d), Ty::Set(a)) | (Ty::Option(d), Ty::Option(a)) => {
            unify(d, a, map)
        }
        (Ty::Result(dt, de), Ty::Result(at, ae)) => {
            unify(dt, at, map);
            unify(de, ae, map);
        }
        (Ty::Map(dk, dv), Ty::Map(ak, av)) => {
            unify(dk, ak, map);
            unify(dv, av, map);
        }
        (Ty::Struct(dn, da), Ty::Struct(an, aa))
        | (Ty::Enum(dn, da), Ty::Enum(an, aa))
        | (Ty::NewType(dn, da), Ty::NewType(an, aa))
            if dn == an && da.len() == aa.len() =>
        {
            da.iter().zip(aa).for_each(|(d, a)| unify(d, a, map));
        }
        (Ty::Tuple(ds), Ty::Tuple(as_)) if ds.len() == as_.len() => {
            ds.iter().zip(as_).for_each(|(d, a)| unify(d, a, map));
        }
        (
            Ty::Func {
                params: dp,
                ret: dr,
            },
            Ty::Func {
                params: ap,
                ret: ar,
            },
        ) if dp.len() == ap.len() => {
            dp.iter().zip(ap).for_each(|(d, a)| unify(d, a, map));
            unify(dr, ar, map);
        }
        _ => {}
    }
}

/// Collect (into `out`, dedup, in first-seen order) the names from `wanted` that appear as a
/// `Ty::Param` anywhere inside `ty`. Used by the un-inferable-closure-param deadlock diagnostic to
/// find which still-unbound type parameters a closure-typed slot mentions.
fn ty_collect_params(ty: &Ty, wanted: &std::collections::HashSet<String>, out: &mut Vec<String>) {
    match ty {
        Ty::Param(n) => {
            if wanted.contains(n) && !out.contains(n) {
                out.push(n.clone());
            }
        }
        Ty::List(e)
        | Ty::Set(e)
        | Ty::Option(e)
        | Ty::Channel(e)
        | Ty::Shared(e)
        | Ty::Atomic(e)
        | Ty::RwShared(e) => ty_collect_params(e, wanted, out),
        Ty::Map(a, b) | Ty::Result(a, b) => {
            ty_collect_params(a, wanted, out);
            ty_collect_params(b, wanted, out);
        }
        Ty::Tuple(parts) | Ty::Struct(_, parts) | Ty::Enum(_, parts) | Ty::NewType(_, parts) => {
            for p in parts {
                ty_collect_params(p, wanted, out);
            }
        }
        Ty::Func { params, ret } => {
            for p in params {
                ty_collect_params(p, wanted, out);
            }
            ty_collect_params(ret, wanted, out);
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

/// Whether a match-arm pattern introduces a binding of `name` (so a free-closure body scan must treat
/// `name` as shadowed inside that arm — see [`Checker::scan_expr_for_pin`]). Covers a tuple/variant
/// sub-position `Ident` binding, an or-pattern alternative (all alternatives bind the same set), and a
/// top-level bare catch-all of the same spelling (parsed as a nullary `Variant`).
fn pattern_binds(p: &Pattern, name: &str) -> bool {
    match p {
        Pattern::Ident(s, _) => s == name,
        // A nullary bare-name pattern (`Variant{ bindings: [] }`) is a catch-all binding when it
        // names neither a qualified variant nor a payload — scope-conservatively, a same-spelling one
        // shadows. A payload variant's bindings are sub-positions; recurse.
        Pattern::Variant {
            name: vn, bindings, ..
        } => (bindings.is_empty() && vn == name) || bindings.iter().any(|b| pattern_binds(b, name)),
        Pattern::Tuple(subs) | Pattern::Or(subs) => subs.iter().any(|s| pattern_binds(s, name)),
        Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => false,
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
        In => "in",
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
            StmtKind::If {
                branches,
                else_block,
            } => {
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

// Editor hover (Tier C): authored method-NAME lists per built-in type, rendered by `builtin_type_doc`
// as a "methods: a, b, c" line. Each slice is drift-guarded by `builtin_method_slices_all_resolve`
// (every name MUST resolve from its `*_method_sig`), so the hover can only advertise methods that
// provably exist. `list.sort` and `bytes`/`bytearray.extend` live in `infer_method_call` (not the
// `*_method_sig` tables), so they are intentionally absent here — see PROGRESS.md.
const STR_METHODS: &[&str] = &[
    "len",
    "upper",
    "lower",
    "trim",
    "strip",
    "split",
    "split_lines",
    "chars",
    "join",
    "starts_with",
    "ends_with",
    "contains",
    "replace",
    "repeat",
    "reverse",
    "pad_left",
    "index_of",
    "count",
    "strip_prefix",
    "strip_suffix",
    "to_int",
    "to_float",
    "encode",
];
const LIST_METHODS: &[&str] = &[
    "len", "push", "pop", "reverse", "contains", "index_of", "concat", "extend", "sum",
];
const MAP_METHODS: &[&str] = &[
    "len", "has", "get", "keys", "values", "remove", "merge", "update",
];
const SET_METHODS: &[&str] = &[
    "len",
    "has",
    "add",
    "remove",
    "union",
    "intersection",
    "difference",
];
const CHANNEL_METHODS: &[&str] = &[
    "send", "try_send", "recv", "try_recv", "close", "trip", "len",
];
const SHARED_METHODS: &[&str] = &["get", "set", "update"];
const RWSHARED_METHODS: &[&str] = &["get", "set", "read", "write"];
const ATOMIC_METHODS: &[&str] = &["load", "store", "exchange", "cas", "add", "sub"];
const SOCKET_METHODS: &[&str] = &["read", "write", "close"];
const LISTENER_METHODS: &[&str] = &["accept", "addr", "close"];
const EXECUTOR_METHODS: &[&str] = &["submit", "shutdown", "shutdown_now"];
const BYTES_METHODS: &[&str] = &["decode", "len"];
const BYTEARRAY_METHODS: &[&str] = &["len", "push", "pop", "decode"];

fn str_method_sig(method: &str) -> Option<FnSig> {
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "upper" | "lower" | "trim" => (vec![], Ty::Str),
        // `str` conforms to `Error`: `message()` returns the string itself.
        "message" => (vec![], Ty::Str),
        "split" => (vec![Ty::Str], Ty::list(Ty::Str)),
        "chars" => (vec![], Ty::list(Ty::Str)),
        "join" => (vec![Ty::list(Ty::Str)], Ty::Str),
        "starts_with" | "contains" | "ends_with" => (vec![Ty::Str], Ty::Bool),
        // `encode()` -> `bytes`: UTF-8 encode (str is UTF-8 internally; copies the bytes out).
        // No encoding-name argument — UTF-8 only (other codecs are an explicit future non-goal).
        "encode" => (vec![], Ty::Bytes),
        // gap #1 (minimal subset): receiver methods forwarding to the `std.str` free fns, so
        // `s.ends_with(x)` works just like `s.starts_with(x)` (no import needed). Native in both
        // engines; byte-identical to the std.str codepoint-loop oracle for valid inputs (repeat
        // raises a recoverable capacity-overflow fault instead of aborting on a huge count).
        "replace" => (vec![Ty::Str, Ty::Str], Ty::Str),
        "repeat" => (vec![Ty::Int], Ty::Str),
        "reverse" => (vec![], Ty::Str),
        "pad_left" => (vec![Ty::Int, Ty::Str], Ty::Str),
        "index_of" | "count" => (vec![Ty::Str], Ty::Int),
        "strip_prefix" | "strip_suffix" => (vec![Ty::Str], Ty::Str),
        "split_lines" => (vec![], Ty::list(Ty::Str)),
        // `strip` is a trim alias.
        "strip" => (vec![], Ty::Str),
        // gap #7: safe numeric parse — Option-returning (None on bad input) instead of raising.
        "to_int" => (vec![], Ty::option(Ty::Int)),
        "to_float" => (vec![], Ty::option(Ty::Float)),
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
        "merge" => (
            vec![Ty::map(k.clone(), v.clone())],
            Ty::map(k.clone(), v.clone()),
        ),
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
        // `trip()` flips a permanent level-trigger latch: the channel then reports ready (`true`) on
        // every `recv`/`try_recv`/`wait`, fanning out to any number of receivers (the primitive behind
        // `std.cancel`'s `done()`). Idempotent; takes no args.
        "trip" => (vec![], Ty::Nil),
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
            vec![Ty::Func {
                params: vec![elem.clone()],
                ret: Box::new(elem.clone()),
            }],
            Ty::Nil,
        ),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `RwShared[T]` — the cross-task read-write box. `elem` is the box's
/// element type. `get`/`set`/`write` mirror `Shared`'s `get`/`set`/`update` (a write-locked RMW).
/// `read(f: fn(T) -> R) -> R` is R-polymorphic: its param is typed `fn(T) -> Unknown` (accepting any
/// closure return) and its `ret` is `Unknown` here — the dispatch arm recovers the real `R` from the
/// supplied closure (`Ty::RwShared` in `infer_method_call`), so the call site sees the true type.
fn rwshared_method_sig(method: &str, elem: &Ty) -> Option<FnSig> {
    let (params, ret) = match method {
        "get" => (vec![], elem.clone()),
        "set" => (vec![elem.clone()], Ty::Nil),
        "write" => (
            vec![Ty::Func {
                params: vec![elem.clone()],
                ret: Box::new(elem.clone()),
            }],
            Ty::Nil,
        ),
        // `read(fn(T) -> R) -> R` — the param's return is `Unknown` (any `R` is accepted), and the
        // real `R` is recovered at the call site by the dispatch arm.
        "read" => (
            vec![Ty::Func {
                params: vec![elem.clone()],
                ret: Box::new(Ty::Unknown),
            }],
            Ty::Unknown,
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
        "read" => Some(FnSig::optional_tail(
            vec![Ty::Int, Ty::Int],
            Ty::result(Ty::Str),
            1,
        )),
        "write" => Some(FnSig::optional_tail(
            vec![Ty::Str, Ty::Int],
            Ty::result(Ty::Int),
            1,
        )),
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
        "accept" => Some(FnSig::optional_tail(
            vec![Ty::Int],
            Ty::result(Ty::Socket),
            1,
        )),
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
        "submit" => (
            vec![Ty::Func {
                params: vec![],
                ret: Box::new(Ty::Unknown),
            }],
            Ty::Nil,
        ),
        "shutdown" => (vec![], Ty::Nil),
        "shutdown_now" => (vec![], Ty::Nil),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `set[T]`. `elem` is the set's element type.
/// Built-in method signatures on `bytearray` (the mutable byte buffer). Mirrors the VM's
/// `core_method` ByteArray arm and the interp's `eval_bytearray_method` — keep all three in lockstep.
/// `push(int)` appends one byte (0–255 validated at runtime); `pop() -> Option[int]` removes the last.
/// `extend` is NOT here — it takes any byte-sequence shape and is handled in `infer_method_call`.
fn bytearray_method_sig(method: &str) -> Option<FnSig> {
    let (params, ret) = match method {
        "len" => (vec![], Ty::Int),
        "push" => (vec![Ty::Int], Ty::Nil),
        "pop" => (vec![], Ty::option(Ty::Int)),
        // `decode()` -> `str`: UTF-8 decode the current buffer (recoverable fault on invalid UTF-8).
        "decode" => (vec![], Ty::Str),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Built-in method signatures on `bytes` (the immutable byte sequence). Mirrors the VM's
/// `bytes_method` and the interp's bytes-method arm — keep all three in lockstep. `decode()`
/// (UTF-8 → str, recoverable fault on invalid UTF-8) and `len()` (byte count).
fn bytes_method_sig(method: &str) -> Option<FnSig> {
    let (params, ret) = match method {
        "decode" => (vec![], Ty::Str),
        "len" => (vec![], Ty::Int),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

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

/// A DISPLAY-only signature for a free / constructor builtin (editor hover, v1). Unlike the
/// `infer_named_call` arms — whose result is arg-dependent (overloads, variadics, named params,
/// type-arg-driven element types) — this returns ONE canonical positional shape per name, mirrored
/// by hand from `docs/stdlib.md §1`. Polymorphic-input slots use `Ty::Unknown` (renders `?`); the
/// precise concrete RETURN is the hover payload. NOT used for checking — only `callee_display_ty`
/// reads it under the hover probe. Covers exactly [`RESERVED_CALLABLE`] (drift-guarded by a test);
/// a future reserved builtin that forgets an entry here fails that test rather than silently losing
/// hover. (`Ok`/`Err`/`Some` are not reserved and stay hover-`None`.)
fn builtin_sig(name: &str) -> Option<FnSig> {
    let (params, ret) = match name {
        // Free functions ----------------------------------------------------------------------
        // Variadic + named args (`sep`/`end`) collapse to one `?` slot; always returns `nil`.
        "print" => (vec![Ty::Unknown], Ty::Nil),
        // Diverging (bottom-typed): renders `fn(str) -> ?`.
        "panic" => (vec![Ty::Str], Ty::Unknown),
        // Overloads `range(end)` / `range(start, end)` / `range(start, end, step)` collapse to the
        // canonical `range(end)`.
        "range" => (vec![Ty::Int], Ty::list(Ty::Int)),
        // `int`/`float`/`str` accept ~anything → `?` arg, concrete scalar return.
        "int" => (vec![Ty::Unknown], Ty::Int),
        "float" => (vec![Ty::Unknown], Ty::Float),
        "str" => (vec![Ty::Unknown], Ty::Str),
        "ord" => (vec![Ty::Str], Ty::Int),
        "chr" => (vec![Ty::Int], Ty::Str),
        // Container constructors --------------------------------------------------------------
        // Element/key/value types are inferred from the argument → `?` slots.
        "List" => (vec![Ty::Unknown], Ty::list(Ty::Unknown)),
        "Set" => (vec![Ty::Unknown], Ty::set(Ty::Unknown)),
        "Map" => (vec![Ty::Unknown], Ty::map(Ty::Unknown, Ty::Unknown)),
        "bytes" => (vec![Ty::Unknown], Ty::Bytes),
        "bytearray" => (vec![Ty::Unknown], Ty::ByteArray),
        // Runtime constructors ----------------------------------------------------------------
        // `Channel[T]()` is type-arg-driven, no value arg → no params.
        "Channel" => (vec![], Ty::channel(Ty::Unknown)),
        "Shared" => (vec![Ty::Unknown], Ty::shared(Ty::Unknown)),
        "RwShared" => (vec![Ty::Unknown], Ty::rwshared(Ty::Unknown)),
        "Atomic" => (vec![Ty::Unknown], Ty::atomic(Ty::Unknown)),
        // `timer(ms) -> Channel[bool]` (one-shot timeout channel).
        "timer" => (vec![Ty::Int], Ty::channel(Ty::Bool)),
        // `Executor()` — zero-arg work queue.
        "Executor" => (vec![], Ty::Executor),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Editor hover (Tier C): a concise one-line "how to use" blurb for a BUILTIN/STDLIB type or ctor
/// name, paraphrased from `docs/stdlib.md §1–3`. For a type that owns a method table, a
/// `\nmethods: a, b, c` line is appended from the authored `*_METHODS` slice (drift-guarded by
/// `builtin_method_slices_all_resolve`, so every advertised method provably exists). Returns `None`
/// for any non-builtin name (a user type's own docstring already flows via `name_docs`). Built only
/// under the hover probe (the call sites are `hover_probe.is_some()`-gated), never on the hot check
/// path. The method-bearing types are listed first; the no-method types (range/tuple/Result/…) only
/// get a usage line.
fn builtin_type_doc(name: &str) -> Option<String> {
    let (usage, methods): (&str, Option<&[&str]>) = match name {
        // Containers (own a method table) --------------------------------------------------------
        "List" => (
            "growable ordered sequence — List[T](); index with xs[i], iterate with `for x in xs:`",
            Some(LIST_METHODS),
        ),
        "Map" => (
            "key→value hash map — Map[K, V](); index with m[k], iterate with `for k, v in m:`",
            Some(MAP_METHODS),
        ),
        "Set" => (
            "unordered collection of unique elements — Set[T](); operators | & - ^",
            Some(SET_METHODS),
        ),
        "str" => (
            "immutable UTF-8 text; index/slice by codepoint, concatenate with +",
            Some(STR_METHODS),
        ),
        "bytes" => (
            "immutable byte sequence — bytes(x); index with b[i] (byte as int)",
            Some(BYTES_METHODS),
        ),
        "bytearray" => (
            "mutable byte buffer — bytearray(x); index/assign b[i], push/pop bytes",
            Some(BYTEARRAY_METHODS),
        ),
        // Concurrency / runtime types (own a method table) ---------------------------------------
        "Channel" => (
            "FIFO mailbox between tasks — Channel[T](); iterate received values with `for v in ch:`",
            Some(CHANNEL_METHODS),
        ),
        "Shared" => (
            "cross-task shared cell — Shared(v) (import std.concurrency)",
            Some(SHARED_METHODS),
        ),
        "RwShared" => (
            "cross-task read-write cell, many readers OR one writer — RwShared(v) (import std.concurrency)",
            Some(RWSHARED_METHODS),
        ),
        "Atomic" => (
            "cross-task atomic cell — Atomic(v) (import std.concurrency; add/sub need numeric T)",
            Some(ATOMIC_METHODS),
        ),
        "Executor" => (
            "task pool — Executor() (import std.concurrency)",
            Some(EXECUTOR_METHODS),
        ),
        "Socket" => (
            "TCP connection handle from std.net connect()/accept()",
            Some(SOCKET_METHODS),
        ),
        "Listener" => (
            "TCP listening socket from std.net listen()",
            Some(LISTENER_METHODS),
        ),
        // Types without a built-in method table (usage line only) --------------------------------
        "range" => (
            "end-exclusive sequence of ints — range(end) / range(start, end) / range(start, end, step)",
            None,
        ),
        "tuple" => (
            "fixed-size heterogeneous group — (a, b); destructure with `x, y := t`",
            None,
        ),
        "Result" => (
            "success-or-error — Result[T] / Result[T, E]; Ok(v) / Err(e), unwrap with ? or match",
            None,
        ),
        "Option" => (
            "a value or nothing — Option[T]; Some(v) / None, unwrap with ? or match",
            None,
        ),
        "Iterator" => (
            "lazy element cursor — Iterator[T] from a generator or `.iter()`; drive with `for x in it:`",
            None,
        ),
        _ => return None,
    };
    let mut doc = usage.to_string();
    if let Some(ms) = methods {
        doc.push_str("\nmethods: ");
        doc.push_str(&ms.join(", "));
    }
    Some(doc)
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
        check_entry(entry)
            .unwrap_err()
            .into_iter()
            .map(|e| e.message)
            .collect()
    }

    // 18. A module member's type resolves: a correct use checks clean, a mismatch is rejected.
    #[test]
    fn module_member_type_resolves() {
        let t = TmpDir::new();
        t.write("a.chz", "fn read() -> int: return 5\n");
        let ok = t.write(
            "ok.chz",
            "import a\nx: int = a.read()\nfn main(): print(x)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "import a\nx: str = a.read()\nfn main(): print(x)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("cannot assign int to variable of type str")),
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
            errs.iter()
                .any(|m| m.contains("cannot assign int to variable of type str")),
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
        assert!(
            errs.iter().any(|m| m.contains("has no member 'nope'")),
            "got: {errs:?}"
        );
    }

    // 21. Types are MODULE-SCOPED: the same struct name in two loaded modules does NOT collide
    // (each is private to its module, reachable only via import).
    #[test]
    fn cross_module_same_type_name_no_collision() {
        let t = TmpDir::new();
        t.write("a.chz", "struct Point:\n    x: int\nfn fa(): print(1)\n");
        t.write("b.chz", "struct Point:\n    y: int\nfn fb(): print(2)\n");
        let entry = t.write("main.chz", "import a\nimport b\nfn main(): print(1)\n");
        assert!(
            check_entry(&entry).is_ok(),
            "two modules with the same type name must NOT collide: {:?}",
            errors(&entry)
        );
    }

    // Module-scoped types: `import S from m` (struct) makes the struct usable bare.
    #[test]
    fn from_import_struct_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "struct S:\n    x: int\n");
        let entry = t.write(
            "main.chz",
            "import S from types\ns := S(1)\nfn main(): print(s.x)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "from-imported struct should be usable: {:?}",
            errors(&entry)
        );
    }

    // `import Color from m` (enum) makes the enum + its variants usable.
    #[test]
    fn from_import_enum_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import Color from types\nc := Color.Red\nfn main(): print(\"ok\")\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "from-imported enum should be usable: {:?}",
            errors(&entry)
        );
    }

    // An imported enum's METHODS must ferry across the module boundary (EnumSigInfo.methods →
    // importer's enum_methods). Both the from-import and qualified-access paths.
    #[test]
    fn imported_enum_methods_usable() {
        let t = TmpDir::new();
        t.write(
            "types.chz",
            "enum Color:\n    Red\n    Green\n    fn cost(self) -> int:\n        match self:\n            Color.Red: return 1\n            Color.Green: return 2\n",
        );
        let entry = t.write(
            "main.chz",
            "import Color from types\nimport types\nfn main():\n    c := Color.Red\n    x: int = c.cost()\n    q: int = types.Color.Green.cost()\n    print(x + q)\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "imported enum methods should resolve cross-module: {:?}",
            errors(&entry)
        );
    }

    // A whole-module-imported newtype is constructible in QUALIFIED form (`m.UserId(10)`), not just
    // usable as a qualified type — mirrors qualified struct/enum-variant construction.
    #[test]
    fn qualified_newtype_construct_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "newtype UserId = int\n");
        let entry = t.write(
            "main.chz",
            "import types\nfn needs(u: types.UserId) -> int:\n    return int(u)\nfn main():\n    u := types.UserId(10)\n    print(needs(u))\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified newtype ctor should resolve cross-module: {:?}",
            errors(&entry)
        );
    }

    // A GENERIC newtype declared in module A is importable in B: constructed (ctor inference) and
    // dispatched with the right instantiation, type-checks clean cross-module.
    #[test]
    fn generic_newtype_cross_module_ok() {
        let t = TmpDir::new();
        t.write(
            "types.chz",
            "newtype Stack[T] = List[T]:\n    fn size(self) -> int:\n        return List(self).len()\n",
        );
        let entry = t.write(
            "main.chz",
            "import Stack from types\nfn main():\n    s: Stack[int] = Stack([1, 2, 3])\n    print(s.size())\n    xs: List[int] = List(s)\n    print(xs[0])\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "generic newtype should resolve cross-module: {:?}",
            errors(&entry)
        );
    }

    // The cross-module generic newtype's type-arg arity is enforced through the import.
    #[test]
    fn generic_newtype_cross_module_arity_rejected() {
        let t = TmpDir::new();
        t.write("types.chz", "newtype Stack[T] = List[T]\n");
        let entry = t.write(
            "main.chz",
            "import types\nfn main():\n    s: types.Stack[int, str] = types.Stack([1])\n    print(1)\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|e| e.contains("type argument")),
            "expected an arity error, got: {errs:?}"
        );
    }

    // `import Alias from m` (type alias) makes the alias usable as a type.
    #[test]
    fn from_import_alias_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "type Len = int\n");
        let entry = t.write(
            "main.chz",
            "import Len from types\nx: Len = 5\nfn main(): print(x)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "from-imported alias should be usable: {:?}",
            errors(&entry)
        );
    }

    // Qualified access: `import geo` then `geo.Point(1,2)` and `x: geo.Point`.
    #[test]
    fn qualified_struct_access() {
        let t = TmpDir::new();
        t.write("geo.chz", "struct Point:\n    x: int\n    y: int\n");
        let entry = t.write(
            "main.chz",
            "import geo\np: geo.Point = geo.Point(1, 2)\nfn main(): print(p.x)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified struct access should check: {:?}",
            errors(&entry)
        );
    }

    // Qualified enum variant access: `geo.Color.Red`.
    #[test]
    fn qualified_enum_access() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nc := geo.Color.Red\nfn main(): print(\"ok\")\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified enum access should check: {:?}",
            errors(&entry)
        );
    }

    // Bare use of a type whose module was imported (whole-module) but NOT `from`-imported is a
    // CHECK-TIME error with a hint to import it.
    #[test]
    fn bare_unimported_type_is_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "struct Point:\n    x: int\n");
        let entry = t.write(
            "main.chz",
            "import geo\np := Point(1)\nfn main(): print(\"ok\")\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("Point")
                && (m.contains("import it from geo") || m.contains("unknown"))),
            "expected unknown-type/import hint for bare Point: {errs:?}"
        );
    }

    // Cross-module width-alias transparency: `type Len = int32` in module m, imported into entry and
    // used in an extern signature — the FFI width license carries cross-module.
    #[test]
    fn cross_module_width_alias_transparent() {
        let t = TmpDir::new();
        t.write("m.chz", "import int32 from std.ffi\ntype Len = int32\n");
        let entry = t.write(
            "main.chz",
            "import Len from m\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Len\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "cross-module width alias should check: {:?}",
            errors(&entry)
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
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "fn f() -> str?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter().any(|m| m.contains("str")),
            "expected Option type mismatch: {errs:?}"
        );
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
            errs.iter()
                .any(|m| m.contains("try_recv") && m.contains("argument")),
            "got: {errs:?}"
        );
    }

    // 24. Qualified enum-variant access `Color.Red` (nullary) type-checks as the enum type, exactly
    // like the bare `Red`. A wrong variant `Color.Bogus` is a targeted error.
    #[test]
    fn qualified_nullary_variant_checks() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\n    Green\nfn f() -> Color:\n    return Color.Red\nfn main(): print(1)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nfn f() -> Color:\n    return Color.Bogus\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("Color") && m.contains("Bogus")),
            "expected 'no variant Bogus' style error: {errs:?}"
        );
    }

    // 25. Qualified payload-variant construction `Shape.Circle(2)` type-checks as the enum type and
    // still validates argument types, exactly like the bare `Circle(2)`.
    #[test]
    fn qualified_payload_variant_checks() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Shape:\n    Circle(int)\n    Dot\nfn f() -> Shape:\n    return Shape.Circle(2)\nfn main(): print(1)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "enum Shape:\n    Circle(int)\n    Dot\nfn f() -> Shape:\n    return Shape.Circle(\"x\")\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("Circle") && m.contains("int")),
            "expected payload type mismatch: {errs:?}"
        );
    }

    // 26. A real binding wins over an enum name: `c.0` where `c` is a tuple is normal field access,
    // never reinterpreted as a qualified variant.
    #[test]
    fn binding_wins_over_enum_qualifier() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\nfn main():\n    pair := (1, 2)\n    print(pair.0)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );
    }

    // 27. Qualified variant patterns `case Color.Red:` check clean (mixed with bare arms); a wrong
    // qualifier `case Color.Circle:` (Circle belongs to a different enum) is a targeted error.
    #[test]
    fn qualified_variant_pattern_checks() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\n    Green\nfn f(c: Color) -> str:\n    return match c:\n        Color.Red: \"r\"\n        Color.Green: \"g\"\nfn main(): print(f(Color.Red))\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nenum Shape:\n    Circle(int)\nfn f(c: Color) -> str:\n    return match c:\n        Color.Circle: \"c\"\n        _: \"x\"\nfn main(): print(f(Color.Red))\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("Color") && m.contains("Circle")),
            "expected qualifier mismatch error: {errs:?}"
        );
    }

    // 28. A *module-global* binding named like an enum does NOT shadow qualified access — both engines
    // gate on locals/captures only, so the checker must too (else it validates a different program
    // than the one that runs → runtime fault). `Color.Red` here is the variant (type Color), so
    // returning it as `int` is a *checker* error, not a clean compile + runtime crash.
    #[test]
    fn global_binding_does_not_shadow_qualified_variant() {
        let t = TmpDir::new();
        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nstruct Box:\n    Red: int\nColor := Box(7)\nfn show() -> int:\n    return Color.Red\nfn main(): print(show())\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter().any(|m| m.contains("Color")),
            "expected a checker type error (Color variant returned as int), got: {errs:?}"
        );
    }

    // 29. The `Enum.` qualifier is validated even under an int/str/bool scrutinee: `case Color.Bogus:`
    // against an `int` must be rejected (a qualified variant is never a catch-all binding), not
    // silently accepted as a binding named `Bogus`.
    #[test]
    fn qualified_pattern_validated_under_literal_scrutinee() {
        let t = TmpDir::new();
        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nfn f(n: int) -> str:\n    return match n:\n        1: \"one\"\n        Color.Bogus: \"x\"\nfn main(): print(f(1))\n",
        );
        let errs = errors(&bad);
        assert!(
            !errs.is_empty(),
            "expected a checker error for `Color.Bogus` against an int scrutinee, got none"
        );
    }

    // 30. Genuine struct collision (Blocker A): two modules declare `Point` and BOTH are imported and
    // used qualified. The checker must resolve a field on the SECOND module's `Point` against ITS
    // layout (module-scoped runtime key), not first-wins by bare name.
    #[test]
    fn collision_struct_field_resolves_via_qualified_key() {
        let t = TmpDir::new();
        t.write("a.chz", "struct Point:\n    x: int\n");
        t.write("b.chz", "struct Point:\n    y: int\n    z: int\n");
        let entry = t.write(
            "main.chz",
            "import a\nimport b\nfn main():\n    pb := b.Point(2, 3)\n    print(pb.y)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "field `y` on b.Point must resolve via b's layout: {:?}",
            errors(&entry)
        );
    }

    // 31. Genuine struct collision for a METHOD (Blocker B): only bb.Box has `dbl`. Resolving the
    // method on a `bb.Box(..)` value must hit bb's layout, not ba's (which lacks the method).
    #[test]
    fn collision_struct_method_resolves_via_qualified_key() {
        let t = TmpDir::new();
        t.write("ba.chz", "struct Box:\n    v: int\n");
        t.write(
            "bb.chz",
            "struct Box:\n    v: int\n    fn dbl(self) -> int:\n        return self.v * 2\n",
        );
        let entry = t.write(
            "main.chz",
            "import ba\nimport bb\nfn main():\n    bb2 := bb.Box(5)\n    print(bb2.dbl())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "method `dbl` on bb.Box must resolve via bb's layout: {:?}",
            errors(&entry)
        );
    }

    // 32. Self-consistency under enum collision (Blocker C, checker side): two modules declare `Color`,
    // cb has `fn classify(c: Color)` matching on its own `Color.Red`/`Color.Green`. With both imported
    // the checker must still type-check the bare-`Color` annotation + the qualified match patterns.
    #[test]
    fn collision_enum_annotation_and_match_typecheck() {
        let t = TmpDir::new();
        t.write("ca.chz", "enum Color:\n    Red\n    Green\n");
        t.write(
            "cb.chz",
            "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\n",
        );
        let entry = t.write(
            "cmain.chz",
            "import ca\nimport cb\nfn main(): print(cb.classify(cb.Color.Red))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "cb.classify + qualified Color match must type-check under collision: {:?}",
            errors(&entry)
        );
    }

    // 33. The "foreign qualifier" match error must render the scrutinee enum's BARE display name,
    // never the qualified identity key. Two modules declare `Color`; a `match` on one using the
    // other's `Color.<variant>` arms must report `enum 'Color'`, not `enum 'modkey::Color'`.
    #[test]
    fn foreign_variant_match_error_renders_bare_enum_name() {
        let t = TmpDir::new();
        t.write("ea.chz", "enum Color:\n    Red\n    Green\n");
        t.write("eb.chz", "enum Color:\n    Cyan\n    Magenta\n");
        let entry = t.write(
            "emain.chz",
            "import Color from ea\nimport Color as C2 from eb\nfn main():\n    c := Color.Red\n    match c:\n        C2.Cyan: print(\"x\")\n        C2.Magenta: print(\"y\")\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("cannot match a value of enum 'Color'")),
            "expected bare 'Color' in foreign-qualifier match error, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::Color'")),
            "qualified identity key must not leak into match error: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_type_checks_clean() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Color.Red: print(\"r\")\n        geo.Color.Green: print(\"g\")\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "module-qualified variant pattern should type-check clean: {:?}",
            check_entry(&entry)
        );
    }

    #[test]
    fn module_qualified_variant_pattern_aliased_binder_clean() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo as g\nfn main():\n    c := g.Color.Red\n    match c:\n        g.Color.Red: print(\"r\")\n        g.Color.Green: print(\"g\")\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "aliased module-qualified variant pattern should type-check clean: {:?}",
            check_entry(&entry)
        );
    }

    #[test]
    fn module_qualified_variant_pattern_payload_binds_clean() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Shape:\n    Circle(int)\n    Square(int)\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    s := geo.Shape.Circle(3)\n    match s:\n        geo.Shape.Circle(r): print(r)\n        geo.Shape.Square(w): print(w)\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "payload-binding module-qualified variant pattern should type-check clean: {:?}",
            check_entry(&entry)
        );
    }

    #[test]
    fn module_qualified_variant_pattern_wrong_variant_bare_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Color.Blue: print(\"b\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("enum 'Color' has no variant 'Blue'")),
            "expected bare-name wrong-variant error, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::Color")),
            "qualified identity key must not leak: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_wrong_enum_bare_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Shape.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("module 'geo' has no enum 'Shape'")),
            "expected bare module/enum error, got: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_unknown_module_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        nope.Color.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|e| e.contains("unknown module 'nope'")),
            "expected unknown-module error, got: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_cross_enum_bare_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        t.write("light.chz", "enum Light:\n    Red\n    Off\n");
        let entry = t.write(
            "main.chz",
            "import geo\nimport light\nfn main():\n    c := geo.Color.Red\n    match c:\n        light.Light.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("cannot match a value of enum 'Color'")),
            "expected bare cross-enum error, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::")),
            "qualified identity key must not leak: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_collision_no_cross_talk() {
        let t = TmpDir::new();
        t.write("a.chz", "enum Color:\n    Red\n    Green\n");
        t.write("b.chz", "enum Color:\n    Cyan\n    Magenta\n");
        // Exhaustive over a's Color via a.Color.* arms => clean.
        let ok = t.write(
            "ok.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Red: print(\"r\")\n        a.Color.Green: print(\"g\")\nmain()\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "a.Color.* arms over an a.Color value should be exhaustive: {:?}",
            check_entry(&ok)
        );
        // Using b's variant under a's enum => wrong-variant error (no cross-talk).
        let xtalk = t.write(
            "xtalk.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Cyan: print(\"c\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&xtalk);
        assert!(
            errs.iter()
                .any(|e| e.contains("enum 'Color' has no variant 'Cyan'")),
            "expected no cross-talk with b's Color, got: {errs:?}"
        );
        // Non-exhaustive over a's Color => missing Green (bare name).
        let nonex = t.write(
            "nonex.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Red: print(\"r\")\nmain()\n",
        );
        let errs = errors(&nonex);
        assert!(
            errs.iter().any(|e| e.contains("Green")),
            "expected non-exhaustive to report missing bare 'Green', got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::")),
            "exhaustiveness error must use bare names: {errs:?}"
        );
    }

    // Soundness: an unannotated closure param (`Ty::Unknown` scrutinee) whose match arms name a
    // single known enum but miss a variant must STILL be rejected — the Skip path used to bypass
    // exhaustiveness, letting `g(E.B)` reach the engine and runtime-trap "no match arm for variant".
    #[test]
    fn match_unknown_param_missing_variant_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\nfn main(): print(g(E.B))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("non-exhaustive") && e.contains("missing B")),
            "expected non-exhaustive missing B, got: {errs:?}"
        );
    }

    // Soundness: literal arms over an `Ty::Unknown` scrutinee with no `_` wildcard cannot be proven
    // complete — must be rejected (the int-scrutinee value-leak `g(99) -> 99` otherwise).
    #[test]
    fn match_unknown_param_literals_no_wildcard_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x): match x:\n    1: \"one\"\nfn main(): print(g(99))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("non-exhaustive") && e.contains("`_`")),
            "expected non-exhaustive add a `_` arm, got: {errs:?}"
        );
    }

    // Boundary (must STAY accepted): unannotated param covering ALL variants of E, no `_`.
    #[test]
    fn match_unknown_param_all_variants_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (full coverage): {:?}",
            errors(&entry)
        );
    }

    // Boundary (must STAY accepted): unannotated param over E variants PLUS a `_` wildcard.
    #[test]
    fn match_unknown_param_variants_plus_wildcard_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\n    _: \"other\"\nfn main(): print(g(E.B))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (wildcard closes): {:?}",
            errors(&entry)
        );
    }

    // Boundary (must STAY accepted): literal arms PLUS a `_` wildcard.
    #[test]
    fn match_unknown_param_literals_plus_wildcard_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    1: \"one\"\n    _: \"other\"\nfn main(): print(g(99))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (wildcard closes literals): {:?}",
            errors(&entry)
        );
    }

    // MIGRATED (closure-param inference, phase 3): the first literal arm (`1`) now PINS the bare
    // param `x` to `int` (source #2 — a bare-param match scrutinee). The `"b"` arm is then a str
    // literal against an `int` scrutinee → rejected. The pre-inference `OpenScrutinee` behaviour
    // (accept heterogeneous literals when a `_` closes the match) no longer applies: the scrutinee is
    // no longer un-inferable.
    #[test]
    fn match_unknown_param_hetero_literals_plus_wildcard_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    1: \"a\"\n    \"b\": \"c\"\n    _: \"d\"\nfn main(): print(g(1))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("literal of type str cannot match scrutinee of type int")),
            "expected str-literal-vs-int-scrutinee mismatch, got: {errs:?}"
        );
    }

    // MIGRATED (phase 3): the first arm (`1`) pins `x` to `int`; the `"b"` arm is then a literal
    // mismatch (and the match is also non-exhaustive). Either way it rejects.
    #[test]
    fn match_unknown_param_hetero_literals_no_wildcard_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x): match x:\n    1: \"a\"\n    \"b\": \"c\"\nfn main(): print(g(true))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("literal of type str cannot match scrutinee of type int")),
            "expected str-literal-vs-int-scrutinee mismatch, got: {errs:?}"
        );
    }

    // Boundary (must STAY accepted): a bare-ident catch-all (`n:`) over an un-inferable scrutinee is
    // an irrefutable binding that closes the match — exactly like a concretely-typed int scrutinee.
    // Regression guard: routing the literal case through the permissive `Skip` bind branch wrongly
    // treated `n` as a refutable nullary variant → spurious non-exhaustive + undeclared binding.
    #[test]
    fn match_unknown_param_bare_ident_catchall_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    0: \"zero\"\n    n: \"got {n}\"\nfn main(): print(g(7))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (bare-ident catch-all closes + binds): {:?}",
            errors(&entry)
        );
    }

    // === Qualified-type-as-static-method-receiver (Part 1) ===

    // `module.Struct.static_method()` — qualified type dotted with a static method checks clean.
    #[test]
    fn qualified_type_struct_static_ok() {
        let t = TmpDir::new();
        t.write(
            "counter.chz",
            "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
        );
        let entry = t.write(
            "main.chz",
            "import counter\nfn main():\n    c := counter.Counter.zero()\n    print(c.n)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean qualified static call: {:?}",
            errors(&entry)
        );
    }

    // Negative: a non-existent static method on a qualified type is a clear "no static method" error.
    #[test]
    fn qualified_type_struct_static_unknown_rejects() {
        let t = TmpDir::new();
        t.write(
            "counter.chz",
            "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
        );
        let entry = t.write(
            "main.chz",
            "import counter\nfn main():\n    c := counter.Counter.no_such()\n    print(c)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|m| m.contains("has no static method 'no_such'")),
            "got: {errs:?}"
        );
    }

    // `module.Enum.static_method()` — qualified enum dotted with a static method (NOT a variant).
    #[test]
    fn qualified_type_enum_static_ok() {
        let t = TmpDir::new();
        t.write(
            "col.chz",
            "enum Color:\n    Red\n    Green\n    fn first() -> Color:\n        return Color.Red\n",
        );
        let entry = t.write(
            "main.chz",
            "import col\nfn main():\n    c := col.Color.first()\n    print(c)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean qualified enum-static call: {:?}",
            errors(&entry)
        );
    }

    // Regression KEEP-WORKING: bare static after `from` import.
    #[test]
    fn qualified_keep_bare_static() {
        let t = TmpDir::new();
        t.write(
            "counter.chz",
            "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
        );
        let entry = t.write(
            "main.chz",
            "import Counter from counter\nfn main():\n    c := Counter.zero()\n    print(c.n)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "bare static regressed: {:?}",
            errors(&entry)
        );
    }

    // Regression KEEP-WORKING: qualified constructor `module.Type(args)`.
    #[test]
    fn qualified_keep_qualified_ctor() {
        let t = TmpDir::new();
        t.write("counter.chz", "struct Counter:\n    n: int\n");
        let entry = t.write(
            "main.chz",
            "import counter\nfn main():\n    c := counter.Counter(7)\n    print(c.n)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified ctor regressed: {:?}",
            errors(&entry)
        );
    }

    // Regression KEEP-WORKING: qualified enum VARIANT (variant-first wins over static).
    #[test]
    fn qualified_keep_enum_variant() {
        let t = TmpDir::new();
        t.write(
            "col.chz",
            "enum Color:\n    Red\n    Green\n    fn first() -> Color:\n        return Color.Red\n",
        );
        let entry = t.write(
            "main.chz",
            "import col\nfn main():\n    c := col.Color.Red\n    print(c)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified enum variant regressed: {:?}",
            errors(&entry)
        );
    }

    // === Two-level path diagnostics (Part 2) ===

    // EXPR position: `std.concurrency.Shared(0)` head `std` is an import-path prefix → two-level hint
    // (NOT the misleading bare "unknown name 'std'").
    #[test]
    fn multilevel_expr_path_two_level_hint() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "import std.concurrency\nfn main():\n    s := std.concurrency.Shared(0)\n    print(s)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("two-level")),
            "expected two-level hint, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m == "unknown name 'std'"),
            "should not emit bare unknown-name: {errs:?}"
        );
    }

    // EXPR position, SIBLING COLLISION: two `std.*` imports share head `std`. A `std.net.Socket(0)`
    // mistake must name the module the user REFERENCED (`net`), not the first-imported sibling
    // (`concurrency`). Regression for the first-wins head-map bug (3 confirmed adversarial charges).
    #[test]
    fn multilevel_expr_collision_names_referenced_module() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "import std.concurrency\nimport std.net\nfn main():\n    s := std.net.Socket(0)\n    print(s)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|m| m.contains("two-level") && m.contains("`net.<Name>`")),
            "expected hint naming the referenced module `net`, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("concurrency")),
            "must NOT steer to the first-imported sibling `concurrency`: {errs:?}"
        );
    }

    // EXPR position, THREE-LEVEL import: `import std.concurrency.collection` binds `collection` (the
    // LAST segment), NOT `concurrency`. A `std.concurrency.collection.X(...)` mistake must name
    // `collection`, not the second segment. Regression for the adversarial 3-level charge.
    #[test]
    fn multilevel_expr_three_level_import_names_bound_name() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "import std.concurrency.collection\nfn main():\n    c := std.concurrency.collection.Nope(0)\n    print(c)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|m| m.contains("two-level") && m.contains("`collection.<Name>`")),
            "expected hint naming the bound name `collection`, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("write `concurrency")),
            "must NOT name the second segment `concurrency` as the module: {errs:?}"
        );
    }

    // Negative: a genuine undefined name still gives the normal "unknown name" error.
    #[test]
    fn multilevel_expr_real_typo_still_unknown_name() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "fn main():\n    nope_xyz()\n");
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("unknown name 'nope_xyz'")),
            "got: {errs:?}"
        );
    }

    // Negative: a real undefined TYPE still gives the normal unknown-type error (not the hint).
    #[test]
    fn multilevel_unknown_type_still_normal_error() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "fn f(x: Nope): print(1)\nfn main(): f(1)\n");
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("Nope")),
            "expected unknown-type error mentioning Nope, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("two-level")),
            "should not emit two-level hint for a real typo: {errs:?}"
        );
    }

    // ===== expected-type-hint inference: an ANNOTATION pins a generic ctor/fn-call's type params =====
    // (a self-contained Heap clone — fields `data: List[T]`, `less: fn(T,T)->bool` — exercises the
    // module-prefixed-key check_graph path, decoupled from the std.collections path.)
    const HEAP_MOD: &str = "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\n\n    fn len(self) -> int:\n        return self.data.len()\n";

    #[test]
    fn annotation_pins_generic_ctor_let() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import H from a\nfn main():\n    h: H[int] = H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "annotation should pin H[int] T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_return() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import H from a\nfn mk() -> H[int]:\n    return H([], fn(x, y): x < y)\nfn main():\n    print(mk().len())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "declared return H[int] should pin T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_call_arg() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import H from a\nfn take(h: H[int]): print(h.len())\nfn main():\n    take(H([], fn(x, y): x < y))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "param type H[int] should pin T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_qualified_generic_ctor() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import a\nfn main():\n    h: a.H[int] = a.H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified ctor annotation should pin T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_generic_free_fn_return() {
        let t = TmpDir::new();
        t.write("a.chz", "fn empty[T]() -> List[T]:\n    return []\n");
        let entry = t.write(
            "main.chz",
            "import empty from a\nfn main():\n    xs: List[int] = empty()\n    print(xs.len())\n",
        );
        let errs = errors_or_empty(&entry);
        assert!(
            errs.is_empty(),
            "annotation should pin empty()'s T=int: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("cannot assign List[T] to")),
            "stale leaked-Param error: {errs:?}"
        );
    }

    #[test]
    fn annotation_does_not_override_explicit_or_args() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        // (a) turbofish, no annotation — still works.
        let tf = t.write(
            "tf.chz",
            "import H from a\nfn main():\n    h := H[int]([], fn(x: int, y: int): x < y)\n    print(h.len())\n",
        );
        assert!(check_entry(&tf).is_ok(), "turbofish: {:?}", errors(&tf));
        // (b) annotated closure params, no annotation — still works.
        let ann = t.write(
            "ann.chz",
            "import H from a\nfn main():\n    h := H([], fn(x: int, y: int): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&ann).is_ok(),
            "annotated closure params: {:?}",
            errors(&ann)
        );
        // (c) args win over annotation: a concrete int element vs an H[str] annotation is STILL a
        // type error (the seed only fills params left FREE by args; it must not override a pinned T).
        let bad = t.write(
            "bad.chz",
            "import H from a\nfn main():\n    h: H[str] = H([1], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&bad).is_err(),
            "args-pinned int vs H[str] annotation must still error"
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_in_if_else_branches() {
        // An if-else in value position is itself the bound value; BOTH branches are the tail
        // value, so each must receive the expected-type hint. The hint is a take()-once slot, so
        // without re-installing it per branch the SECOND-inferred branch starves and a generic
        // ctor there deadlocks on `T` — making acceptance depend purely on branch order.
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let e1 = t.write(
            "e1.chz",
            "import H from a\nfn main():\n    r := true\n    h: H[int] = if r: H([], fn(x, y): x > y) else: H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&e1).is_ok(),
            "if-else both branches: {:?}",
            errors(&e1)
        );
        // Swapping the branches must behave identically (no order dependence).
        let e2 = t.write(
            "e2.chz",
            "import H from a\nfn main():\n    r := true\n    h: H[int] = if r: H([], fn(x, y): x < y) else: H([], fn(x, y): x > y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&e2).is_ok(),
            "if-else swapped branches: {:?}",
            errors(&e2)
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_in_match_arms() {
        // Same per-branch hint requirement for an expression-`match` value: every arm body is a
        // tail value and must see the hint, not just the first-inferred arm.
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let e = t.write(
            "m.chz",
            "import H from a\nfn main():\n    k := 1\n    h: H[int] = match k:\n        0: H([], fn(x, y): x > y)\n        _: H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&e).is_ok(),
            "match arms both pinned: {:?}",
            errors(&e)
        );
    }

    fn errors_or_empty(entry: &Path) -> Vec<String> {
        match check_entry(entry) {
            Ok(()) => Vec::new(),
            Err(es) => es.into_iter().map(|e| e.message).collect(),
        }
    }
}
