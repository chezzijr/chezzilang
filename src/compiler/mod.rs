//! Bytecode compiler (M5): lowers a resolved module graph (or a single `Module`) to a [`Program`]
//! of function prototypes for the stack VM. The compiler is the *only* place that knows about slots
//! — locals resolve to operand-stack slots, and (M19 Phase 2b) module globals resolve to stable
//! per-module global slots here; the rest (struct/variant names, builtins) is resolved by name,
//! in a fixed lookup order the VM reproduces exactly.
//!
//! Two passes:
//!   1. **Hoist** — register every module's struct / enum declarations into the program-global
//!      type tables (with the "type already defined" collision rule), plus the built-in
//!      `Ok`/`Err`/`Some`/`None` variants.
//!   2. **Compile** — for each module emit a `<toplevel>` proto (top-level `fn`s hoisted first so
//!      forward references resolve) and one proto per `fn` / method / closure.

use crate::ast::{
    AssignOp, BinaryOp, Block, CompClause, CompKind, DeferTarget, Expr, ExprKind, FnDecl, Import,
    LitPattern, MatchArm, MatchExprArm, Module, Pattern, Span, SpawnTarget, Stmt, StmtKind, Type,
    UnaryOp, WaitArm, WaitArmKind, WaitTarget,
};
use crate::interpolation::{Chunk, parse_interpolation};
use crate::native::cffi::CType;
use crate::resolver::{ModuleGraph, ResolvedImport};
use crate::vm::op::{
    AssertCmp, CapEntry, CapSrc, CffiDef, LIFECYCLE_HOOKS, ModuleProto, NO_IC, Op, Program, Proto,
    ProtoId, StructDef, SuiteInfo, VariantDef, WaitMeta,
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

/// The name a builtin resolves to.
/// ROOT REDESIGN — the module key used to qualify user-type identity keys in the single-source
/// (standalone / `<main>`) compile + run paths, where there is no module graph to derive a real
/// label from. The checker, compiler, and VM must all agree on it, so it lives here as one constant. Only
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

/// M24 — the UNSPELLABLE local name holding the runtime type witness for type param `t`. `$` is not
/// an identifier character, so no source binding can shadow or read it.
fn witness_local(t: &str) -> String {
    format!("{WITNESS_PREFIX}{t}")
}

/// The prefix of every [`witness_local`] name — the marker a capture-entry filter matches on.
const WITNESS_PREFIX: &str = "$w:";

/// M24 Task 4 — where a frame keeps the hidden witness for a type param: its own trailing `$w:T`
/// parameter, or a capture of the enclosing frame's.
#[derive(Clone, Copy)]
enum WitnessRef {
    /// A plain, never-boxed local slot (`$w:T` cannot be in `boxed_names` — it is unspellable).
    Local(usize),
    /// A positional capture slot, holding the witness `str` RAW (not a cell) — see
    /// [`Compiler::with_witness_captures`], which snapshots the local's value, not a cell handle.
    Captured(u32),
}

/// W7-49 — a checker→compiler side-table key that was asked to hold two DIFFERENT decisions stops
/// the build. Refusing to compile is the whole point: the backend is type-blind, so an aliased key
/// means it would apply one expression's decision to another — a silent wrong VALUE under a green
/// `chezzi check`. First conflict wins (they share one cause).
fn reject_table_conflicts(conflicts: crate::checker::TableConflicts) -> Result<(), CompileError> {
    match conflicts.into_iter().next() {
        Some((span, message)) => Err(CompileError { message, span }),
        None => Ok(()),
    }
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
    // M24 rides the same pass: the witness table says which generic fns take hidden trailing
    // witness params and what fills each witness at each call site. The compiler CONSUMES it — it
    // never re-derives which protocols carry a static requirement (that resolves through
    // imports/aliases/embeds, which is checker work).
    let (kw, wt, ct, pe, lw, ns, conflicts) = crate::checker::resolve_call_tables(graph);
    reject_table_conflicts(conflicts)?;
    c.keyword_calls = kw;
    c.witnesses = wt;
    c.carriers = ct;
    c.proto_eq_calls = pe;
    c.list_widen = lw;
    c.newtype_sums = ns;
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
        let toplevel = c.compile_module(idx, &lm.ast, &lm.imports, idx == entry_idx, lm.native)?;
        let global_slots = std::mem::take(&mut c.global_slots);
        c.program.modules.push(ModuleProto {
            id: lm.id.clone(),
            label: lm.label(),
            toplevel,
            imports: lm.imports.clone(),
            native: lm.native,
            global_slots,
            file: lm.file,
        });
    }
    c.program.field_ic_sites = c.field_ic_next;
    c.program.method_ic_sites = c.method_ic_next;
    c.program.rebuild_struct_names();
    c.build_eq_hooks();
    c.build_provider_table()?;
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
    let (kw, wt, ct, pe, lw, ns, conflicts) =
        crate::checker::resolve_call_tables_standalone(&module.stmts);
    reject_table_conflicts(conflicts)?;
    c.keyword_calls = kw;
    c.witnesses = wt;
    c.carriers = ct;
    c.proto_eq_calls = pe;
    c.list_widen = lw;
    c.newtype_sums = ns;
    let toplevel = c.compile_module(0, module, &[], true, None)?;
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
        file: 0,
    });
    c.program.field_ic_sites = c.field_ic_next;
    c.program.method_ic_sites = c.method_ic_next;
    c.program.rebuild_struct_names();
    c.build_eq_hooks();
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
    /// M23 — struct/enum runtime key → its `Eq` protocol hook (`(proto, home module index)`), for the
    /// types whose `eq` method [`binds_eq_hook`] accepts. Materialized into the `tid`- and
    /// `variant_id`-indexed `Program::eq_struct` / `eq_enum` by [`Compiler::build_eq_hooks`] once every
    /// module is compiled (the dense ids are only final then).
    eq_hooks: HashMap<String, (ProtoId, usize)>,
    /// Default-argument provider NAME → its dense `Op::MakeFuncIn` operand. Ids are handed out at
    /// EMIT sites (a call site in a module that cannot name the provider), which run before the
    /// declaring module is compiled — hence the indirection. See [`Compiler::build_provider_table`].
    provider_ids: HashMap<String, u32>,
    /// Default-argument provider NAME → `(its proto, the index of the module declaring it)`, filled
    /// as each declaring module is compiled. Materialized into `Program::providers` at the end.
    provider_defs: HashMap<String, (ProtoId, usize)>,
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
    /// reserved/native type (`Result`/`Option`/`Match`/`Response`/`Iterator`/FFI widths),
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
    /// M24 — the checker-produced static-witness contract (see [`crate::checker::WitnessTable`]),
    /// consumed verbatim: `fns` drives the hidden trailing `$w:T` params a generic fn's proto gets,
    /// `calls` drives the extra argument each call site pushes.
    witnesses: crate::checker::WitnessTable,
    /// W7-43 — the checker's per-`?.`-carrier lowering decision (see
    /// [`crate::checker::CarrierTable`]), consumed verbatim: the backend is type-blind and cannot
    /// re-derive whether a carrier's operand was an `Option` or a `Result`.
    carriers: crate::checker::CarrierTable,
    /// W7-53 I1′ — which `.eq(x)` call sites are PROTOCOL dispatch through a generic bound. The
    /// backend is type-blind, so this is CONSUMED from the checker and never re-derived; a MISS
    /// means "ordinary by-name call", which is the pre-W7-53 lowering. See
    /// [`crate::checker::ProtoEqTable`].
    proto_eq_calls: crate::checker::ProtoEqTable,
    /// The checker's per-list-literal int→float element-widen verdict (see
    /// [`crate::checker::ListWidenTable`]), consumed verbatim: the backend is type-blind and cannot
    /// re-derive the SLOT element type that decides it. A MISS means "widen" — the pre-fix lowering.
    list_widen: crate::checker::ListWidenTable,
    /// Which `.sum()` sites need a `T(0)` newtype SEED pushed as a hidden argument. The backend is
    /// type-blind — an empty `List[Cents]` carries no element to read a `type_key` off — so this is
    /// CONSUMED from the checker and never re-derived; a MISS means "plain numeric sum", which is the
    /// pre-fix lowering. See [`crate::checker::NewtypeSumTable`].
    newtype_sums: crate::checker::NewtypeSumTable,
    /// W7-43 — counter for the fresh `__optN` temp names the Option lowering mints, mirroring the
    /// checker's own. Frame-local and `__`-prefixed (unwritable by user code), so uniqueness within
    /// one compilation is all that is ever needed — and this makes it true by construction rather
    /// than by argument.
    next_opt_tmp: usize,
    /// M24 — the witness TYPE-PARAM names the NEXT [`Self::compile_fn_body`] declares as trailing
    /// `$w:T` params, in declaration order. DECLARATION only: a nested body's REACH is read off the
    /// frame it is emitting into ([`FnComp::witness_ref`]), never from here, so this cannot drift
    /// from what was actually captured.
    witness_locals: Vec<String>,
    /// M24 — the witness params for the NEXT `compile_fn_body`, set only at the module-level `fn`
    /// emit site (so a nested `fn` sharing a top-level fn's name can never inherit its arity).
    pending_witnesses: Vec<String>,
    /// M24 Task 3 — the CURRENT module's `from`-imported FUNCTION names: bound name (the `as` alias
    /// when there is one) → `(declaring module index, declared name)`. Rebuilt per `compile_module`.
    /// [`crate::checker::WitnessTable::fns`] is keyed by the DECLARING module, so a bare call to an
    /// imported fn must be resolved through this before it can be read — with this module's own index
    /// the entry is simply missing, which is how a cross-module witness call used to lower one
    /// argument short. A name this module declares as a top-level `fn` is never in here (the checker
    /// rejects that collision outright), so a local fn always wins by construction.
    imported_fns: HashMap<String, (usize, String)>,
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

/// `literal_numeric_mix` over ALL the LEAF value branches of a whole `if … elif … else` chain: an
/// `elif` desugars to a nested `IfElse` in the `els` slot, so a float constant in ANY arm — head,
/// middle, or tail — must license widening the int-constant arms, ORDER-INDEPENDENTLY (like a list
/// literal / `match`). Flattens the chain to its leaves (each `then`, plus the final non-`IfElse`
/// `els`) before applying the peephole. Computed ONCE at the chain head and threaded down the nested
/// recursion (`compile_if_expr_chain` / the checker's `infer_if_else_chain` carry it as
/// `inherited_mix`), so every level sees the SAME whole-chain mix rather than recomputing a narrower
/// sub-chain mix. Used IDENTICALLY by both, so they cannot drift; each level then coerces only its own
/// immediate leaf (`untyped_int_const`).
pub(crate) fn if_chain_numeric_mix(then: &Expr, els: &Expr) -> bool {
    let mut leaves: Vec<&Expr> = Vec::new();
    fn collect<'a>(then: &'a Expr, els: &'a Expr, out: &mut Vec<&'a Expr>) {
        out.push(then);
        if let ExprKind::IfElse {
            then: t2, els: e2, ..
        } = &els.kind
        {
            collect(t2, e2, out);
        } else {
            out.push(els);
        }
    }
    collect(then, els, &mut leaves);
    literal_numeric_mix(leaves.into_iter())
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

/// Decompose a validated send-arm call `chan.send(value)` into `(&chan, &value)`. The CHECKER has
/// already rejected any other shape (`wait_send_arm_shape`), so a compiled program is guaranteed to
/// match — an unexpected shape here is a checker/compiler drift bug, not user error.
fn send_arm_parts(call: &Expr) -> (&Expr, &Expr) {
    if let ExprKind::Call { callee, args, .. } = &call.kind
        && let ExprKind::Field { obj, name, .. } = &callee.kind
        && name == "send"
        && args.len() == 1
    {
        return (obj, &args[0]);
    }
    unreachable!("send-arm call shape must be validated by the checker before compile");
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

/// Does this struct/enum method declaration bind the `Eq` protocol hook — the `fn eq(self, o: Self)
/// -> bool` that `==`/`!=` dispatch to (`Vm::user_eq_method`)?
///
/// The backend is type-blind, so this is the SYNTACTIC twin of the checker's `validate_eq_shape`
/// (`src/checker/sig.rs`), which has already forced every struct/enum `eq` into exactly one of two
/// shapes: the hook, or an ordinary method whose operand is a type PARAMETER (`Opt[T].eq(self,
/// x: T)` — legal, and the operator must leave it alone). So the only thing left to ask here is which
/// of the two it is, and "the operand names a type parameter in scope" answers it without resolving a
/// single type. The two therefore agree by construction on every program that type-checks.
///
/// Deliberately does NOT re-test the return type: `type B = bool` is an alias the backend cannot see,
/// and a non-`bool` return already faults cleanly at the operator ("eq() must return bool, got …").
fn binds_eq_hook(m: &FnDecl, owner_params: &[crate::ast::TypeParam]) -> bool {
    if m.name != "eq" || m.params.len() != 2 {
        return false;
    }
    let Some(Type::Named { name, .. }) = &m.params[1].ty else {
        return true; // `o: Box[T]` / `o: Self` / unannotated — never a bare type-param name
    };
    !owner_params
        .iter()
        .chain(m.type_params.iter())
        .any(|tp| &tp.name == name)
}

impl Compiler {
    /// Materialize the `Eq`-hook tables the VM's `==` reads (`Program::eq_struct` / `eq_enum`) from
    /// the key-based `eq_hooks` gathered while compiling. Runs last: `tid`s and `variant_id`s are the
    /// index space, and both are only final once every module has been hoisted and compiled.
    /// Materialize `Program::providers` from the emit-site ids and the declaration-site protos.
    /// Runs last, for the same reason [`Compiler::build_eq_hooks`] does: the caller's module is
    /// compiled BEFORE the definer's, so a provider's `ProtoId` is only known once every module has
    /// been compiled.
    ///
    /// A mentioned provider with no compiled declaration is a hard `CompileError`, never a hole in
    /// the table. That is what makes the checker's side of this safe: it types an out-of-closure
    /// provider call from the parameter slot rather than resolving the definer's signature, so a
    /// desugar/compiler disagreement about which defaults get providers must surface as a build
    /// failure here rather than as a missing runtime symbol.
    fn build_provider_table(&mut self) -> Result<(), CompileError> {
        if self.provider_ids.is_empty() {
            return Ok(());
        }
        let mut table = vec![None; self.provider_ids.len()];
        for (name, &id) in &self.provider_ids {
            let Some(&def) = self.provider_defs.get(name) else {
                return Err(CompileError {
                    message: format!(
                        "internal: no provider function was compiled for {}",
                        crate::desugar::display_fn_name(name)
                    ),
                    span: Span::RUNTIME,
                });
            };
            table[id as usize] = Some(def);
        }
        self.program.providers = table
            .into_iter()
            .map(|e| e.expect("every id was just filled"))
            .collect();
        Ok(())
    }

    /// The dense `Op::MakeFuncIn` operand for `name`, allocating one on first mention.
    fn provider_id(&mut self, name: &str) -> u32 {
        let next = self.provider_ids.len() as u32;
        *self.provider_ids.entry(name.to_string()).or_insert(next)
    }

    fn build_eq_hooks(&mut self) {
        if self.eq_hooks.is_empty() {
            return; // the overwhelmingly common case — leave both tables empty (every lookup misses)
        }
        let mut by_tid = vec![None; self.program.struct_names.len()];
        for (key, def) in &self.program.structs {
            if let Some(hook) = self.eq_hooks.get(key) {
                by_tid[def.tid as usize] = Some(*hook);
            }
        }
        let mut by_vid = vec![None; self.program.variants_by_id.len()];
        for (vid, vd) in self.program.variants_by_id.iter().enumerate() {
            by_vid[vid] = self.eq_hooks.get(&vd.enum_name).copied();
        }
        self.program.eq_struct = by_tid;
        self.program.eq_enum = by_vid;
    }

    fn new() -> Self {
        let mut program = Program {
            protos: Vec::new(),
            structs: HashMap::new(),
            enum_methods: HashMap::new(),
            enum_home: HashMap::new(),
            newtype_methods: HashMap::new(),
            newtype_home: HashMap::new(),
            providers: Vec::new(),
            native_methods: HashMap::new(),
            native_home: HashMap::new(),
            variants: HashMap::new(),
            variants_by_id: Vec::new(),
            struct_names: Vec::new(),
            eq_struct: Vec::new(),
            eq_enum: Vec::new(),
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
            // gaps §6 — `FileInfo` from `std.fs` (returned by `fs.stat`). Field order load-bearing
            // (matches `native/fs.rs` stat builder + checker `seed_stdlib_structs`).
            (
                "FileInfo",
                &["size", "mtime", "mode", "is_dir", "is_file", "is_symlink"][..],
            ),
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
            eq_hooks: HashMap::new(),
            float_shadow: std::collections::HashSet::new(),
            globals: HashMap::new(),
            provider_ids: HashMap::new(),
            provider_defs: HashMap::new(),
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
            witnesses: crate::checker::WitnessTable::default(),
            carriers: crate::checker::CarrierTable::new(),
            proto_eq_calls: crate::checker::ProtoEqTable::new(),
            list_widen: crate::checker::ListWidenTable::new(),
            newtype_sums: crate::checker::NewtypeSumTable::new(),
            next_opt_tmp: 0,
            witness_locals: Vec::new(),
            pending_witnesses: Vec::new(),
            imported_fns: HashMap::new(),
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
                    "std.fs" => Some("FileInfo"),
                    _ => None,
                } {
                    self.module_types[idx].insert(sname.to_string());
                    self.program.type_names.insert(sname.to_string());
                }
                continue;
            }
            // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (no qualified
            // key entry → `type_key` falls back to bare), so `Iterator`/FFI widths resolve bare.
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
            && fc.is_unbound(mname)
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
    ///
    /// `native` is the module's native-std name (`Some("std.fs")`) — a HYBRID native module's
    /// `native fn`s are module globals too, see the `StmtKind::Native` arm below.
    fn collect_globals(
        &mut self,
        imports: &[ResolvedImport],
        stmts: &[Stmt],
        native: Option<&'static str>,
    ) {
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
            // W7-8 — a `native fn` in a HYBRID native module (`std.fs`'s `_exists`) is a module global
            // too: `Vm::run_module` injects its Rust `NativeFn` BY NAME and `module_define` reuses an
            // existing index, so reserving the slot here is exactly what lets a BODIED sibling in the
            // same file call it (`fs.exists`'s `PathLike` wrapper calling `_exists`). Without the slot
            // the call compiled to `global_slot` → the "global has no slot" panic.
            // Restricted to names the runtime actually injects (`native_members`), so an OPCODE-backed
            // decl (`std.time.timer`) still gets no slot and no nil global; `native ctor` is not
            // first-class and never takes one. NOT added to `self.fn_names` — that licenses the
            // generic-fn-as-value turbofish erase, which a native fn is not.
            if let Some(nat) = native
                && let StmtKind::Native(d) = &stmt.kind
                && d.kind == crate::ast::NativeKind::Fn
                && crate::native::native_members(nat)
                    .iter()
                    .any(|(n, _, _)| *n == d.name)
            {
                add(d.name.clone(), &mut self.globals, &mut self.global_slots);
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
        native: Option<&'static str>,
    ) -> Result<ProtoId, CompileError> {
        // M19 Phase 2b: assign a stable slot to every module global before emitting any code, so
        // forward references (method/fn bodies, imports used before their line) resolve to a slot.
        self.collect_globals(imports, &module.stmts, native);
        // Module-scoped types: record this module's index + its imported module bindings, so a
        // qualified `geo.Point(...)` resolves to the right module's runtime key.
        self.current_module_idx = module_idx;
        self.imported_modules.clear();
        self.imported_fns.clear();
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
                        // the checker's std exception). User
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
                            let bind = alias.clone().unwrap_or_else(|| member.clone());
                            // A `from`-imported user type becomes bare-resolvable under its bind name,
                            // keyed by the DECLARING module's runtime key.
                            if self
                                .module_types
                                .get(tidx)
                                .is_some_and(|t| t.contains(member))
                            {
                                let key = self.type_key(tidx, member);
                                self.bare_types.insert(bind, key);
                            } else {
                                // …anything else is a value/function member: remember where it was
                                // DECLARED, so a call site can read the callee's witness arity out of
                                // the checker's declaring-module-keyed table (M24 Task 3).
                                self.imported_fns.insert(bind, (tidx, member.clone()));
                            }
                        }
                    }
                }
            }
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
                    self.pending_witnesses = self.member_witnesses(module_idx, &key, &m.name);
                    let pid = self.compile_fn(m, false)?;
                    let def = self.program.structs.get_mut(&key).expect("hoisted");
                    def.module_idx = module_idx;
                    def.methods.insert(m.name.clone(), pid);
                    if binds_eq_hook(m, type_params) {
                        self.eq_hooks.insert(key.clone(), (pid, module_idx));
                    }
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
                    self.pending_witnesses = self.member_witnesses(module_idx, &key, &m.name);
                    let pid = self.compile_fn(m, false)?;
                    compiled.insert(m.name.clone(), pid);
                    if binds_eq_hook(m, type_params) {
                        self.eq_hooks.insert(key.clone(), (pid, module_idx));
                    }
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
        // Compile native-struct BODIED methods (`fn lines(self) -> …: <body>` on a `native struct`),
        // keyed by the reserved handle's BARE name (`"Reader"`) — reserved handle names are unique and
        // import-gated, so there is no user-type collision, and it matches the checker's bare re-seed of
        // the method table. Like the enum-method pass: type-erased (no `StructDef`/`tid`), routed via
        // `Program::native_methods` at the handle's `do_method_call` arm. The bodyless `native fn` sigs
        // compile NOTHING (their dispatch stays native).
        for stmt in &module.stmts {
            if let StmtKind::NativeStruct {
                name,
                bodied_methods,
                type_params,
                ..
            } = &stmt.kind
            {
                if bodied_methods.is_empty() {
                    continue;
                }
                let prev_shadow = std::mem::replace(
                    &mut self.float_shadow,
                    type_params.iter().map(|tp| tp.name.clone()).collect(),
                );
                let mut compiled: HashMap<String, ProtoId> = HashMap::new();
                for m in bodied_methods {
                    let pid = self.compile_fn(m, false)?;
                    compiled.insert(m.name.clone(), pid);
                }
                self.float_shadow = prev_shadow;
                self.program
                    .native_methods
                    .entry(name.clone())
                    .or_default()
                    .extend(compiled);
                self.program.native_home.insert(name.clone(), module_idx);
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
                    self.pending_witnesses = self.member_witnesses(module_idx, &key, &m.name);
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
        // `CellLoad on a non-handle value` at runtime (check-OK / host-panic). Its
        // boxed-name set is computed exactly like every other fn body (`compile_fn_captured`). Names
        // that resolve as GLOBALS here (top-level `let`s / hoisted fns) are never `add_local`'d, so
        // `is_boxed_slot` never fires for them — only genuine frame locals (loop vars, block-lets) box.
        fc.boxed_names = captured_names_of_body(&module.stmts, &[]);
        for stmt in &module.stmts {
            if let StmtKind::Fn(decl) = &stmt.kind {
                // M24: a module-level generic fn whose bound carries a static protocol requirement
                // gets hidden trailing witness params — the checker's `fns` table is the ONLY source.
                self.pending_witnesses = self
                    .witnesses
                    .fns
                    .get(&(module_idx, decl.name.clone()))
                    .cloned()
                    .unwrap_or_default();
                let pid = self.compile_fn(decl, false)?;
                // A hidden default-argument provider is reachable from a module that cannot name it
                // (`Op::MakeFuncIn`); record where it lives so `build_provider_table` can resolve it.
                if decl.name.starts_with(crate::desugar::PROVIDER_PREFIX) {
                    self.provider_defs
                        .insert(decl.name.clone(), (pid, module_idx));
                }
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
            fc.emit(Op::EnterNursery, Span::RUNTIME);
            fc.nursery_scopes += 1;
        }
        self.compile_block_flat(&mut fc, &module.stmts)?;
        if implicit {
            fc.nursery_scopes -= 1;
        }
        fc.emit(Op::Nil, Span::RUNTIME);
        fc.emit(Op::Return, Span::RUNTIME);
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
        // M24 — this body's OWN hidden witness params. `pending_witnesses` is set ONLY at the
        // module-level `fn`/member emit site, so a nested `fn` takes the empty default: it declares
        // none, and reaches the enclosing frame's through the `$w:T` capture entries
        // `with_witness_captures` appended at its `MakeClosure` (Task 4).
        let witnesses = std::mem::take(&mut self.pending_witnesses);
        let prev_w = std::mem::replace(&mut self.witness_locals, witnesses);
        let r = self.compile_fn_body(decl, captured_names);
        self.witness_locals = prev_w;
        self.float_shadow = prev_shadow;
        r
    }

    fn compile_fn_body(
        &mut self,
        decl: &FnDecl,
        captured_names: Vec<String>,
    ) -> Result<ProtoId, CompileError> {
        // M24: the hidden trailing witness params sit AFTER the declared params, so every existing
        // slot index (float/box prologues, capture entries) is untouched and only the arity grows.
        let mut fc = FnComp::new(
            decl.name.clone(),
            decl.params.len() + self.witness_locals.len(),
            false,
        );
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
        for w in &self.witness_locals {
            fc.add_local(witness_local(w));
        }
        // A `float` param coerces any int argument at the callee prologue — so EVERY caller (incl. an
        // int VARIABLE, not just a literal) widens.
        // Callee-side default fill FIRST, so a filled value is coerced and boxed like a supplied one.
        self.emit_default_param_prologue(&mut fc, &decl.params)?;
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
            fc.emit(Op::EnterNursery, Span::RUNTIME);
            fc.nursery_scopes += 1;
        }
        self.compile_block_scoped(&mut fc, &decl.body)?;
        if implicit {
            fc.nursery_scopes -= 1;
        }
        // Fall off the end → return Nil (do_return joins the implicit nursery).
        fc.emit(Op::Nil, Span::RUNTIME);
        fc.emit(Op::Return, Span::RUNTIME);
        Ok(self.finish(fc))
    }

    /// **Callee-side default fill.** For each trailing parameter that carries a default, emit
    ///
    /// ```text
    ///     JumpIfProvided(slot, after)
    ///     <the default expression>
    ///     SetLocal(slot)
    ///   after:
    /// ```
    ///
    /// and lower [`FnComp::min_arity`] accordingly, so the runtime arity checks admit a call that
    /// omits them. The default is compiled HERE, in the module that declares the function, which is
    /// what makes this correct for the shapes no call-site rewrite can reach: a call through a
    /// first-class function value (the caller has no signature to consult) and a default whose type
    /// mentions `Self` or an enclosing type parameter (unspellable in the free `fn` a provider is).
    ///
    /// Runs BEFORE [`Compiler::emit_float_param_prologue`] so a filled `int` default still widens
    /// into a `float` parameter, and therefore before [`Compiler::emit_box_param_prologue`] so a
    /// filled value is boxed like a supplied one.
    ///
    /// **Emits nothing, and leaves `min_arity == arity`, unless every condition holds:**
    /// the parameters carrying defaults form a SUFFIX (a short call can only ever drop a suffix, so
    /// a hole before a supplied argument is not expressible), the proto carries no hidden trailing
    /// WITNESS parameters (they live past the declared ones, so a short declared count would land a
    /// witness in a defaulted slot), and no parameter is VARIADIC (its surplus collapse is a
    /// call-site rewrite the callee cannot reconstruct). A function with no defaults is
    /// byte-identical to before.
    fn emit_default_param_prologue(
        &mut self,
        fc: &mut FnComp,
        params: &[crate::ast::Param],
    ) -> Result<(), CompileError> {
        // ONE shared predicate with the checker — see `ast::min_callable_params` for why they must
        // agree by construction rather than by hand-sync.
        let first = crate::ast::min_callable_params(params, !self.witness_locals.is_empty());
        if first == params.len() {
            return Ok(()); // no short entry for this shape (no trailing defaults, variadic, witnesses)
        }
        for (i, p) in params.iter().enumerate().skip(first) {
            let Some(d) = &p.default else { continue };
            let span = d.span;
            // Patched below once the body's length is known.
            let jump_at = fc.code.len();
            fc.emit(Op::JumpIfProvided(i as u32, usize::MAX), span);
            // **The default is evaluated in MODULE scope, not the callee's.** `docs/syntax.md`: "a
            // default is evaluated on its own, where parameters are not in scope" — and the provider
            // `fn` that serves every reachable call site is a free top-level function, so it resolves
            // its free names against the module's globals. This prologue must give the SAME answer, or
            // one function's default means two different things depending on whether the caller could
            // see the definer. Measured before this hid the locals, `n := 100` at module level and
            // `fn f(n: int, x: str = "n={n}")`: a direct `f(3)` printed `n=100` (provider, module
            // scope) while `g := f; g(3)` printed `n=3` (prologue, callee scope) — the same default,
            // two answers. Hiding the frame's bindings for the duration of
            // this one expression makes the prologue resolve exactly as the provider does.
            let saved_locals = std::mem::take(&mut fc.locals);
            let saved_slots = fc.slot_count;
            let saved_caps = std::mem::take(&mut fc.captured_names);
            let compiled = self.compile_expr(fc, d);
            fc.locals = saved_locals;
            fc.slot_count = saved_slots;
            fc.captured_names = saved_caps;
            // Propagate rather than swallow: the checker has ALREADY advertised short entry for this
            // signature (`FnSig::min_params`), so silently declining to emit the fill would leave a
            // call the checker accepts and the runtime rejects — the exact checker⊆compiler violation
            // `ast::min_callable_params` exists to make impossible.
            compiled?;
            fc.emit_set_local_raw(i, span);
            let after = fc.code.len();
            if let Op::JumpIfProvided(_, t) = &mut fc.code[jump_at] {
                *t = after;
            }
        }
        fc.min_arity = first;
        Ok(())
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
                let span = Span::RUNTIME;
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
                let span = Span::RUNTIME;
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
            min_arity: fc.min_arity,
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
                // `stmt.span`) so the error location points at the exact failing expression.
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
                    let snap = fc.snapshot_entries();
                    let (kept, free) =
                        filter_entries_free_block(&snap, &decl.body, &decl.params);
                    let entries = self.with_witness_captures(&snap, kept, Some(&free));
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
                    let snap = fc.snapshot_entries();
                    let (kept, free) =
                        filter_entries_free_block(&snap, &decl.body, &decl.params);
                    let entries = self.with_witness_captures(&snap, kept, Some(&free));
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
                // TASK B — cancel any `parallel:` nursery this `break` leaves before its
                // join. Order on the jump path is body defers first, then the nursery reclaim
                // (silent cancel, §2c1) — distinct from the fall-through order (JoinNursery then the
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
                // A comparison condition is split so the operand VALUES survive the comparison
                // opcode: duplicate them (`Op::Dup2`) before comparing, compare the copies, and on
                // the failing path the originals are still on the stack (under `msg`) for
                // `Op::Assert` to render. `In` is excluded (see `assert_cmp`).
                if let ExprKind::Binary { op, lhs, rhs } = &cond.kind
                    && let Some(c) = assert_cmp(*op)
                {
                    self.compile_expr(fc, lhs)?;
                    self.compile_expr(fc, rhs)?;
                    fc.emit(Op::Dup2, cond.span);
                    fc.emit(binary_op(*op), cond.span);
                    let to_fail = fc.emit_jump(Op::JumpIfFalse(0), stmt.span);
                    // Passing path: pop the two duplicated operands so a passing comparison assert
                    // leaves no residue on the stack (see ## Decisions — unbounded growth in a loop).
                    fc.emit(Op::Pop, stmt.span);
                    fc.emit(Op::Pop, stmt.span);
                    let to_end = fc.emit_jump(Op::Jump(0), stmt.span);
                    fc.patch_jump(to_fail);
                    if let Some(m) = msg {
                        self.compile_expr(fc, m)?;
                    }
                    fc.emit(
                        Op::Assert {
                            has_msg: msg.is_some(),
                            cmp: Some(c),
                        },
                        stmt.span,
                    );
                    fc.patch_jump(to_end);
                    return Ok(());
                }
                // Lazy message evaluation — `msg` is only evaluated on failure: compile `cond`, and
                // only on the false path compile `msg` then
                // `Op::Assert` (which always faults). A passing assert never touches `msg`, so a
                // side-effecting/faulting message expression never runs on the passing path.
                // `Op::Assert` carries `stmt.span` so the fault location points at the `assert` statement itself.
                self.compile_expr(fc, cond)?;
                let to_fail = fc.emit_jump(Op::JumpIfFalse(0), stmt.span);
                let to_end = fc.emit_jump(Op::Jump(0), stmt.span);
                fc.patch_jump(to_fail);
                if let Some(m) = msg {
                    self.compile_expr(fc, m)?;
                }
                fc.emit(
                    Op::Assert {
                        has_msg: msg.is_some(),
                        cmp: None,
                    },
                    stmt.span,
                );
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
            // Concurrency C4 — sequential, run-to-completion executor.
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
    /// A SEND arm (`ch.send(v):`) instead leaves `[chan, value]` and binds nothing.
    fn compile_wait(
        &mut self,
        fc: &mut FnComp,
        arms: &[WaitArm],
        else_block: Option<&[Stmt]>,
        span: Span,
    ) -> Result<(), CompileError> {
        // Evaluate each arm's operands once, source order (left on the stack for the poll to re-read
        // on every re-park). A recv arm pushes ONE handle (the channel); a send arm pushes TWO (the
        // channel THEN the value) — the exact top-to-bottom eval order Go's `select` requires.
        let mut is_send = Vec::with_capacity(arms.len());
        for arm in arms {
            match &arm.kind {
                WaitArmKind::Recv { chan, .. } => {
                    self.compile_expr(fc, chan)?;
                    is_send.push(false);
                }
                WaitArmKind::Send { call } => {
                    let (obj, value) = send_arm_parts(call);
                    self.compile_expr(fc, obj)?;
                    self.compile_expr(fc, value)?;
                    is_send.push(true);
                }
            }
        }
        // Placeholder; back-patched with the arm/else targets once the bodies are laid out.
        let poll_at = fc.emit_jump(
            Op::WaitPoll(Box::new(WaitMeta {
                n: arms.len(),
                arm_targets: Vec::new(),
                else_target: None,
                is_send: is_send.clone(),
            })),
            span,
        );
        let mut arm_targets = Vec::with_capacity(arms.len());
        let mut end_jumps = Vec::new();
        for arm in arms {
            arm_targets.push(fc.here());
            fc.begin_scope();
            match &arm.kind {
                // The selected value is on the stack top — deliver it per the arm's target.
                WaitArmKind::Recv { target, .. } => match target {
                    WaitTarget::Bind(name) => {
                        fc.emit_decl_named(name.clone(), arm.span);
                    }
                    WaitTarget::Discard => fc.emit(Op::Pop, arm.span),
                    WaitTarget::Assign(target) => self.emit_wait_assign(fc, target, arm.span)?,
                },
                // A send arm binds nothing — `take_wait_send_arm` pushes no value, so no prologue.
                WaitArmKind::Send { .. } => {}
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
                is_send,
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
        // `ReclaimNursery` (silent cancel) before its loop-exit jump. Mirrors `defer_scopes`.
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
                // M24-5b — `spawn Type.m(..)`: no receiver value to hold, so it rides the eager-args
                // wrapper instead of `Op::SpawnMethod`.
                if self.receiverless_call_head(fc, callee) {
                    let n = self.compile_receiverless_target(fc, callee, args, named, call.span)?;
                    fc.emit(Op::SpawnCall(n), call.span);
                    return Ok(());
                }
                if let ExprKind::Field {
                    obj,
                    name,
                    name_span,
                } = &callee.kind
                {
                    // Same hidden `Cents(0)` seed the eager `Op::CallMethod` emit pushes: a spawned
                    // member call runs through the identical `Vm::do_method_call`, so a missing seed
                    // is a check-clean / run-faulting `List[Cents].sum()`. The seed is a plain
                    // newtype-wrapped scalar, so it crosses `do_spawn`'s `deep_clone_all` airlock
                    // exactly like any other spawned argument.
                    if let Some((nt_key, is_float)) = self.newtype_sum_seed(name, args, *name_span)
                    {
                        self.compile_expr(fc, obj)?;
                        fc.emit(
                            if is_float {
                                Op::ConstFloat(0.0)
                            } else {
                                Op::ConstInt(0)
                            },
                            call.span,
                        );
                        fc.emit(Op::NewType(nt_key), call.span);
                        fc.emit(Op::SpawnMethod(name.clone(), 1), call.span);
                        return Ok(());
                    }
                    self.compile_expr(fc, obj)?;
                    self.compile_args(fc, args)?;
                    // M24-5: the hidden witness arguments ride LAST, exactly as they do on the eager
                    // `Op::CallMethod`, so the widened `argc` reaches the same proto.
                    let w =
                        self.emit_member_witness_args(fc, callee, name, *name_span, call.span)?;
                    fc.emit(Op::SpawnMethod(name.clone(), args.len() + w), call.span);
                } else if !named.is_empty() {
                    // A spawned VALUE call carrying keyword arguments: reorder to positional by the
                    // checker-recorded permutation, then spawn positionally (same as the eager form).
                    let perm = self.keyword_perm(named, call.span)?;
                    self.compile_expr(fc, callee)?;
                    for &ci in &perm {
                        let e = if ci < args.len() {
                            &args[ci]
                        } else {
                            &named[ci - args.len()].1
                        };
                        self.compile_expr(fc, e)?;
                    }
                    // M24-5: TRAILING — after the permuted args, never in source order.
                    let w = self.emit_indirect_witness_args(fc, callee, call.span)?;
                    fc.emit(Op::SpawnCall(perm.len() + w), call.span);
                } else {
                    self.compile_expr(fc, callee)?;
                    self.compile_args(fc, args)?;
                    let w = self.emit_indirect_witness_args(fc, callee, call.span)?;
                    fc.emit(Op::SpawnCall(args.len() + w), call.span);
                }
                Ok(())
            }
            SpawnTarget::Block(body) => {
                // Capture only the names this block references (its free-variable set); the values
                // are deep-copied across the airlock at `SpawnBlock`. The block becomes a synthetic
                // zero-arg proto whose free names resolve via `GetCaptured`. Free-variable capture
                // (Finding D) avoids dragging unused non-sendable siblings across the airlock.
                let snap = fc.snapshot_entries();
                let (kept, free) = filter_entries_free_block(&snap, body, &[]);
                let entries = self.with_witness_captures(&snap, kept, Some(&free));
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
                // M24 Task 4: the block reaches the enclosing frame's witnesses through
                // `with_witness_captures` above — a `str` per witness, deep-copied across the
                // airlock like any captured string.
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

    /// Load a name's value (local → captured → global).
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
            //     mutates the map mid-loop can't perturb the bindings);
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
        // the outermost loop, matching left-to-right source order (Python comprehension semantics).
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

    /// The IDENTITY KEY of a struct pattern in the current module (L2). Two spellings: BARE
    /// `Point(x, y)` (`name` resolves via `bare_types` to a registered STRUCT, not an enum variant),
    /// or QUALIFIED `geo.Point(x, y)` (the qualifier — the `enum_name` slot filled by the 2-part parse
    /// — is an imported MODULE binder whose `type_key(mod, name)` is a registered struct; the only
    /// spelling for a whole-module-imported struct, symmetric with qualified construction).
    ///
    /// `None` for an enum variant, an ENUM-name qualifier (`E.Point` — not a module), or an unknown
    /// name. This is what separates a struct pattern from an enum-variant pattern at lowering time; it
    /// must AGREE with the checker's `resolve_struct_ctor` so nothing check-accepted fails to lower.
    fn struct_key_of_pattern(&self, enum_name: Option<&str>, name: &str) -> Option<String> {
        if let Some(q) = enum_name {
            let &tidx = self.imported_modules.get(q)?;
            let key = self.type_key(tidx, name);
            return self.program.structs.contains_key(&key).then_some(key);
        }
        if self.variant_pair(None, name).is_some() {
            return None;
        }
        let key = self.bare_types.get(name)?;
        self.program.structs.contains_key(key).then(|| key.clone())
    }

    /// Whether an arm pattern is a GENUINE enum-variant pattern (so `compile_match_general` must
    /// emit the `EnsureEnum` scrutinee guard). A `Pattern::Variant` is NOT one when it resolves to a
    /// struct (`Point(x, y)`, L2) or is a bare whole-value catch-all binding (`rest:` — no qualifier,
    /// no payload, not a registered variant). Everything else (a payload/qualified/nullary-variant
    /// arm) needs the guard.
    fn pattern_needs_enum(&self, p: &Pattern) -> bool {
        let Pattern::Variant {
            name,
            bindings,
            enum_name,
            ..
        } = p
        else {
            return false;
        };
        if self
            .struct_key_of_pattern(enum_name.as_deref(), name)
            .is_some()
        {
            return false;
        }
        // Bare whole-value catch-all binding — binds, never tests an enum tag.
        if enum_name.is_none() && bindings.is_empty() && self.variant_pair(None, name).is_none() {
            return false;
        }
        true
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
        // the checker couldn't infer the type) is a clean runtime error, not a panic. Tuple AND
        // struct matches need no such guard — a struct pattern `Point(x, y)` AND a bare whole-value
        // catch-all binding (`rest:`) are both `Pattern::Variant`, so only a GENUINE enum-variant arm
        // arms the guard. Else a struct-only match would emit `Op::EnsureEnum` and fault at runtime
        // (the checker-superset trap, L2).
        if arms.iter().any(|a| self.pattern_needs_enum(a.pattern())) {
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
                // Struct pattern (L2): `Point(x, y)` destructures a struct by DECLARED FIELD NAME.
                // A struct has one constructor, so this is structurally IRREFUTABLE — no `MatchArm`
                // tag test, no fail-jump of its own; refutable SUB-patterns (`Point(0, y)`) push their
                // own fails when they recurse. Mirrors the `Pattern::Tuple` arm but keys `GetField` on
                // the field name instead of a numeric index (the VM resolves both the same way).
                // Gate on NON-empty bindings: a BARE name with no payload (`Node:`) is never a
                // destructure — even when it happens to resolve to an in-scope struct — it is the
                // whole-value catch-all binding the checker declared (`!is_ctor && bindings.is_empty()`
                // in checker/pattern.rs). Taking the destructure branch here (type-blind, by NAME)
                // would bind NOTHING, leaving the name unbound in the body → `global has no slot` panic.
                if !bindings.is_empty()
                    && let Some(key) = self.struct_key_of_pattern(enum_name.as_deref(), name)
                {
                    let field_names = self.program.structs[&key].fields.clone();
                    for (b, fname) in bindings.iter().zip(field_names.iter()) {
                        fc.emit_hidden_get(scrut, span);
                        fc.emit(
                            Op::GetField {
                                name: fname.clone(),
                                ic: NO_IC,
                            },
                            span,
                        );
                        let elem = fc.add_hidden();
                        fc.emit_hidden_set(elem, span);
                        self.emit_pattern(fc, b, elem, fails, span)?;
                    }
                    return Ok(());
                }
                // A bare whole-value binding catch-all (`rest:` after a refutable struct arm) — only
                // reachable in a struct match (the checker gates the bare binding there; enum/tuple
                // scrutinees require `_`). Bind the scrutinee like a plain `Pattern::Ident`.
                if enum_name.is_none()
                    && bindings.is_empty()
                    && self.variant_pair(None, name).is_none()
                {
                    fc.emit_hidden_get(scrut, span);
                    fc.emit_decl_named(name.clone(), span);
                    return Ok(());
                }
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

    /// W7-43 — the next `__optN` temp index for the Option carrier lowering.
    fn next_opt_tmp(&mut self) -> usize {
        let n = self.next_opt_tmp;
        self.next_opt_tmp += 1;
        n
    }

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
            // The desugared form: fragments are already-parsed, already-normalized children.
            ExprKind::Interp(chunks) => self.compile_interp(fc, chunks, expr.span)?,
            // Raw string: emit the literal directly — does NOT go through `compile_str` /
            // `parse_interpolation`, so braces stay literal and backslashes are verbatim.
            ExprKind::RawStr(s) => fc.emit(Op::ConstStr(s.clone()), expr.span),
            ExprKind::Bytes(b) => fc.emit(Op::ConstBytes(b.clone().into_boxed_slice()), expr.span),
            ExprKind::Ident(name) => self.compile_ident(fc, name, expr.span),
            ExprKind::List(items, origin) => {
                // One-way int→float widening for THIS list: widen an element when the `List[float]`
                // annotation says so OR the constant peephole fires (≥1 untyped float CONSTANT sibling
                // → widen the untyped int CONSTANT siblings) — UNLESS the checker recorded that this
                // literal sits in a `List[Any]` SLOT, where the slot sanctions the mix and no numeric
                // type asks for the widen. That verdict is CONSUMED, never re-derived (the slot's
                // element type is invisible here), and it is looked up under the SAME
                // `literal_numeric_mix` gate the checker recorded it under, so the two cannot drift.
                // A miss = widen, the pre-fix lowering.
                let annotated = elem_hint == Some(crate::ast::ElemFloatHint::Elem);
                let peephole = literal_numeric_mix(items.iter())
                    && self.list_widen.get(&crate::checker::list_widen_key(
                        self.current_module_idx,
                        self.kw_frag_ctx,
                        self.kw_frag_ord,
                        expr.span,
                        *origin,
                    )) != Some(&true);
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
                // `run_capture` skip type-checking entirely.
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
                    && fc.is_unbound(mname)
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
                    && fc.is_unbound(tname)
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
                    && fc.is_unbound(ename)
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
                    && fc.is_unbound(name)
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
            // W7-43 — `?.`/`??` carriers reach the backend intact; clone-and-lower here, then
            // recurse, so the synthesized nodes route through the ordinary `Field`/`compile_call`
            // arms and pick up method inline caches, keyword permutations and witness args exactly
            // as the equivalent hand-written spelling does. Same house pattern as `DecodeCall`
            // below. `lower_carrier_*` are the SAME functions the checker lowered with, on the same
            // input, so spans (and therefore every table key derived from them) cannot drift.
            ExprKind::NullCoalesce { .. } => {
                // `??` is Option-only (the checker rejects everything else), so there is nothing to
                // look up and no decision to make.
                let mut c = expr.clone();
                crate::desugar::lower_carrier_option(&mut c, self.next_opt_tmp());
                self.compile_expr(fc, &c)?;
            }
            ExprKind::OptChain { name_span, .. } => {
                let key = crate::checker::carrier_key(
                    self.current_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    *name_span,
                );
                let mut c = expr.clone();
                // A MISSING entry is a hard error, never a default: it means the checker's
                // traversal and this one disagree about the program, and silently picking the
                // Option lowering would emit an `Option[T]` where the checker promised `T` — a
                // wrong runtime VALUE SHAPE under a green `chezzi check`. Same discipline as the
                // missing-witness guard in `witness_srcs`.
                match self.carriers.get(&key) {
                    Some(crate::checker::CarrierMode::Try) => {
                        crate::desugar::lower_carrier_try(&mut c)
                    }
                    Some(
                        crate::checker::CarrierMode::Option | crate::checker::CarrierMode::Unknown,
                    ) => crate::desugar::lower_carrier_option(&mut c, self.next_opt_tmp()),
                    None => {
                        return Err(CompileError {
                            message: "internal: no lowering recorded for this '?.' — the \
                                      type-checker and the backend disagree about this expression"
                                .to_string(),
                            span: expr.span,
                        });
                    }
                }
                self.compile_expr(fc, &c)?;
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
        // One-way int→float widening across arm VALUES: coerce an untyped int CONSTANT arm when a
        // float CONSTANT sibling arm is present, under the same `literal_numeric_mix` predicate the
        // checker's `branch_widen` licenses (identical `untyped_int_const` guard) — so the join always
        // leaves the `float` the static type promises. Mirrors `compile_if_expr`.
        let mix = literal_numeric_mix(arms.iter().map(|a| &a.body));
        let widen = |s: &mut Self, fc: &mut FnComp, body: &Expr| -> Result<(), CompileError> {
            s.compile_expr(fc, body)?; // leaves the arm's value on the stack
            if mix && crate::ast::untyped_int_const(body) {
                fc.emit(Op::CoerceFloat, body.span);
            }
            Ok(())
        };
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, widen);
        }
        self.compile_match_general(fc, scrutinee, arms, span, widen)
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
        self.compile_if_expr_chain(fc, cond, then, els, None)
    }

    /// Chain-aware body of `compile_if_expr`. `inherited_mix` threads the WHOLE-chain
    /// `if_chain_numeric_mix` down an `if … elif … else` chain (an `elif` is a nested `IfElse` in
    /// `els`), so a float constant in an EARLIER arm licenses coercing the int constants in a later
    /// all-int suffix. MUST mirror the checker's `infer_if_else_chain` predicate exactly (same
    /// whole-chain mix + `untyped_int_const` guard), or static type and stored value drift. Each level
    /// coerces only its own immediate leaf; the nested `els` sub-chain is compiled by a DIRECT
    /// recursive call carrying the head's mix. `None` = chain head (compute the full-chain mix).
    fn compile_if_expr_chain(
        &mut self,
        fc: &mut FnComp,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
        inherited_mix: Option<bool>,
    ) -> Result<(), CompileError> {
        let mix = inherited_mix.unwrap_or_else(|| if_chain_numeric_mix(then, els));
        self.compile_expr(fc, cond)?;
        fc.emit(Op::AsBool, cond.span);
        let skip = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
        self.compile_expr(fc, then)?;
        if mix && crate::ast::untyped_int_const(then) {
            fc.emit(Op::CoerceFloat, then.span);
        }
        let end = fc.emit_jump(Op::Jump(0), cond.span);
        fc.patch_jump(skip);
        // A nested-`IfElse` `els` is the `elif` tail — recurse DIRECTLY, threading the head's mix; any
        // other `els` is the final leaf, coerced here if it is an int constant.
        if let ExprKind::IfElse {
            cond: c2,
            then: t2,
            els: e2,
        } = &els.kind
        {
            self.compile_if_expr_chain(fc, c2, t2, e2, Some(mix))?;
        } else {
            self.compile_expr(fc, els)?;
            if mix && crate::ast::untyped_int_const(els) {
                fc.emit(Op::CoerceFloat, els.span);
            }
        }
        fc.patch_jump(end);
        Ok(())
    }

    fn compile_ident(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        // A bare nullary *built-in* variant used as a value (`None`) — resolved before any env
        // lookup. User variants are qualified (handled in the `Field`
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
            && fc.is_unbound(name)
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
                let snap = fc.snapshot_entries();
                let (kept, free) = filter_entries_free_block(&snap, body, &[]);
                let entries = self.with_witness_captures(&snap, kept, Some(&free));
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
                // M24 Task 4: the block reaches the enclosing frame's witnesses through
                // `with_witness_captures` above.
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
        // M24-5b — `defer Type.m(..)`: no receiver value to hold, so it rides the eager-args wrapper
        // instead of `Op::DeferMethod`.
        if self.receiverless_call_head(fc, callee) {
            let n = self.compile_receiverless_target(fc, callee, args, named, call.span)?;
            fc.emit(Op::DeferCall(n), call.span);
            return Ok(());
        }
        if let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &callee.kind
        {
            // `defer xs.sum()` over a scalar-numeric-newtype list needs the same hidden `Cents(0)`
            // seed the eager `Op::CallMethod` emit pushes — `Op::DeferMethod` lands in the very same
            // `Vm::do_method_call`, so without it the fold sees a `NewType` element and faults at
            // RUN time on a program that CHECKED clean. Method dispatch has exactly three opcodes
            // (`CallMethod`/`DeferMethod`/`SpawnMethod`); all three consult the seed.
            if let Some((nt_key, is_float)) = self.newtype_sum_seed(name, args, *name_span) {
                self.compile_expr(fc, obj)?;
                fc.emit(
                    if is_float {
                        Op::ConstFloat(0.0)
                    } else {
                        Op::ConstInt(0)
                    },
                    call.span,
                );
                fc.emit(Op::NewType(nt_key), call.span);
                fc.emit(Op::DeferMethod(name.clone(), 1), call.span);
                return Ok(());
            }
            self.compile_expr(fc, obj)?;
            self.compile_args(fc, args)?;
            // M24-5: the hidden witness arguments ride LAST, exactly as on the eager `Op::CallMethod`.
            let w = self.emit_member_witness_args(fc, callee, name, *name_span, call.span)?;
            fc.emit(Op::DeferMethod(name.clone(), args.len() + w), call.span);
            return Ok(());
        }
        // A deferred VALUE call carrying keyword arguments (Swift-style): reorder the combined
        // `[positional ++ named]` args by the checker-recorded permutation, then defer positionally —
        // same lowering as the eager value keyword call, just via `DeferCall`.
        if !named.is_empty() {
            let perm = self.keyword_perm(named, call.span)?;
            self.compile_expr(fc, callee)?;
            for &ci in &perm {
                let e = if ci < args.len() {
                    &args[ci]
                } else {
                    &named[ci - args.len()].1
                };
                self.compile_expr(fc, e)?;
            }
            // M24-5: TRAILING — after the permuted args, never in source order.
            let w = self.emit_indirect_witness_args(fc, callee, call.span)?;
            fc.emit(Op::DeferCall(perm.len() + w), call.span);
            return Ok(());
        }
        self.compile_expr(fc, callee)?;
        self.compile_args(fc, args)?;
        let w = self.emit_indirect_witness_args(fc, callee, call.span)?;
        fc.emit(Op::DeferCall(args.len() + w), call.span);
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

    /// Compile a positional argument list onto the stack (no float-field coercion — that is the
    /// struct-ctor-only job of [`compile_ctor_args`]). Replaces the flat `for a in args { compile_expr }`
    /// loop repeated at every non-struct call/variant/static/defer emit site.
    fn compile_args(&mut self, fc: &mut FnComp, args: &[Expr]) -> Result<(), CompileError> {
        for a in args {
            self.compile_expr(fc, a)?;
        }
        Ok(())
    }

    /// M24 Task 5 — the hidden witness params a MEMBER's proto carries, from the checker's `fns`
    /// table. Keyed `<type key>.<method>` under the DECLARING module, which cannot collide with a
    /// free fn's entry (no fn name contains a `.`).
    fn member_witnesses(&self, module_idx: usize, type_key: &str, method: &str) -> Vec<String> {
        self.witnesses
            .fns
            .get(&(module_idx, format!("{type_key}.{method}")))
            .cloned()
            .unwrap_or_default()
    }

    /// M24 Task 5 — the witness arguments recorded for a MEMBER call site (`h.make(c)`,
    /// `Holder.build(c)`, `lib.Type.build(c)`). Keyed on the member-name TOKEN
    /// ([`crate::checker::witness_key_span`]), which is unique per link of a postfix chain — the call
    /// span is not.
    /// W7-53 I1′ — is this `Field`-callee call the protocol-dispatched `.eq(x)` the checker marked?
    /// Keyed on the method-NAME token, exactly like [`Self::member_witness_srcs`] and the `?.`
    /// carriers. The `name`/arity re-test is not redundant: it keeps the lookup from ever firing on
    /// a call shape the checker could not have recorded.
    fn is_proto_eq_call(&self, name: &str, args: &[Expr], name_span: Span) -> bool {
        name == "eq"
            && args.len() == 1
            && self.proto_eq_calls.get(&crate::checker::carrier_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                name_span,
            )) == Some(&true)
    }

    /// The `T(0)` seed a `xs.sum()` site needs, per the checker's [`crate::checker::NewtypeSumTable`]
    /// — `Some((runtime type key, underlying-is-float))` for a scalar-numeric-newtype list. A MISS,
    /// or a recorded `None`, means the plain numeric sum (the pre-fix lowering), so a stale/absent
    /// entry can only under-apply. Keyed on the method-NAME token, which is distinct per link of a
    /// postfix/pipe chain (the call node's span is not — see [`crate::checker::CarrierKey`]).
    fn newtype_sum_seed(
        &self,
        name: &str,
        args: &[Expr],
        name_span: Span,
    ) -> Option<(String, bool)> {
        if name != "sum" || !args.is_empty() {
            return None;
        }
        self.newtype_sums
            .get(&crate::checker::carrier_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                name_span,
            ))?
            .clone()
    }

    fn member_witness_srcs(&self, name_span: Span) -> Option<&Vec<crate::checker::WitnessSrc>> {
        self.witnesses.calls.get(&crate::checker::witness_key(
            self.current_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        ))
    }

    /// Push `args` and emit `Op::CallStatic` for a `Type.method(args)` static call. The six call
    /// sites differ only in how they derive `type_key` (bare / qualified / turbofish); this collapses
    /// the identical push-then-emit tail — including the hidden witness arguments of a generic static
    /// method (M24 Task 5), which ride LAST so no declared slot index moves.
    fn emit_call_static(
        &mut self,
        fc: &mut FnComp,
        type_key: String,
        name: &str,
        args: &[Expr],
        name_span: Span,
        span: Span,
    ) -> Result<(), CompileError> {
        self.compile_args(fc, args)?;
        let w = match self.member_witness_srcs(name_span).cloned() {
            Some(srcs) => self.emit_witness_args(fc, &srcs, name, span)?,
            None => 0,
        };
        fc.emit(
            Op::CallStatic {
                type_key,
                method: name.to_string(),
                argc: args.len() + w,
            },
            span,
        );
        Ok(())
    }

    /// M24-5b — does this `spawn`/`defer` call target have a TYPE, rather than a value, at its head?
    /// `Op::SpawnMethod`/`DeferMethod` record a RECEIVER value plus a member name, and
    /// `Type.static_method(..)` has no receiver: compiling the head as a value panicked in
    /// [`Self::global_slot`] (a bare type name has no global slot) or — for a `from`-imported type,
    /// whose slot exists and holds `Nil` — faulted with `type nil has no method`. Both are answered
    /// by [`Self::compile_receiverless_target`] instead.
    ///
    /// The question asked is deliberately "does the head NAME A TYPE OR A MODULE" — a NAMESPACE
    /// rather than a value — not "which of [`Self::compile_call`]'s receiverless arms matches": one
    /// stable question rather than a second copy of that arm list to drift out of sync with. A head
    /// that is a local/capture (including one SHADOWING a type or module name) or any other value
    /// never answers yes, so every genuine receiver shape keeps its `SpawnMethod`/`DeferMethod`
    /// lowering.
    fn receiverless_call_head(&self, fc: &FnComp, callee: &Expr) -> bool {
        // A member-side turbofish (`Type[T].member[U](x)`) wraps the `Field` in an `Index`.
        let inner = match &callee.kind {
            ExprKind::Index { obj, .. } => obj,
            _ => callee,
        };
        let ExprKind::Field { obj, .. } = &inner.kind else {
            return false;
        };
        // `module.Type` / `module.Type[T…]` — a type reached through a bound module name.
        if self.qualified_turbofish_key(fc, &obj.kind).is_some() {
            return true;
        }
        // `module.helper(…)` — a plain call through a NAMESPACE. A module is not a receiver value,
        // so lowering it as one pushed the module HANDLE as the receiver: `defer` survives that
        // (it runs in the same task) but `spawn` cannot — the airlock refuses a module handle at
        // run time, on a program `chezzi check` had just passed. Replaying the call through the
        // wrapper proto emits exactly the module-member call the eager spelling emits, so nothing
        // module-shaped crosses. `is_unbound` first, so a local that merely SHADOWS the module name
        // — or is BOUND to it (`m := math`) — stays a genuine receiver. The checker's spawn-receiver
        // skip (`sig.rs`) asks this same question, on the same two clauses, so the check-time verdict
        // and the lowering cannot disagree; keying it there on the resolved `Ty::Module` alone was
        // exactly that disagreement (check ok, then a run-time airlock fault).
        if let ExprKind::Ident(mname) = &obj.kind
            && fc.is_unbound(mname)
            && self.imported_modules.contains_key(mname)
        {
            return true;
        }
        if let ExprKind::Field {
            obj: mobj,
            name: tname,
            ..
        } = &obj.kind
            && let ExprKind::Ident(mname) = &mobj.kind
            && fc.is_unbound(mname)
            && let Some(&tidx) = self.imported_modules.get(mname)
            && self
                .module_types
                .get(tidx)
                .is_some_and(|t| t.contains(tname))
        {
            return true;
        }
        // A bare `Type` / `Type[T…]` (local, `from`-imported or std — exactly `bare_types`), or a
        // generic type PARAM whose hidden `$w:T` witness is reachable here. `is_unbound` first, so a
        // local/param/loop var that merely SHADOWS a type name stays an ordinary receiver.
        let Some(head) = type_apply_head_name(&obj.kind).or(match &obj.kind {
            ExprKind::Ident(n) => Some(n.as_str()),
            _ => None,
        }) else {
            return false;
        };
        fc.is_unbound(head)
            && (self.bare_types.contains_key(head) || fc.witness_ref(head).is_some())
    }

    /// M24-5b — lower a receiverless `defer Type.m(a, b)` / `spawn Type.m(a, b)`. The arguments are
    /// evaluated EAGERLY here (Go semantics — `defer pkg.F(x)` snapshots `x`, and so does every other
    /// Chezzi `defer`/`spawn` argument) and handed to a synthetic wrapper proto whose body REPLAYS the
    /// original callee through [`Self::compile_call`] with the parameters standing in for the
    /// arguments. So every static spelling — bare, type-level turbofish, `from`-imported,
    /// module-qualified, combined turbofish, and the `$w:T` witness call — reaches exactly the
    /// bytecode its eager form emits, and there is no second dispatch list to keep in sync.
    ///
    /// The wrapper is a `MakeClosure` rather than a `MakeFunc` only so it can reach the enclosing
    /// frame's `$w:T` witnesses the way a `defer:` block does ([`Self::with_witness_captures`]); nothing
    /// else is captured, so a witness-free target is still a capture-free callee and crosses the
    /// `spawn` airlock by handle. Returns the operand count for `DeferCall`/`SpawnCall`.
    fn compile_receiverless_target(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        span: Span,
    ) -> Result<usize, CompileError> {
        let argc = args.len() + named.len();
        let snap = fc.snapshot_entries();
        // No free-name walk exists for a SYNTHESIZED body: the wrapper replays `callee(args)`, which
        // is a call site of the ENCLOSING body — so every witness rides (`None`), and the replay's
        // own `compile_call` finds whatever it needs.
        let entries = self.with_witness_captures(&snap, Vec::new(), None);
        let mut child = FnComp::new("<deferred static call>".to_string(), argc, false);
        child.captured_names = entries.iter().map(|e| e.name.clone()).collect();
        // One unspellable parameter per argument, in source order, so the replayed call reads them
        // back as ordinary locals. `$` is not an identifier character (same trick as `$w:`).
        let params: Vec<Expr> = (0..argc)
            .map(|i| {
                let n = format!("$a{i}");
                child.add_local(n.clone());
                Expr {
                    kind: ExprKind::Ident(n),
                    span,
                }
            })
            .collect();
        let kw: Vec<(String, Expr)> = named
            .iter()
            .map(|(k, _)| k.clone())
            .zip(params[args.len()..].iter().cloned())
            .collect();
        self.compile_call(&mut child, callee, &params[..args.len()], &kw, span)?;
        child.emit(Op::Return, span);
        let pid = self.finish(child);
        fc.emit(Op::MakeClosure(pid, entries), span);
        self.compile_args(fc, args)?;
        for (_, e) in named {
            self.compile_expr(fc, e)?;
        }
        Ok(argc)
    }

    /// M24-5 — the hidden witness arguments a `spawn`/`defer` VALUE-callee target must push on top
    /// of its already-pushed declared args; the count widens `Op::SpawnCall`/`Op::DeferCall`'s
    /// `argc`. Same by-name shape rule as the eager `Op::Call` site: only a bare `Ident` naming a
    /// module-level fn can be a witness call, so a chained head link (`defer mk(c)(5)`, whose callee
    /// is itself a `Call`) never reads its head's entry.
    fn emit_indirect_witness_args(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        span: Span,
    ) -> Result<usize, CompileError> {
        let ExprKind::Ident(fname) = &callee.kind else {
            return Ok(0);
        };
        if !fc.is_unbound(fname) {
            return Ok(0);
        }
        match self.witness_srcs(fc, callee, fname, span)? {
            Some(srcs) => {
                let fname = fname.clone();
                self.emit_witness_args(fc, &srcs, &fname, span)
            }
            None => Ok(0),
        }
    }

    /// M24-5 — the same, for a MEMBER target (`spawn h.make(c)`, `defer lib.reset(c)`). The
    /// module-QUALIFIED spelling goes through [`Self::witness_srcs`] so its stray-entry guard still
    /// applies; an instance method has no `witness_fn_key`, so it reads the checker's record at the
    /// method-NAME token directly ([`Self::member_witness_srcs`]) — the same key `Op::CallMethod`
    /// uses for the eager form.
    fn emit_member_witness_args(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        name: &str,
        name_span: Span,
        span: Span,
    ) -> Result<usize, CompileError> {
        let srcs = match self.witness_srcs(fc, callee, name, span)? {
            Some(srcs) => Some(srcs),
            None => self.member_witness_srcs(name_span).cloned(),
        };
        match srcs {
            Some(srcs) => self.emit_witness_args(fc, &srcs, name, span),
            None => Ok(0),
        }
    }

    /// M24 Task 3 — does the fn a call site NAMES take hidden trailing witness params?
    /// [`crate::checker::WitnessTable::fns`] is keyed by the module that DECLARES the fn, so the
    /// call site's own index is only right for a local callee; a `from`-imported one resolves through
    /// [`Self::imported_fns`] and a qualified one (`lib.reset(...)`) through the module bind. Callees
    /// this cannot classify (a value, a method, a local shadow) answer `false` — and the stray-entry
    /// guard in [`Self::witness_srcs`] is what keeps such a miss loud instead of one `argc` short.
    fn callee_takes_witnesses(&self, fc: &FnComp, callee: &Expr, fname: &str) -> bool {
        self.witness_fn_key(fc, callee, fname)
            .is_some_and(|k| self.witnesses.fns.contains_key(&k))
    }

    /// The `(module index, fn name)` [`crate::checker::WitnessTable::fns`] would key this callee
    /// under — the module that DECLARES it and the name it is DECLARED as (an `import reset as again`
    /// binding is keyed `reset`, not `again`) — or `None` when the callee is not a by-name call on a
    /// module-level fn.
    fn witness_fn_key(&self, fc: &FnComp, callee: &Expr, fname: &str) -> Option<(usize, String)> {
        match &callee.kind {
            ExprKind::Ident(_) if fc.is_unbound(fname) => self.witness_fn_key_named(None, fname),
            ExprKind::Field { obj, .. } => match &obj.kind {
                ExprKind::Ident(m) if fc.is_unbound(m) => self.witness_fn_key_named(Some(m), fname),
                _ => None,
            },
            _ => None,
        }
    }

    /// The module-resolution half of [`Self::witness_fn_key`], for a callee already reduced to the
    /// `(module, name)` pair a [`CallSite`] carries. `None` for a qualified head that names no
    /// imported module (a receiver value, or a type).
    fn witness_fn_key_named(&self, module: Option<&str>, name: &str) -> Option<(usize, String)> {
        match module {
            None => Some(
                self.imported_fns
                    .get(name)
                    .cloned()
                    .unwrap_or((self.current_module_idx, name.to_string())),
            ),
            Some(m) => self
                .imported_modules
                .get(m)
                .map(|&idx| (idx, name.to_string())),
        }
    }

    /// M24-2 — could a call the free-name walk recorded as `name(…)` / `head.name(…)` take hidden
    /// witness arguments? The [`nested_body_needs_witness`] lookup.
    ///
    /// A FIELD-HEADED call always answers yes. What the head `X` in `X.name(…)` denotes — an imported
    /// module, a struct/enum/newtype TYPE, a receiver value, or a module name shadowed by an enclosing
    /// binding — is a question about the enclosing scopes and the type namespace that this syntactic
    /// walk does not carry, and two rounds of guessing at it each shipped a check-ok/run-fault (a
    /// shadowing param, then a type named like an imported module). Answering yes for every head
    /// retires the whole class by construction: there is no head shape left to classify.
    ///
    /// So a NO comes only from a BARE-IDENT callee, resolved through the real
    /// [`crate::checker::WitnessTable::fns`] table — a call the compiler itself would thread no
    /// witness into at a real call site.
    fn call_may_take_witnesses(&self, module: Option<&str>, name: &str) -> bool {
        module.is_some()
            || self
                .witness_fn_key_named(None, name)
                .is_some_and(|k| self.witnesses.fns.contains_key(&k))
    }

    /// M24 Task 4 — append the enclosing frame's hidden `$w:T` bindings to a nested body's capture
    /// entries (closure, nested `fn`, `spawn:`/`defer:` block). They can never survive the
    /// free-variable filter — `$` is unspellable, so no source name makes one free — so a child proto
    /// reaches its witness only because this puts it back. `snapshot` is the SAME `snapshot_entries()`
    /// the free filter ran over, so a `$w:T` the enclosing frame itself only CAPTURED (a closure in a
    /// closure) rides along with its `CapSrc::Captured` stamp already correct.
    ///
    /// M24-2 — a witness rides only when the body can REACH it ([`nested_body_needs_witness`], which
    /// states the invariant that keeps `FnComp::witness_ref` a superset of the checker's
    /// `witness_scope`). `free` is that body's already-computed free-name walk; `None` means the body
    /// is SYNTHESIZED rather than written — the receiverless `defer Type.m(…)` wrapper replays a call
    /// that is in no walk at all — so every witness rides, as it did everywhere before.
    fn with_witness_captures(
        &self,
        snapshot: &[CapEntry],
        mut entries: Vec<CapEntry>,
        free: Option<&FreeNames>,
    ) -> Vec<CapEntry> {
        entries.extend(
            snapshot
                .iter()
                .filter(|e| match e.name.strip_prefix(WITNESS_PREFIX) {
                    // The suffix is the whole type-PARAM name (`witness_local` formats exactly
                    // `$w:<t>`, and a type param is a bare identifier — never module-qualified).
                    Some(t) => free.is_none_or(|f| {
                        nested_body_needs_witness(f, t, &|m, n| self.call_may_take_witnesses(m, n))
                    }),
                    None => false,
                })
                .cloned(),
        );
        entries
    }

    /// M24 — the witness arguments this call site must push, `None` when the callee takes none.
    /// BOTH directions are hard errors, never a silent short `argc`: a callee that needs witnesses
    /// with no recorded entry, and a recorded entry at a call the compiler would not thread (the
    /// checker recorded a witness the backend is about to drop).
    fn witness_srcs(
        &self,
        fc: &FnComp,
        callee: &Expr,
        fname: &str,
        span: Span,
    ) -> Result<Option<Vec<crate::checker::WitnessSrc>>, CompileError> {
        // Only a BY-NAME call on a module-level fn can be a witness call, and only such a callee is
        // held to the stray-entry half of the guard below. A chained postfix link shares its
        // primary expression's span (`lib.reset(c).tag(1)` keys where `lib.reset(c)` recorded), so a
        // blanket check would read the head link's entry and reject a legal program.
        if self.witness_fn_key(fc, callee, fname).is_none() {
            return Ok(None);
        }
        let key = crate::checker::witness_key(
            self.current_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            crate::checker::witness_key_span(callee, span),
        );
        let recorded = self.witnesses.calls.get(&key);
        match (self.callee_takes_witnesses(fc, callee, fname), recorded) {
            (true, Some(srcs)) => Ok(Some(srcs.clone())),
            (true, None) => Err(CompileError {
                message: format!(
                    "internal: no static-type witness recorded for the call to '{fname}'"
                ),
                span,
            }),
            (false, Some(_)) => Err(CompileError {
                message: format!(
                    "internal: a static-type witness is recorded for the call to '{fname}', which \
                     this call site does not thread"
                ),
                span,
            }),
            (false, None) => Ok(None),
        }
    }

    /// M24 — push one argument per witness slot, on top of the already-pushed declared args: a
    /// `ConstStr` type-identity key for a CONCRETE witness, or a load of the caller's own `$w:p` local
    /// for a FORWARDED one. Returns how many were pushed, which widens the call's `argc`.
    fn emit_witness_args(
        &mut self,
        fc: &mut FnComp,
        srcs: &[crate::checker::WitnessSrc],
        fname: &str,
        span: Span,
    ) -> Result<usize, CompileError> {
        for src in srcs {
            match src {
                crate::checker::WitnessSrc::Concrete(k) => fc.emit(Op::ConstStr(k.clone()), span),
                // FORWARDING: the caller's own witness for `p` IS the argument — its `$w:p` local,
                // or (Task 4, inside a nested body) the capture of it. The checker records this only
                // where that witness is reachable (`Checker::witness_scope`), so a missing one is an
                // internal invariant break, never a short `argc`.
                crate::checker::WitnessSrc::Forward(p) => {
                    let Some(w) = fc.witness_ref(p) else {
                        return Err(CompileError {
                            message: format!(
                                "internal: no type witness in scope to forward as '{p}' into \
                                 the call to '{fname}'"
                            ),
                            span,
                        });
                    };
                    fc.emit_witness(w, span);
                }
            }
        }
        Ok(srcs.len())
    }

    fn compile_call(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        span: Span,
    ) -> Result<(), CompileError> {
        // A default-argument provider call whose declaring module this one cannot name — no synthetic
        // import was (or could be) emitted for it, so there is no global slot to read. Lower it to a
        // direct, call-time reference to the definer's proto. See [`Op::MakeFuncIn`] and
        // `desugar::Walker::splice_default`. Same-module and in-closure providers have a global slot
        // and fall through to the ordinary path below.
        if let ExprKind::Ident(n) = &callee.kind
            && n.starts_with(crate::desugar::PROVIDER_PREFIX)
            && fc.is_unbound(n)
            && !self.globals.contains_key(n)
        {
            let id = self.provider_id(n);
            fc.emit(Op::MakeFuncIn(id), span);
            fc.emit(Op::Call(0), span);
            return Ok(());
        }
        // Method / module-member call: `obj.name(args)`.
        if let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &callee.kind
        {
            // `module.Struct(args)` → qualified struct constructor. `module` is a bound module name
            // whose target declares struct `name`; emit `NewStruct` keyed by that module's runtime key.
            if let ExprKind::Ident(mname) = &obj.kind
                && fc.is_unbound(mname)
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
                    self.compile_args(fc, args)?;
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
                && fc.is_unbound(mname)
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
                    self.compile_args(fc, args)?;
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
                && fc.is_unbound(mname)
                && let Some(&tidx) = self.imported_modules.get(mname)
                && self
                    .module_types
                    .get(tidx)
                    .is_some_and(|t| t.contains(tname))
                && let key = self.type_key(tidx, tname)
                && self.static_methods.contains(&static_key(&key, name))
            {
                self.emit_call_static(fc, key, name, args, *name_span, span)?;
                return Ok(());
            }
            // M24 — `T.method(args)` through a generic bound's STATIC requirement. NO table lookup is
            // needed here: `T` is a type PARAM, and this compiler itself created the `$w:T` binding —
            // a trailing param of the fns the checker's `fns` table named, or (Task 4) a capture of
            // one in a nested body — so "a `Field` call on a bare `T` for which a `$w:T` binding is
            // reachable" IS the witness call. That reach (`FnComp::witness_ref`) is the compiler's
            // half of `Checker::witness_scope`.
            //
            // PLACED FIRST among the bare-`Ident` receiver arms, because that is where the CHECKER
            // puts it (`infer_call`: the type-param arm runs before the enum/struct static arms). A
            // type param SHADOWS a real type name (`fn f[Item: Tagged](x: Item)` next to a `struct
            // Item` means the PARAM — Rust's and Go's answer, measured 2026-08-10), and both halves
            // must mean the same `Item`: resolving the struct here while the checker resolved the
            // param is a green `chezzi check` followed by a wrong runtime answer.
            if let ExprKind::Ident(t) = &obj.kind
                && fc.is_unbound(t)
                && let Some(w) = fc.witness_ref(t)
            {
                self.compile_args(fc, args)?;
                // The SAME `witness_ref` that admitted this call emits it — there is no second
                // lookup that could come back empty and leave `CallStaticDyn` reading an operand.
                fc.emit_witness(w, span);
                fc.emit(
                    Op::CallStaticDyn {
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
                && fc.is_unbound(ename)
                && let ekey = self.enum_bare_key(ename)
                && self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
            {
                self.compile_args(fc, args)?;
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
                && fc.is_unbound(tname)
                && let Some(key) = self.bare_types.get(tname).cloned()
                && self.static_methods.contains(&static_key(&key, name))
            {
                self.emit_call_static(fc, key, name, args, *name_span, span)?;
                return Ok(());
            }
            // `Type[T…].Variant(args)` → declaration-site turbofish VARIANT constructor
            // (`Box[int].Full(9)`, `E[int, str].Pair(…)`). The type args are RUNTIME-erased (they
            // only drove the checker), so emit `Op::NewEnum` by the bare key — identical bytecode to
            // the bare `Enum.Variant(args)` form. Both carriers converge: single-arg `Index{Ident}`
            // and multi-arg `TypeApply{name}`. VARIANT-FIRST (a same-named static is barred at decl
            // time), mirroring the checker.
            if let Some(tname) = type_apply_head_name(&obj.kind)
                && fc.is_unbound(tname)
                && let ekey = self.enum_bare_key(tname)
                && self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
            {
                self.compile_args(fc, args)?;
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
                && fc.is_unbound(tname)
                && let Some(key) = self.bare_types.get(tname).cloned()
                && self.static_methods.contains(&static_key(&key, name))
            {
                self.emit_call_static(fc, key, name, args, *name_span, span)?;
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
                    self.compile_args(fc, args)?;
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
                    self.emit_call_static(fc, key, name, args, *name_span, span)?;
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
                && fc.is_unbound(mname)
                && let Some(&tidx) = self.imported_modules.get(mname)
                && let Some(nat) = self.program.modules.get(tidx).and_then(|m| m.native)
            {
                let op = match (nat, name.as_str()) {
                    ("std.concurrency", "Shared") => Some(Op::NewShared),
                    ("std.concurrency", "RwShared") => Some(Op::NewRwShared),
                    ("std.concurrency", "Atomic") => Some(Op::NewAtomic),
                    ("std.concurrency", "AtomicInt") => Some(Op::NewAtomicInt),
                    ("std.concurrency", "Executor") => Some(Op::NewExecutor),
                    ("std.time", "timer") => Some(Op::NewTimer),
                    _ => None,
                };
                if let Some(op) = op {
                    self.compile_args(fc, args)?;
                    fc.emit(op, span);
                    return Ok(());
                }
            }
            // M24 Task 3 — `module.fn(args)` where the member is a generic fn that takes hidden
            // trailing witness params. Same shape as the bare spelling, one opcode later: the member
            // is looked up on the module object and the witness arguments ride on top of the declared
            // ones, so `CallMethod`'s widened `argc` reaches the SAME proto (which the declaring
            // module compiled with those hidden params) as `reset(...)` would.
            if let Some(srcs) = self.witness_srcs(fc, callee, name, span)? {
                self.compile_expr(fc, obj)?;
                self.compile_args(fc, args)?;
                let w = self.emit_witness_args(fc, &srcs, name, span)?;
                let ic = self.next_method_ic();
                fc.emit(
                    Op::CallMethod {
                        name: name.clone(),
                        argc: args.len() + w,
                        ic,
                    },
                    span,
                );
                return Ok(());
            }
            // `xs.sum()` over a scalar-numeric-newtype list (`List[Cents]`): push a `Cents(0)` SEED as
            // `sum`'s one hidden argument, so the runtime folds through the same-newtype `+` path
            // (unwrap → native checked op → rewrap) and answers `Cents`. The seed IS the answer for an
            // EMPTY list, which is why it must come from here: the backend is type-blind and an empty
            // list carries no element to read a `type_key` off. The checker decided it; a miss falls
            // through to the plain numeric lowering below.
            if let Some((nt_key, is_float)) = self.newtype_sum_seed(name, args, *name_span) {
                self.compile_expr(fc, obj)?;
                fc.emit(
                    if is_float {
                        Op::ConstFloat(0.0)
                    } else {
                        Op::ConstInt(0)
                    },
                    span,
                );
                fc.emit(Op::NewType(nt_key), span);
                let ic = self.next_method_ic();
                fc.emit(
                    Op::CallMethod {
                        name: name.clone(),
                        argc: 1,
                        ic,
                    },
                    span,
                );
                return Ok(());
            }
            // W7-53 I1′ — `.eq(x)` through a generic bound (`fn f[T: Eq](a: T, b: T): a.eq(b)`) is
            // the PROTOCOL's equality, not whatever ordinary method the receiver happens to spell
            // `eq`. Lower it to the very opcode `==` uses, so the two spellings are one dispatch by
            // construction: `Op::Eq` runs `values_equal_guarded`, which already dispatches a real
            // user `eq` HOOK and falls back to structural equality otherwise. Rust resolves the same
            // call to `<T as PartialEq>::eq`; a CONCRETE receiver keeps the inherent-wins rule and
            // is recorded `false` by the checker, so it never lands here.
            if self.is_proto_eq_call(name, args, *name_span) {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, &args[0])?;
                fc.emit(Op::Eq, span);
                return Ok(());
            }
            // M24 Task 5 — an INSTANCE method that declares its own witnessed `[T]`
            // (`h.make(c)`): the hidden witness arguments ride LAST, after the declared args, so
            // `CallMethod`'s widened `argc` matches the proto the method was compiled with. The
            // checker's record is keyed on the method-name token, which is unique per link of a
            // postfix chain (`h.make(a).make(b)` shares one call span, not one name token).
            self.compile_expr(fc, obj)?;
            self.compile_args(fc, args)?;
            let w = match self.member_witness_srcs(*name_span).cloned() {
                Some(srcs) => self.emit_witness_args(fc, &srcs, name, span)?,
                None => 0,
            };
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: name.clone(),
                    argc: args.len() + w,
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
                obj: head,
                name,
                name_span,
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
                    self.compile_args(fc, args)?;
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
                    self.emit_call_static(fc, key, name, args, *name_span, span)?;
                    return Ok(());
                }
            }
            let tname = type_apply_head_name(&head.kind).or(match &head.kind {
                ExprKind::Ident(n) => Some(n.as_str()),
                _ => None,
            });
            if let Some(tname) = tname
                && fc.is_unbound(tname)
            {
                // VARIANT-FIRST (a same-named static is barred at decl time), mirroring the checker.
                let ekey = self.enum_bare_key(tname);
                if self
                    .program
                    .variants
                    .contains_key(&(ekey.clone(), name.clone()))
                {
                    self.compile_args(fc, args)?;
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
                    self.emit_call_static(fc, key, name, args, *name_span, span)?;
                    return Ok(());
                }
            }
        }
        // Bare-ident callees resolve by name in this order:
        // print → builtin → struct ctor → variant ctor → value.
        if let ExprKind::Ident(name) = &callee.kind {
            // Concurrency C4: `Channel[T]()` → a fresh mailbox; `Shared(v)` → a fresh box over the
            // deep-copied init value. The checker validated arity (Channel: 0 args, Shared: 1).
            if name == "Channel" {
                // `Channel[T](cap)` — compile the optional capacity expr so it sits on the operand
                // stack for `NewChannel(true)` to pop. `Channel[T]()` → `NewChannel(false)` (unbounded).
                let has_cap = !args.is_empty();
                if has_cap {
                    self.compile_expr(fc, &args[0])?;
                }
                fc.emit(Op::NewChannel(has_cap), span);
                return Ok(());
            }
            if name == "Shared" {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewShared, span);
                return Ok(());
            }
            // `RwShared(v)` → a fresh read-write box over the deep-copied init value (checker: 1 arg).
            if name == "RwShared" {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewRwShared, span);
                return Ok(());
            }
            // `Atomic(v)` → a fresh atomic box over the deep-copied init value (checker validated 1 arg).
            if name == "Atomic" {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewAtomic, span);
                return Ok(());
            }
            // `AtomicInt(v)` → a fresh lock-free int atomic (checker validated 1 int arg).
            if name == "AtomicInt" {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewAtomicInt, span);
                return Ok(());
            }
            // `timer(ms)` → a fresh one-shot timeout channel (checker validated 1 int arg).
            if name == "timer" {
                self.compile_args(fc, args)?;
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
                self.compile_args(fc, args)?;
                match p.intrinsic {
                    crate::checker::Intrinsic::Print => {
                        if named.is_empty() {
                            // Plain `print(...)`: byte-identical (space-join, trailing newline).
                            fc.emit(Op::CallPrint(args.len()), span);
                        } else {
                            // `print(..., sep=, end=)`: push `sep` then `end` (each the user expr or
                            // its default str), then a dedicated op joins+terminates. Eval order:
                            // positional args, then sep, then end.
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
                self.compile_args(fc, args)?;
                fc.emit(Op::CallBuiltin(name.clone(), args.len()), span);
                return Ok(());
            }
            // A bare newtype ctor: `UserId(x)` wraps the single arg. Resolved exactly like the struct
            // ctor — only a BARE-resolvable newtype in THIS module — keyed by its runtime key.
            if let Some(nt_key) = self.bare_types.get(name).cloned()
                && self.program.newtype_home.contains_key(&nt_key)
            {
                self.compile_args(fc, args)?;
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
                self.compile_args(fc, args)?;
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
        // M24 — a call to a generic fn that takes hidden trailing witness params, by its bare name
        // (declared here, or `from`-imported): push the declared args, then one argument per witness
        // slot, and call with the widened `argc`. The compiler CONSUMES the checker's table; a
        // mismatch either way is a hard error, never a short `argc` (see `witness_srcs`).
        if let ExprKind::Ident(fname) = &callee.kind
            && fc.is_unbound(fname)
            && let Some(srcs) = self.witness_srcs(fc, callee, fname, span)?
        {
            // `named` needs no handling here: desugar has already normalized every keyword argument
            // of a by-name call into its positional slot, so `args` is the full argument list.
            self.compile_expr(fc, callee)?;
            self.compile_args(fc, args)?;
            let w = self.emit_witness_args(fc, &srcs, fname, span)?;
            fc.emit(Op::Call(args.len() + w), span);
            return Ok(());
        }
        // General callable value.
        // Swift-style keyword arguments through a function VALUE (`g(name="Bob")`): the checker
        // recorded a slot PERMUTATION over the combined `[positional args ++ named exprs]` list. Emit
        // the callee, then the combined exprs in slot order, and a plain positional `Op::Call` — the
        // runtime ABI is unchanged. Positional-only calls (`named` empty) never consult the table.
        if !named.is_empty() {
            let perm = self.keyword_perm(named, span)?;
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
        self.compile_args(fc, args)?;
        fc.emit(Op::Call(args.len()), span);
        Ok(())
    }

    /// The checker-recorded slot PERMUTATION for a value call carrying keyword arguments. A MISSING
    /// entry is a hard error, never a fall-through: the plain positional `Op::Call(args.len())` this
    /// used to fall through to silently DROPPED every named argument — a wrong value under a green
    /// `chezzi check`, where the carrier (`compile_expr`'s `?.` arm) and witness (`witness_srcs`)
    /// lookups both faulted (`docs/gaps.md` W7-49). Only ever called with `named` non-empty, which is
    /// exactly the condition the checker records under; `print(sep=…, end=…)` — the one path that
    /// deliberately leaves `named` populated without a table entry — returns from the `prelude_fn`
    /// arm long before any of the three call sites, and `spawn`/`defer` of a `print` with named args
    /// is a type error.
    fn keyword_perm(
        &self,
        named: &[(String, Expr)],
        span: Span,
    ) -> Result<Vec<usize>, CompileError> {
        self.keyword_calls
            .get(&crate::checker::keyword_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                named,
                span,
            ))
            .cloned()
            .ok_or_else(|| CompileError {
                message: "internal: no keyword-argument permutation recorded for this call — the \
                          type-checker and the backend disagree about this expression"
                    .to_string(),
                span,
            })
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
        let params_set: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut free = FreeNames::default();
        free_names_expr(body, &params_set, &mut free);
        let snap = fc.snapshot_entries();
        // M24 Task 4: a closure captures only FREE VARIABLES, and the enclosing frame's `$w:T` is
        // never spelled in the source — so it is appended explicitly (M24-2: when this body can
        // reach it), and travels BY VALUE (the point of case 1: a closure that outlives its defining
        // frame still constructs the right type).
        let entries: Vec<CapEntry> = self.with_witness_captures(
            &snap,
            snap.iter()
                .filter(|e| free.names.contains(&e.name))
                .cloned()
                .collect(),
            Some(&free),
        );
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
    fn compile_str(
        &mut self,
        fc: &mut FnComp,
        raw: &crate::ast::StrLit,
        span: Span,
    ) -> Result<(), CompileError> {
        let chunks = parse_interpolation(raw, span).map_err(|e| CompileError {
            message: e.message,
            span: e.span,
        })?;
        self.compile_interp(fc, &chunks, span)
    }

    /// Emit an already-parsed interpolation — the desugared [`ExprKind::Interp`] path, and the body
    /// of [`Self::compile_str`]'s fallback for a literal `desugar` left un-parsed.
    fn compile_interp(
        &mut self,
        fc: &mut FnComp,
        chunks: &[Chunk],
        span: Span,
    ) -> Result<(), CompileError> {
        if let [Chunk::Lit(s)] = chunks {
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
                Chunk::Lit(s) => fc.emit(Op::ConstStr(s.clone()), span),
                Chunk::Expr(e, spec) => {
                    self.kw_frag_ctx = span;
                    self.kw_frag_ord = ord;
                    // The fragment root is compiled with its OWN span, so a RUNTIME fault inside a
                    // fragment keeps the span it had before M24. (The checker re-anchors its
                    // fragment-root DIAGNOSTIC at the string literal — a diagnostic anchor, not a
                    // key: every per-call table keys on a sub-node the rewrite cannot touch, the
                    // CALLEE TOKEN for `WitnessTable::calls` and the first named-arg value for
                    // `KeywordTable`. `49bd9f80` conflated the two; keeping them separate is what
                    // lets both halves be right.)
                    self.compile_expr(fc, e)?;
                    match spec {
                        None => fc.emit(Op::ToStr, span),
                        Some(fs) => fc.emit(Op::ToStrFmt(Box::new(fs.clone())), span),
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

/// The `AssertCmp` an `assert`'s top-level condition renders, if it is a comparison. `In` is
/// excluded — its right operand is a whole collection, so rendering it would turn a one-line fault
/// into an unbounded dump — and `And`/`Or` never reach `binary_op` at all (short-circuit path).
fn assert_cmp(op: BinaryOp) -> Option<AssertCmp> {
    match op {
        BinaryOp::Lt => Some(AssertCmp::Lt),
        BinaryOp::LtEq => Some(AssertCmp::LtEq),
        BinaryOp::Gt => Some(AssertCmp::Gt),
        BinaryOp::GtEq => Some(AssertCmp::GtEq),
        BinaryOp::Eq => Some(AssertCmp::Eq),
        BinaryOp::NotEq => Some(AssertCmp::NotEq),
        _ => None,
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
                    match &arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            collect_frame_binds_expr(chan, out);
                            match target {
                                WaitTarget::Bind(n) => {
                                    out.insert(n.clone());
                                }
                                WaitTarget::Assign(e) => collect_frame_binds_expr(e, out),
                                WaitTarget::Discard => {}
                            }
                        }
                        WaitArmKind::Send { call } => collect_frame_binds_expr(call, out),
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
                    match &arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            find_boundary_free_expr(chan, out);
                            if let WaitTarget::Assign(e) = target {
                                find_boundary_free_expr(e, out);
                            }
                        }
                        WaitArmKind::Send { call } => find_boundary_free_expr(call, out),
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
                out.extend(free_names_of_block(b, &HashSet::new()))
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
                out.extend(free_names_of_block(&decl.body, &params));
            }
            // Remaining statements contain no capture boundary.
            _ => {}
        }
    }
}

/// The fragment expressions of an already-parsed interpolation (`ExprKind::Interp`) — the desugared
/// counterpart of [`interp_exprs`], with no re-parse. This is the path a compiled program actually
/// takes; `interp_exprs` below remains for a literal `desugar` left un-parsed.
fn chunk_exprs(chunks: &[Chunk]) -> impl Iterator<Item = &Expr> {
    chunks.iter().filter_map(|c| match c {
        Chunk::Expr(e, _) => Some(e),
        Chunk::Lit(_) => None,
    })
}

/// Best-effort: the interpolation sub-expressions of a string literal (`"a{x}b"` → the `x` expr).
/// Used by the capture pre-pass so a name referenced ONLY inside a `{…}` interpolation is still seen
/// as a free variable (and therefore boxed) — in an un-desugared `Str` the interpolation exprs are
/// embedded in the raw text, so the AST walk would otherwise miss them. A malformed interpolation
/// yields no exprs here; the real `compile_str` surfaces that error.
fn interp_exprs(raw: &crate::ast::StrLit) -> Vec<Expr> {
    match parse_interpolation(raw, Span::RUNTIME) {
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
            out.extend(free_names_of_expr(body, &bound));
        }
        // A string literal may carry `{…}` interpolation exprs (a closure could nest in one).
        ExprKind::Str(raw) => {
            for ie in interp_exprs(raw) {
                find_boundary_free_expr(&ie, out);
            }
        }
        ExprKind::Interp(chunks) => {
            for ie in chunk_exprs(chunks) {
                find_boundary_free_expr(ie, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
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
///
/// Returns the walk alongside the entries: M24-2's witness question is asked of the SAME walk, never
/// a second one over the same body.
fn filter_entries_free_block(
    entries: &[CapEntry],
    stmts: &[Stmt],
    params: &[crate::ast::Param],
) -> (Vec<CapEntry>, FreeNames) {
    let bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut free = FreeNames::default();
    free_names_block(stmts, &bound, &mut free);
    let kept = entries
        .iter()
        .filter(|e| free.names.contains(&e.name))
        .cloned()
        .collect();
    (kept, free)
}

/// What a free-variable walk collects. `names` is the free-variable answer every capture consumer
/// wants. `calls` is the walk's second channel: the CALL STRUCTURE of every bare (`f(…)`) or
/// module-qualified (`m.f(…)`) call it passed, which M24's witness-forwarding charge asks
/// per-call-site (`Checker::witness_params_of`). A name alone cannot answer that question — naming
/// `lib` and having *some* member spelled `reset` is not a call to `lib.reset`, and a call whose
/// arguments are all concrete forwards nothing of the caller's own type param.
#[derive(Default)]
pub(crate) struct FreeNames {
    pub names: HashSet<String>,
    pub calls: Vec<CallSite>,
    /// True once the walk passed a call whose callee it could NOT put in `calls` — a method on a
    /// non-ident receiver (`a.b.c()`), an indexed one (`xs[i].m()`), a call result (`f()()`), a
    /// bound-name head. Such a call can still be a witness call: M24 Task 5 threads witnesses
    /// through MEMBERS too, and the checker keys those on the method-NAME TOKEN, a coordinate this
    /// syntactic walk does not carry. So the readers that must not UNDER-approximate
    /// ([`nested_body_needs_witness`]) treat it as "could be one".
    pub opaque_calls: bool,
    /// Every `<anything>.m(…)` / `<anything>?.m(…)` the walk passed — recorded for EVERY field-headed
    /// callee, including the ones that also land in `calls` (`lib.f(…)`, `T.default()`), since the
    /// head's MEANING is exactly what those two channels disagree about.
    /// `Checker::witness_params_of` uses it to charge a body whose only witness use is a MEMBER
    /// forward (`h.build[T](x)`), which appears in neither `names` (`T` is a type ARGUMENT) nor
    /// `calls` (the head is a receiver).
    ///
    /// Empty unless [`Self::record_members`] is set — only the checker reads this channel, and the
    /// compiler runs the same walk at every capture site.
    pub member_calls: Vec<MemberCall>,
    /// Opt in to [`Self::member_calls`]. Off by default so the compiler's per-capture-site walks —
    /// which read only `names` / `opaque_calls` / `calls` — allocate nothing for it.
    pub record_members: bool,
}

/// M24-2 — one MEMBER call (`recv.m(…)`), carrying the three things a PRE-TYPE walk can hand the
/// forwarding charge: the method NAME, the call's own TYPE ARGUMENTS, and every identifier occurring
/// anywhere in an argument expression. `Checker::member_call_forwards_a_witness` charges its
/// enclosing fn only when BOTH halves answer yes — this call site carries something of that fn's (a
/// type parameter in the turbofish `h.make[T](x)`, or a value parameter whose annotation mentions
/// one, `h.make(x)` with `x: T`) AND the NAME is declared as a witness-taking member somewhere in
/// the module graph. The name is a NECESSARY condition, never a sufficient one: alone it made one
/// unpinnable `get` poison every `m.get("a")` in the program; alone the call-site half charged
/// `sink.push(x)` on a BUILTIN `List`, which can never take a witness at all.
pub(crate) struct MemberCall {
    /// The method name (`h.make(x)` → `make`). Matched against the graph-wide index of method names
    /// that some declaration DOES take a witness for — a receiver whose method name is nowhere
    /// declared witness-taking is a builtin or a plain method, and cannot be a forward.
    pub name: String,
    /// The call's explicit type arguments (`h.make[T](x)` → `[T]`); empty for an inferred call and
    /// for `a?.m(…)`, which has no type-argument syntax.
    pub type_args: Vec<crate::ast::Type>,
    /// Every identifier occurring in any positional or named argument, at any depth and with NO
    /// bound-name subtraction — `h.make(f(xs[0]))` records `f`, `xs`. Depth and the empty bound set
    /// are both the CHARGE direction: a name that turns out to be a local rather than the enclosing
    /// fn's parameter merely over-charges, while missing one under-charges.
    pub arg_idents: Vec<String>,
}

/// One call the free-name walk passed, recorded only when the callee is a plain name or a
/// `module.name` pair (every other callee shape — a method receiver, an indexed value, a closure
/// result — records nothing and is invisible to the readers of this channel).
pub(crate) struct CallSite {
    /// `Some(m)` for `m.f(…)`, `None` for a bare `f(…)`.
    pub module: Option<String>,
    /// The callee name `f`.
    pub name: String,
    /// `Some(heads)` when EVERY argument (positional and named) is provably closed — a literal, or
    /// an ident-headed call whose own arguments are all closed — and `heads` lists those ident
    /// heads, so a reader can ask what each one names. `None` means "not provably closed", which is
    /// the DEFAULT for every shape not listed in [`closed_expr`]: an EMPTY argument list (a
    /// zero-arg generic call takes its type from the expected type, invisible here), an explicit
    /// turbofish (`f[T](1)` names the caller's own type param), an identifier, a field read, an
    /// operator — anything whose type this syntactic walk cannot pin.
    pub closed_arg_heads: Option<Vec<String>>,
}

/// Record `f(…)` / `m.f(…)` on `out`. A callee that is neither a free name nor a free name's field
/// is not recorded at all — it raises [`FreeNames::opaque_calls`] instead, so a reader that must not
/// under-approximate can see that SOME call went unclassified.
fn record_call_site(
    callee: &Expr,
    args: &[Expr],
    named: &[(String, Expr)],
    type_args: &[crate::ast::Type],
    bound: &HashSet<String>,
    out: &mut FreeNames,
) {
    let (module, name) = match &callee.kind {
        ExprKind::Ident(n) if !bound.contains(n) => (None, n.clone()),
        ExprKind::Field { obj, name, .. } => {
            // Every field-headed callee is a possible MEMBER call, whatever its head turns out to be.
            if out.record_members {
                out.member_calls
                    .push(member_call(name, args, named, type_args.to_vec()));
            }
            match &obj.kind {
                ExprKind::Ident(m) if !bound.contains(m) => (Some(m.clone()), name.clone()),
                _ => {
                    out.opaque_calls = true;
                    return;
                }
            }
        }
        _ => {
            out.opaque_calls = true;
            return;
        }
    };
    out.calls.push(CallSite {
        module,
        name,
        closed_arg_heads: closed_arg_heads(args, named, type_args, bound),
    });
}

/// Build a [`MemberCall`] from one member call site's name, arguments and type arguments.
fn member_call(
    name: &str,
    args: &[Expr],
    named: &[(String, Expr)],
    type_args: Vec<crate::ast::Type>,
) -> MemberCall {
    let nothing_bound = HashSet::new();
    let arg_idents = args
        .iter()
        .chain(named.iter().map(|(_, v)| v))
        .flat_map(|a| free_names_of_expr(a, &nothing_bound))
        .collect();
    MemberCall {
        name: name.to_string(),
        type_args,
        arg_idents,
    }
}

/// `Some(heads)` iff every argument is provably closed (see [`CallSite::closed_arg_heads`]).
fn closed_arg_heads(
    args: &[Expr],
    named: &[(String, Expr)],
    type_args: &[crate::ast::Type],
    bound: &HashSet<String>,
) -> Option<Vec<String>> {
    if !type_args.is_empty() || (args.is_empty() && named.is_empty()) {
        return None;
    }
    let mut heads = Vec::new();
    for a in args.iter().chain(named.iter().map(|(_, v)| v)) {
        if !closed_expr(a, bound, &mut heads) {
            return None;
        }
    }
    Some(heads)
}

/// Is `e`'s type fixed by its own syntax, independent of any enclosing type parameter? Literals are
/// (`"a{x}"` is `str` whatever `x` is — and a call INSIDE the interpolation is its own [`CallSite`],
/// judged separately), and so is an ident-headed call whose arguments are all closed — its head is
/// pushed onto `heads` for the reader to identify. A head SHADOWED by a binding in scope is not the
/// declaration the reader would look up, so it is not closed. Everything else defaults to NOT closed
/// — a bare ident included, `nil` with it: `nil` has no value spelling in this language (`f(nil)`,
/// `x: int? = nil` and a `= nil` parameter default are all "unknown name 'nil'"), so an arm admitting
/// it could only ever loosen the charge for a program the checker already rejects.
fn closed_expr(e: &Expr, bound: &HashSet<String>, heads: &mut Vec<String>) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Str(_)
        | ExprKind::RawStr(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_) => true,
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
        } => match &callee.kind {
            ExprKind::Ident(n) if type_args.is_empty() && !bound.contains(n) => {
                heads.push(n.clone());
                args.iter()
                    .chain(named.iter().map(|(_, v)| v))
                    .all(|a| closed_expr(a, bound, heads))
            }
            _ => false,
        },
        _ => false,
    }
}

/// M24-2 — can this nested body (closure, nested `fn`, `spawn:`/`defer:` block) REACH a witness for
/// the enclosing frame's type param `t`? `free` is the body's already-computed free-name walk;
/// `is_witness_fn` answers "could a call spelled `(module, name)` take hidden witness arguments?".
///
/// **The invariant: the compiler captures `$w:t` into a nested body whenever that body can reach a
/// witness for `t`.** `$w:` is unspellable (`$` is not an identifier character), so a body reaches a
/// witness only by CALLING something that consumes one, and every call the walk passes lands in
/// exactly one of the three channels below — so the disjunction is exhaustive over calls:
/// * `free.names` — a DIRECT `t.static(…)` parses as a `Field` on `Ident(t)`, so `t` is a free name;
/// * `free.opaque_calls` — a callee [`record_call_site`] could not classify at all (a non-ident
///   receiver, an indexed head, a call result, a name bound inside the body): always yes;
/// * `free.calls` — a classified `f(…)` / `head.f(…)`, put to `is_witness_fn`.
///
/// `is_witness_fn` (`Compiler::call_may_take_witnesses`) answers NO in exactly ONE case: a BARE-IDENT
/// callee `f(…)` whose resolved key is absent from [`crate::checker::WitnessTable::fns`] — i.e. a
/// callee the compiler itself would thread no witness into at a real call site. A bare ident is the
/// only head this walk resolves the way the emitter does, because a bare call carries no head to
/// misread.
///
/// EVERY field-headed call `X.f(…)` answers yes, whatever `X` turns out to name — an imported module,
/// a TYPE, a receiver value, or a module name shadowed by an enclosing binding. The walk cannot tell
/// those apart (its `bound` set holds only names bound INSIDE the body, and it sees no type
/// namespace), and a Task-5 MEMBER witness call is keyed on the method-name token, a coordinate it
/// does not carry, while naming `T` only as a type ARGUMENT, which is not a free name. Two earlier
/// cuts tried to classify the head and each shipped a check-ok/run-fault; not classifying it at all
/// is what makes the disjunction exhaustive rather than merely untested.
///
/// The narrowing that remains is therefore modest and worth stating exactly: a nested body keeps
/// every witness unless it does only arithmetic, indexing, literal/name work and bare calls to
/// non-witness fns. Any `x.m(…)`, `a.b.c()`, `xs[i].m()`, `f()()` or `a?.m()` keeps them all.
///
/// [`free_names_block`]/[`free_names_expr`] flatten every nested body's names AND calls into the
/// enclosing walk, so a `t.default()` two levels down keeps the outer capture alive. Hence
/// `FnComp::witness_ref` stays a SUPERSET of the checker's `witness_scope`, which is carried
/// unconditionally into nested bodies and needs no mirroring rule.
///
/// Being too narrow here is not a silent wrong value: [`FnComp::witness_ref`] returns `None` and the
/// compile fails loudly ("no type witness in scope to forward").
pub(crate) fn nested_body_needs_witness(
    free: &FreeNames,
    t: &str,
    is_witness_fn: &dyn Fn(Option<&str>, &str) -> bool,
) -> bool {
    free.names.contains(t)
        || free.opaque_calls
        || free
            .calls
            .iter()
            .any(|c| is_witness_fn(c.module.as_deref(), &c.name))
}

/// [`free_names_block`] for callers that want only the free-variable set.
pub(crate) fn free_names_of_block(stmts: &[Stmt], bound: &HashSet<String>) -> HashSet<String> {
    let mut f = FreeNames::default();
    free_names_block(stmts, bound, &mut f);
    f.names
}

/// [`free_names_expr`] for callers that want only the free-variable set.
pub(crate) fn free_names_of_expr(e: &Expr, bound: &HashSet<String>) -> HashSet<String> {
    let mut f = FreeNames::default();
    free_names_expr(e, bound, &mut f);
    f.names
}

/// Free names of a capture-boundary BLOCK body (`defer:`/`spawn:`): names referenced but not bound
/// within, relative to `bound`. Threads bindings left-to-right (a later stmt sees earlier lets).
pub(crate) fn free_names_block(stmts: &[Stmt], bound: &HashSet<String>, out: &mut FreeNames) {
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
                    let mut b2 = b.clone();
                    match &arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            free_names_expr(chan, &b, out);
                            match target {
                                WaitTarget::Bind(n) => {
                                    b2.insert(n.clone());
                                }
                                WaitTarget::Assign(e) => free_names_expr(e, &b, out),
                                WaitTarget::Discard => {}
                            }
                        }
                        WaitArmKind::Send { call } => free_names_expr(call, &b, out),
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
pub(crate) fn free_names_expr(e: &Expr, bound: &HashSet<String>, out: &mut FreeNames) {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !bound.contains(n) {
                out.names.insert(n.clone());
            }
        }
        // A string literal's `{…}` interpolation exprs reference names too (parsed at compile time,
        // so absent from the AST) — collect their free names or a capture-via-interpolation is missed.
        ExprKind::Str(raw) => {
            for ie in interp_exprs(raw) {
                free_names_expr(&ie, bound, out);
            }
        }
        ExprKind::Interp(chunks) => {
            for ie in chunk_exprs(chunks) {
                free_names_expr(ie, bound, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
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
            type_args,
        } => {
            record_call_site(callee, args, named, type_args, bound, out);
            free_names_expr(callee, bound, out);
            args.iter().for_each(|a| free_names_expr(a, bound, out));
            named
                .iter()
                .for_each(|(_, v)| free_names_expr(v, bound, out));
        }
        ExprKind::Field { obj, .. } => {
            free_names_expr(obj, bound, out);
        }
        ExprKind::OptChain {
            obj, name, call, ..
        } => {
            free_names_expr(obj, bound, out);
            if let Some(c) = call {
                // `a?.m(…)` is a member call `record_call_site` never sees — and a member call is a
                // possible witness call (see [`FreeNames::opaque_calls`] / `member_calls`).
                out.opaque_calls = true;
                // No type-argument channel on an `?.` call (`a?.m[T]()` does not parse), and the
                // arguments are the ordinary ones — so the forward question is asked exactly as it
                // is for a plain member call.
                if out.record_members {
                    out.member_calls
                        .push(member_call(name, &c.args, &c.named, Vec::new()));
                }
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
        ExprKind::Interp(chunks) => {
            for ie in chunk_exprs(chunks) {
                collect_frame_binds_expr(ie, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
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
    /// See [`crate::vm::op::Proto::min_arity`]. Starts at `arity`; only the default prologue lowers it.
    min_arity: usize,
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
    /// A NEW `is_toplevel = false` proto that can hold a user statement needs a matching checker
    /// flag in the W8-2 discarded-carrier gate (`src/checker/sig.rs`, the `StmtKind::Expr` arm):
    /// its statements are invisible to `Op::PopExprStmt`'s top-level check, so the drop is silent.
    fn new(name: String, arity: usize, is_toplevel: bool) -> Self {
        FnComp {
            name,
            arity,
            // Short entry is opt-in: `emit_default_param_prologue` lowers this when, and only when,
            // it actually emits a fill for a trailing defaulted parameter.
            min_arity: arity,
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

    /// M24 — THE derivation of "is the hidden type witness for `t` reachable in this frame?": it is
    /// either this body's own trailing `$w:t` parameter, or — Task 4 — a capture appended by
    /// [`Compiler::with_witness_captures`] at the nested-body construction site. Nothing else can produce a
    /// `$w:` binding, so this answer IS the compiler's real capture behavior, which is what the
    /// checker's `witness_scope` predicate has to mirror.
    fn witness_ref(&self, t: &str) -> Option<WitnessRef> {
        let name = witness_local(t);
        if let Some(slot) = self.resolve_local(&name) {
            return Some(WitnessRef::Local(slot));
        }
        self.captured_names
            .iter()
            .position(|n| *n == name)
            .map(|s| WitnessRef::Captured(s as u32))
    }

    /// Push a witness [`Self::witness_ref`] already resolved. Infallible on purpose: taking the
    /// resolved reference (never a name to re-look-up) makes "the guard said yes but nothing was
    /// pushed" — a wrong `argc` / an operand read as a witness — unrepresentable.
    fn emit_witness(&mut self, w: WitnessRef, span: Span) {
        match w {
            WitnessRef::Local(slot) => self.emit_get_local_raw(slot, span),
            // The captured witness is the raw `str` (never boxed, because `$w:T` is unspellable and
            // so can never be in `boxed_names`), so NO trailing `CellLoad` — the one capture read
            // that legitimately skips it.
            WitnessRef::Captured(slot) => self.emit(Op::GetCaptured(slot), span),
        }
    }

    /// A free/global name in this frame: neither a local nor a captured binding. The guard used to
    /// decide whether a bare identifier resolves to a module/type/variant/builtin rather than a value.
    fn is_unbound(&self, name: &str) -> bool {
        self.resolve_local(name).is_none() && !self.captures(name)
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
    use crate::ast::StrLit;

    fn sp() -> Span {
        Span::RUNTIME
    }

    // The compiler's standalone-path `ctype_of`/`ctype_of_visiting` second resolver was DELETED in
    // fix5 (the single-resolver redesign); extern C types now come VERBATIM from the checker's
    // `resolve_extern_signatures{,_standalone}`. Its behavioral coverage (widths, owned/nullable str,
    // flat struct, cyclic-alias-no-overflow) lives in `checker::tests::resolve_extern_ctype`.

    #[test]
    fn parse_interpolation_attaches_spec() {
        let chunks = parse_interpolation(&StrLit::from("{x:>5}"), sp()).unwrap();
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
        let chunks = parse_interpolation(&StrLit::from("{x}"), sp()).unwrap();
        assert!(matches!(&chunks[..], [Chunk::Expr(_, None)]));
    }

    #[test]
    fn parse_interpolation_width_cap_is_compile_error() {
        let err = parse_interpolation(&StrLit::from("{x:>99999999}"), sp()).unwrap_err();
        assert!(
            err.message.contains("exceeds maximum 4096"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_interpolation_colon_inside_index_not_a_separator() {
        // The `:` inside the string-key index is NOT the spec separator; only the trailing one is.
        let chunks = parse_interpolation(&StrLit::from("{m[\"a:b\"]:>3}"), sp()).unwrap();
        match &chunks[..] {
            [Chunk::Expr(_, Some(spec))] => assert_eq!(spec.width, 3),
            _ => panic!("expected spec'd expr"),
        }
        // And with no trailing spec, the inner `:` stays part of the expression.
        let chunks = parse_interpolation(&StrLit::from("{m[\"a:b\"]}"), sp()).unwrap();
        assert!(matches!(&chunks[..], [Chunk::Expr(_, None)]));
    }

    #[test]
    fn parse_interpolation_scanner_is_quote_and_depth_aware() {
        // A `}` inside a nested string literal does NOT close the fragment.
        let chunks = parse_interpolation(&StrLit::from("v={d['a}}b']}"), sp()).unwrap();
        assert!(matches!(
            &chunks[..],
            [Chunk::Lit(l), Chunk::Expr(_, None)] if l == "v="
        ));
        // Nor does one nested inside `{`/`[`/`(` — the set literal's brace is at depth 1.
        let chunks = parse_interpolation(&StrLit::from("{ {1, 2}.len() }"), sp()).unwrap();
        assert!(matches!(&chunks[..], [Chunk::Expr(_, None)]));
        // Padding around a fragment is insignificant (CPython allows `f"{ x }"`).
        let chunks = parse_interpolation(&StrLit::from("{ x }"), sp()).unwrap();
        assert!(matches!(&chunks[..], [Chunk::Expr(_, None)]));
        // A depth-0 `}` still terminates, and an unclosed fragment is still an error.
        assert!(
            parse_interpolation(&StrLit::from("{x"), sp())
                .unwrap_err()
                .message
                .contains("unterminated '{'")
        );
        assert!(
            parse_interpolation(&StrLit::from("a}b"), sp())
                .unwrap_err()
                .message
                .contains("unmatched '}'")
        );
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

#[cfg(test)]
mod back_edge_tests {
    //! Cancellation points: a cancel is delivered at loop BACK-EDGES (plus blocking ops), so a hot
    //! loop stays cancellable (`Vm::jump_checked`, src/vm/exec.rs). That rests on every loop form
    //! lowering to a BACKWARD `Op::Jump` — pin it here so a future peephole pass cannot thread the
    //! back-edge out of existence and silently make hot loops uncancellable.
    use super::*;
    use crate::vm::op::Op;

    fn has_back_edge(src: &str) -> bool {
        let tokens = crate::lexer::tokenize(src).expect("lex");
        let module = crate::parser::parse(tokens).expect("parse");
        let prog = compile_module_standalone(&module).expect("compile");
        prog.protos.iter().any(|p| {
            p.code
                .iter()
                .enumerate()
                .any(|(k, op)| matches!(op, Op::Jump(t) if *t <= k))
        })
    }

    #[test]
    fn loop_back_edge_is_a_backward_jump() {
        assert!(
            has_back_edge("fn f():\n    i := 0\n    while i < 3:\n        i = i + 1\nf()\n"),
            "`while` must lower to a backward Op::Jump (the cancellation checkpoint)"
        );
        assert!(
            has_back_edge("fn f():\n    for x in [1, 2]:\n        print(x)\nf()\n"),
            "`for` must lower to a backward Op::Jump (the cancellation checkpoint)"
        );
    }
}

#[cfg(test)]
mod carrier_lowering_tests {
    //! W7-43 — `?.` on a `Result` is `?` then `.`. THE load-bearing test in the change: the two
    //! spellings must compile to byte-identical bytecode. If that holds, the VM and the
    //! `recover:`/`defer:`/nursery interactions are green BY CONSTRUCTION —
    //! there is no second code path to disagree about.
    use super::*;
    use crate::vm::op::Op;

    /// Parse → desugar → compile, i.e. the production pipeline (`resolver::build_graph` always
    /// desugars before the checker/backend). The carrier SURVIVES desugar now; the checker records
    /// the lowering into the `CarrierTable` that `compile_module_standalone` harvests.
    fn compile(src: &str) -> crate::vm::op::Program {
        let tokens = crate::lexer::tokenize(src).expect("lex");
        let mut module = crate::parser::parse(tokens).expect("parse");
        crate::desugar::run_standalone(&mut module).expect("desugar");
        compile_module_standalone(&module).expect("compile")
    }

    /// Every proto rendered as `name | opcodes`, in emission order. `Op` has no `PartialEq` (it is a
    /// big hot enum, and deriving one for a test would be the tail wagging the dog), so compare its
    /// `Debug` — which is strictly FINER than `==` would be.
    fn ops(prog: &crate::vm::op::Program) -> Vec<String> {
        prog.protos
            .iter()
            .map(|p| format!("{} | {:?}", p.name, p.code))
            .collect()
    }

    /// The same, plus each op's source span.
    fn ops_and_spans(prog: &crate::vm::op::Program) -> Vec<String> {
        prog.protos
            .iter()
            .map(|p| format!("{} | {:?} | {:?}", p.name, p.code, p.lines))
            .collect()
    }

    /// Compile `template` (which must contain the `?C?` marker) in three spellings and assert the
    /// identity that W7-43 rests on.
    ///
    /// - `f()?.len()` vs `f()? .len()` — the two spellings a USER writes. Their OPCODES must match;
    ///   their spans cannot, because inserting the space physically moves every column to its right
    ///   (that is a property of the source text, not of the lowering).
    /// - `f() ?.len()` vs `f()? .len()` — column-ALIGNED (same length, `len` at the same column), so
    ///   here the per-op SPANS must match too. Same alignment trick as
    ///   `desugar::tests::lower_carrier_try_matches_the_spaced_spelling_exactly`.
    fn assert_spellings_agree(template: &str) {
        assert!(template.contains("?C?"), "template needs the `?C?` marker");
        let tight = compile(&template.replace("?C?", "?."));
        let spaced = compile(&template.replace("?C?", "? ."));
        let aligned = compile(&template.replace("?C?", " ?."));
        assert_eq!(
            ops(&tight),
            ops(&spaced),
            "`?.` and `? .` must emit identical bytecode"
        );
        assert_eq!(
            ops_and_spans(&aligned),
            ops_and_spans(&spaced),
            "column-aligned, the per-op SPANS must match too"
        );
    }

    #[test]
    fn result_carrier_compiles_identically_to_the_spaced_spelling() {
        let src = "fn f() -> str!str:\n    return Ok(\"hi\")\nfn g() -> int!str:\n    return Ok(f()?C?len())\ng()\n";
        assert_spellings_agree(src);
        // …and the emitted program really IS the try-then-dot one (a SHARED bug would also be
        // "identical"): `Op::Try` is present, which only the Result lowering emits.
        let dot = compile(&src.replace("?C?", "?."));
        assert!(
            dot.protos
                .iter()
                .any(|p| p.code.iter().any(|o| matches!(o, Op::Try))),
            "the Result lowering must emit Op::Try"
        );
    }

    #[test]
    fn result_carrier_method_with_args_compiles_identically() {
        assert_spellings_agree(
            "struct B:\n    v: int\n    fn add(self, n: int) -> int:\n        return self.v + n\nfn f() -> B!str:\n    return Ok(B(1))\nfn g() -> int!str:\n    return Ok(f()?C?add(5))\ng()\n",
        );
    }

    #[test]
    fn result_carrier_with_named_args_compiles_identically() {
        // Named args + an omitted default are bound by `normalize_opt_call` in desugar; the spaced
        // spelling is an ordinary `Call` bound by `normalize_call`. Identical bytecode is the proof
        // that the two normalizations agree.
        assert_spellings_agree(
            "struct B:\n    v: int\n    fn tag(self, prefix: str = \"p\", n: int = 1) -> str:\n        return \"{prefix}{self.v + n}\"\nfn f() -> B!str:\n    return Ok(B(1))\nfn g() -> str!str:\n    return Ok(f()?C?tag(n=5))\ng()\n",
        );
    }

    #[test]
    fn result_carrier_inside_a_string_fragment_compiles_identically() {
        // Interpolation fragments are re-parsed after the module pass, so they carry their own
        // `kw_frag_ctx`/`kw_frag_ord` key discriminators — two fragments in one literal included.
        assert_spellings_agree(
            "fn f() -> str!str:\n    return Ok(\"hi\")\nfn g() -> str!str:\n    return Ok(\"{f()?C?len()}|{f()?C?len()}\")\ng()\n",
        );
    }

    #[test]
    fn a_mixed_chain_lowers_each_link_by_its_own_operand() {
        // `a?.b?.c` with `a: Result` and `a.b: Option` — the two links take DIFFERENT lowerings and
        // share ONE node span, so the mode table must key on the name token. The Result link emits
        // `Op::Try`; the Option link emits the `match` lowering (a jump, and a `None` variant
        // construction). Both present in one proto = both links lowered on their own operand.
        let prog = compile(
            "struct I:\n    c: int\nstruct O:\n    b: Option[I]\nfn f() -> O!str:\n    return Ok(O(Some(I(7))))\nfn g() -> Result[Option[int], str]:\n    return Ok(f()?.b?.c)\ng()\n",
        );
        let g = prog
            .protos
            .iter()
            .find(|p| p.name == "g")
            .expect("proto for g");
        assert!(
            g.code.iter().any(|o| matches!(o, Op::Try)),
            "the Result link must emit Op::Try; got {:?}",
            g.code
        );
        assert!(
            g.code.iter().any(|o| matches!(o, Op::Jump(_))),
            "the Option link must emit the match lowering's jump; got {:?}",
            g.code
        );
        // And the whole thing is identical to spelling the Result link with a space.
        let spaced = compile(
            "struct I:\n    c: int\nstruct O:\n    b: Option[I]\nfn f() -> O!str:\n    return Ok(O(Some(I(7))))\nfn g() -> Result[Option[int], str]:\n    return Ok(f()? .b?.c)\ng()\n",
        );
        assert_eq!(ops(&prog), ops(&spaced));
    }
}

/// A1 — `ModuleProto` carries the module's [`crate::lexer::Span::file`] id, so a compiled `Program`
/// can map a span back to the file it came from (`Program::file_path`). Nothing observable changes
/// yet; these are the plumbing tests later tasks build on.
#[cfg(test)]
mod file_id_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("chezzi_fid_{}_{}", std::process::id(), n));
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

    /// Entry imports TWO modules (`a`, `b`) that each import a further module (`c`, `d`) — so the
    /// resolver's DFS pre-order `file` id assignment (parent parses before its imports recurse,
    /// `resolver::mod.rs`) genuinely disagrees with `graph.modules`' deps-first post-order (a child
    /// is PUSHED to the order vec only after it returns, i.e. before its importer). A `modules[file -
    /// 1]` indexing bug would silently pick the wrong module here.
    fn build_diamond() -> crate::resolver::ModuleGraph {
        let t = TmpDir::new();
        t.write("chezzi.toml", "[project]\nname = \"g\"\n");
        t.write("c.chz", "fn fc() -> int:\n    return 1\n");
        t.write(
            "a.chz",
            "import c\nfn fa() -> int:\n    return c.fc() + 1\n",
        );
        t.write("d.chz", "fn fd() -> int:\n    return 1\n");
        t.write(
            "b.chz",
            "import d\nfn fb() -> int:\n    return d.fd() + 1\n",
        );
        let entry = t.write(
            "main.chz",
            "import a\nimport b\nfn main():\n    print(a.fa() + b.fb())\nmain()\n",
        );
        // `t` (and its files) must outlive the graph build, but the graph itself holds no file
        // handles — dropping `t` after `build_graph` returns is fine, same as the resolver's own
        // fixture tests.
        crate::resolver::build_graph(&entry).expect("graph should build")
    }

    #[test]
    fn module_protos_carry_their_graph_file_ids() {
        let graph = build_diamond();
        assert!(graph.modules.len() >= 5, "expected the 5 written modules");
        let program = compile_graph(&graph).expect("compile");
        assert_eq!(program.modules.len(), graph.modules.len());

        let mut seen = std::collections::HashSet::new();
        for (i, lm) in graph.modules.iter().enumerate() {
            assert_eq!(
                program.modules[i].file,
                lm.file,
                "module {} (index {i}) lost its graph file id",
                lm.label()
            );
            assert_ne!(lm.file, 0, "module {} got sentinel file id 0", lm.label());
            assert!(
                seen.insert(lm.file),
                "file id {} is not unique across modules",
                lm.file
            );
        }
    }

    #[test]
    fn file_path_resolves_by_id_not_by_index() {
        let graph = build_diamond();
        let program = compile_graph(&graph).expect("compile");

        for lm in &graph.modules {
            assert_eq!(
                program.file_path(lm.file),
                Some(lm.id.0.as_path()),
                "file_path({}) should resolve to {}'s path — a `modules[file - 1]` index bug \
                 returns some OTHER module's path here (pre-order file ids vs. post-order modules)",
                lm.file,
                lm.label()
            );
        }
        assert_eq!(program.file_path(0), None, "file id 0 must never resolve");
        assert_eq!(
            program.file_path(99999),
            None,
            "an unknown file id must resolve to None, not panic"
        );
    }

    #[test]
    fn single_module_compile_has_no_file_id() {
        let tokens = crate::lexer::tokenize("fn main():\n    print(1)\nmain()\n").expect("lex");
        let module = crate::parser::parse(tokens).expect("parse");
        let program = compile_module_standalone(&module).expect("compile");

        assert_eq!(program.modules.len(), 1);
        assert_eq!(program.modules[0].file, 0);
        assert_eq!(program.file_path(0), None);
    }
}

/// W8-13 — a failing comparison `assert` splits the condition so the operand VALUES survive the
/// comparison opcode. These pin the bytecode shape: a passing comparison assert must pop both
/// duplicated operands (no stack growth per execution), and a non-comparison assert must keep its
/// existing (unchanged) lowering.
#[cfg(test)]
mod assert_lowering_tests {
    use super::*;
    use crate::vm::op::Op;

    fn compile(src: &str) -> crate::vm::op::Program {
        let tokens = crate::lexer::tokenize(src).expect("lex");
        let mut module = crate::parser::parse(tokens).expect("parse");
        crate::desugar::run_standalone(&mut module).expect("desugar");
        compile_module_standalone(&module).expect("compile")
    }

    fn toplevel(p: &crate::vm::op::Program) -> &crate::vm::op::Proto {
        p.protos
            .iter()
            .find(|q| q.name == "<toplevel>")
            .expect("toplevel proto")
    }

    #[test]
    fn passing_comparison_assert_pops_both_operands() {
        let prog = compile("i := 0\nassert i < 3\n");
        let code = &toplevel(&prog).code;
        let j = code
            .iter()
            .position(|op| matches!(op, Op::JumpIfFalse(_)))
            .unwrap_or_else(|| panic!("no JumpIfFalse in {code:?}"));
        assert!(matches!(code[j + 1], Op::Pop), "{code:?}");
        assert!(matches!(code[j + 2], Op::Pop), "{code:?}");
        assert!(matches!(code[j + 3], Op::Jump(_)), "{code:?}");
        assert_eq!(
            code.iter().filter(|op| matches!(op, Op::Dup2)).count(),
            1,
            "{code:?}"
        );
    }

    #[test]
    fn non_comparison_assert_keeps_the_jump_adjacent() {
        let prog = compile("assert 4 in [1, 2, 3]\n");
        let code = &toplevel(&prog).code;
        let j = code
            .iter()
            .position(|op| matches!(op, Op::JumpIfFalse(_)))
            .unwrap_or_else(|| panic!("no JumpIfFalse in {code:?}"));
        assert!(matches!(code[j + 1], Op::Jump(_)), "{code:?}");
        assert_eq!(
            code.iter().filter(|op| matches!(op, Op::Dup2)).count(),
            0,
            "{code:?}"
        );
    }
}
