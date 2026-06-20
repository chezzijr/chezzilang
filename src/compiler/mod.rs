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
use crate::native::cffi::CType;
use crate::resolver::{ModuleGraph, ResolvedImport};
use crate::vm::op::{
    CapEntry, CapSrc, CffiDef, LIFECYCLE_HOOKS, ModuleProto, NO_IC, Op, Program, Proto, ProtoId,
    StructDef, SuiteInfo, VariantDef, WaitMeta,
};
use crate::{lexer, parser};
use std::collections::HashMap;

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

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "range"
            | "int"
            | "float"
            | "str"
            | "ord"
            | "chr"
            | "set"
            | "list"
            | "map"
            | "bytes"
            | "bytearray"
    )
}

/// Compile a whole resolved module graph in dependency order.
pub fn compile_graph(graph: &ModuleGraph) -> Result<Program, CompileError> {
    let mut c = Compiler::new();
    // FFI ROOT FIX (fix4): resolve every extern fn's C signature ONCE in the checker (module-scoped,
    // every alias spelling), then lower each extern from that table — never re-resolving alias names
    // in the backend's flat bare map (the collision whack-a-mole's root). Keyed by graph module idx.
    c.extern_sigs = crate::checker::resolve_extern_signatures(graph);
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
    /// M19 Phase 2b — the current module's global name → slot map, rebuilt at the start of each
    /// `compile_module`. Shared across the toplevel proto and every fn/method/closure compiled for
    /// the module, so a global reference anywhere in the module resolves to the same slot.
    globals: HashMap<String, u32>,
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
}

/// M19 lever #2 — register an enum variant into BOTH program tables, assigning it the next dense
/// `variant_id` (`variants_by_id.len()`, a global gap-free counter — the analogue of how `StructDef`
/// gets its `tid` from `structs.len()`). Keeps `variants` (`(enum, variant)`→def) and `variants_by_id`
/// (id→def) in lockstep so cold-path id→names resolution is O(1). The map is keyed by the
/// `(enum, variant)` pair, so two enums may share a variant name yet get distinct ids.
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
        // `std.regex`, `Response` from `std.request`). They have no AST (the checker seeds their
        // shapes in `seed_stdlib_structs`), so with the positional struct layout the runtime must
        // know their declaration-order field names HERE to resolve field reads + Display. Order must
        // match the native builders (`match_to_ret` / `response_ret`) and the checker's seed.
        for (name, fields) in [
            ("Match", &["text", "start", "end", "groups"][..]),
            ("Response", &["status", "body", "headers"][..]),
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
            globals: HashMap::new(),
            global_slots: Vec::new(),
            field_ic_next: 0,
            method_ic_next: 0,
            type_keys: HashMap::new(),
            module_types: Vec::new(),
            imported_modules: HashMap::new(),
            current_module_idx: 0,
            bare_types: HashMap::new(),
            extern_sigs: crate::checker::ExternTable::new(),
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
            if lm.native.is_some() {
                continue;
            }
            // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (no qualified
            // key entry → `type_key` falls back to bare), so `Ref`/`Iterator`/FFI widths resolve bare.
            let is_std = lm.is_std();
            for s in &lm.ast.stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
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
        let add = |name: String, globals: &mut HashMap<String, u32>, slots: &mut Vec<String>| {
            if !globals.contains_key(&name) {
                globals.insert(name.clone(), slots.len() as u32);
                slots.push(name);
            }
        };
        for imp in imports {
            match &imp.import {
                Import::Module { path, alias } => {
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
                StmtKind::Struct { name, fields, .. } => {
                    let key = self.type_key(module_idx, name);
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
                StmtKind::Enum { name, variants, .. } => {
                    // M19 lever #2 — assign each variant the next dense `variant_id` (user variants
                    // follow the fixed native ids at `4..`, in declaration order). The enum is keyed
                    // by its module-scoped runtime key (bare unless a cross-module clash).
                    let key = self.type_key(module_idx, name);
                    for v in variants {
                        register_variant(&mut self.program, &key, &v.name, v.payload.len());
                    }
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
                Import::Module { path, alias } => {
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
                for m in methods {
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
        // The synthetic toplevel function: top-level `fn`s are hoisted as globals before the body.
        let mut fc = FnComp::new("<toplevel>".to_string(), 0, true);
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
                Some(d) => self.compile_expr(&mut fc, d)?,
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
        let mut fc = FnComp::new(decl.name.clone(), decl.params.len(), false);
        fc.is_generator = decl.is_generator;
        fc.is_test = decl.is_test;
        for p in &decl.params {
            fc.add_local(p.name.clone());
        }
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
            StmtKind::Let { names, value, .. } => {
                self.compile_expr(fc, value)?;
                if names.len() > 1 {
                    // destructuring let `a, b := value`: stash the tuple in a hidden local, then for
                    // each binding load it and read element `.i` (the tuple-aware `GetField`). No new
                    // index op — `GetField("i")` on a tuple is the element access.
                    let tuple_slot = fc.add_hidden();
                    fc.emit(Op::SetLocal(tuple_slot), stmt.span);
                    for (i, name) in names.iter().enumerate() {
                        fc.emit(Op::GetLocal(tuple_slot), stmt.span);
                        fc.emit(Op::GetField { name: i.to_string(), ic: NO_IC }, stmt.span); // tuple element
                        if fc.is_global_scope() {
                            fc.emit(Op::DefineGlobalSlot(self.global_slot(name)), stmt.span);
                        } else {
                            let slot = fc.add_local(name.clone());
                            fc.emit(Op::SetLocal(slot), stmt.span);
                        }
                    }
                } else if fc.is_global_scope() {
                    fc.emit(Op::DefineGlobalSlot(self.global_slot(&names[0])), stmt.span);
                } else {
                    let slot = fc.add_local(names[0].clone());
                    fc.emit(Op::SetLocal(slot), stmt.span);
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
            // statement nested in a block defines a local function value.
            StmtKind::Fn(decl) => {
                if fc.is_global_scope() {
                    Ok(()) // already hoisted
                } else {
                    let pid = self.compile_fn(decl, false)?;
                    fc.emit(Op::MakeFunc(pid), stmt.span);
                    let slot = fc.add_local(decl.name.clone());
                    fc.emit(Op::SetLocal(slot), stmt.span);
                    Ok(())
                }
            }
            StmtKind::Struct { .. }
            | StmtKind::Enum { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Extern { .. } // bound at module init (see compile_module), like top-level fn
            | StmtKind::TypeAlias { .. }
            | StmtKind::Import(_) => Ok(()),
            StmtKind::Return(value) => {
                match value {
                    Some(e) => self.compile_expr(fc, e)?,
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
                // join (after the defers, matching the interp's unwind order: body defers, then the
                // `exec_parallel` reclaim).
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
            StmtKind::For { vars, iter, body } => self.compile_for(fc, vars, iter, body, stmt.span),
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
                    let slot = fc.add_local(name.clone());
                    fc.emit(Op::SetLocal(slot), arm.span);
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
            ExprKind::Field { obj, name } => {
                let tmp = fc.add_hidden();
                fc.emit(Op::SetLocal(tmp), span);
                self.compile_expr(fc, obj)?;
                fc.emit(Op::GetLocal(tmp), span);
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
                fc.emit(Op::SetLocal(tmp), span);
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                fc.emit(Op::GetLocal(tmp), span);
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
    /// immediately), then join at the dedent. The body is also a defer scope (mirrors the
    /// interpreter's `exec_scoped_block`), so a `defer` directly inside the block runs at the dedent.
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
        self.compile_defer_scoped_block(fc, body)?;
        fc.nursery_scopes -= 1;
        fc.emit(Op::JoinNursery, span);
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
                let ExprKind::Call { callee, args, .. } = &call.kind else {
                    return Err(CompileError {
                        message: "spawn requires a function or method call".to_string(),
                        span,
                    });
                };
                if let ExprKind::Field { obj, name } = &callee.kind {
                    self.compile_expr(fc, obj)?;
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::SpawnMethod(name.clone(), args.len()), call.span);
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
                // Capture every visible enclosing binding (like a closure); the values are
                // deep-copied across the airlock at `SpawnBlock`. The block becomes a synthetic
                // zero-arg proto whose free names resolve via `GetCaptured`.
                let entries = fc.snapshot_entries();
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<spawned task>".to_string(), 0, false);
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
            ExprKind::Field { obj, name } => {
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
                fc.emit(Op::SetLocal(tuple_slot), span);
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
                fc.emit(Op::GetLocal(tuple_slot), span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                self.emit_store(fc, name, span);
            }
            ExprKind::Field { obj, name } => {
                self.compile_expr(fc, obj)?;
                fc.emit(Op::GetLocal(tuple_slot), span);
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
                fc.emit(Op::GetLocal(tuple_slot), span);
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
            Some(slot) => fc.emit(Op::SetLocal(slot), span),
            None => fc.emit(Op::SetGlobalSlot(self.global_slot(name)), span),
        }
    }

    /// Load a name's value (local → captured → global), mirroring the interpreter's lookup order.
    fn emit_load(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            Some(slot) => fc.emit(Op::GetLocal(slot), span),
            None if fc.captures(name) => {
                // Positional capture (lever #3): the slot is the name's index in this closure's
                // `captured_names` (the snapshot order `MakeClosure` populated). `captures` just
                // confirmed membership, so `position` always finds it.
                let slot = fc
                    .captured_names
                    .iter()
                    .position(|n| n == name)
                    .expect("captures() implies a capture slot") as u32;
                fc.emit(Op::GetCaptured(slot), span);
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
            fc.emit(Op::SetLocal(end_slot), span);
            self.compile_expr(fc, start)?;
            fc.emit(Op::AsInt, start.span);
            let i_slot = fc.add_local(vars[0].clone());
            fc.emit(Op::SetLocal(i_slot), span);

            let loop_start = fc.here();
            fc.emit(Op::GetLocal(i_slot), span);
            fc.emit(Op::GetLocal(end_slot), span);
            fc.emit(Op::Lt, span);
            let exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` must land HERE — on the increment, not the condition (re-testing without
            // advancing `i` would loop forever) and not after it (skipping the advance also hangs).
            let inc_target = fc.here();
            fc.emit(Op::GetLocal(i_slot), span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit(Op::SetLocal(i_slot), span);
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
            fc.emit(Op::SetLocal(iter_slot), span);
            // ONE-TIME pure-`Iterable` conversion: a struct with `iter()` but no `next()` becomes its
            // cursor here (then drives via the seq path); every other iterand (struct-with-`next`,
            // generator, collection) passes through unchanged, so their fast paths are byte-identical.
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::IterableToCursor, iter.span);
            fc.emit(Op::SetLocal(iter_slot), span);
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::IsChannel, span);
            let chan_mode_slot = fc.add_hidden(); // true ⇒ channel path (ChanRecvOrClosed)
            fc.emit(Op::SetLocal(chan_mode_slot), span);
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::IsStruct, span);
            let struct_mode_slot = fc.add_hidden(); // true ⇒ struct-iterator path (next())
            fc.emit(Op::SetLocal(struct_mode_slot), span);
            // A generator result (experimental, VM-only) answers `next()` intrinsically, so it rides
            // the exact same lazy step as a struct iterator: force `struct_mode` true when the iterand
            // is a generator. (Kept off the seq path, which would wrongly snapshot it to a list.)
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::IsGenerator, span);
            let not_gen = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::True, span);
            fc.emit(Op::SetLocal(struct_mode_slot), span);
            fc.patch_jump(not_gen);
            // The loop variable, plus the seq-path bookkeeping slots (allocated unconditionally; the
            // lazy paths simply never touch them) and the lazy paths' `Option` result slot.
            let item_slot = fc.add_local(vars[0].clone());
            let lst_slot = fc.add_hidden();
            let len_slot = fc.add_hidden();
            let idx_slot = fc.add_hidden();
            let opt_slot = fc.add_hidden();

            // Seq init (skipped on BOTH lazy paths): snapshot the iterand to a list, take its length,
            // start the index at 0. Skip when channel OR struct.
            fc.emit(Op::GetLocal(chan_mode_slot), span);
            let chan_to_check_struct = fc.emit_jump(Op::JumpIfFalse(0), span); // not chan ⇒ check struct
            let chan_skip_init = fc.emit_jump(Op::Jump(0), span); // chan ⇒ skip seq init
            fc.patch_jump(chan_to_check_struct);
            fc.emit(Op::GetLocal(struct_mode_slot), span);
            let to_seq_init = fc.emit_jump(Op::JumpIfFalse(0), span); // not struct ⇒ run seq init
            let struct_skip_init = fc.emit_jump(Op::Jump(0), span); // struct ⇒ skip seq init
            fc.patch_jump(to_seq_init);
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit(Op::SetLocal(lst_slot), span);
            fc.emit(Op::GetLocal(lst_slot), span);
            fc.emit(Op::ArrLen, span);
            fc.emit(Op::SetLocal(len_slot), span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit(Op::SetLocal(idx_slot), span);
            fc.patch_jump(chan_skip_init);
            fc.patch_jump(struct_skip_init);

            let loop_head = fc.here();
            // Channel step: `ChanRecvOrClosed` → opt_slot (blocks on empty-open, None on closed+drained).
            fc.emit(Op::GetLocal(chan_mode_slot), span);
            let chan_to_struct = fc.emit_jump(Op::JumpIfFalse(0), span); // not chan ⇒ struct/seq
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::ChanRecvOrClosed, iter.span);
            fc.emit(Op::SetLocal(opt_slot), span);
            let chan_to_opt = fc.emit_jump(Op::Jump(0), span); // ⇒ shared Option decoder
            fc.patch_jump(chan_to_struct);
            // Struct vs seq split.
            fc.emit(Op::GetLocal(struct_mode_slot), span);
            let to_seq_step = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ seq step
            // ----- struct step: call next() → opt_slot -----
            fc.emit(Op::GetLocal(iter_slot), span);
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: "next".to_string(),
                    argc: 0,
                    ic,
                },
                span,
            );
            fc.emit(Op::SetLocal(opt_slot), span);
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
            // `Some(v)`: a match binds the payload into the loop variable's slot and falls through to
            // the body jump; a non-Some jumps to the trap below.
            let some_arm = fc.emit_jump(
                Op::MatchArm {
                    scrut: opt_slot,
                    variant: "Some".to_string(),
                    variant_id: crate::vm::op::VID_SOME,
                    enum_name: None,
                    nbind: 1,
                    bind_start: item_slot,
                    next: 0,
                },
                iter.span,
            );
            let to_body = fc.emit_jump(Op::Jump(0), span); // Some matched ⇒ run the body
            fc.patch_jump(some_arm); // neither None nor Some ⇒ the trap
            fc.emit(Op::MatchNoArm(opt_slot), iter.span); // not Option ⇒ runtime trap
            // ----- seq step: bounds-check the index, read the element -----
            fc.patch_jump(to_seq_step);
            fc.emit(Op::GetLocal(idx_slot), span);
            fc.emit(Op::GetLocal(len_slot), span);
            fc.emit(Op::Lt, span);
            let seq_exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::GetLocal(lst_slot), span);
            fc.emit(Op::GetLocal(idx_slot), span);
            fc.emit(Op::GetIndex, span);
            fc.emit(Op::SetLocal(item_slot), span);

            fc.patch_jump(to_body);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the advance step. For a channel/struct, "advance" is just
            // re-looping (the next lazy step); for a sequence, it's the index increment.
            let inc_target = fc.here();
            fc.emit(Op::GetLocal(chan_mode_slot), span);
            let inc_check_struct = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // channel: re-loop, ChanRecvOrClosed advances
            fc.patch_jump(inc_check_struct);
            fc.emit(Op::GetLocal(struct_mode_slot), span);
            let to_seq_inc = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // struct: re-loop, next() advances
            fc.patch_jump(to_seq_inc);
            fc.emit(Op::GetLocal(idx_slot), span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit(Op::SetLocal(idx_slot), span);
            fc.emit(Op::Jump(loop_head), span);
            // All exit paths land here (past the back-edge).
            fc.patch_jump(lazy_exit);
            fc.patch_jump(seq_exit);
            self.patch_loop(fc, inc_target);
        } else {
            // Multi-name `for`: either `for k, v in m` over a MAP (key, value) or tuple-destructuring
            // `for a, b, … in xs` over a `list[(A, B, …)]`. The compiler is type-erased, so we branch
            // at RUNTIME on `IsMap` (mirroring the single-var `IsStruct` split):
            //   - map: snapshot keys + values up front and index them in lockstep (so a body that
            //     mutates the map mid-loop can't perturb the bindings; matches the interpreter);
            //   - list of tuples: index the list, then destructure each element tuple into the N
            //     loop vars via `GetField(j)` (the destructure-`:=` pattern, generalized to N).
            self.compile_expr(fc, iter)?;
            let src_slot = fc.add_hidden();
            fc.emit(Op::SetLocal(src_slot), span);
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::IsMap, span);
            let mode_slot = fc.add_hidden(); // true ⇒ map path
            fc.emit(Op::SetLocal(mode_slot), span);

            let lst = fc.add_hidden(); // the list we index (map keys, or the list of tuples)
            let vals = fc.add_hidden(); // map values snapshot (map path only)
            let len = fc.add_hidden();
            let idx = fc.add_hidden();
            let elem = fc.add_hidden(); // the element read at lst[idx]
            let var_slots: Vec<usize> = vars.iter().map(|v| fc.add_local(v.clone())).collect();

            // ----- init: branch map vs list -----
            fc.emit(Op::GetLocal(mode_slot), span);
            let to_list_init = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ list init
            // map init: keys snapshot into `lst`, values snapshot into `vals` (same instant/order)
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit(Op::SetLocal(lst), span);
            fc.emit(Op::GetLocal(src_slot), span);
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: "values".to_string(),
                    argc: 0,
                    ic,
                },
                span,
            );
            fc.emit(Op::SetLocal(vals), span);
            let after_init = fc.emit_jump(Op::Jump(0), span);
            // list init: clone the list of tuples into `lst`
            fc.patch_jump(to_list_init);
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit(Op::SetLocal(lst), span);
            fc.patch_jump(after_init);
            // common: len = lst.len(), idx = 0
            fc.emit(Op::GetLocal(lst), span);
            fc.emit(Op::ArrLen, span);
            fc.emit(Op::SetLocal(len), span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit(Op::SetLocal(idx), span);

            let loop_start = fc.here();
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::GetLocal(len), span);
            fc.emit(Op::Lt, span);
            let exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            // elem = lst[idx]
            fc.emit(Op::GetLocal(lst), span);
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::GetIndex, span);
            fc.emit(Op::SetLocal(elem), span);
            // ----- bind: branch map vs list -----
            fc.emit(Op::GetLocal(mode_slot), span);
            let to_list_bind = fc.emit_jump(Op::JumpIfFalse(0), span);
            // map bind: var[0] = key (elem), var[1] = vals[idx]
            fc.emit(Op::GetLocal(elem), span);
            fc.emit(Op::SetLocal(var_slots[0]), span);
            fc.emit(Op::GetLocal(vals), span);
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::GetIndex, span);
            fc.emit(Op::SetLocal(var_slots[1]), span);
            let after_bind = fc.emit_jump(Op::Jump(0), span);
            // list bind: destructure the tuple element into each loop var (var[j] = elem.j)
            fc.patch_jump(to_list_bind);
            for (j, &vs) in var_slots.iter().enumerate() {
                fc.emit(Op::GetLocal(elem), span);
                fc.emit(
                    Op::GetField {
                        name: j.to_string(),
                        ic: NO_IC,
                    },
                    span,
                ); // tuple element
                fc.emit(Op::SetLocal(vs), span);
            }
            fc.patch_jump(after_bind);

            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the index increment, so the loop advances instead of looping.
            let inc_target = fc.here();
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit(Op::SetLocal(idx), span);
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
        let acc_slot = fc.add_local(acc_name.clone());
        fc.emit(Op::SetLocal(acc_slot), span);

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
                },
            ..
        } = &body[0]
        else {
            unreachable!("a comprehension always has at least one for clause")
        };
        self.compile_for(fc, vars, iter, inner, span)?;
        // The comprehension's value is the finished accumulator.
        fc.emit(Op::GetLocal(acc_slot), span);
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
            Pattern::Ident(n) if !self.is_nullary_variant(None, n) => {
                out.insert(n.clone());
            }
            Pattern::Ident(_) => {}
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
        fc.emit(Op::SetLocal(scrut), span);
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
            Pattern::Ident(name) => {
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
                    let s = fc.add_local(name.clone());
                    fc.emit(Op::GetLocal(scrut), span);
                    fc.emit(Op::SetLocal(s), span);
                }
            }
            Pattern::Literal(lit) => {
                fc.emit(Op::GetLocal(scrut), span);
                emit_lit_const(fc, lit, span);
                fc.emit(Op::Eq, span);
                fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
            }
            Pattern::Range { start, end } => {
                emit_range_test(fc, scrut, *start, *end, fails, span);
            }
            Pattern::Tuple(subs) => {
                for (i, sub) in subs.iter().enumerate() {
                    fc.emit(Op::GetLocal(scrut), span);
                    fc.emit(
                        Op::GetField {
                            name: i.to_string(),
                            ic: NO_IC,
                        },
                        span,
                    ); // tuple element `.i`
                    let elem = fc.add_hidden();
                    fc.emit(Op::SetLocal(elem), span);
                    self.emit_pattern(fc, sub, elem, fails, span)?;
                }
            }
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                ..
            } => {
                // One slot per payload element. A plain `Ident` *binding* names its slot directly (so
                // `Some(c)` binds `c` with no copy); a nested nullary-variant `Ident` (e.g. the
                // `None` in `Some(None)`) and any other sub-pattern get a hidden slot to test/
                // destructure afterwards.
                let bind_start = fc.next_slot();
                for b in bindings {
                    match b {
                        Pattern::Ident(n) if !self.is_nullary_variant(None, n) => {
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
                    let is_plain_binding =
                        matches!(b, Pattern::Ident(n) if !self.is_nullary_variant(None, n));
                    if !is_plain_binding {
                        self.emit_pattern(fc, b, bind_start + i, fails, span)?;
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
                    .map(|n| (n.clone(), fc.add_local(n.clone())))
                    .collect();
                let mut matched_jumps = Vec::new();
                for (idx, alt) in alts.iter().enumerate() {
                    // Scope each alternative's scratch slots so they don't leak between alternatives.
                    fc.begin_scope();
                    let mut alt_fails = Vec::new();
                    self.emit_pattern(fc, alt, scrut, &mut alt_fails, span)?;
                    // Copy this alternative's bindings into the canonical slots, then jump to the
                    // shared matched-label (fall-through past the remaining alternatives).
                    for (name, slot) in &canon {
                        let src = fc.resolve_local(name).expect("alt binds the agreed name");
                        if src != *slot {
                            fc.emit(Op::GetLocal(src), span);
                            fc.emit(Op::SetLocal(*slot), span);
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
        fc.emit(Op::SetLocal(scrut), span);
        let mut end_jumps = Vec::new();
        for arm in arms {
            match arm.pattern() {
                Pattern::Literal(lit) => {
                    fc.begin_scope();
                    fc.emit(Op::GetLocal(scrut), scrutinee.span);
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
                    let s = fc.add_local(name.clone());
                    fc.emit(Op::GetLocal(scrut), span);
                    fc.emit(Op::SetLocal(s), span);
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
                                fc.emit(Op::GetLocal(scrut), scrutinee.span);
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
                Pattern::Variant { .. } | Pattern::Tuple(_) | Pattern::Ident(_) => {
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
        match &expr.kind {
            ExprKind::Int(n) => fc.emit(Op::ConstInt(*n), expr.span),
            ExprKind::Float(x) => fc.emit(Op::ConstFloat(*x), expr.span),
            ExprKind::Bool(b) => fc.emit(if *b { Op::True } else { Op::False }, expr.span),
            ExprKind::Str(raw) => self.compile_str(fc, raw, expr.span)?,
            ExprKind::Bytes(b) => fc.emit(Op::ConstBytes(b.clone().into_boxed_slice()), expr.span),
            ExprKind::Ident(name) => self.compile_ident(fc, name, expr.span),
            ExprKind::List(items) => {
                for it in items {
                    self.compile_expr(fc, it)?;
                }
                fc.emit(Op::NewList(items.len()), expr.span);
            }
            ExprKind::Tuple(items) => {
                for it in items {
                    self.compile_expr(fc, it)?;
                }
                fc.emit(Op::NewTuple(items.len()), expr.span);
            }
            ExprKind::Map(entries) => {
                // Push `[k0, v0, k1, v1, …]`, then build the map (last duplicate key wins at runtime).
                for (k, v) in entries {
                    self.compile_expr(fc, k)?;
                    self.compile_expr(fc, v)?;
                }
                fc.emit(Op::NewMap(entries.len()), expr.span);
            }
            ExprKind::Set(elems) => {
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
                // The interpreter only evaluates ranges inside `for`; a bare range has no value.
                return Err(CompileError {
                    message: "a range can only be used as the iterable of a `for` loop".to_string(),
                    span: expr.span,
                });
            }
            // `type_args` are type-erased — the compiler never sees them (checker already used them).
            ExprKind::Call { callee, args, .. } => {
                self.compile_call(fc, callee, args, expr.span)?
            }
            ExprKind::Field { obj, name } => {
                // `module.Enum.Variant` (nullary, qualified value form) → construct the variant.
                if let ExprKind::Field {
                    obj: inner,
                    name: ename,
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
                self.compile_expr(fc, obj)?;
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
            ExprKind::DecodeCall { obj, ty, arg } => {
                // Reuse the module's own `parse` (`obj.parse(arg)` → Result[Json]), then coerce the
                // parsed value into the target type with a descriptor built from `ty`.
                let parse_call = Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            kind: ExprKind::Field {
                                obj: obj.clone(),
                                name: "parse".to_string(),
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
                // `defer:` block → a synthetic zero-arg closure capturing the visible bindings by
                // value at the defer point (exactly `compile_spawn`'s Block arm, minus the airlock),
                // then defer-invoke it with 0 args. Reuses `MakeClosure` + `DeferCall` — no new op.
                let entries = fc.snapshot_entries();
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<deferred block>".to_string(), 0, false);
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
        let ExprKind::Call { callee, args, .. } = &call.kind else {
            return Err(CompileError {
                message: "defer requires a function or method call".to_string(),
                span,
            });
        };
        if let ExprKind::Field { obj, name } = &callee.kind {
            self.compile_expr(fc, obj)?;
            for a in args {
                self.compile_expr(fc, a)?;
            }
            fc.emit(Op::DeferMethod(name.clone(), args.len()), call.span);
            return Ok(());
        }
        self.compile_expr(fc, callee)?;
        for a in args {
            self.compile_expr(fc, a)?;
        }
        fc.emit(Op::DeferCall(args.len()), call.span);
        Ok(())
    }

    fn compile_call(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> Result<(), CompileError> {
        // Method / module-member call: `obj.name(args)`.
        if let ExprKind::Field { obj, name } = &callee.kind {
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
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::NewStruct(key, args.len()), span);
                    return Ok(());
                }
            }
            // `module.Enum.Variant(args)` → qualified payload-variant constructor. `obj` is the
            // qualified `module.Enum`; resolve the enum's runtime key in the target module.
            if let ExprKind::Field {
                obj: inner,
                name: ename,
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
            if name == "print" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::CallPrint(args.len()), span);
                return Ok(());
            }
            if is_builtin(name) {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::CallBuiltin(name.clone(), args.len()), span);
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
                for a in args {
                    self.compile_expr(fc, a)?;
                }
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
        // Snapshot every binding currently visible in the enclosing frame (matches the interpreter
        // capturing all in-scope local frames).
        let entries = fc.snapshot_entries();
        let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

        let mut child = FnComp::new("<closure>".to_string(), params.len(), false);
        child.captured_names = captured_names;
        for p in params {
            child.add_local(p.name.clone());
        }
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
        let chunks = parse_interpolation(raw, span)?;
        if let [Chunk::Lit(s)] = chunks.as_slice() {
            fc.emit(Op::ConstStr(s.clone()), span);
            return Ok(());
        }
        if chunks.is_empty() {
            fc.emit(Op::ConstStr(String::new()), span);
            return Ok(());
        }
        let n = chunks.len();
        for chunk in chunks {
            match chunk {
                Chunk::Lit(s) => fc.emit(Op::ConstStr(s), span),
                Chunk::Expr(e, spec) => {
                    self.compile_expr(fc, &e)?;
                    match spec {
                        None => fc.emit(Op::ToStr, span),
                        Some(fs) => fc.emit(Op::ToStrFmt(Box::new(fs)), span),
                    }
                }
            }
        }
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
    fc.emit(Op::GetLocal(scrut), span);
    fc.emit(Op::ConstInt(start), span);
    fc.emit(Op::GtEq, span);
    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
    // scrut < end
    fc.emit(Op::GetLocal(scrut), span);
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

// ===== string interpolation (compile-time pre-parse) =====

#[derive(Debug)]
enum Chunk {
    Lit(String),
    /// An interpolated `{expr}` or `{expr:spec}`; the format spec (parsed at compile time) is
    /// `None` for a bare `{expr}`.
    Expr(Expr, Option<crate::fmtspec::FormatSpec>),
}

/// Split an interpolated string literal into literal/expr chunks, mirroring `interp::interpolate`
/// (but at compile time): `{{`/`}}` are literal braces; each `{ … }` is lexed + parsed as an
/// expression. A malformed interpolation surfaces here as a compile error.
fn parse_interpolation(raw: &str, span: Span) -> Result<Vec<Chunk>, CompileError> {
    let mut chunks = Vec::new();
    let mut lit = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                if !lit.is_empty() {
                    chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
                }
                let mut inner = String::new();
                let mut closed = false;
                for ic in chars.by_ref() {
                    if ic == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(ic);
                }
                if !closed {
                    return Err(CompileError {
                        message: "unterminated '{' in interpolated string".to_string(),
                        span,
                    });
                }
                // Split on the first top-level `:` into (expr, spec); a `:` inside brackets/quotes
                // (e.g. `{m["a:b"]}`, slices `a[1:2]`) is NOT a separator. Spec parse errors are
                // surfaced as compile errors (good UX); type/value mismatches are deferred to the VM.
                let (expr_src, spec_src) = crate::fmtspec::split_spec(&inner);
                let spec = match spec_src {
                    Some(s) => Some(
                        crate::fmtspec::parse(s)
                            .map_err(|message| CompileError { message, span })?,
                    ),
                    None => None,
                };
                let expr = parse_expr_str(expr_src, span)?;
                chunks.push(Chunk::Expr(expr, spec));
            }
            '}' => {
                return Err(CompileError {
                    message: "unmatched '}' in string (use '}}' for a literal brace)".to_string(),
                    span,
                });
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        chunks.push(Chunk::Lit(lit));
    }
    Ok(chunks)
}

fn parse_expr_str(src: &str, span: Span) -> Result<Expr, CompileError> {
    let tokens = lexer::tokenize(src).map_err(|e| CompileError {
        message: e.to_string(),
        span,
    })?;
    let mut expr = parser::parse_expr(tokens).map_err(|e| CompileError {
        message: e.message,
        span,
    })?;
    // Fragments bypass the module-wide desugar pass; lower `?.`/`??` carriers here (both engines do).
    crate::desugar::lower_carriers(&mut expr);
    Ok(expr)
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
        Some(Type::Named(n)) => n.clone(),
        Some(Type::Qualified { module, name, .. }) => format!("{module}.{name}"),
        Some(Type::Generic(n, _)) => n.clone(),
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
            loops: Vec::new(),
            defer_scopes: 0,
            nursery_scopes: 0,
            has_implicit_nursery: false,
            is_generator: false,
            is_test: false,
        }
    }

    /// The innermost loop being compiled, or `None` outside any loop.
    fn current_loop(&mut self) -> Option<&mut LoopCtx> {
        self.loops.last_mut()
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
}
