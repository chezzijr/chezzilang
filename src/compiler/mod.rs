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
    AssignOp, BinaryOp, Block, Chunk, DeferTarget, Expr, ExprKind, FnDecl, Import, LitPattern,
    MatchArm, MatchExprArm, Module, Pattern, Span, SpawnTarget, Stmt, StmtKind, Type, WaitArmKind,
};
use crate::base::module_id::ResolvedImport;
use crate::base::op::{
    AssertCmp, CapEntry, CapSrc, CffiDef, CompileError, LIFECYCLE_HOOKS, ModuleProto, NO_IC, Op,
    Program, ProtoId, StructDef, SuiteInfo, VariantDef,
};
use crate::native::cffi::CType;
use crate::resolver::ModuleGraph;
use std::collections::HashMap;
use std::collections::HashSet;

mod assign;
mod call;
mod capture;
mod expr;
mod fn_body;
mod loops;
mod pattern;
mod peephole;
mod stmt;
mod witness;
pub(crate) use capture::*;
mod fncomp;
use fncomp::*;
#[cfg(test)]
mod tests;

/// The name a builtin resolves to.
/// ROOT REDESIGN — the module key used to qualify user-type identity keys in the single-source
/// (standalone / `<main>`) compile + run paths, where there is no module graph to derive a real
/// label from. The checker, compiler, and VM must all agree on it, so it lives here as one constant. Only
/// the test-only standalone paths use it (the CLI always runs the module-graph path).
#[cfg(test)]
pub(crate) const STANDALONE_MODULE_KEY: &str = "<main>";

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

/// Resolve the module graph of the file at `entry` (`root` pins the graph root, as in
/// [`crate::resolver::build_graph_with_entry_source_and_root`]) and compile it. A resolve error
/// becomes a `CompileError` at its span. The one way from an entry path to a [`Program`].
pub fn compile_entry_file(
    entry: &std::path::Path,
    root: Option<std::path::PathBuf>,
) -> Result<Program, CompileError> {
    let graph = crate::resolver::build_graph_with_entry_source_and_root(entry, None, root)
        .map_err(|e| CompileError {
            message: e.message,
            span: e.span,
        })?;
    compile_graph(&graph)
}

/// Compile a whole resolved module graph in dependency order.
pub fn compile_graph(graph: &ModuleGraph) -> Result<Program, CompileError> {
    let mut c = Compiler::new();
    c.range_free = !graph.range_used;
    // FFI ROOT FIX (fix4): resolve every extern fn's C signature ONCE in the checker (module-scoped,
    // every alias spelling), then lower each extern from that table — never re-resolving alias names
    // in the backend's flat bare map (the collision whack-a-mole's root). Keyed by graph module idx.
    let (externs, tables) = crate::checker::resolve_compile_tables(graph);
    c.extern_sigs = externs;
    // Swift-style keyword args through a function VALUE: the checker resolves each value+keyword call
    // to a positional slot permutation (module-scoped, keyed like extern sigs); the value path lowers
    // it to a plain positional `Op::Call`, so the runtime ABI stays positional.
    // M24 rides the same pass: the witness table says which generic fns take hidden trailing
    // witness params and what fills each witness at each call site. The compiler CONSUMES it — it
    // never re-derives which protocols carry a static requirement (that resolves through
    // imports/aliases/embeds, which is checker work).
    let (kw, wt, ct, ns, rc, conflicts, fo, rs, nf, gc, wr, ht, hc, fw) = tables;
    reject_table_conflicts(conflicts)?;
    c.hook_table = ht;
    c.hook_calls = hc;
    c.field_widths = fw;
    c.fall_off = nf;
    c.call_plans = kw;
    c.witnesses = wt;
    c.carriers = ct;
    c.sum_seeds = ns;
    c.crossings = fo;
    c.gen_crossings = gc;
    c.resolutions = rs;
    c.ret_coerce = rc;
    c.wraps = wr;
    // Pass 0: collision pre-pass — assign runtime keys for module-scoped user types. A type name
    // declared in exactly one module keeps its BARE name (the common case → unchanged Display/print
    // output); a name declared in ≥2 modules that are BOTH in the program is disambiguated (the
    // first/entry-most keeps bare, the rest get `<dotted.path>::Name`).
    c.assign_type_keys(graph);
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
        let let_lines = std::mem::take(&mut c.global_let_lines);
        debug_assert_eq!(let_lines.len(), global_slots.len());
        c.program.modules.push(ModuleProto {
            id: lm.id.clone(),
            label: lm.label(),
            toplevel,
            imports: lm.imports.clone(),
            native: lm.native,
            std: lm.is_std(),
            global_slots,
            let_lines,
            file: lm.file,
        });
    }
    c.program.field_ic_sites = c.field_ic_next;
    c.program.task_entry = c.spawn_entry_proto("<task entry>", None, 0, Span::RUNTIME);
    c.program.method_ic_sites = c.method_ic_next;
    c.program.range_key = crate::resolver::range_key(graph);
    c.program.rebuild_struct_names(&c.field_widths);
    c.build_hooks();
    c.build_provider_table()?;
    Ok(c.program)
}

/// Compile a single in-memory module (test helper — no imports, treated as the entry).
#[cfg(test)]
pub fn compile_module_standalone(module: &Module) -> Result<Program, CompileError> {
    // Mirror the file-backed path: normalize named/default call arguments before compiling.
    let mut module = module.clone();
    // TICKET-253: every module sees the prelude's `Range` struct; a standalone module has no graph,
    // so it carries the declaration itself (unless it declares one).
    let is_range = |s: &Stmt| matches!(&s.kind, StmtKind::Struct { name, .. } if name == "Range");
    if !module.stmts.iter().any(is_range)
        && let Ok(src) = crate::resolver::std_source(&["std".to_string(), "prelude".to_string()])
        && let Ok(toks) = crate::lexer::tokenize(&src)
        && let Ok(prelude) = crate::parser::parse(toks)
    {
        module
            .stmts
            .extend(prelude.stmts.into_iter().filter(is_range));
    }
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
    c.hoist_types(0, &module.stmts)?;
    // SINGLE-RESOLVER: extern C types come from the checker's standalone pass — the SAME resolver the
    // multi-file CLI uses (no second backend resolver exists). The backend reads this table verbatim.
    c.extern_sigs = crate::checker::resolve_extern_signatures_standalone(&module.stmts);
    let (kw, wt, ct, ns, rc, conflicts, fo, rs, nf, gc, wr, ht, hc, fw) =
        crate::checker::resolve_call_tables_standalone(&module.stmts);
    reject_table_conflicts(conflicts)?;
    c.hook_table = ht;
    c.hook_calls = hc;
    c.field_widths = fw;
    c.fall_off = nf;
    c.call_plans = kw;
    c.witnesses = wt;
    c.carriers = ct;
    c.sum_seeds = ns;
    c.crossings = fo;
    c.gen_crossings = gc;
    c.resolutions = rs;
    c.ret_coerce = rc;
    c.wraps = wr;
    let toplevel = c.compile_module(0, module, &[], true, None)?;
    let global_slots = std::mem::take(&mut c.global_slots);
    let let_lines = std::mem::take(&mut c.global_let_lines);
    debug_assert_eq!(let_lines.len(), global_slots.len());
    // A synthetic module id so the run driver has something to key the namespace cache on.
    let id = crate::base::module_id::ModuleId(std::path::PathBuf::from("<main>"));
    c.program.modules.push(ModuleProto {
        id,
        label: "<main>".to_string(),
        toplevel,
        imports: Vec::new(),
        native: None,
        std: false,
        global_slots,
        let_lines,
        file: 0,
    });
    c.program.field_ic_sites = c.field_ic_next;
    c.program.task_entry = c.spawn_entry_proto("<task entry>", None, 0, Span::RUNTIME);
    c.program.method_ic_sites = c.method_ic_next;
    c.program.range_key =
        crate::resolver::range_key(&crate::checker::standalone_graph(&module.stmts));
    c.program.rebuild_struct_names(&c.field_widths);
    c.build_provider_table()?;
    c.build_hooks();
    Ok(c.program)
}

/// A compiler-synthesized fn value, keyed by (item, the type args its body depends on)
/// (TICKET-226, generics design R4). A new synthesized fn kind adds an arm here and goes through
/// [`Compiler::synth_fn_proto`]; never a per-kind memo beside it.
#[derive(Clone, PartialEq, Eq, Hash)]
enum SynthFn {
    /// A bound's instance method read through a type parameter (`T.get`): one per (method, arity).
    ParamMethod { method: String, arity: usize },
    /// `json.decode[T]` read as a value (TICKET-214), keyed by the target's shape.
    Decode(crate::base::json_desc::TypeDescriptor<()>),
}

/// The provider-table name of a `json.decode[T]` value proto. Unspellable (`$`), so no user decl
/// can collide with it.
fn decode_fn_name(pid: ProtoId) -> String {
    format!("$json.decode#{pid}")
}

struct Compiler {
    program: Program,
    /// TICKET-255 — the graph recorded no `Range` value ([`ModuleGraph::range_used`] is false): the prelude
    /// `Range` methods are not compiled, and [`Compiler::emit_new_struct`] stops a build that emits one.
    range_free: bool,
    /// TICKET-250 — the checker's verdict per declared type, and the call sites it typed through a
    /// reserved protocol requirement. Consumed verbatim; the compiler decides no hook.
    hook_table: crate::checker::HookTable,
    hook_calls: crate::checker::HookCallTable,
    /// TICKET-262 — the checker's [`FieldWidthTable`]; read only by `rebuild_struct_names`.
    field_widths: crate::checker::FieldWidthTable,
    /// Runtime type key → its hooks, gathered at each declaration and materialized into
    /// `Program::hooks_struct` / `hooks_enum` by [`Compiler::build_hooks`].
    hook_sets: HashMap<String, crate::base::hooks::HookSet>,
    /// Default-argument provider NAME → its dense `Op::MakeFuncIn` operand. Ids are handed out at
    /// EMIT sites (a call site in a module that cannot name the provider), which run before the
    /// declaring module is compiled — hence the indirection. See [`Compiler::build_provider_table`].
    provider_ids: HashMap<String, u32>,
    /// Default-argument provider NAME → `(its proto, the index of the module declaring it)`, filled
    /// as each declaring module is compiled. Materialized into `Program::providers` at the end.
    provider_defs: HashMap<String, (ProtoId, usize)>,
    /// The ONE memo of compiler-synthesized fn values (TICKET-226): one proto per [`SynthFn`], so
    /// two reads of the same item are one `Vm::fn_value`. See [`Compiler::synth_fn_proto`].
    synth_fns: HashMap<SynthFn, ProtoId>,
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
    /// TICKET-183 — parallel to `global_slots`: the source line of the top-level let that created
    /// slot `i`, `0` when an import, fn, extern or native created it. Recorded into
    /// [`ModuleProto::let_lines`].
    global_let_lines: Vec<u32>,
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
    /// The module index that declared `std/json.chz`'s bodyless `native fn _to_json` (set once, in
    /// `hoist_types`, which runs over every module before any `compile_module`). `None` if
    /// `std.json` is not part of this compile. Gates the bare-call `_to_json(...)` lowering to
    /// `Op::JsonToValue` so a user module cannot spell that name and hijack the opcode — a user
    /// module can't declare `native fn` at all, so the name is otherwise self-scoping too.
    json_to_value_home: Option<usize>,
    /// FFI ROOT FIX (fix4): the checker-resolved C signature of every `extern` fn, keyed by `(graph
    /// module index, fn name)`. The extern lowering consumes THIS instead of re-resolving alias names
    /// itself — so every qualified/imported/aliased width resolves in its DEFINING module's scope
    /// (collision-proof). Filled once in `compile_graph` (multi-file) or by
    /// `resolve_extern_signatures_standalone` (single-file) — the ONE extern-type resolver.
    extern_sigs: crate::checker::ExternTable,
    /// The checker's argument slot plan per bound call, keyed `(graph module index, call NodeId)`
    /// (see [`crate::checker::CallPlanTable`]). Filled in `compile_graph`/`compile_module_standalone`
    /// from `resolve_call_tables`; read only by [`Self::compile_plan_args`].
    call_plans: crate::checker::CallPlanTable,
    /// M24 — the checker-produced static-witness contract (see [`crate::checker::WitnessTable`]),
    /// consumed verbatim: `fns` drives the hidden trailing `$w:T` params a generic fn's proto gets,
    /// `calls` drives the extra argument each call site pushes.
    witnesses: crate::checker::WitnessTable,
    /// W7-43 — the checker's per-`?.`-carrier lowering decision (see
    /// [`crate::checker::CarrierTable`]), consumed verbatim: the backend is type-blind and cannot
    /// re-derive whether a carrier's operand was an `Option` or a `Result`.
    carriers: crate::checker::CarrierTable,
    /// Which `.sum()` sites need a `0.0` SEED pushed as a hidden argument. The backend is
    /// type-blind — an empty `List[float]` carries no element to read a kind off — so this is
    /// CONSUMED from the checker and never re-derived; a MISS means "plain numeric sum", which is the
    /// pre-fix lowering. See [`crate::checker::SumSeedTable`].
    sum_seeds: crate::checker::SumSeedTable,
    /// D4 (TICKET-189, TICKET-240) — the checker's per-slot shape for each `spawn` call, read by
    /// [`Self::crossing_mask`]. See [`crate::checker::CrossingTable`].
    crossings: crate::checker::CrossingTable,
    /// The index of each distinct entry of `Program.fresh_calls`.
    fresh_index: HashMap<Vec<crate::checker::Fresh>, usize>,
    /// TICKET-190 — the checker's generator creation stamps and private frame slot names. See
    /// [`crate::checker::GenCrossings`].
    gen_crossings: crate::checker::GenCrossings,
    /// TICKET-180 — the checker's answer to what every name head means; the ONLY source for
    /// it. Read through [`Compiler::resolution`].
    resolutions: crate::checker::ResolutionTable,
    /// TICKET-184 — fns the checker proved cannot fall off their end; their fall-off traps.
    fall_off: crate::checker::FallOffTable,
    /// W8-21 — which implicit success-coercion (if any) each declared `T?`/`T!E` return sink applies
    /// to its bare success value, consumed verbatim: the backend is type-blind and cannot re-derive
    /// whether the returned expression is already a carrier. A MISS means `NoWrap` — the pre-fix
    /// lowering. See [`crate::checker::RetCoerceTable`].
    ret_coerce: crate::checker::RetCoerceTable,
    /// TICKET-227 -- the checker's implicit wraps, keyed by (module idx, NodeId). Applied only by
    /// [`Self::compile_expr`].
    wraps: crate::checker::WrapTable,
    /// TICKET-054 review fix — which call arguments must widen int→float AT THE CALL SITE (a
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
    /// String-interpolation fragment discriminators for the [`crate::checker::WitnessKey`] and
    /// [`crate::checker::CarrierKey`]: the
    /// whole-string span + the fragment's 0-based ordinal, maintained (save/restore) around each
    /// fragment in `compile_interp`. Mirrors the checker's `kw_frag_ctx`/`kw_frag_ord` so the keyword-call
    /// key computed at lookup matches the one the checker recorded. Inert (`Span::default()`/`0`)
    /// outside interpolation.
    kw_frag_ctx: crate::lexer::Span,
    kw_frag_ord: usize,
}

/// M19 lever #2 — register an enum variant into BOTH program tables, assigning it the next dense
/// `variant_id` (`variants_by_id.len()`, a global gap-free counter — the analogue of how `StructDef`
/// gets its `tid` from `structs.len()`). Keeps `variants` (`(enum, variant)`→def) and `variants_by_id`
/// (id→def) in lockstep so cold-path id→names resolution is O(1). The map is keyed by the
/// `(enum, variant)` pair, so two enums may share a variant name yet get distinct ids.
/// The "no self ⇒ static" classification: a method whose FIRST param is not named `self`, or which
/// has no params, is a STATIC (associated) method (`FnDecl::takes_self`).
fn is_static_method(m: &FnDecl) -> bool {
    !m.takes_self()
}

/// Key into [`Compiler::static_methods`]: the type's runtime key + the method name, joined by a
/// control char that can never appear in a type key or identifier.
fn static_key(type_key: &str, method: &str) -> String {
    format!("{type_key}\u{1}{method}")
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

impl Compiler {
    /// Materialize `Program::providers` from the emit-site ids and the declaration-site protos.
    /// Runs last, for the same reason [`Compiler::build_hooks`] does: the caller's module is
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
                        crate::base::provider_name::display_fn_name(name)
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

    /// Materialize `Program::hooks_struct` (by `tid`) and `Program::hooks_enum` (by `variant_id`)
    /// from the checker's verdicts. Runs last: both id spaces are final only then.
    fn build_hooks(&mut self) {
        let mut by_tid = vec![0u32; self.program.struct_names.len()];
        for (key, def) in &self.program.structs {
            if let Some(set) = self.hook_sets.get(key) {
                by_tid[def.tid as usize] = set.0;
            }
        }
        let by_vid = self
            .program
            .variants_by_id
            .iter()
            .map(|vd| self.hook_sets.get(&vd.enum_name).map_or(0, |set| set.0))
            .collect();
        let eq = crate::base::hooks::Hook::Eq.bit();
        self.program.any_eq_hook = self.hook_sets.values().any(|set| set.0 & eq != 0);
        self.program.hooks_struct = by_tid;
        self.program.hooks_enum = by_vid;
    }

    fn new() -> Self {
        let mut program = Program {
            protos: Vec::new(),
            structs: HashMap::new(),
            range_key: "Range".to_string(),
            enum_methods: HashMap::new(),
            enum_home: HashMap::new(),
            providers: Vec::new(),
            native_methods: HashMap::new(),
            native_home: HashMap::new(),
            fresh_calls: Vec::new(),
            variants: HashMap::new(),
            variants_by_id: Vec::new(),
            struct_names: Vec::new(),
            struct_widths: Vec::new(),
            any_eq_hook: false,
            hooks_struct: Vec::new(),
            hooks_enum: Vec::new(),
            modules: Vec::new(),
            field_ic_sites: 0,
            method_ic_sites: 0,
            task_entry: 0,
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
        debug_assert_eq!(vid(&program, "Result", "Ok"), crate::base::op::VID_OK);
        debug_assert_eq!(vid(&program, "Result", "Err"), crate::base::op::VID_ERR);
        debug_assert_eq!(vid(&program, "Option", "Some"), crate::base::op::VID_SOME);
        debug_assert_eq!(
            vid(&program, "Option", "None"),
            crate::base::op::VID_NONE_VARIANT
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
            range_free: false,
            hook_table: crate::checker::HookTable::new(),
            hook_calls: crate::checker::HookCallTable::new(),
            field_widths: crate::checker::FieldWidthTable::new(),
            hook_sets: HashMap::new(),
            globals: HashMap::new(),
            provider_ids: HashMap::new(),
            provider_defs: HashMap::new(),
            synth_fns: HashMap::new(),
            fn_names: std::collections::HashSet::new(),
            global_slots: Vec::new(),
            global_let_lines: Vec::new(),
            field_ic_next: 0,
            method_ic_next: 0,
            type_keys: HashMap::new(),
            module_types: Vec::new(),
            static_methods: std::collections::HashSet::new(),
            imported_modules: HashMap::new(),
            current_module_idx: 0,
            json_to_value_home: None,
            extern_sigs: crate::checker::ExternTable::new(),
            call_plans: crate::checker::CallPlanTable::new(),
            witnesses: crate::checker::WitnessTable::default(),
            carriers: crate::checker::CarrierTable::new(),
            sum_seeds: crate::checker::SumSeedTable::new(),
            crossings: crate::checker::CrossingTable::new(),
            fresh_index: HashMap::new(),
            gen_crossings: crate::checker::GenCrossings::default(),
            resolutions: crate::checker::ResolutionTable::new(),
            fall_off: crate::checker::FallOffTable::new(),
            ret_coerce: crate::checker::RetCoerceTable::new(),
            wraps: crate::checker::WrapTable::new(),
            next_opt_tmp: 0,
            witness_locals: Vec::new(),
            pending_witnesses: Vec::new(),
            imported_fns: HashMap::new(),
            kw_frag_ctx: crate::lexer::Span::default(),
            kw_frag_ord: 0,
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
                // TICKET-218 — a `native type` export (std.ffi's `ptr`/`int8`..) carries no runtime
                // value; register its name so `Vm::bind_import` skips it like any known type name.
                for s in &lm.ast.stmts {
                    if let StmtKind::NativeType { name, .. } = &s.kind {
                        self.program.type_names.insert(name.clone());
                    }
                }
                continue;
            }
            // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (no qualified
            // key entry → `type_key` falls back to bare), so `Iterator`/FFI widths resolve bare.
            for s in &lm.ast.stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::TypeAlias { name, .. } = &s.kind
                {
                    self.module_types[idx].insert(name.clone());
                    self.program.type_names.insert(name.clone());
                }
                // A protocol has NO runtime representation, so only its NAME is registered — for
                // `Vm::bind_import`'s skip — deliberately NOT `module_types`/`type_keys`, which key
                // runtime LAYOUTS a protocol never has.
                if let StmtKind::Protocol { name, .. } = &s.kind {
                    self.program.type_names.insert(name.clone());
                }
            }
        }
        // A protocol has no runtime layout, so it gets no key here.
        for k in crate::resolver::user_type_keys(graph) {
            if !k.is_protocol {
                self.type_keys.insert((k.module, k.name), k.key);
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
        use crate::resolver::GlobalSource;
        self.globals.clear();
        self.global_slots.clear();
        self.global_let_lines.clear();
        self.fn_names.clear();
        let add = |name: String, globals: &mut HashMap<String, u32>, slots: &mut Vec<String>| {
            if !globals.contains_key(&name) {
                globals.insert(name.clone(), slots.len() as u32);
                slots.push(name);
            }
        };
        // Every slot before the first `let` holds a hoisted value from the start: no let line.
        for (name, source) in crate::resolver::module_global_binds(imports, stmts, native) {
            match source {
                GlobalSource::Import | GlobalSource::Native | GlobalSource::Extern => {
                    add(name, &mut self.globals, &mut self.global_slots);
                }
                GlobalSource::Fn => {
                    // Same-module top-level fn — licenses the generic-fn-as-value turbofish erase.
                    // A native fn does not.
                    self.fn_names.insert(name.clone());
                    add(name, &mut self.globals, &mut self.global_slots);
                }
                GlobalSource::Let { line } => {
                    // TICKET-183: only a slot this let CREATES starts uninitialized at its line.
                    if !self.globals.contains_key(&name) {
                        self.global_let_lines.resize(self.global_slots.len(), 0);
                        add(name, &mut self.globals, &mut self.global_slots);
                        self.global_let_lines.push(line);
                    }
                }
            }
        }
        self.global_let_lines.resize(self.global_slots.len(), 0);
        debug_assert_eq!(self.global_let_lines.len(), self.global_slots.len());
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
                    ..
                } => {
                    let key = self.type_key(module_idx, name);
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
                // `std/json.chz`'s bodyless `native fn _to_json` — record its home module so the
                // bare-call lowering (mirroring the `timer` arm) can gate on it. Runs in pass 1
                // (every module) before pass 2 compiles any module's bare calls.
                StmtKind::Native(d) if d.name == "_to_json" => {
                    self.json_to_value_home = Some(module_idx);
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
                | StmtKind::Protocol { .. }
                | StmtKind::TypeAlias { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(_)
                | StmtKind::Yield(_)
                | StmtKind::Defer(_)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(_)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::Import(_)
                | StmtKind::Extern { .. }
                | StmtKind::Native(_)
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
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
        for imp in imports {
            match &imp.import {
                Import::Module { path, alias, .. } => {
                    let bind = alias
                        .clone()
                        .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                    if let Some(tidx) = self.program.module_index(&imp.target) {
                        self.imported_modules.insert(bind, tidx);
                    }
                }
                // The resolver never resolves a variant import, so none reaches here.
                Import::Variants { .. } => {}
                Import::From { names, .. } => {
                    if let Some(tidx) = self.program.module_index(&imp.target) {
                        for (member, alias) in names {
                            let bind = alias.clone().unwrap_or_else(|| member.clone());
                            // Remember where every from-imported member was DECLARED, so a call
                            // site can read the callee's witness arity out of the checker's
                            // declaring-module-keyed table (M24 Task 3). A bind that is only a
                            // type is never a witness callee.
                            self.imported_fns.insert(bind, (tidx, member.clone()));
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
                ..
            } = &stmt.kind
            {
                let key = self.type_key(module_idx, name);
                // The struct's `[T]`s are in scope for every method body + field default below (they
                // shadow a same-named module `type T = float` alias at each coercion site).
                let mut test_methods: Vec<String> = Vec::new();
                let mut suite_tests: Vec<(String, ProtoId)> = Vec::new();
                let mut hooks: HashMap<String, ProtoId> = HashMap::new();
                if let Some(set) = self.hook_table.get(&(module_idx, key.clone())) {
                    self.hook_sets.insert(key.clone(), *set);
                }
                // TICKET-255 — `Range` methods are compiled only for a graph that builds a `Range` value.
                let skip =
                    self.range_free && module_idx == 0 && name == crate::resolver::RANGE_STRUCT;
                for m in methods.iter().filter(|_| !skip) {
                    self.pending_witnesses = self.member_witnesses(module_idx, &key, &m.name);
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
            }
        }
        // Compile enum methods next (type-erased — no `StructDef`/`tid`), recording proto ids under
        // the enum's module-scoped runtime key. Mirrors the struct-method pass above.
        for stmt in &module.stmts {
            if let StmtKind::Enum { name, methods, .. } = &stmt.kind {
                if methods.is_empty() {
                    continue;
                }
                let key = self.type_key(module_idx, name);
                let mut compiled: HashMap<String, ProtoId> = HashMap::new();
                if let Some(set) = self.hook_table.get(&(module_idx, key.clone())) {
                    self.hook_sets.insert(key.clone(), *set);
                }
                for m in methods {
                    self.pending_witnesses = self.member_witnesses(module_idx, &key, &m.name);
                    let pid = self.compile_fn(m, false)?;
                    compiled.insert(m.name.clone(), pid);
                }
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
                ..
            } = &stmt.kind
            {
                if bodied_methods.is_empty() {
                    continue;
                }
                let mut compiled: HashMap<String, ProtoId> = HashMap::new();
                for m in bodied_methods {
                    let pid = self.compile_fn(m, false)?;
                    compiled.insert(m.name.clone(), pid);
                }
                self.program
                    .native_methods
                    .entry(name.clone())
                    .or_default()
                    .extend(compiled);
                self.program.native_home.insert(name.clone(), module_idx);
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
                if decl
                    .name
                    .starts_with(crate::base::provider_name::PROVIDER_PREFIX)
                {
                    self.provider_defs
                        .insert(decl.name.clone(), (pid, module_idx));
                }
                fc.emit(Op::MakeFunc(pid), stmt.span);
                let fn_slot = self.global_slot(&decl.name);
                fc.emit(Op::DefineGlobalSlot(fn_slot), stmt.span);
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
                                         (v1 supports only int, float, bool, str, ptr, and a \
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
                        c_variadic: sig.is_some_and(|s| s.c_variadic),
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
            // TICKET-048 — an implicit nursery has no keyword of its own, so it carries the span of
            // the first statement of the block it covers; the deadlock headline renders this.
            fc.emit(
                Op::EnterNursery,
                module
                    .stmts
                    .first()
                    .map(|s| s.span)
                    .unwrap_or(Span::RUNTIME),
            );
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

    /// TICKET-255 — the one emit of `Op::NewStruct`. A `Range` built in a graph that recorded none stops the
    /// build: the skipped methods would otherwise fault at run time with a missing method.
    fn emit_new_struct(
        &self,
        fc: &mut FnComp,
        key: String,
        argc: usize,
        span: Span,
    ) -> Result<(), CompileError> {
        if self.range_free && key == self.type_key(0, crate::resolver::RANGE_STRUCT) {
            return Err(CompileError {
                message: "internal error: this program builds a Range, and ModuleGraph::range_used recorded none"
                    .into(),
                span,
            });
        }
        fc.emit(Op::NewStruct(key, argc), span);
        Ok(())
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
        self.emit_new_struct(&mut fc, key, fields.len(), span)?;
        fc.emit(Op::Return, span);
        Ok(self.finish(fc))
    }
}

/// Build a synthesized `obj.method(args)` expression-statement (used to desugar comprehension
/// accumulation into a method call the existing codegen already handles).
fn method_call_stmt(obj: Expr, method: &str, args: Vec<Expr>, span: Span) -> Stmt {
    let callee = Expr {
        id: crate::ast::NodeId::SYNTH,
        kind: ExprKind::Field {
            obj: Box::new(obj),
            name: method.to_string(),
            name_span: span,
        },
        span,
    };
    Stmt {
        kind: StmtKind::Expr(Expr {
            id: crate::ast::NodeId::SYNTH,
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args,
                named: Vec::new(),
                type_args: Vec::new(),
                bracket: None,
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
        BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Mod
        | BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::In => None,
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
    (start, end, inclusive): (i64, i64, bool),
    fails: &mut Vec<usize>,
    span: Span,
) {
    // scrut >= start
    fc.emit_hidden_get(scrut, span);
    fc.emit(Op::ConstInt(start), span);
    fc.emit(Op::GtEq, span);
    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
    // scrut < end (scrut <= end for `a..=b`)
    fc.emit_hidden_get(scrut, span);
    fc.emit(Op::ConstInt(end), span);
    fc.emit(if inclusive { Op::LtEq } else { Op::Lt }, span);
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
/// Where `emit_pattern` puts a name it binds. `Match` reads the checker's resolution (a refutable
/// pattern: a bare name may be a nullary variant) and declares a fresh local. The binding targets
/// never test: `Decl` is a `:=` (a module global at global scope, else a fresh local) and `Loop`
/// writes a `for` loop's raw slots in binder order.
enum BindInto<'a> {
    Match,
    Decl,
    Loop(std::slice::Iter<'a, usize>),
}

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

/// Render an extern param/return source `Type` for a marshallability error message (the surface
/// spelling the user wrote, e.g. `cdefs.DivT`). Used only by the never-panic backstop, so it covers
/// the spellings that reach an extern boundary; anything else falls back to a plain placeholder.
fn ffi_type_display(ty: Option<&Type>) -> String {
    match ty {
        Some(Type::Named { name: n, .. }) => n.clone(),
        Some(Type::Qualified { module, name, .. }) => format!("{module}.{name}"),
        Some(Type::Generic(n, ..)) => crate::ast::unprivate(n).to_string(),
        Some(_) => "<unsupported>".to_string(),
        None => "None".to_string(),
    }
}

pub(crate) fn block_has_bare_spawn(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_bare_spawn)
}

/// The expression-side counterpart to [`stmt_has_bare_spawn`]: does this expression contain a bare
/// `spawn` statement reachable through an `ExprKind::Recover` block? Exhaustive over `ExprKind` with
/// NO wildcard arm, so a future block-carrying expression form is a compile error here rather than
/// another silent hole. `Closure` is a BOUNDARY, not a recursion: a `spawn` inside a closure body
/// binds to the closure's own implicit nursery (gated separately at its compile site), never to the
/// enclosing frame's.
fn expr_has_bare_spawn(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Closure { .. } => false,
        ExprKind::Recover(block, _) => block_has_bare_spawn(block),
        ExprKind::ElseGuard { value, body, .. } => {
            expr_has_bare_spawn(value) || block_has_bare_spawn(body)
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_) => false,
        ExprKind::Interp(chunks) => chunk_exprs(chunks).into_iter().any(expr_has_bare_spawn),
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().any(expr_has_bare_spawn)
        }
        ExprKind::Map(pairs) => pairs
            .iter()
            .any(|(k, v)| expr_has_bare_spawn(k) || expr_has_bare_spawn(v)),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            clauses
                .iter()
                .any(|c| expr_has_bare_spawn(&c.iter) || c.guards.iter().any(expr_has_bare_spawn))
                || key.as_deref().is_some_and(expr_has_bare_spawn)
                || expr_has_bare_spawn(elem)
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => expr_has_bare_spawn(expr),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
            expr_has_bare_spawn(lhs) || expr_has_bare_spawn(rhs)
        }
        ExprKind::Compare { operands, .. } => operands.iter().any(expr_has_bare_spawn),
        ExprKind::Range { start, end, .. } => [start, end]
            .into_iter()
            .flatten()
            .any(|b| expr_has_bare_spawn(b)),
        ExprKind::Call {
            callee,
            args,
            named,
            bracket,
            ..
        } => {
            expr_has_bare_spawn(callee)
                || args.iter().any(expr_has_bare_spawn)
                || bracket.as_deref().is_some_and(expr_has_bare_spawn)
                || named.iter().any(|(_, v)| expr_has_bare_spawn(v))
        }
        ExprKind::Field { obj, .. } => expr_has_bare_spawn(obj),
        ExprKind::OptChain { obj, call, .. } => {
            expr_has_bare_spawn(obj)
                || call.as_ref().is_some_and(|c| {
                    c.args.iter().any(expr_has_bare_spawn)
                        || c.named.iter().any(|(_, v)| expr_has_bare_spawn(v))
                })
        }
        ExprKind::Index { obj, index, .. } => {
            expr_has_bare_spawn(obj) || index.as_deref().is_some_and(expr_has_bare_spawn)
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_has_bare_spawn(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(expr_has_bare_spawn)
                        || expr_has_bare_spawn(&a.body)
                })
        }
        ExprKind::IfElse { cond, then, els } => {
            expr_has_bare_spawn(cond) || expr_has_bare_spawn(then) || expr_has_bare_spawn(els)
        }
    }
}

fn stmt_has_bare_spawn(s: &Stmt) -> bool {
    match &s.kind {
        StmtKind::Spawn(_) => true,
        // Boundaries: spawns here do not belong to the enclosing implicit nursery.
        StmtKind::Parallel { .. } | StmtKind::Fn(_) => false,
        // Recurse through ordinary control flow, also walking each header expression.
        StmtKind::Let { value, .. } => expr_has_bare_spawn(value),
        StmtKind::Assign { target, value, .. } => {
            expr_has_bare_spawn(target) || expr_has_bare_spawn(value)
        }
        StmtKind::Return(v) => v.as_ref().is_some_and(expr_has_bare_spawn),
        StmtKind::Yield(e) => expr_has_bare_spawn(e),
        StmtKind::Assert { cond, msg } => {
            expr_has_bare_spawn(cond) || msg.as_ref().is_some_and(expr_has_bare_spawn)
        }
        StmtKind::Expr(e) => expr_has_bare_spawn(e),
        StmtKind::If {
            branches,
            else_block,
        } => {
            branches
                .iter()
                .any(|(c, b)| expr_has_bare_spawn(c) || block_has_bare_spawn(b))
                || else_block.as_ref().is_some_and(|b| block_has_bare_spawn(b))
        }
        StmtKind::For { iter, body, .. } => expr_has_bare_spawn(iter) || block_has_bare_spawn(body),
        StmtKind::While { cond, body } => expr_has_bare_spawn(cond) || block_has_bare_spawn(body),
        StmtKind::Match { scrutinee, arms } => {
            expr_has_bare_spawn(scrutinee)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(expr_has_bare_spawn)
                        || block_has_bare_spawn(&a.body)
                })
        }
        StmtKind::Wait { arms, else_block } => {
            arms.iter().any(|a| {
                let header = match &a.kind {
                    WaitArmKind::Recv { chan, .. } => expr_has_bare_spawn(chan),
                    WaitArmKind::Send { call } => expr_has_bare_spawn(call),
                };
                header || block_has_bare_spawn(&a.body)
            }) || else_block.as_ref().is_some_and(|b| block_has_bare_spawn(b))
        }
        // Boundaries / non-expression-carrying: a struct/enum declaration's field defaults compile
        // in a different frame (their own gate, if any). `Defer(DeferTarget::Block)` is gated at its
        // own compile site (`:4716`) since it runs in its own frame; `Defer(DeferTarget::Call)`'s
        // arguments compile EAGERLY in this (parent) frame (`compile_receiverless_target`, `:5070`)
        // and are NOT walked here — a bare `spawn` inside a `recover:` nested in a deferred call's
        // argument stays ungated (known residual, not exercised by this ticket's tests).
        StmtKind::Defer(_)
        | StmtKind::Struct { .. }
        | StmtKind::Protocol { .. }
        | StmtKind::Enum { .. }
        | StmtKind::TypeAlias { .. }
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_)
        | StmtKind::Extern { .. }
        | StmtKind::Native(_)
        | StmtKind::NativeStruct { .. }
        | StmtKind::NativeEnum { .. }
        | StmtKind::NativeType { .. } => false,
    }
}
