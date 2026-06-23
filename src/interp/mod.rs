//! Tree-walk interpreter (M3): executes an AST `Module` directly — the reference semantics for
//! Chezzi before the bytecode VM (M5). Single-file programs run here.

use crate::ast::{
    AssignOp, BinaryOp, Block, CompClause, CompKind, DeferTarget, Expr, ExprKind, FnDecl, Import,
    LitPattern, MatchArm, MatchExprArm, Pattern, Span, SpawnTarget, Stmt, StmtKind, UnaryOp,
    WaitArm, WaitTarget,
};
use crate::{lexer, parser};

mod builtins;
mod env;
mod value;

pub use value::Value;
use value::{MapData, SetData};

/// A runtime error, with the source span it occurred at.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// One frame of a runtime stack trace: a function and the call site that entered it. Mirrors
/// `vm::TraceFrame`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceFrame {
    pub function: String,
    pub span: Span,
}

/// A runtime error enriched with a stack trace, produced at the run boundary for an uncaught fault.
/// `Display` matches [`RuntimeError`] exactly (message only — the trace is printed separately) so
/// parity tests that compare error strings are unaffected. Mirrors `vm::RunError`.
#[derive(Debug, Clone)]
pub struct RunError {
    pub message: String,
    pub span: Span,
    pub trace: Vec<TraceFrame>,
}

impl RunError {
    fn from_error(e: RuntimeError, trace: Vec<TraceFrame>) -> Self {
        RunError {
            message: e.message,
            span: e.span,
            trace,
        }
    }
    fn plain(e: RuntimeError) -> Self {
        RunError {
            message: e.message,
            span: e.span,
            trace: Vec::new(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// When rendering a stack trace, after run-collapsing show at most this many collapsed lines from
/// the head (innermost — closest to the fault) and tail (outermost — includes `main`); the middle is
/// elided. Bounds deep non-recursive chains. Mirrored byte-identically in `vm::format_trace`.
const TRACE_HEAD: usize = 10;
const TRACE_TAIL: usize = 10;

/// Render a runtime error plus its stack trace for the CLI: the error line, then one indented
/// `  at <function> (<call site>)` line per frame, innermost first. Mirrors `vm::format_trace`.
///
/// Two bounding transforms keep an infinite-recursion fault from flooding ~10_001 lines (gap #8):
/// (1) runs of consecutive frames with the SAME function name collapse to the run's innermost `at`
/// line plus a `  … (× N more identical frames) …` marker when the run length N>1; (2) if the
/// collapsed line list still exceeds `TRACE_HEAD + TRACE_TAIL`, the head and tail collapsed lines are
/// kept and the middle replaced by a `  … (M frames elided) …` marker. Both transforms are no-ops on
/// small traces with distinct names, so existing exact-trace goldens are unchanged.
pub fn format_trace(message: &str, span: Span, trace: &[TraceFrame]) -> String {
    let mut s = format!("runtime error ({span}): {message}");
    // (1) Collapse consecutive same-name runs into one entry: the run's innermost `at` line plus an
    // optional `× N` marker (kept in the SAME entry so the cap below can never orphan the marker).
    let mut entries: Vec<String> = Vec::new();
    let mut i = 0;
    while i < trace.len() {
        let frame = &trace[i];
        let mut j = i + 1;
        while j < trace.len() && trace[j].function == frame.function {
            j += 1;
        }
        let mut entry = format!("  at {} (called at {})", frame.function, frame.span);
        let run = j - i;
        if run > 1 {
            entry.push_str(&format!("\n  … (× {} more identical frames) …", run - 1));
        }
        entries.push(entry);
        i = j;
    }
    // (2) Cap the collapsed entries: keep head + tail entries, elide the middle. Capping whole
    // entries (not raw lines) keeps each `× N` marker attached to its `at` line across the boundary.
    if entries.len() > TRACE_HEAD + TRACE_TAIL {
        let elided = entries.len() - TRACE_HEAD - TRACE_TAIL;
        let tail_start = entries.len() - TRACE_TAIL;
        for entry in &entries[..TRACE_HEAD] {
            s.push('\n');
            s.push_str(entry);
        }
        s.push_str(&format!("\n  … ({elided} frames elided) …"));
        for entry in &entries[tail_start..] {
            s.push('\n');
            s.push_str(entry);
        }
    } else {
        for entry in &entries {
            s.push('\n');
            s.push_str(entry);
        }
    }
    s
}

/// Control-flow signal threaded out of statement execution so `return` (and `?` propagation)
/// can unwind cleanly through nested blocks.
enum Flow {
    /// Fell off the end of the block normally.
    Normal,
    /// `return value` — unwind to the enclosing function call.
    Return(Value),
    /// `break` — unwind to (and stop) the innermost enclosing loop.
    Break,
    /// `continue` — unwind to the innermost enclosing loop's next iteration.
    Continue,
}

/// The accumulator a comprehension builds, threaded through the clause recursion so the innermost
/// clause can append/insert directly (with the same dedup/upsert as a set/map literal).
enum CompAcc {
    List(Vec<Value>),
    Set(SetData),
    Map(MapData),
}

/// Whether a block directly contains a `defer` statement (so it is a defer scope). Nested blocks
/// own their own scope and are not inspected. Mirrors the compiler's predicate.
fn block_has_defer(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s.kind, StmtKind::Defer(_)))
}

/// One-way int→float coercion (C-like implicit widening) — the tree-walk interpreter's equivalent of
/// the VM's `Op::CoerceFloat`, applied at the SAME AST annotation boundaries (typed `let`, float
/// params, float returns, float struct fields, annotated/all-literal float collections) so both
/// engines coerce deterministically from the IDENTICAL annotation → byte-identical parity. `Int(n)` →
/// `Float(n as f64)`; a `Float` (or anything else) is returned unchanged (the checker guarantees the
/// value is numeric at every emit site, and idempotence makes a double-coerce a harmless no-op).
fn coerce_float(v: Value) -> Value {
    match v {
        Value::Int(n) => Value::Float(n as f64),
        other => other,
    }
}

/// One-way int→float widening at a typed `let` binding — coerce `v` IN PLACE to match a `float`-shaped
/// annotation: a scalar `float` coerces the whole value; a `list[float]` coerces every int element; a
/// `map[_, float]` coerces every int VALUE (keys untouched). Mirrors the VM, where the typed-`let`
/// sink emits `Op::CoerceFloat` (scalar) and the `list[float]`/`map[_, float]` element hint coerces
/// each int element at the literal-compile site. The element walk runs ONLY when `is_literal` (the
/// value was a list/map LITERAL) — exactly matching the VM, which coerces elements at the literal
/// compile site, so a non-literal value (`xs: list[float] = f()`) is element-coerced by neither
/// engine. Any other annotation is a no-op.
fn coerce_value_to_annotation(v: &mut Value, ty: &crate::ast::Type, is_literal: bool) {
    if crate::ast::is_float_ty(ty) {
        let taken = std::mem::replace(v, Value::Nil);
        *v = coerce_float(taken);
        return;
    }
    if !is_literal {
        return;
    }
    match ty {
        crate::ast::Type::Generic(n, args)
            if n == "list" && args.len() == 1 && crate::ast::is_float_ty(&args[0]) =>
        {
            if let Value::List(items) = v {
                for elem in items.borrow_mut().iter_mut() {
                    let taken = std::mem::replace(elem, Value::Nil);
                    *elem = coerce_float(taken);
                }
            }
        }
        crate::ast::Type::Generic(n, args)
            if n == "map" && args.len() == 2 && crate::ast::is_float_ty(&args[1]) =>
        {
            if let Value::Map(map) = v {
                for entry in map.borrow_mut().entries.iter_mut() {
                    let taken = std::mem::replace(&mut entry.2, Value::Nil);
                    entry.2 = coerce_float(taken);
                }
            }
        }
        _ => {}
    }
}

/// Deep-copy a value across a task airlock (`spawn`): data — scalars, `str`, collections, structs,
/// enums — is recursively cloned into fresh `Rc<RefCell<…>>` cells so a spawned task can't share
/// mutable state with the spawner. Callables and modules pass by handle (a task's entry point, not
/// data that crosses the airlock); sendability of what a task uses is enforced statically by the
/// checker in C2. (Channel / Shared handles, added in C2 / C3, pass by handle too.)
fn deep_clone(v: &Value) -> Value {
    use std::cell::RefCell;
    use std::rc::Rc;
    match v {
        Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Str(_)
        // `bytes` is immutable: clone the `Rc` (sendable, like `Str`) — never deep-copied.
        | Value::Bytes(_)
        | Value::Nil
        | Value::Func(_, _)
        | Value::Closure(_)
        | Value::Native(_)
        // An extern C fn passes by handle (clone the `Arc`) — a callable entry point, not data.
        | Value::Cffi(_)
        | Value::Module(_)
        // A Channel is a shared mailbox: the handle crosses the airlock by reference (clone the
        // `Rc`), never deep-copied — both ends must see the same queue. A `Shared` box is the same:
        // the handle is copied so every task reaches the one box (that's the point of `Shared`).
        // An `Executor` likewise: the handle is copied so every task reaches the one work queue.
        | Value::Channel(_)
        | Value::Shared(_)
        | Value::Atomic(_)
        // An opaque `ptr` handle crosses by value: clone the `usize` address (a `Send` plain scalar,
        // never deep-copied — there is nothing behind the address that Chezzi owns).
        | Value::Ptr(_)
        | Value::Executor(_) => v.clone(),
        Value::List(items) => {
            let cloned = items.borrow().iter().map(deep_clone).collect::<Vec<_>>();
            Value::List(Rc::new(RefCell::new(cloned)))
        }
        // `bytearray` is MUTABLE: deep-copy into a FRESH `Rc<RefCell>` (a new independent buffer, like
        // `List`) — UNLIKE `Bytes`, which clones its `Rc`. This is what makes a `bytearray` cross the
        // `--parallel` airlock by value (no shared mutable view across tasks).
        Value::ByteArray(b) => Value::ByteArray(Rc::new(RefCell::new(b.borrow().clone()))),
        // A cursor deep-copies into a FRESH `Rc<RefCell>` (an independent copy, like `List`) — its
        // elements are deep-cloned too. This is also the airlock copy: a cursor IS sendable (it is a
        // snapshot `Vec` + index, plain data), so this arm fires both for in-task `deep_clone` (e.g.
        // inside `set()`/`map()` building) AND when a cursor crosses a `spawn`/channel boundary. The
        // VM's `to_wire`/`from_wire` deep-copy a cursor identically, keeping VM↔interp parity.
        Value::Iter(c) => {
            let src = c.borrow();
            Value::Iter(Rc::new(RefCell::new(value::IterCursor {
                items: src.items.iter().map(deep_clone).collect(),
                pos: src.pos,
            })))
        }
        Value::Tuple(items) => Value::Tuple(Rc::new(items.iter().map(deep_clone).collect())),
        Value::Map(m) => {
            let src = m.borrow();
            let mut out = value::MapData::default();
            for (h, k, val) in &src.entries {
                out.push(*h, deep_clone(k), deep_clone(val));
            }
            Value::Map(Rc::new(RefCell::new(out)))
        }
        Value::Set(s) => {
            let src = s.borrow();
            let mut out = value::SetData::default();
            for (h, e) in &src.entries {
                out.push(*h, deep_clone(e));
            }
            Value::Set(Rc::new(RefCell::new(out)))
        }
        Value::Struct { name, fields } => {
            let cloned = fields
                .borrow()
                .iter()
                .map(|(k, val)| (k.clone(), deep_clone(val)))
                .collect::<Vec<_>>();
            Value::Struct { name: name.clone(), fields: Rc::new(RefCell::new(cloned)) }
        }
        Value::Enum { ty, variant, payload } => Value::Enum {
            ty: ty.clone(),
            variant: variant.clone(),
            payload: payload.iter().map(deep_clone).collect(),
        },
        Value::NewType { type_key, inner } => Value::NewType {
            type_key: type_key.clone(),
            inner: Box::new(deep_clone(inner)),
        },
    }
}

/// A struct type's runtime shape: ordered field names + methods by name, plus the module globals
/// its methods resolve top-level names against (the module that defined the struct).
struct StructDef {
    fields: Vec<String>,
    methods: std::collections::HashMap<String, std::rc::Rc<FnDecl>>,
    home: value::ModEnv,
}

/// An enum variant's runtime shape: which enum it belongs to and how many payload values it holds.
#[derive(Clone)]
struct VariantDef {
    enum_name: std::rc::Rc<str>,
    arity: usize,
}

/// An enum's methods (`fn area(self) …`) by name, plus the module globals its method bodies resolve
/// top-level names against (the module that declared the enum). The enum analogue of `StructDef`'s
/// `methods`/`home` (enums carry no fields — variants live in the `variants` map).
struct EnumDef {
    methods: std::collections::HashMap<String, std::rc::Rc<FnDecl>>,
    home: value::ModEnv,
}

/// The interpreter: environment, output buffer, and the declared struct / enum types.
struct Interp {
    env: env::Env,
    out: String,
    /// Captured stderr (written by `std.io.eprint`). Separate from `out` so streams don't mix;
    /// the CLI flushes it to the real stderr.
    stderr: String,
    /// Runtime configuration the native std modules read (args/env/stdin). Default = inert.
    host: crate::native::HostConfig,
    structs: std::collections::HashMap<String, std::rc::Rc<StructDef>>,
    /// ROOT REDESIGN — struct IDENTITY KEY (`<module-key>::Name`) → declared fields (with types), for
    /// building `json.decode` descriptors. Qualified-keyed, like the compiler's `struct_fields`.
    struct_fields: std::collections::HashMap<String, Vec<crate::ast::Field>>,
    /// ROOT REDESIGN — struct IDENTITY KEY → the graph index of the module that DECLARED it, so a
    /// nested `json.decode` field type expands in its own defining module's scope (the nested-collision
    /// fix). Populated alongside `struct_fields` at hoist.
    struct_module: std::collections::HashMap<String, usize>,
    /// Keyed by `(enum_name, variant_name)`: variants are scoped under their enum, so two enums may
    /// share a variant name. Built-in `Result`/`Option` variants register under those enum names.
    variants: std::collections::HashMap<(String, String), VariantDef>,
    /// Enum runtime KEY → its methods + home globals (the enum analogue of `structs`). Resolves
    /// `enumval.method(args)`, the `str(self)` Stringable hook, and the arith/compare overloads.
    enum_defs: std::collections::HashMap<String, std::rc::Rc<EnumDef>>,
    /// Newtype runtime KEY → its methods + home globals (mirrors `enum_defs`). Resolves
    /// `ntval.method(args)`, the `str(self)`/`hash(self)` hooks; same-type operators are native.
    newtype_defs: std::collections::HashMap<String, std::rc::Rc<EnumDef>>,
    /// Evaluated module namespaces, keyed by module id (run-once cache for a multi-file program).
    namespaces:
        std::collections::HashMap<crate::resolver::ModuleId, std::rc::Rc<value::ModuleNamespace>>,
    /// Set by the `?` operator when it hits `Err`/`None`: the value to early-return from the
    /// enclosing function. While set, an `Err(RuntimeError)` carries the unwind up to the nearest
    /// call boundary (`call`/`call_closure`), which converts it into that function's return value.
    /// No evaluation runs between `?` raising and the boundary catching, so the channel can't be
    /// clobbered.
    propagating: Option<Value>,
    /// Set by `std.os.exit(code)` (clamped to `0..=255`). While set, the `Err` sentinel returned by
    /// the native `exit` unwinds the stack like a fault, but every `recover:` boundary and the top
    /// level re-propagate instead of catching it — so the exit is a hard, uncatchable halt and the
    /// driver reports `code` as the process exit status.
    pending_exit: Option<i32>,
    /// Current user-function call depth. Bounds native-stack recursion so an infinite/very deep
    /// Chezzi recursion returns a `RuntimeError` instead of overflowing the host stack (SIGABRT).
    call_depth: usize,
    /// One entry per active call frame (function/method/closure): the calls registered by `defer`
    /// in that frame, in source order. Drained LIFO when the frame exits (normal return, `?`
    /// short-circuit, or panic). The receiver/args are evaluated at the `defer` statement (Go
    /// semantics) and stored here as values; the call itself runs at drain.
    deferred: Vec<Vec<Deferred>>,
    /// Active call frames (function/method/closure), outermost first, for runtime stack traces. A
    /// frame is pushed on entry and popped only on a **successful** return — on the error path it is
    /// left in place, so when an uncaught fault reaches the driver this holds the call chain from the
    /// outermost call down to the fault. `recover:` truncates it back to its entry depth on catch.
    call_stack: Vec<TraceFrame>,
    /// Active `parallel:` nurseries, innermost last. Each `spawn` registers a [`Task`] onto the
    /// innermost list; at the nursery's dedent the list is drained and its tasks run to completion
    /// FIFO (the sequential executor). Empty outside any `parallel:` block.
    nurseries: Vec<Vec<Task>>,
    /// Every `Executor` created during the run, in creation order. Keeping the `Rc` here both keeps
    /// each executor's queued work alive (the interp analog of a GC root) and lets the driver
    /// gracefully drain any executor never explicitly `shutdown`/`shutdown_now`-ed at program exit
    /// (C5 / A2) — without it, that submitted work would silently never run.
    executors: Vec<std::rc::Rc<std::cell::RefCell<value::ExecState>>>,
    /// FFI ROOT FIX: the checker-resolved C signature of every `extern` fn, keyed by `(graph
    /// module index, fn name)` — the SAME table + key the VM consumes. The extern lowering reads CTypes
    /// from HERE instead of re-resolving alias names itself, so every qualified/imported/aliased width
    /// resolves in its DEFINING module's scope (collision-proof, byte-identical to the VM). The ONLY
    /// extern-type resolver: filled in `run_file_inner` (multi-file) via `resolve_extern_signatures`,
    /// and in `execute` (standalone) via `resolve_extern_signatures_standalone`.
    extern_sigs: crate::checker::ExternTable,
    /// User type names (struct / enum / type-alias) declared in ANY module, gathered before
    /// evaluation. Module-scoped types: a `from`-imported TYPE name carries no runtime value (types
    /// resolve through the program-global struct/variant tables by name), so `bind_import` skips a
    /// from-member that is in this set — mirroring how `std.ffi` width type imports are skipped.
    type_names: std::collections::HashSet<String>,
    /// Module-scoped types: `(declaring module_idx, bare type name) → runtime key`. Bare in the
    /// no-collision case; `<dotted>::Name` on a genuine cross-module clash. Computed identically to
    /// the compiler's `assign_type_keys` so both engines agree on the runtime key (parity).
    type_keys: std::collections::HashMap<(usize, String), String>,
    /// `module id → its graph index`, so `eval_module` can find the current module's index.
    module_idx_of: std::collections::HashMap<crate::resolver::ModuleId, usize>,
    /// `module_idx → its declared user type names` (struct/enum/alias).
    module_types: Vec<std::collections::HashSet<String>>,
    /// The CURRENT module's index (set per `eval_module`).
    cur_module_idx: usize,
    /// The CURRENT module's bound module name → that module's index (for qualified `geo.X`).
    imported_module_idx: std::collections::HashMap<String, usize>,
    /// The CURRENT module's bare-resolvable type names → their runtime key (locally declared +
    /// `from`-imported + std whole-module). The bare struct/enum constructor only fires for a name
    /// here — a type merely present in the global tables (another module's) is NOT bare-constructible,
    /// so a `from`-imported function named like another module's type still resolves as a call.
    /// Mirrors the compiler's `bare_types` for 3-engine parity.
    bare_types: std::collections::HashMap<String, String>,
    /// `module_idx → that module's built `bare_types`` map, cached at `eval_module` so a cross-module
    /// runtime call can RE-ESTABLISH the callee's bare-type context (which drives `enum_bare_key` for
    /// match-pattern key resolution). Without this, a disambiguated enum matched inside its declaring
    /// module's function — but called from another module — would resolve its pattern key against the
    /// caller's context and never match (Blocker C).
    module_bare_types: Vec<std::collections::HashMap<String, String>>,
    /// `home ModEnv pointer-address → its module idx`, recorded when each module's globals are built,
    /// so a call can find the callee's module context from the `home` env it swaps in.
    home_module_idx: std::collections::HashMap<usize, usize>,
}

/// Saved `bare_types` from [`Interp::enter_module_ctx`]: `Some` iff the context was swapped (a
/// cross-module call), `None` for a same-module call where restoration is a no-op.
type ModuleCtxSaved = Option<std::collections::HashMap<String, String>>;

/// A task registered by `spawn`, awaiting its nursery's join barrier. The callee/receiver and
/// arguments are evaluated and deep-copied across the airlock at the `spawn` statement (Go's
/// arg-evaluation timing); the body runs at the `parallel:` dedent. Mirrors [`Deferred`].
enum Task {
    /// `spawn f(args)` — invoke the callable value with the (already deep-copied) args.
    Call {
        callee: Value,
        args: Vec<Value>,
        span: Span,
    },
    /// `spawn recv.name(args)` — dispatch the named method on the (deep-copied) receiver.
    Method {
        recv: Value,
        name: String,
        args: Vec<Value>,
        span: Span,
    },
    /// `spawn:` block — run the statement body against a deep-copied snapshot of the captured
    /// locals plus the home module globals.
    Block {
        body: Block,
        locals: Vec<std::collections::HashMap<String, Value>>,
        home: value::ModEnv,
        span: Span,
    },
}

/// A call registered by `defer`, with its receiver/arguments already evaluated. Drained at frame
/// exit via [`Interp::run_deferred`].
enum Deferred {
    /// `defer f(args)` — invoke the callable value with the args.
    Call {
        callee: Value,
        args: Vec<Value>,
        span: Span,
    },
    /// `defer recv.name(args)` — dispatch the named method on the receiver.
    Method {
        recv: Value,
        name: String,
        args: Vec<Value>,
        span: Span,
    },
    /// `defer:` block — run the body in its own frame against the locals snapshotted (by value,
    /// shallow — sharing heap handles, matching the VM's `MakeClosure` capture) at the defer point.
    Block {
        body: Block,
        locals: Vec<std::collections::HashMap<String, Value>>,
        home: value::ModEnv,
        span: Span,
    },
}

/// Maximum user-function call depth. Bounds recursion well within the dedicated interpreter
/// thread's [`INTERP_STACK_BYTES`] stack, so infinite recursion returns a `RuntimeError` instead
/// of overflowing the host stack — while still allowing deep, legitimate recursion.
const MAX_CALL_DEPTH: usize = 10_000;

/// Maximum structural recursion depth for display (`stringify`) and equality (`values_equal_guarded`).
/// A cyclic data structure (e.g. a struct with a `list[Self]` field that points back at itself) would
/// otherwise recurse unbounded on the *host* stack inside these routines and SIGABRT — uncatchable by
/// `recover:`. Tripping this contained guard surfaces a recoverable `RuntimeError` instead. The limit
/// matches the VM engine's (parity tested).
const MAX_STRUCTURAL_DEPTH: usize = 10_000;

impl Interp {
    fn new() -> Self {
        let mut interp = Interp {
            env: env::Env::new(),
            out: String::new(),
            stderr: String::new(),
            host: crate::native::HostConfig::default(),
            structs: std::collections::HashMap::new(),
            struct_fields: std::collections::HashMap::new(),
            struct_module: std::collections::HashMap::new(),
            variants: std::collections::HashMap::new(),
            enum_defs: std::collections::HashMap::new(),
            newtype_defs: std::collections::HashMap::new(),
            namespaces: std::collections::HashMap::new(),
            propagating: None,
            pending_exit: None,
            call_depth: 0,
            deferred: Vec::new(),
            call_stack: Vec::new(),
            nurseries: Vec::new(),
            executors: Vec::new(),
            extern_sigs: crate::checker::ExternTable::new(),
            type_names: std::collections::HashSet::new(),
            type_keys: std::collections::HashMap::new(),
            module_idx_of: std::collections::HashMap::new(),
            module_types: Vec::new(),
            cur_module_idx: 0,
            imported_module_idx: std::collections::HashMap::new(),
            bare_types: std::collections::HashMap::new(),
            module_bare_types: Vec::new(),
            home_module_idx: std::collections::HashMap::new(),
        };
        // Built-in Result / Option variants — available without any declaration.
        interp.register_variant("Ok", "Result", 1);
        interp.register_variant("Err", "Result", 1);
        interp.register_variant("Some", "Option", 1);
        interp.register_variant("None", "Option", 0);
        interp
    }

    fn register_variant(&mut self, variant: &str, enum_name: &str, arity: usize) {
        self.variants.insert(
            (enum_name.to_string(), variant.to_string()),
            VariantDef {
                enum_name: enum_name.into(),
                arity,
            },
        );
    }

    /// The module-scoped runtime key for a type `name` declared in module `module_idx` (bare unless
    /// disambiguated by a genuine cross-module clash). Mirrors the compiler's `type_key`.
    fn type_key(&self, module_idx: usize, name: &str) -> String {
        self.type_keys
            .get(&(module_idx, name.to_string()))
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    /// IDENTITY KEY for a bare-written enum name in the CURRENT module: its `bare_types` key when
    /// bare-visible. ROOT REDESIGN — a WHOLE-module-imported enum (`Color` matched as `Color.Red`
    /// against a `geo::Color` value) is NOT bare-visible, so resolve it through the imported modules:
    /// pick an imported module that declares `ename` whose qualified key is a registered enum. Falls
    /// back to the bare name (built-in `Result`/`Option`, or a miss). Mirrors the compiler's
    /// `enum_bare_key` byte-for-byte (parity).
    fn enum_bare_key(&self, ename: &str) -> String {
        if let Some(k) = self.bare_types.get(ename) {
            return k.clone();
        }
        for &tidx in self.imported_module_idx.values() {
            if self
                .module_types
                .get(tidx)
                .is_some_and(|t| t.contains(ename))
            {
                let key = self.type_key(tidx, ename);
                if self.variants.keys().any(|(e, _)| *e == key) {
                    return key;
                }
            }
        }
        ename.to_string()
    }

    /// ROOT REDESIGN — build the module-aware [`crate::json_decode::DecodeEnv`] for the CURRENT module
    /// so `json.decode[T]` resolves its target (and nested field types) to qualified identity keys,
    /// byte-identically to the VM.
    fn decode_env(&self) -> InterpDecodeEnv<'_> {
        InterpDecodeEnv { i: self }
    }

    /// Evaluate an expression to a value.
    fn eval(&mut self, expr: &Expr) -> Result<Value, RuntimeError> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(s) => Ok(Value::Str(self.interpolate(s, expr.span)?.into())),
            // Raw string: verbatim — does NOT call `self.interpolate`, so braces stay literal.
            ExprKind::RawStr(s) => Ok(Value::Str(s.as_str().into())),
            ExprKind::Bytes(b) => Ok(Value::Bytes(b.as_slice().into())),
            ExprKind::Ident(name) => {
                // A bare nullary *built-in* variant used as a value (`None`). User variants are
                // qualified (the `Field` arm), so only built-ins resolve bare here.
                if let Some(def) = variant_pair(None, name)
                    .and_then(|k| self.variants.get(&k))
                    .filter(|d| d.arity == 0)
                {
                    return Ok(Value::Enum {
                        ty: def.enum_name.clone(),
                        variant: name.as_str().into(),
                        payload: Vec::new(),
                    });
                }
                self.env.get(name).ok_or_else(|| RuntimeError {
                    message: format!("undefined name '{name}'"),
                    span: expr.span,
                })
            }
            ExprKind::Unary { op, expr: inner } => {
                let v = self.eval(inner)?;
                match (op, &v) {
                    (UnaryOp::Neg, Value::Int(n)) => {
                        n.checked_neg().map(Value::Int).ok_or(RuntimeError {
                            message: "integer overflow in negation".to_string(),
                            span: expr.span,
                        })
                    }
                    (UnaryOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    _ => Err(RuntimeError {
                        message: format!("cannot apply {op:?} to {}", v.type_name()),
                        span: expr.span,
                    }),
                }
            }
            // `and`/`or` short-circuit: evaluate the right operand only when needed.
            ExprKind::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                lhs,
                rhs,
            } => {
                let l = as_bool(self.eval(lhs)?, lhs.span)?;
                match (op, l) {
                    (BinaryOp::And, false) => Ok(Value::Bool(false)),
                    (BinaryOp::Or, true) => Ok(Value::Bool(true)),
                    _ => Ok(Value::Bool(as_bool(self.eval(rhs)?, rhs.span)?)),
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                // Operator overloading: ordering (`< <= > >=`) on two structs dispatches to the
                // type's `compare(self, other) -> int` method (the `Comparable` protocol). The
                // checker has already verified conformance. Equality stays structural (handled by
                // `eval_binary`); only ordering is overloaded.
                if matches!(
                    op,
                    BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
                ) && matches!(
                    (&l, &r),
                    (Value::Struct { .. }, Value::Struct { .. })
                        | (Value::Enum { .. }, Value::Enum { .. })
                ) {
                    return self.struct_ordering(*op, l, r, expr.span);
                }
                // Arithmetic overloading: `+`/`-`/`*` on two structs (or two enums) dispatch to
                // `add`/`sub`/`mul` (the `Add`/`Sub`/`Mul` protocols). The checker has verified
                // conformance.
                if matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul)
                    && matches!(
                        (&l, &r),
                        (Value::Struct { .. }, Value::Struct { .. })
                            | (Value::Enum { .. }, Value::Enum { .. })
                    )
                {
                    return self.struct_arith(*op, l, r, expr.span);
                }
                // Same-newtype operators auto-flow to the UNDERLYING's NATIVE op (unwrap→op→rewrap for
                // arithmetic; unwrap→compare for ordering), NOT a user method. The checker rejected
                // `Meters + float` / `Meters + Seconds` / `Meters < Seconds`. Mirrors the VM.
                if let (
                    Value::NewType {
                        type_key: ka,
                        inner: ia,
                    },
                    Value::NewType {
                        type_key: kb,
                        inner: ib,
                    },
                ) = (&l, &r)
                    && ka == kb
                {
                    if matches!(
                        op,
                        BinaryOp::Add
                            | BinaryOp::Sub
                            | BinaryOp::Mul
                            | BinaryOp::Div
                            | BinaryOp::Mod
                    ) {
                        let inner = eval_binary(*op, (**ia).clone(), (**ib).clone(), expr.span)?;
                        return Ok(Value::NewType {
                            type_key: ka.clone(),
                            inner: Box::new(inner),
                        });
                    }
                    if matches!(
                        op,
                        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
                    ) {
                        // ordering on the inners (native compare), mapped to bool.
                        return eval_binary(*op, (**ia).clone(), (**ib).clone(), expr.span);
                    }
                    // ==/!= fall through to `eval_binary`'s structural newtype equality.
                }
                eval_binary(*op, l, r, expr.span)
            }
            ExprKind::List(items) => {
                // One-way int→float widening — the all-literal peephole (mirrors the VM): an
                // un-annotated list literal with ≥1 float literal widens its int LITERAL siblings.
                let peephole = crate::compiler::literal_numeric_mix(items.iter());
                let vals = items
                    .iter()
                    .map(|e| {
                        let v = self.eval(e)?;
                        Ok(if peephole && matches!(e.kind, ExprKind::Int(_)) {
                            coerce_float(v)
                        } else {
                            v
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(vals))))
            }
            ExprKind::Tuple(items) => {
                let vals = items
                    .iter()
                    .map(|e| self.eval(e))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::Tuple(std::rc::Rc::new(vals)))
            }
            ExprKind::Map(entries) => {
                // Evaluate key then value per entry; duplicate keys upsert (last wins). A struct
                // key's hash() re-enters the interpreter (fine — the Rc heap never moves).
                let mut map = MapData::default();
                // One-way int→float widening — all-literal peephole over the VALUE column (mirrors
                // the VM): a mixed int/float-literal value set widens its int VALUE literals.
                let peephole = crate::compiler::literal_numeric_mix(entries.iter().map(|(_, v)| v));
                for (k_expr, v_expr) in entries {
                    let k = self.eval(k_expr)?;
                    let mut v = self.eval(v_expr)?;
                    if peephole && matches!(v_expr.kind, ExprKind::Int(_)) {
                        v = coerce_float(v);
                    }
                    let hk = self.hash_value(&k, expr.span)?;
                    match map
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| values_equal(&map.entries[p].1, &k))
                    {
                        Some(i) => map.entries[i].2 = v,
                        None => map.push(hk, k, v),
                    }
                }
                Ok(Value::Map(std::rc::Rc::new(std::cell::RefCell::new(map))))
            }
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.eval_comprehension(*kind, key.as_deref(), elem, clauses, expr.span),
            ExprKind::Set(elems) => {
                let mut set = SetData::default();
                for e in elems {
                    let v = self.eval(e)?;
                    let hv = self.hash_value(&v, expr.span)?;
                    if !set
                        .candidates(hv)
                        .iter()
                        .copied()
                        .any(|p| values_equal(&set.entries[p].1, &v))
                    {
                        set.push(hv, v);
                    }
                }
                Ok(Value::Set(std::rc::Rc::new(std::cell::RefCell::new(set))))
            }
            ExprKind::Closure { params, body, .. } => {
                Ok(Value::Closure(std::rc::Rc::new(value::Closure {
                    params: params.clone(),
                    body: (**body).clone(),
                    captured: self.env.snapshot_locals(),
                    home: self.env.globals_rc(),
                })))
            }
            ExprKind::Field { obj, name } => {
                // `module.Enum.Variant` (nullary, qualified value form) → the variant value.
                if let ExprKind::Field {
                    obj: inner,
                    name: ename,
                } = &obj.kind
                    && let ExprKind::Ident(mname) = &inner.kind
                    && self.env.get_local(mname).is_none()
                    && let Some(&tidx) = self.imported_module_idx.get(mname)
                    && self
                        .module_types
                        .get(tidx)
                        .is_some_and(|t| t.contains(ename))
                    && let ekey = self.type_key(tidx, ename)
                    && let Some(def) = self.variants.get(&(ekey, name.clone()))
                    && def.arity == 0
                {
                    return Ok(Value::Enum {
                        ty: def.enum_name.clone(),
                        variant: name.as_str().into(),
                        payload: Vec::new(),
                    });
                }
                // `Enum.Variant` (nullary) → the variant value, mirroring the bare Ident path. A
                // binding named like the enum wins, matching the checker/compiler. The enum resolves
                // to the current module's runtime key (bare unless disambiguated).
                if let ExprKind::Ident(ename) = &obj.kind
                    && self.env.get_local(ename).is_none()
                    && let ekey = self.enum_bare_key(ename)
                    && let Some(def) = self.variants.get(&(ekey, name.clone()))
                    && def.arity == 0
                {
                    return Ok(Value::Enum {
                        ty: def.enum_name.clone(),
                        variant: name.as_str().into(),
                        payload: Vec::new(),
                    });
                }
                let target = self.eval(obj)?;
                match &target {
                    // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
                    Value::Tuple(items) => {
                        match name.parse::<usize>().ok().and_then(|i| items.get(i)) {
                            Some(v) => Ok(v.clone()),
                            None => Err(RuntimeError {
                                message: format!(
                                    "tuple has no element '.{name}' (len {})",
                                    items.len()
                                ),
                                span: expr.span,
                            }),
                        }
                    }
                    Value::Struct { fields, .. } => fields
                        .borrow()
                        .iter()
                        .find(|(k, _)| k == name)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| RuntimeError {
                            message: format!("no field '{name}' on {}", target),
                            span: expr.span,
                        }),
                    Value::Module(ns) => {
                        ns.members
                            .0
                            .borrow()
                            .get(name)
                            .cloned()
                            .ok_or_else(|| RuntimeError {
                                message: format!("module '{}' has no member '{name}'", ns.name),
                                span: expr.span,
                            })
                    }
                    other => Err(RuntimeError {
                        message: format!("cannot read field '{name}' of {}", other.type_name()),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Index { obj, index } => {
                // Evaluate the object FIRST: a map indexes by an arbitrary key (str/bool/int), so we
                // only force an int index for the list/str paths. The int-validation error reports
                // at `expr.span` (the whole index expression) to match the VM's `GetIndex` span.
                let target = self.eval(obj)?;
                let want_int = |v: Value| -> Result<i64, RuntimeError> {
                    match v {
                        Value::Int(n) => Ok(n),
                        other => Err(RuntimeError {
                            message: format!("expected int, found {}", other.type_name()),
                            span: expr.span,
                        }),
                    }
                };
                match &target {
                    Value::Map(entries) => {
                        let key = self.eval(index)?;
                        // Hash BEFORE borrowing the map: a struct key's hash() may read this same
                        // map (re-entrant) and a live `borrow()` would double-borrow-panic.
                        let hk = self.hash_value(&key, expr.span)?;
                        let m = entries.borrow();
                        m.candidates(hk)
                            .iter()
                            .copied()
                            .find(|&p| values_equal(&m.entries[p].1, &key))
                            .map(|p| m.entries[p].2.clone())
                            .ok_or_else(|| RuntimeError {
                                message: "key not found".to_string(),
                                span: expr.span,
                            })
                    }
                    Value::List(items) => {
                        let idx = want_int(self.eval(index)?)?;
                        let items = items.borrow();
                        crate::slice::norm_index(idx, items.len())
                            .map(|i| items[i].clone())
                            .ok_or_else(|| RuntimeError {
                                message: format!("index {idx} out of bounds (len {})", items.len()),
                                span: expr.span,
                            })
                    }
                    Value::Str(s) => {
                        let idx = want_int(self.eval(index)?)?;
                        let chars: Vec<char> = s.chars().collect();
                        crate::slice::norm_index(idx, chars.len())
                            .map(|i| Value::Str(chars[i].to_string().into()))
                            .ok_or_else(|| RuntimeError {
                                message: format!("index {idx} out of bounds (len {})", chars.len()),
                                span: expr.span,
                            })
                    }
                    // `bytes[i]` → `int` (0–255); out-of-range faults recoverably (like list/str).
                    Value::Bytes(b) => {
                        let idx = want_int(self.eval(index)?)?;
                        crate::slice::norm_index(idx, b.len())
                            .map(|i| Value::Int(b[i] as i64))
                            .ok_or_else(|| RuntimeError {
                                message: format!("index {idx} out of bounds (len {})", b.len()),
                                span: expr.span,
                            })
                    }
                    // `bytearray[i]` → `int` (0–255); same out-of-range behavior as `bytes`.
                    Value::ByteArray(b) => {
                        let idx = want_int(self.eval(index)?)?;
                        let b = b.borrow();
                        crate::slice::norm_index(idx, b.len())
                            .map(|i| Value::Int(b[i] as i64))
                            .ok_or_else(|| RuntimeError {
                                message: format!("index {idx} out of bounds (len {})", b.len()),
                                span: expr.span,
                            })
                    }
                    // A struct satisfying `Index` dispatches `obj[k]` to `index(self, k)`.
                    Value::Struct { .. } => {
                        let key = self.eval(index)?;
                        self.call_struct_method(target.clone(), "index", vec![key], expr.span)
                    }
                    other => Err(RuntimeError {
                        message: format!("cannot index {}", other.type_name()),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => self.eval_slice(
                obj,
                start.as_deref(),
                end.as_deref(),
                step.as_deref(),
                expr.span,
            ),
            // `type_args` are type-erased — the interpreter ignores them (checker already used them).
            ExprKind::Call { callee, args, .. } => self.eval_call(callee, args, expr.span),
            ExprKind::Match { scrutinee, arms } => self.eval_match_expr(scrutinee, arms),
            ExprKind::IfElse { cond, then, els } => {
                if as_bool(self.eval(cond)?, cond.span)? {
                    self.eval(then)
                } else {
                    self.eval(els)
                }
            }
            ExprKind::Recover(block) => self.eval_recover(block),
            ExprKind::Try(inner) => {
                let v = self.eval(inner)?;
                match &v {
                    // Unwrap the success case. Gate on the *type* (`Result`/`Option`), not the bare
                    // variant name, so a user enum that shadows `Ok`/`Some` isn't unwrapped by `?`.
                    Value::Enum {
                        ty,
                        variant,
                        payload,
                    } if (ty.as_ref() == "Result" && variant.as_ref() == "Ok"
                        || ty.as_ref() == "Option" && variant.as_ref() == "Some")
                        && payload.len() == 1 =>
                    {
                        Ok(payload[0].clone())
                    }
                    // Early-return the failure case from the enclosing function (builtin types only).
                    Value::Enum { ty, variant, .. }
                        if ty.as_ref() == "Result" && variant.as_ref() == "Err"
                            || ty.as_ref() == "Option" && variant.as_ref() == "None" =>
                    {
                        self.propagating = Some(v.clone());
                        Err(RuntimeError {
                            message: "? propagation".to_string(),
                            span: expr.span,
                        })
                    }
                    other => Err(RuntimeError {
                        message: format!(
                            "'?' expects Result or Option, found {}",
                            other.type_name()
                        ),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::DecodeCall { obj, ty, arg } => {
                // Reuse the json module's `parse` (obj.parse(arg) → Result[Json]), then coerce.
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
                let res = self.eval(&parse_call)?;
                // ROOT REDESIGN — resolve the decode target (+ nested field struct types) to qualified
                // identity keys via a module-aware env, mirroring the VM (parity).
                let env = self.decode_env();
                let desc =
                    crate::json_decode::from_type(ty, self.cur_module_idx, &env, &mut Vec::new())
                        .map_err(|message| RuntimeError {
                        message,
                        span: expr.span,
                    })?;
                match &res {
                    Value::Enum {
                        ty: rty,
                        variant,
                        payload,
                    } if rty.as_ref() == "Result"
                        && variant.as_ref() == "Ok"
                        && payload.len() == 1 =>
                    {
                        let jv = payload[0].clone();
                        match self.coerce_json(&jv, &desc, "$") {
                            Ok(v) => Ok(enum_val("Result", "Ok", vec![v])),
                            Err(msg) => Ok(enum_val("Result", "Err", vec![Value::Str(msg.into())])),
                        }
                    }
                    // Parse error: the Result Err(str) is already a valid Result[T].
                    Value::Enum {
                        ty: rty, variant, ..
                    } if rty.as_ref() == "Result" && variant.as_ref() == "Err" => Ok(res),
                    _ => Err(RuntimeError {
                        message: "decode: parse did not return a Result".to_string(),
                        span: expr.span,
                    }),
                }
            }
            other => Err(RuntimeError {
                message: format!("evaluation of {other:?} is not implemented yet"),
                span: expr.span,
            }),
        }
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type. Mirrors the
    /// VM's `coerce_json` (identical error wording — the parity suite checks this).
    fn coerce_json(
        &self,
        jv: &Value,
        desc: &crate::json_decode::TypeDescriptor,
        path: &str,
    ) -> Result<Value, String> {
        use crate::json_decode::TypeDescriptor as D;
        let Value::Enum {
            ty,
            variant,
            payload,
        } = jv
        else {
            return Err(format!("decode: expected a JSON value at {path}"));
        };
        let _ = ty;
        let kind = crate::json_decode::json_kind(variant);
        let mismatch = |want: &str| format!("decode: expected {want} at {path}, found {kind}");
        match desc {
            D::Int => {
                let f = json_num(variant, payload).ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}"));
                }
                Ok(Value::Int(f as i64))
            }
            D::Float => Ok(Value::Float(
                json_num(variant, payload).ok_or_else(|| mismatch("float"))?,
            )),
            D::Bool => match (variant.as_ref(), payload.first()) {
                ("Bool", Some(Value::Bool(b))) => Ok(Value::Bool(*b)),
                _ => Err(mismatch("bool")),
            },
            D::Str => match (variant.as_ref(), payload.first()) {
                ("Str", Some(Value::Str(s))) => Ok(Value::Str(s.clone())),
                _ => Err(mismatch("str")),
            },
            D::Option(inner) => {
                if variant.as_ref() == "Null" {
                    Ok(enum_val("Option", "None", Vec::new()))
                } else {
                    Ok(enum_val(
                        "Option",
                        "Some",
                        vec![self.coerce_json(jv, inner, path)?],
                    ))
                }
            }
            D::List(inner) => {
                let Value::List(items) = payload.first().cloned().unwrap_or(Value::Nil) else {
                    return Err(mismatch("array"));
                };
                if variant.as_ref() != "Arr" {
                    return Err(mismatch("array"));
                }
                let src = items.borrow().clone();
                let mut out = Vec::with_capacity(src.len());
                for (i, it) in src.iter().enumerate() {
                    out.push(self.coerce_json(it, inner, &format!("{path}[{i}]"))?);
                }
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            D::Map(inner) => {
                if variant.as_ref() != "Obj" {
                    return Err(mismatch("object"));
                }
                let Value::Map(entries) = payload.first().cloned().unwrap_or(Value::Nil) else {
                    return Err(mismatch("object"));
                };
                let src = entries.borrow().clone();
                let mut out = MapData::default();
                for (hk, k, v) in &src.entries {
                    let key = match k {
                        Value::Str(s) => s.to_string(),
                        _ => String::new(),
                    };
                    // str keys are unchanged → reuse their cached hash.
                    out.push(
                        *hk,
                        k.clone(),
                        self.coerce_json(v, inner, &format!("{path}.{key}"))?,
                    );
                }
                Ok(Value::Map(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            D::Struct {
                key,
                display,
                fields,
            } => {
                if variant.as_ref() != "Obj" {
                    // ROOT REDESIGN — error text shows the BARE display name, never the identity key.
                    return Err(mismatch(&format!("object for {display}")));
                }
                let Value::Map(entries) = payload.first().cloned().unwrap_or(Value::Nil) else {
                    return Err(mismatch("object"));
                };
                let src = entries.borrow().clone();
                let mut field_vals: Vec<(String, Value)> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = src
                        .entries
                        .iter()
                        .find(|(_, k, _)| matches!(k, Value::Str(s) if s.as_ref() == fname));
                    let fpath = format!("{path}.{fname}");
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(jval, fdesc, &fpath)?,
                        None => match fdesc {
                            D::Option(_) => enum_val("Option", "None", Vec::new()),
                            _ => return Err(format!("decode: missing key '{fname}' at {path}")),
                        },
                    };
                    field_vals.push((fname.clone(), v));
                }
                // ROOT REDESIGN — tag the value with the qualified IDENTITY KEY (downstream
                // field/method lookups key on it); the inline field names + display drive rendering.
                Ok(Value::Struct {
                    name: key.clone().into(),
                    fields: std::rc::Rc::new(std::cell::RefCell::new(field_vals)),
                })
            }
        }
    }

    /// Expand a string literal's `{expr}` interpolations. `{{` / `}}` are literal braces; each
    /// `{ … }` is lexed + parsed as a Chezzi expression, evaluated in the current scope, and its
    /// `Display` form spliced in.
    fn interpolate(&mut self, raw: &str, span: Span) -> Result<String, RuntimeError> {
        let mut out = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '{' => {
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
                        return Err(RuntimeError {
                            message: "unterminated '{' in interpolated string".to_string(),
                            span,
                        });
                    }
                    // Split on the first top-level `:` into (expr, spec) — same bracket/quote-aware
                    // logic the compiler uses (shared `fmtspec::split_spec`), so a `:` inside an
                    // index/string is not a separator. Spec parse errors surface as runtime errors
                    // here (the interpreter has no separate compile phase); the VM catches them at
                    // compile time — the message string is identical.
                    let (expr_src, spec_src) = crate::fmtspec::split_spec(&inner);
                    let expr = parse_expr_str(expr_src)?;
                    let value = self.eval(&expr)?;
                    match spec_src {
                        None => out.push_str(&self.stringify(&value, span, 0)?),
                        Some(spec_src) => {
                            let spec = crate::fmtspec::parse(spec_src)
                                .map_err(|message| RuntimeError { message, span })?;
                            // Scalars map straight to a FmtArg; everything else is rendered via
                            // `stringify` first then formatted as a plain string (fill/align/width).
                            match &value {
                                Value::Int(n) => crate::fmtspec::apply(
                                    &spec,
                                    crate::fmtspec::FmtArg::Int(*n),
                                    &mut out,
                                )
                                .map_err(|message| RuntimeError { message, span })?,
                                Value::Float(x) => crate::fmtspec::apply(
                                    &spec,
                                    crate::fmtspec::FmtArg::Float(*x),
                                    &mut out,
                                )
                                .map_err(|message| RuntimeError { message, span })?,
                                Value::Str(s) => crate::fmtspec::apply(
                                    &spec,
                                    crate::fmtspec::FmtArg::Str(s),
                                    &mut out,
                                )
                                .map_err(|message| RuntimeError { message, span })?,
                                _ => {
                                    let rendered = self.stringify(&value, span, 0)?;
                                    crate::fmtspec::apply(
                                        &spec,
                                        crate::fmtspec::FmtArg::Other(&rendered),
                                        &mut out,
                                    )
                                    .map_err(|message| RuntimeError { message, span })?;
                                }
                            }
                        }
                    }
                }
                '}' => {
                    return Err(RuntimeError {
                        message: "unmatched '}' in string (use '}}' for a literal brace)"
                            .to_string(),
                        span,
                    });
                }
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// Evaluate a call expression: builtins by name, otherwise a user function value.
    fn eval_call(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // A method call `obj.name(args)` — bind `obj` as `self`. But `Enum.Variant(args)` is a
        // qualified variant constructor, not a method: an unbound enum name dotted with its variant.
        if let ExprKind::Field { obj, name } = &callee.kind {
            // `module.Struct(args)` → qualified struct constructor.
            if let ExprKind::Ident(mname) = &obj.kind
                && self.env.get_local(mname).is_none()
                && let Some(&tidx) = self.imported_module_idx.get(mname)
                && self
                    .module_types
                    .get(tidx)
                    .is_some_and(|t| t.contains(name))
            {
                let key = self.type_key(tidx, name);
                if let Some(def) = self.structs.get(&key).cloned() {
                    let arg_vals = args
                        .iter()
                        .map(|a| self.eval(a))
                        .collect::<Result<Vec<_>, _>>()?;
                    return self.construct_struct(&key, &def, arg_vals, span);
                }
            }
            // `module.Enum.Variant(args)` → qualified payload-variant constructor.
            if let ExprKind::Field {
                obj: inner,
                name: ename,
            } = &obj.kind
                && let ExprKind::Ident(mname) = &inner.kind
                && self.env.get_local(mname).is_none()
                && let Some(&tidx) = self.imported_module_idx.get(mname)
                && self
                    .module_types
                    .get(tidx)
                    .is_some_and(|t| t.contains(ename))
            {
                let ekey = self.type_key(tidx, ename);
                if let Some(def) = self.variants.get(&(ekey.clone(), name.clone())).cloned() {
                    return self.build_variant(&def, name, args, span);
                }
            }
            // `Enum.Variant(args)` → variant constructor (current module's runtime key for the enum).
            if let ExprKind::Ident(ename) = &obj.kind
                && self.env.get_local(ename).is_none()
                && let ekey = self.enum_bare_key(ename)
                && self.variants.contains_key(&(ekey.clone(), name.clone()))
            {
                let def = self.variants.get(&(ekey, name.clone())).cloned().unwrap();
                return self.build_variant(&def, name, args, span);
            }
            return self.eval_method_call(obj, name, args, span);
        }

        let arg_vals = args
            .iter()
            .map(|a| self.eval(a))
            .collect::<Result<Vec<_>, _>>()?;

        // Builtins and struct constructors are resolved by name.
        if let ExprKind::Ident(name) = &callee.kind {
            if name == "print" {
                let mut parts = Vec::with_capacity(arg_vals.len());
                for v in &arg_vals {
                    parts.push(self.stringify(v, span, 0)?);
                }
                self.out.push_str(&parts.join(" "));
                self.out.push('\n');
                return Ok(Value::Nil);
            }
            // `str(x)` dispatches to a `Stringable` struct's `str` method (else default repr).
            // Arity ≠ 1 falls through to the builtin so its arity error is preserved.
            if name == "str" && arg_vals.len() == 1 {
                // `str` is dual: a `newtype N = str` with NO `str(self)` override UNWRAPS to its
                // inner str (cast-unwrap). A `str` override or other underlying goes through
                // `stringify` (display cast, which honors the override). Mirrors the VM.
                if let Value::NewType { type_key, inner } = &arg_vals[0]
                    && let Value::Str(_) = inner.as_ref()
                    && !self
                        .newtype_defs
                        .get(type_key.as_ref())
                        .is_some_and(|d| d.methods.contains_key("str"))
                {
                    return Ok((**inner).clone());
                }
                let s = self.stringify(&arg_vals[0], span, 0)?;
                return Ok(Value::Str(s.into()));
            }
            // `set(it)` can take an iterable of structs whose `hash()` re-enters the engine (and may
            // drain a user iterator's `next()`), so it can't live in the pure `builtins` table — route
            // it here. `list(it)` / `map(it)` likewise drain any for-iterable (re-entrant). Other
            // builtins stay pure.
            if name == "set" {
                return self.builtin_set(arg_vals, span);
            }
            if name == "list" {
                return self.builtin_list(arg_vals, span);
            }
            if name == "map" {
                return self.builtin_map(arg_vals, span);
            }
            // `bytearray(...)` / `bytes(...)` — constructors + conversion bridge. Routed on `Interp`
            // (not the pure `builtins` table) so the result shares the engine's value model.
            if name == "bytearray" {
                return self.builtin_bytearray(arg_vals, span);
            }
            if name == "bytes" {
                return self.builtin_bytes(arg_vals, span);
            }
            // `panic(msg)` — raise the SAME recoverable `RuntimeError` the runtime uses for
            // overflow/OOB/decode. It unwinds (running `defer`s) to the nearest `recover:` as
            // `Err(e)` with `e.message() == msg`, else aborts the program. Routed here (not the pure
            // `builtins::call` table, which returns `Ok`) because it must return `Err`. The checker
            // guarantees a single `str` arg; guard defensively for a non-str.
            if name == "panic" {
                let message = match arg_vals.first() {
                    Some(Value::Str(s)) => s.to_string(),
                    Some(other) => other.type_name().to_string(),
                    None => String::new(),
                };
                return Err(RuntimeError { message, span });
            }
            // `Channel[T]()` (C2) — a fresh empty mailbox (type arg erased at runtime).
            if name == "Channel" {
                if !arg_vals.is_empty() {
                    return Err(RuntimeError {
                        message: "Channel() takes no arguments".to_string(),
                        span,
                    });
                }
                return Ok(Value::Channel(std::rc::Rc::new(std::cell::RefCell::new(
                    value::ChanState::new(),
                ))));
            }
            // `Shared(v)` (C3) — a fresh cross-task box owning a copy of `v` (move-in across the
            // airlock keeps the box isolated from the caller's binding).
            if name == "Shared" {
                if arg_vals.len() != 1 {
                    return Err(RuntimeError {
                        message: format!(
                            "Shared(v) takes exactly one argument, got {}",
                            arg_vals.len()
                        ),
                        span,
                    });
                }
                let init = deep_clone(&arg_vals[0]);
                return Ok(Value::Shared(std::rc::Rc::new(std::cell::RefCell::new(
                    init,
                ))));
            }
            // `Atomic(v)` — a fresh cross-task atomic box owning a copy of `v` (value-first, like `Shared`).
            if name == "Atomic" {
                if arg_vals.len() != 1 {
                    return Err(RuntimeError {
                        message: format!(
                            "Atomic(v) takes exactly one argument, got {}",
                            arg_vals.len()
                        ),
                        span,
                    });
                }
                let init = deep_clone(&arg_vals[0]);
                return Ok(Value::Atomic(std::rc::Rc::new(std::cell::RefCell::new(
                    init,
                ))));
            }
            // `timer(ms)` — a one-shot timeout channel. The interp is single-threaded, so the value is
            // synthesised on `recv` (inline-sleep to the deadline); here we only stamp the deadline.
            if name == "timer" {
                if arg_vals.len() != 1 {
                    return Err(RuntimeError {
                        message: format!(
                            "timer(ms) takes exactly one argument, got {}",
                            arg_vals.len()
                        ),
                        span,
                    });
                }
                let ms = match &arg_vals[0] {
                    Value::Int(ms) => (*ms).max(0) as u64,
                    other => {
                        return Err(RuntimeError {
                            message: format!("timer(ms) expects int, got {}", other.type_name()),
                            span,
                        });
                    }
                };
                let deadline = std::time::Instant::now()
                    .checked_add(std::time::Duration::from_millis(ms))
                    .unwrap_or_else(|| {
                        std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
                    });
                let mut state = value::ChanState::new();
                state.timer = Some(deadline);
                return Ok(Value::Channel(std::rc::Rc::new(std::cell::RefCell::new(
                    state,
                ))));
            }
            // `Executor()` (C5 escape hatch) — a fresh, empty, explicitly-owned work queue.
            if name == "Executor" {
                if !arg_vals.is_empty() {
                    return Err(RuntimeError {
                        message: "Executor() takes no arguments".to_string(),
                        span,
                    });
                }
                let ex = std::rc::Rc::new(std::cell::RefCell::new(value::ExecState::new()));
                // Register for the program-exit auto-drain (and to keep its queued work alive).
                self.executors.push(std::rc::Rc::clone(&ex));
                return Ok(Value::Executor(ex));
            }
            // `int(newtype)` / `float(newtype)` UNWRAP the inner value (the cast-unwrap; the checker
            // verified the underlying matches). Intercepted before the pure `builtins` table, which
            // doesn't know newtypes. `str` is handled above (unwrap iff str-underlying + no override).
            if matches!(name.as_str(), "int" | "float")
                && arg_vals.len() == 1
                && let Value::NewType { inner, .. } = &arg_vals[0]
            {
                let inner = (**inner).clone();
                return builtins::call(name, vec![inner], span);
            }
            if builtins::is_builtin(name) {
                return builtins::call(name, arg_vals, span);
            }
            // A bare newtype ctor: `UserId(x)` wraps the single arg. Resolved like the struct ctor —
            // only a BARE-resolvable newtype — keyed by its runtime key.
            if let Some(nt_key) = self.bare_types.get(name).cloned()
                && self.newtype_defs.contains_key(&nt_key)
            {
                if arg_vals.len() != 1 {
                    return Err(RuntimeError {
                        message: format!(
                            "newtype '{}' expects 1 value, got {}",
                            crate::compiler::bare_display(&nt_key),
                            arg_vals.len()
                        ),
                        span,
                    });
                }
                return Ok(Value::NewType {
                    type_key: nt_key.into(),
                    inner: Box::new(arg_vals.into_iter().next().unwrap()),
                });
            }
            // A bare struct ctor: only a BARE-resolvable struct in THIS module (local / from-import /
            // std), keyed by its declaring module's runtime key. A struct merely present in the global
            // table (another module's) is NOT bare-constructible — the name falls through (e.g. to a
            // from-imported function of the same name).
            if let Some(struct_key) = self.bare_types.get(name).cloned()
                && let Some(def) = self.structs.get(&struct_key).cloned()
            {
                return self.construct_struct(&struct_key, &def, arg_vals, span);
            }
            // A bare *built-in* variant constructor (`Ok(x)`, `Some(x)`) — user variants are qualified
            // (the `Field` arm above), so only built-ins resolve bare here.
            if let Some(def) = variant_pair(None, name)
                .and_then(|k| self.variants.get(&k))
                .cloned()
            {
                if arg_vals.len() != def.arity {
                    return Err(RuntimeError {
                        message: format!(
                            "variant '{}' expects {} value(s), got {}",
                            name,
                            def.arity,
                            arg_vals.len()
                        ),
                        span,
                    });
                }
                return Ok(Value::Enum {
                    ty: def.enum_name.clone(),
                    variant: name.as_str().into(),
                    payload: arg_vals,
                });
            }
        }

        let callee_val = self.eval(callee)?;
        self.call_value(callee_val, arg_vals, span)
    }

    /// Dispatch an already-evaluated callable value (function or closure) on evaluated args.
    fn call_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Value::Func(decl, home) => self.call(&decl, &home, args, span),
            Value::Closure(clo) => self.call_closure(&clo, args, span),
            Value::Native(e) => self.call_native(e.func, args, span),
            Value::Cffi(c) => self.call_cffi(&c, args, span),
            other => Err(RuntimeError {
                message: format!("'{}' is not callable", other.type_name()),
                span,
            }),
        }
    }

    /// `defer <call>` — evaluate the receiver + arguments now (Go semantics) and register the call on
    /// the current frame; it runs at frame exit. The checker guarantees `call` is an `ExprKind::Call`.
    fn exec_defer(&mut self, target: &DeferTarget, span: Span) -> Result<Flow, RuntimeError> {
        let deferred = match target {
            DeferTarget::Block(body) => {
                // Snapshot the locals by value at the defer point. Shallow `.clone()` (NOT
                // `deep_clone`): the block runs in the same task, so it shares heap handles with the
                // parent — matching the VM's `MakeClosure` capture (which copies `Value` handles).
                let locals = self
                    .env
                    .snapshot_locals()
                    .iter()
                    .map(|frame| frame.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .collect();
                Deferred::Block {
                    body: body.clone(),
                    locals,
                    home: self.env.globals_rc(),
                    span,
                }
            }
            DeferTarget::Call(call) => {
                let ExprKind::Call { callee, args, .. } = &call.kind else {
                    return Err(RuntimeError {
                        message: "defer requires a function or method call".to_string(),
                        span,
                    });
                };
                let arg_vals = args
                    .iter()
                    .map(|a| self.eval(a))
                    .collect::<Result<Vec<_>, _>>()?;
                if let ExprKind::Field { obj, name } = &callee.kind {
                    let recv = self.eval(obj)?;
                    Deferred::Method {
                        recv,
                        name: name.clone(),
                        args: arg_vals,
                        span: call.span,
                    }
                } else {
                    let callee_val = self.eval(callee)?;
                    Deferred::Call {
                        callee: callee_val,
                        args: arg_vals,
                        span: call.span,
                    }
                }
            }
        };
        if let Some(frame) = self.deferred.last_mut() {
            frame.push(deferred);
        }
        Ok(Flow::Normal)
    }

    /// `parallel:` — open a nursery, run its body (spawn statements register tasks; inline
    /// statements run immediately), then at the dedent run the registered tasks to completion FIFO.
    /// The first task to fault aborts the remaining siblings and propagates (composing with
    /// `recover:` / `defer`, which see it as an ordinary `Err`). A fault in the body itself
    /// propagates without running any queued task.
    /// `wait:` — Chezzi's `select` (§6d), the sequential reference semantics (the VM's parity oracle).
    /// Evaluate each arm's channel once (source order), poll once in source order: the first channel
    /// with a queued value (or a fired timer) wins. A closed+empty arm is skipped. Nothing ready →
    /// run `else` if present; otherwise, if any arm is a live timer, inline-sleep to the soonest
    /// deadline and take it (deterministic, single-threaded); else fault — `"wait: all channels
    /// closed"` if every arm is closed+empty, the sequential `deadlock` fault otherwise.
    fn exec_wait(
        &mut self,
        arms: &[WaitArm],
        else_block: Option<&[Stmt]>,
        span: Span,
    ) -> Result<Flow, RuntimeError> {
        // 1. Evaluate each arm's channel expression once, in source order.
        let mut chans = Vec::with_capacity(arms.len());
        for arm in arms {
            match self.eval(&arm.chan)? {
                Value::Channel(q) => chans.push(q),
                other => {
                    return Err(RuntimeError {
                        message: format!(
                            "a wait arm must recv from a Channel, found {}",
                            other.type_name()
                        ),
                        span: arm.chan.span,
                    });
                }
            }
        }
        // 2. Poll source order. Track the soonest live-timer deadline and whether every arm is
        //    closed+empty (so an all-closed `wait` with no `else` faults distinctly).
        let mut soonest_timer: Option<(usize, std::time::Instant)> = None;
        let mut all_closed = true;
        for (i, q) in chans.iter().enumerate() {
            let (popped, closed, timer, latch) = {
                let mut s = q.borrow_mut();
                (s.queue.pop_front(), s.closed, s.timer, s.done_latch)
            };
            if let Some(v) = popped {
                return self.run_wait_arm(&arms[i], v, span);
            }
            // A tripped latch (`trip()`) is ready like a fired timer — take the arm with `true`.
            if latch {
                return self.run_wait_arm(&arms[i], Value::Bool(true), span);
            }
            if let Some(deadline) = timer {
                // A timer channel is never closed and always eventually ready: fired now, or a live
                // waiter whose deadline we may sleep to below.
                if std::time::Instant::now() >= deadline {
                    return self.run_wait_arm(&arms[i], Value::Bool(true), span);
                }
                all_closed = false;
                if soonest_timer.is_none_or(|(_, d)| deadline < d) {
                    soonest_timer = Some((i, deadline));
                }
            } else if !closed {
                all_closed = false;
            }
        }
        // 3. Nothing ready → the non-blocking fallback.
        if let Some(b) = else_block {
            return self.exec_scoped_block(b);
        }
        // 4. A live timer arm → inline-sleep to the soonest deadline and take it (deterministic).
        if let Some((i, deadline)) = soonest_timer {
            let now = std::time::Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
            return self.run_wait_arm(&arms[i], Value::Bool(true), span);
        }
        // 5. No `else`, no live timer: every arm closed+empty faults distinctly; otherwise the
        //    sequential `deadlock` fault (the interp cannot block waiting for a producer — like `recv`).
        if all_closed {
            return Err(RuntimeError {
                message: "wait: all channels closed".to_string(),
                span,
            });
        }
        Err(RuntimeError {
            message: "wait on channels that are all empty: deadlock — nothing is queued and the \
                      sequential executor cannot block waiting for a producer (a consumer that \
                      waits mid-flight on a live producer needs C5)"
                .to_string(),
            span,
        })
    }

    /// Run a chosen `wait` arm: deliver `val` per the arm's target in a fresh arm-local scope, then
    /// execute the arm body in that scope.
    fn run_wait_arm(
        &mut self,
        arm: &WaitArm,
        val: Value,
        span: Span,
    ) -> Result<Flow, RuntimeError> {
        self.env.push();
        let r = match &arm.target {
            WaitTarget::Bind(name) => {
                self.env.define(name, val);
                self.exec_block(&arm.body)
            }
            WaitTarget::Discard => self.exec_block(&arm.body),
            WaitTarget::Assign(target) => {
                // Reuse the ordinary assignment path so `=` semantics (outer lvalue, index/field
                // mutation) match a plain `target = recv()` exactly. The received value is delivered
                // through a reserved, un-lexable temp binding (no user identifier can collide).
                self.env.define(WAIT_RECV_TMP, val);
                let value_expr = Expr {
                    kind: ExprKind::Ident(WAIT_RECV_TMP.to_string()),
                    span,
                };
                match self.exec_assign(target, AssignOp::Eq, &value_expr, span) {
                    Ok(()) => self.exec_block(&arm.body),
                    Err(e) => Err(e),
                }
            }
        };
        self.env.pop();
        r
    }

    fn exec_parallel(&mut self, body: &[Stmt]) -> Result<Flow, RuntimeError> {
        self.nurseries.push(Vec::new());
        let body_result = self.exec_scoped_block(body);
        // Always reclaim our task list, even if the body faulted, so it can't dangle.
        let tasks = self.nurseries.pop().unwrap_or_default();
        // TASK B — cancel-and-report on early escape. The body escapes the join when it faults (`?`,
        // an `Err`) OR when a non-`Normal` flow (`return`/`break`/`continue`) unwinds past the dedent.
        // In every such case the unstarted `spawn` tasks are CANCELLED, not run (the same end-state a
        // started sibling reaches under B3.4), and ONE report line is written to stdout — byte-identical
        // to the VM's `drain_escaped_nursery`. Only the NORMAL fall-through runs the queued tasks.
        let escaped = body_result.is_err()
            || matches!(
                body_result,
                Ok(Flow::Return(_)) | Ok(Flow::Break) | Ok(Flow::Continue)
            );
        if escaped {
            if !tasks.is_empty() {
                self.out
                    .push_str(&crate::runtime::pending_cancel_report(tasks.len()));
            }
            return body_result;
        }
        let body_flow = body_result?;
        for task in tasks {
            self.run_task(task)?;
        }
        Ok(body_flow)
    }

    /// M-C implicit nurseries — drain the implicit function / method / `spawn:`-block nursery that the
    /// body wrap pushed at entry. The function's `return`/`?`/end is the JOIN barrier (vs. an explicit
    /// `parallel:`, whose dedent is the barrier and whose early escapes cancel):
    /// - clean exit (Normal fall-through, explicit `return`, or a `?` early-return — which surfaces as
    ///   `Err("? propagation")` with `propagating` set) ⇒ JOIN: run the queued tasks FIFO;
    /// - genuine fault (`Err` with no `propagating`) or a stray `break`/`continue` ⇒ CANCEL-and-report,
    ///   byte-identical to the VM's escaped-nursery drain on its unwind path.
    ///
    /// A task that faults during the join supersedes the body result (clearing any in-flight `?`
    /// value), so the function faults with the task's error — mirroring the VM's `join_nursery()?` in
    /// `do_return`. The returned `Flow` flows on into the normal `finish_frame` teardown (so the
    /// implicit join runs BEFORE the frame's defers — tasks complete, then cleanup).
    fn leave_implicit_nursery(
        &mut self,
        body_result: Result<Flow, RuntimeError>,
    ) -> Result<Flow, RuntimeError> {
        let tasks = self.nurseries.pop().unwrap_or_default();
        let joins = matches!(body_result, Ok(Flow::Return(_)) | Ok(Flow::Normal))
            || (body_result.is_err() && self.propagating.is_some());
        if joins {
            // Protect the in-flight `?` value (set when the body short-circuited via `?`) from the
            // joined tasks: each `run_task` descends into `finish_frame`, which does
            // `propagating.take()` and would clear it before this function's own `finish_frame` can
            // surface it (mirrors `run_deferred`'s save/restore). Without this, a `?`-returning body
            // with a bare `spawn` leaks the `Err("? propagation")` sentinel instead of the user's
            // `Err(...)` — a divergence from the VM, where joined tasks run in a swapped-out `FiberCtx`.
            let saved_prop = self.propagating.take();
            for task in tasks {
                if let Err(e) = self.run_task(task) {
                    self.propagating = None; // a task fault supersedes any in-flight `?` value
                    return Err(e);
                }
            }
            self.propagating = saved_prop;
        } else if !tasks.is_empty() {
            self.out
                .push_str(&crate::runtime::pending_cancel_report(tasks.len()));
        }
        body_result
    }

    /// `spawn` — register a task on the innermost nursery (the checker guarantees one is open). The
    /// receiver/args (form 1) or captured locals (form 2) are deep-copied across the airlock now;
    /// the body runs later, at the nursery's dedent.
    fn exec_spawn(&mut self, target: &SpawnTarget, span: Span) -> Result<Flow, RuntimeError> {
        let task = match target {
            SpawnTarget::Call(call) => {
                let ExprKind::Call { callee, args, .. } = &call.kind else {
                    return Err(RuntimeError {
                        message: "spawn requires a function or method call".to_string(),
                        span,
                    });
                };
                // Evaluate the receiver (method form) before the arguments, matching
                // `eval_method_call`'s documented order so `spawn obj.m(a)` and a direct call agree
                // (interp/VM parity).
                if let ExprKind::Field { obj, name } = &callee.kind {
                    let recv = deep_clone(&self.eval(obj)?);
                    let arg_vals = args
                        .iter()
                        .map(|a| self.eval(a).map(|v| deep_clone(&v)))
                        .collect::<Result<Vec<_>, _>>()?;
                    Task::Method {
                        recv,
                        name: name.clone(),
                        args: arg_vals,
                        span: call.span,
                    }
                } else {
                    let callee_val = self.eval(callee)?;
                    let arg_vals = args
                        .iter()
                        .map(|a| self.eval(a).map(|v| deep_clone(&v)))
                        .collect::<Result<Vec<_>, _>>()?;
                    Task::Call {
                        callee: callee_val,
                        args: arg_vals,
                        span: call.span,
                    }
                }
            }
            SpawnTarget::Block(body) => {
                // Deep-copy the captured locals so the task can't share mutable state with the
                // parent (the airlock). Functions/modules pass by handle — sendability of the
                // bindings a task actually uses is enforced statically by the checker in C2.
                let locals = self
                    .env
                    .snapshot_locals()
                    .iter()
                    .map(|frame| {
                        frame
                            .iter()
                            .map(|(k, v)| (k.clone(), deep_clone(v)))
                            .collect()
                    })
                    .collect();
                Task::Block {
                    body: body.clone(),
                    locals,
                    home: self.env.globals_rc(),
                    span,
                }
            }
        };
        match self.nurseries.last_mut() {
            Some(nursery) => nursery.push(task),
            None => {
                return Err(RuntimeError {
                    message: "spawn must be inside a parallel: block".to_string(),
                    span,
                });
            }
        }
        Ok(Flow::Normal)
    }

    /// Run one registered task to completion. Its return value is discarded — tasks communicate
    /// only through side effects (C1) and, later, channels / shared boxes (C2 / C3).
    fn run_task(&mut self, task: Task) -> Result<(), RuntimeError> {
        match task {
            Task::Call { callee, args, span } => self.call_value(callee, args, span).map(|_| ()),
            Task::Method {
                recv,
                name,
                args,
                span,
            } => self.dispatch_method(recv, &name, args, span).map(|_| ()),
            Task::Block {
                body,
                locals,
                home,
                span,
            } => self.run_block_task("<spawned task>", &body, locals, home, span),
        }
    }

    /// Run a `spawn:` block task in its own call frame against the captured locals + home globals
    /// (mirrors [`Interp::call_closure`]'s frame setup). Any `return` / fall-through ends the task.
    fn run_block_task(
        &mut self,
        name: &str,
        body: &[Stmt],
        locals: Vec<std::collections::HashMap<String, Value>>,
        home: value::ModEnv,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.enter_call(span)?;
        self.deferred.push(Vec::new());
        self.call_stack.push(TraceFrame {
            function: name.to_string(),
            span,
        });
        let saved_ctx = self.enter_module_ctx(&home);
        let saved_globals = self.env.swap_globals(home);
        let saved = self.env.swap_locals(locals);
        // M-C: a `spawn:` block (or deferred block) is its own function body — a nested bare `spawn`
        // binds to the block's own implicit nursery, joined when the block returns/ends.
        let implicit = crate::compiler::block_has_bare_spawn(body);
        if implicit {
            self.nurseries.push(Vec::new());
        }
        let result = self.exec_block_inner(body);
        let result = if implicit {
            self.leave_implicit_nursery(result)
        } else {
            result
        };
        let outcome = match self.finish_frame(saved, saved_globals) {
            Err(e) => Err(e),
            // A `?` short-circuit inside the block sets `propagating`, which `finish_frame` surfaces
            // as `Ok(Some(_))`. A block (spawn task or deferred body) has no error-return contract,
            // so the propagated value is discarded — mirroring `call_closure` and the VM, which runs
            // the block as a closure and discards its return at the task/defer boundary. Without this
            // the "? propagation" `Err` in `result` would escape on the interp but not the VM.
            Ok(Some(_)) => Ok(()),
            Ok(None) => result.map(|_| ()),
        };
        self.restore_module_ctx(saved_ctx);
        if outcome.is_ok() {
            self.call_stack.pop();
        }
        outcome
    }

    /// Run a frame's deferred calls in LIFO order (last `defer` registered runs first). Each call
    /// gets a clean propagation channel (saved/restored) so it can't consume the frame's in-flight
    /// `?` value. A fault in a deferred call is remembered and returned after the rest still run
    /// (Go: all defers run; the latest fault wins) — but a deferred `std.os.exit` stops the drain.
    fn run_deferred(&mut self, mut defers: Vec<Deferred>) -> Result<(), RuntimeError> {
        let mut err = None;
        while let Some(d) = defers.pop() {
            let saved_prop = self.propagating.take();
            let r = match d {
                Deferred::Call { callee, args, span } => self.call_value(callee, args, span),
                Deferred::Method {
                    recv,
                    name,
                    args,
                    span,
                } => self.dispatch_method(recv, &name, args, span),
                Deferred::Block {
                    body,
                    locals,
                    home,
                    span,
                } => self
                    .run_block_task("<deferred block>", &body, locals, home, span)
                    .map(|_| Value::Nil),
            };
            self.propagating = saved_prop;
            if let Err(e) = r {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Invoke a native (Rust) function value (M6c). Builds an [`InterpHost`] over the evaluated
    /// args, runs the binding, and lowers its engine-neutral [`NativeRet`] into a `Value`. A
    /// [`HostError`] becomes a `RuntimeError` carrying the call site's span.
    fn call_native(
        &mut self,
        func: crate::native::NativeFn,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let mut host = InterpHost {
            args,
            out: &mut self.out,
            stderr: &mut self.stderr,
            cfg: &mut self.host,
            exit: &mut self.pending_exit,
        };
        let ret = func(&mut host).map_err(|e| RuntimeError {
            message: e.message,
            span,
        })?;
        Ok(lower_native(ret))
    }

    /// Call a dynamic C-ABI FFI function (`extern "lib":`). Reuses the same `Host`/`NativeRet` seam
    /// as `call_native` (so the VM and interp produce identical output): build an [`InterpHost`] over
    /// the evaluated args, invoke the C fn through libffi, and lower its `NativeRet` to a `Value`.
    fn call_cffi(
        &mut self,
        cffi: &crate::native::cffi::Cffi,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // Arity is checker-guaranteed; guard defensively so a wrong count can't index out of bounds.
        if args.len() != cffi.param_count() {
            return Err(RuntimeError {
                message: format!(
                    "function '{}' expects {} argument(s), got {}",
                    cffi.name(),
                    cffi.param_count(),
                    args.len()
                ),
                span,
            });
        }
        // Use the callback-capable host (holds `&mut Interp`) so a `fn(...)`-typed extern param can
        // re-enter `call_closure` (callbacks #4). `call_native` keeps the lighter split-borrow
        // `InterpHost` (it never carries callbacks). The two route I/O through the same Interp fields,
        // so behavior is byte-identical (asserted by the two-engine parity tests).
        let mut host = InterpCallbackHost {
            interp: self,
            args,
            span,
        };
        let ret = cffi.call(&mut host).map_err(|e| RuntimeError {
            message: e.message,
            span,
        })?;
        Ok(lower_native(ret))
    }

    /// Call a closure: restore its captured frames, bind params on top, evaluate the body
    /// expression (a closure body is a single expression).
    fn call_closure(
        &mut self,
        clo: &value::Closure,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != clo.params.len() {
            return Err(RuntimeError {
                message: format!(
                    "closure expects {} argument(s), got {}",
                    clo.params.len(),
                    args.len()
                ),
                span,
            });
        }
        let mut frame = std::collections::HashMap::new();
        for (param, arg) in clo.params.iter().zip(args) {
            // One-way int→float widening at the closure prologue (mirrors `compile_closure`): coerce
            // an int argument to a `float`-typed closure param. (A closure declares no return type.)
            let arg = if param.ty.as_ref().is_some_and(crate::ast::is_float_ty) {
                coerce_float(arg)
            } else {
                arg
            };
            frame.insert(param.name.clone(), arg);
        }
        let mut new_locals = clo.captured.clone();
        new_locals.push(frame);
        self.enter_call(span)?;
        self.deferred.push(Vec::new());
        self.call_stack.push(TraceFrame {
            function: "<closure>".to_string(),
            span,
        });
        let saved_globals = self.env.swap_globals(clo.home.clone());
        let saved_ctx = self.enter_module_ctx(&clo.home);
        let saved = self.env.swap_locals(new_locals);
        let result = self.eval(&clo.body);
        let outcome = match self.finish_frame(saved, saved_globals) {
            Err(e) => Err(e),
            Ok(Some(v)) => Ok(v),
            Ok(None) => result,
        };
        self.restore_module_ctx(saved_ctx);
        if outcome.is_ok() {
            self.call_stack.pop();
        }
        outcome
    }

    /// Increment the call-depth counter, erroring (instead of overflowing the host stack) past
    /// `MAX_CALL_DEPTH`. The matching `self.call_depth -= 1` runs after the body completes.
    fn enter_call(&mut self, span: Span) -> Result<(), RuntimeError> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return Err(RuntimeError {
                message: format!(
                    "maximum call depth ({MAX_CALL_DEPTH}) exceeded (infinite recursion?)"
                ),
                span,
            });
        }
        Ok(())
    }

    /// Execute a `match`: find the first arm whose variant pattern matches the scrutinee, bind
    /// its payload, and run the arm body in a fresh scope. No matching arm is a runtime error
    /// (static exhaustiveness checking arrives with the type checker in M4).
    fn exec_match(&mut self, scrutinee: &Expr, arms: &[MatchArm]) -> Result<Flow, RuntimeError> {
        let value = self.eval(scrutinee)?;
        // A variant pattern against a non-enum value is a checker-prevented case; in `Skip` mode
        // (un-inferable scrutinee) it can still slip through, so report it as a clean runtime error.
        if let Some(arm) = arms.first()
            && pattern_needs_enum(&arm.pattern)
            && !matches!(value, Value::Enum { .. })
        {
            return Err(RuntimeError {
                message: format!("cannot match on {}", value.type_name()),
                span: scrutinee.span,
            });
        }
        for arm in arms {
            if let Some(binds) = self.try_bind(&arm.pattern, &value) {
                self.env.push();
                for (name, v) in binds {
                    self.env.define(&name, v);
                }
                // Evaluate the optional guard with the pattern's bindings in scope; a false guard
                // falls through to the next arm.
                if let Some(guard) = &arm.guard {
                    match self.eval(guard) {
                        Ok(Value::Bool(true)) => {}
                        Ok(_) => {
                            self.env.pop();
                            continue;
                        }
                        Err(e) => {
                            self.env.pop();
                            return Err(e);
                        }
                    }
                }
                let flow = self.exec_block(&arm.body);
                self.env.pop();
                return flow;
            }
        }
        Err(RuntimeError {
            message: no_match_arm_message(&value),
            span: scrutinee.span,
        })
    }

    /// Expression-position `match`: evaluate the chosen arm's value-expression and return it
    /// (vs `exec_match`, whose arm bodies are statement blocks producing a control-flow `Flow`).
    fn eval_match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchExprArm],
    ) -> Result<Value, RuntimeError> {
        let value = self.eval(scrutinee)?;
        if let Some(arm) = arms.first()
            && pattern_needs_enum(&arm.pattern)
            && !matches!(value, Value::Enum { .. })
        {
            return Err(RuntimeError {
                message: format!("cannot match on {}", value.type_name()),
                span: scrutinee.span,
            });
        }
        for arm in arms {
            if let Some(binds) = self.try_bind(&arm.pattern, &value) {
                self.env.push();
                for (name, v) in binds {
                    self.env.define(&name, v);
                }
                if let Some(guard) = &arm.guard {
                    match self.eval(guard) {
                        Ok(Value::Bool(true)) => {}
                        Ok(_) => {
                            self.env.pop();
                            continue;
                        }
                        Err(e) => {
                            self.env.pop();
                            return Err(e);
                        }
                    }
                }
                let result = self.eval(&arm.body);
                self.env.pop();
                return result;
            }
        }
        Err(RuntimeError {
            message: no_match_arm_message(&value),
            span: scrutinee.span,
        })
    }

    /// Construct a struct instance, binding the positional args to fields in declaration order.
    /// Evaluate `args`, arity-check against the variant `def`, and build the enum value. Shared by the
    /// bare and the qualified (`module.Enum.Variant`) constructor paths. `def.enum_name` is the enum's
    /// runtime key (bare unless disambiguated), stamped into `Value::Enum.ty` for Display/match parity.
    fn build_variant(
        &mut self,
        def: &VariantDef,
        variant: &str,
        args: &[Expr],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let arg_vals = args
            .iter()
            .map(|a| self.eval(a))
            .collect::<Result<Vec<_>, _>>()?;
        if arg_vals.len() != def.arity {
            return Err(RuntimeError {
                message: format!(
                    "variant '{}' expects {} value(s), got {}",
                    variant,
                    def.arity,
                    arg_vals.len()
                ),
                span,
            });
        }
        Ok(Value::Enum {
            ty: def.enum_name.clone(),
            variant: variant.into(),
            payload: arg_vals,
        })
    }

    fn construct_struct(
        &mut self,
        name: &str,
        def: &StructDef,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != def.fields.len() {
            return Err(RuntimeError {
                message: format!(
                    "struct '{}' expects {} field(s), got {}",
                    crate::compiler::bare_display(name),
                    def.fields.len(),
                    args.len()
                ),
                span,
            });
        }
        // One-way int→float widening (mirrors the VM's per-field `CoerceFloat` at `NewStruct`): coerce
        // an int argument whose declared field type is `float`. Field types come from `struct_fields`
        // (keyed by the struct's runtime key == `name`); a generic field typed `T` is not `float`, so
        // it is left untouched (the no-generic-widening carve-out).
        let mut args = args;
        if let Some(decl_fields) = self.struct_fields.get(name) {
            for (i, f) in decl_fields.iter().enumerate() {
                if crate::ast::is_float_ty(&f.ty)
                    && let Some(slot) = args.get_mut(i)
                {
                    let taken = std::mem::replace(slot, Value::Nil);
                    *slot = coerce_float(taken);
                }
            }
        }
        let fields = def.fields.iter().cloned().zip(args).collect::<Vec<_>>();
        Ok(Value::Struct {
            name: name.into(),
            fields: std::rc::Rc::new(std::cell::RefCell::new(fields)),
        })
    }

    /// Evaluate `obj.method(args)`: look up the method on the receiver's struct type and call it
    /// with the receiver bound as `self`.
    fn eval_method_call(
        &mut self,
        obj: &Expr,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let receiver = self.eval(obj)?;
        // Evaluate the arguments up front — *before* any method-lookup or type error — so the
        // interp matches the VM, which evaluates call operands (bytecode) before the `CallMethod`
        // op. Without this, `(5).frob(1 / 0)` would error on the receiver type here while the VM
        // errors on the argument, breaking interp/VM parity (caught by the parity suite).
        let arg_vals = args
            .iter()
            .map(|a| self.eval(a))
            .collect::<Result<Vec<_>, _>>()?;
        self.dispatch_method(receiver, method, arg_vals, span)
    }

    /// Dispatch a method on an already-evaluated receiver + argument values. Split out of
    /// `eval_method_call` so `defer obj.m(a)` can re-invoke the same dispatch at frame exit with the
    /// receiver/args it captured at the `defer` statement.
    fn dispatch_method(
        &mut self,
        receiver: Value,
        method: &str,
        arg_vals: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // `compare` on a primitive (int/float/str): these intrinsically satisfy `Comparable`, so an
        // erased generic body may call `.compare()` on a concrete primitive receiver. Return the
        // sign of the ordering (-1/0/1). Structs with their own `compare` fall through to the
        // normal method dispatch below.
        if method == "compare"
            && arg_vals.len() == 1
            && matches!(receiver, Value::Int(_) | Value::Float(_) | Value::Str(_))
            && let Some(ord) = compare(&receiver, &arg_vals[0])
        {
            return Ok(Value::Int(ord as i64));
        }
        // A cursor (`Value::Iter`, the `Iterable` `.iter()` result): `.next()` advances the snapshot
        // (idempotent `None` past the end) and `.iter()` returns self. Interior-mutable via `borrow_mut`.
        if let Value::Iter(cur) = &receiver {
            if !arg_vals.is_empty() {
                return Err(RuntimeError {
                    message: format!("a cursor's '{method}' takes no arguments"),
                    span,
                });
            }
            return match method {
                "iter" => Ok(receiver.clone()), // idempotent
                "next" => {
                    let mut c = cur.borrow_mut();
                    if c.pos < c.items.len() {
                        let item = c.items[c.pos].clone();
                        c.pos += 1;
                        Ok(enum_val("Option", "Some", vec![item]))
                    } else {
                        Ok(enum_val("Option", "None", vec![]))
                    }
                }
                _ => Err(RuntimeError {
                    message: format!("a cursor has no method '{method}' (only `next()`/`iter()`)"),
                    span,
                }),
            };
        }
        // `.iter()` on a built-in collection → a FRESH cursor SNAPSHOTTING current contents in the
        // SAME order/elements as `for x in X` (reusing `iter_rows_from_value`, the for-loop's single
        // source of truth: list/set elems, map → keys, str → per-char str, bytes/bytearray → int).
        if method == "iter"
            && arg_vals.is_empty()
            && matches!(
                receiver,
                Value::List(_)
                    | Value::Set(_)
                    | Value::Map(_)
                    | Value::Str(_)
                    | Value::Bytes(_)
                    | Value::ByteArray(_)
            )
        {
            let rows = iter_rows_from_value(&receiver, 1, span)?;
            let items: Vec<Value> = rows.into_iter().map(|mut r| r.remove(0)).collect();
            return Ok(Value::Iter(std::rc::Rc::new(std::cell::RefCell::new(
                value::IterCursor { items, pos: 0 },
            ))));
        }
        // Higher-order list methods call a Chezzi function value per element, so they need the
        // interpreter handle (`self.call_value`) and can't live in the pure `builtins` table.
        if let Value::List(items) = &receiver
            && matches!(
                method,
                "map" | "filter" | "fold" | "sort_by" | "sort_by_key"
            )
        {
            // Clone the elements out so we don't hold the `RefCell` borrow across `call_value`
            // (the closure body could re-borrow this same list). This clone is also the SNAPSHOT
            // that defines HOF semantics: map/filter/fold iterate the receiver's elements as of call
            // time, so a callback that mutates (shrinks/grows) the receiver does not perturb the
            // iteration. The VM mirrors this (see `list_hof` in `src/vm/mod.rs`, which allocates a
            // rooted snapshot) — keep both engines on this same snapshot semantics for parity.
            let elems: Vec<Value> = items.borrow().clone();
            if method == "sort_by" {
                // `sort_by` sorts in place; keep the `Rc` so we can write the result back.
                let list = std::rc::Rc::clone(items);
                return self.eval_list_sort_by(list, elems, arg_vals, span);
            }
            if method == "sort_by_key" {
                let list = std::rc::Rc::clone(items);
                return self.eval_list_sort_by_key(list, elems, arg_vals, span);
            }
            return self.eval_list_hof(method, elems, arg_vals, span);
        }
        // `xs.sort()` over a list of structs must call each struct's `compare` (engine access), so
        // it can't live in the pure `builtins` table — route it here. Primitive lists fall through
        // to the fast `builtins::value_order` path below.
        if let Value::List(items) = &receiver
            && method == "sort"
            && arg_vals.is_empty()
            && matches!(items.borrow().first(), Some(Value::Struct { .. }))
        {
            let elems: Vec<Value> = items.borrow().clone();
            let list = std::rc::Rc::clone(items);
            return self.eval_list_sort(list, elems, span);
        }
        // Map/set methods can hash a struct key (engine access for the user `hash()`), so they live
        // here rather than the pure `builtins` table. (Mirrors the `sort`-over-structs routing.)
        if let Value::Map(m) = &receiver {
            let m = std::rc::Rc::clone(m);
            return self.eval_map_method(&m, method, arg_vals, span);
        }
        if let Value::Set(s) = &receiver {
            let s = std::rc::Rc::clone(s);
            return self.eval_set_method(&s, method, arg_vals, span);
        }
        if let Value::Channel(q) = &receiver {
            let q = std::rc::Rc::clone(q);
            return self.eval_channel_method(&q, method, arg_vals, span);
        }
        if let Value::Shared(cell) = &receiver {
            let cell = std::rc::Rc::clone(cell);
            return self.eval_shared_method(&cell, method, arg_vals, span);
        }
        if let Value::Atomic(cell) = &receiver {
            let cell = std::rc::Rc::clone(cell);
            return self.eval_atomic_method(&cell, method, arg_vals, span);
        }
        if let Value::Executor(ex) = &receiver {
            let ex = std::rc::Rc::clone(ex);
            return self.eval_executor_method(&ex, method, arg_vals, span);
        }
        // `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Mirrors the VM's
        // `bytes_method` and the checker's `bytes_method_sig`.
        if let Value::Bytes(b) = &receiver {
            let b = b.clone();
            return self.eval_bytes_method(&b, method, arg_vals, span);
        }
        // `bytearray` methods (mutable buffer): `len`/`push`/`pop`/`extend`/`decode`. Mutate in place
        // via the shared `Rc<RefCell>` (`borrow_mut`), so a second binding observes the change (like
        // `list`).
        if let Value::ByteArray(buf) = &receiver {
            let buf = std::rc::Rc::clone(buf);
            return self.eval_bytearray_method(&buf, method, arg_vals, span);
        }
        // Core-type methods (M6): built-in methods on `str` and `list` dispatch on the value.
        if matches!(receiver, Value::Str(_) | Value::List(_)) {
            return builtins::call_method(&receiver, method, arg_vals, span);
        }
        // `module.fn(args)` is a plain call on the looked-up member — no `self` is bound.
        if let Value::Module(ns) = &receiver {
            let member =
                ns.members
                    .0
                    .borrow()
                    .get(method)
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!("module '{}' has no member '{method}'", ns.name),
                        span,
                    })?;
            return self.call_value(member, arg_vals, span);
        }
        // Anything else (a struct, or an unsupported receiver) goes through the struct-method path,
        // which dispatches the named method or reports "type … has no method …".
        self.call_struct_method(receiver, method, arg_vals, span)
    }

    /// Evaluate `obj[start..end]`: bounds-clamped half-open copy of a list/str, or a struct's
    /// `slice(self, start, end)`. Kept out of `eval`'s match to keep that frame small (deep recursion).
    fn eval_slice(
        &mut self,
        obj: &Expr,
        start: Option<&Expr>,
        end: Option<&Expr>,
        step: Option<&Expr>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let target = self.eval(obj)?;
        // Each present component must be an int; an omitted one is `None` (direction-dependent default).
        let comp = |me: &mut Self, c: Option<&Expr>| -> Result<Option<i64>, RuntimeError> {
            match c {
                Some(e) => Ok(Some(as_int_val(me.eval(e)?, span)?)),
                None => Ok(None),
            }
        };
        let s = comp(self, start)?;
        let e = comp(self, end)?;
        let st = comp(self, step)?;
        match &target {
            Value::List(items) => {
                let items = items.borrow();
                let idxs = crate::slice::slice_indices(s, e, st, items.len()).map_err(|m| {
                    RuntimeError {
                        message: m.to_string(),
                        span,
                    }
                })?;
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(
                    idxs.iter().map(|&i| items[i].clone()).collect(),
                ))))
            }
            Value::Str(string) => {
                let chars: Vec<char> = string.chars().collect();
                let idxs = crate::slice::slice_indices(s, e, st, chars.len()).map_err(|m| {
                    RuntimeError {
                        message: m.to_string(),
                        span,
                    }
                })?;
                Ok(Value::Str(
                    idxs.iter().map(|&i| chars[i]).collect::<String>().into(),
                ))
            }
            // `bytes[a:b:c]` slices over BYTE offsets and yields a new `bytes` (shared `slice_indices`).
            Value::Bytes(b) => {
                let idxs =
                    crate::slice::slice_indices(s, e, st, b.len()).map_err(|m| RuntimeError {
                        message: m.to_string(),
                        span,
                    })?;
                let sub: Vec<u8> = idxs.iter().map(|&i| b[i]).collect();
                Ok(Value::Bytes(sub.into()))
            }
            // `bytearray[a:b:c]` yields a NEW `bytearray` (a fresh `Rc<RefCell>`, mutable copy).
            Value::ByteArray(b) => {
                let b = b.borrow();
                let idxs =
                    crate::slice::slice_indices(s, e, st, b.len()).map_err(|m| RuntimeError {
                        message: m.to_string(),
                        span,
                    })?;
                let sub: Vec<u8> = idxs.iter().map(|&i| b[i]).collect();
                Ok(Value::ByteArray(std::rc::Rc::new(std::cell::RefCell::new(
                    sub,
                ))))
            }
            // A struct satisfying `Slice` dispatches `obj[a:b:c]` to `slice(self, start?, end?, step?)`,
            // passing real `Option[int]` components (`None`/`Some(n)`) the user body can match/`??`.
            Value::Struct { .. } => {
                let opt = |c: Option<i64>| match c {
                    None => enum_val("Option", "None", Vec::new()),
                    Some(n) => enum_val("Option", "Some", vec![Value::Int(n)]),
                };
                self.call_struct_method(
                    target.clone(),
                    "slice",
                    vec![opt(s), opt(e), opt(st)],
                    span,
                )
            }
            other => Err(RuntimeError {
                message: format!("cannot slice {}", other.type_name()),
                span,
            }),
        }
    }

    /// Invoke a struct's named method with an already-evaluated receiver + argument values, binding
    /// `self` to the receiver. Shared by ordinary method calls and the `Index`/`IndexSet`/`Slice`
    /// protocol dispatch (`obj[k]` → `index`, `obj[k] = v` → `set_index`, `obj[a..b]` → `slice`).
    fn call_struct_method(
        &mut self,
        receiver: Value,
        method: &str,
        arg_vals: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // Enum method dispatch (name-resolved, like structs). Resolves `enum_defs[ty][method]` off
        // the value's enum key and binds the whole enum value as `self`.
        if let Value::Enum { ty, .. } = &receiver {
            let key = ty.to_string();
            if let Some(def) = self.enum_defs.get(&key).cloned()
                && let Some(decl) = def.methods.get(method).cloned()
            {
                let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
                call_args.push(receiver.clone());
                call_args.extend(arg_vals);
                return self.call(&decl, &def.home, call_args, span);
            }
            return Err(RuntimeError {
                message: format!(
                    "type {} has no method '{method}'",
                    crate::compiler::bare_display(&key)
                ),
                span,
            });
        }
        // Newtype method dispatch (name-resolved, like enums). Resolves `newtype_defs[key][method]`
        // off the wrapper's key. The underlying's methods are NOT inherited.
        if let Value::NewType { type_key, .. } = &receiver {
            let key = type_key.to_string();
            if let Some(def) = self.newtype_defs.get(&key).cloned()
                && let Some(decl) = def.methods.get(method).cloned()
            {
                let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
                call_args.push(receiver.clone());
                call_args.extend(arg_vals);
                return self.call(&decl, &def.home, call_args, span);
            }
            return Err(RuntimeError {
                message: format!(
                    "type {} has no method '{method}'",
                    crate::compiler::bare_display(&key)
                ),
                span,
            });
        }
        let Value::Struct { name, fields } = &receiver else {
            return Err(RuntimeError {
                message: format!("type {} has no method '{method}'", receiver.type_name()),
                span,
            });
        };
        let def = self
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| RuntimeError {
                message: format!("unknown struct type '{name}'"),
                span,
            })?;
        if let Some(decl) = def.methods.get(method).cloned() {
            let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
            call_args.push(receiver.clone());
            call_args.extend(arg_vals);
            return self.call(&decl, &def.home, call_args, span);
        }
        // No method named `method`: fall back to a function-typed *field* — `recv.f(args)` where
        // `f` holds a function value (the checker verified `f: fn(...) -> ...`). Calls the field
        // value directly (no `self` is bound — it's not a method).
        if let Some(f) = fields
            .borrow()
            .iter()
            .find(|(k, _)| k == method)
            .map(|(_, v)| v.clone())
        {
            return self.call_value(f, arg_vals, span);
        }
        // A user iterator struct (one with `next`, no explicit `iter`) IS Iterable — `.iter()` on it
        // returns self (idempotent). Lets a struct-with-`next` flow into an `[S: Iterable[T]]` body
        // that calls `s.iter()`. A struct that defines its own `iter` resolved above.
        if method == "iter" && arg_vals.is_empty() && def.methods.contains_key("next") {
            return Ok(receiver.clone());
        }
        Err(RuntimeError {
            message: format!(
                "struct '{}' has no method '{method}'",
                crate::compiler::bare_display(name)
            ),
            span,
        })
    }

    /// Render a value the way `print` / `str()` / `{…}` interpolation should: a struct that defines
    /// `str(self) -> str` (the `Stringable` protocol) dispatches to that method; everything else
    /// uses the default structural repr, recursing through `stringify` so a struct nested in a list
    /// / tuple / map / set / enum payload still honours the protocol. Mirrors `Value`'s `Display`
    /// for the non-dispatch cases (kept in lock-step with the VM's `stringify`, parity-tested).
    fn stringify(&mut self, v: &Value, span: Span, depth: usize) -> Result<String, RuntimeError> {
        // Contained structural-depth guard (Bug A): a cyclic data structure would otherwise recurse
        // unbounded on the host stack here and SIGABRT (uncatchable). Tripping this returns a
        // recoverable `RuntimeError`. The Stringable `str()` re-stringify below passes `depth`
        // unchanged (its own `enter_call` guard bounds that path); container recursions pass depth+1.
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(RuntimeError {
                message: "maximum structural depth (10000) exceeded (cyclic data structure?)"
                    .to_string(),
                span,
            });
        }
        match v {
            Value::Struct { name, fields } => {
                // `str(self) -> str` overrides the default repr. Only a self-only method is the hook
                // (a `str` taking extra args is an unrelated method).
                if let Some(def) = self.structs.get(name.as_ref()).cloned()
                    && let Some(decl) = def.methods.get("str").cloned()
                    && decl.params.len() == 1
                {
                    // Count the dispatch against the call-depth guard: `stringify` adds native frames
                    // per cycle on top of `call`'s, so a self-referential `str` must trip the soft
                    // limit before exhausting the host stack (parity with the VM's graceful error).
                    self.enter_call(span)?;
                    let res = self.call(&decl, &def.home, vec![v.clone()], span);
                    self.call_depth -= 1;
                    return self.stringify(&res?, span, depth);
                }
                let parts = fields.borrow().clone();
                let mut rendered = Vec::with_capacity(parts.len());
                for (k, fv) in &parts {
                    rendered.push(format!("{k}={}", self.stringify(fv, span, depth + 1)?));
                }
                // ROOT REDESIGN — `name` is the qualified IDENTITY KEY; render the bare display name.
                let display = crate::compiler::bare_display(name.as_ref());
                Ok(format!("{display}({})", rendered.join(", ")))
            }
            Value::List(items) => {
                let elems = items.borrow().clone();
                Ok(format!(
                    "[{}]",
                    self.stringify_seq(&elems, span, depth + 1)?
                ))
            }
            Value::Tuple(items) => {
                let elems = (**items).clone();
                Ok(format!(
                    "({})",
                    self.stringify_seq(&elems, span, depth + 1)?
                ))
            }
            Value::Map(m) => {
                let entries = m.borrow().entries.clone();
                let mut rendered = Vec::with_capacity(entries.len());
                for (_, k, mv) in &entries {
                    rendered.push(format!(
                        "{}: {}",
                        self.stringify(k, span, depth + 1)?,
                        self.stringify(mv, span, depth + 1)?
                    ));
                }
                Ok(format!("{{{}}}", rendered.join(", ")))
            }
            Value::Set(s) => {
                let entries = s.borrow().entries.clone();
                if entries.is_empty() {
                    Ok("set()".to_string())
                } else {
                    let elems: Vec<Value> = entries.into_iter().map(|(_, e)| e).collect();
                    Ok(format!(
                        "{{{}}}",
                        self.stringify_seq(&elems, span, depth + 1)?
                    ))
                }
            }
            Value::Enum {
                ty,
                variant,
                payload,
            } => {
                // `str(self) -> str` overrides the default `Variant(payload)` repr (Stringable).
                // Only a self-only method is the hook (mirrors the struct arm above).
                if let Some(def) = self.enum_defs.get(ty.as_ref()).cloned()
                    && let Some(decl) = def.methods.get("str").cloned()
                    && decl.params.len() == 1
                {
                    self.enter_call(span)?;
                    let res = self.call(&decl, &def.home, vec![v.clone()], span);
                    self.call_depth -= 1;
                    return self.stringify(&res?, span, depth);
                }
                if payload.is_empty() {
                    Ok(variant.to_string())
                } else {
                    Ok(format!(
                        "{variant}({})",
                        self.stringify_seq(payload, span, depth + 1)?
                    ))
                }
            }
            // A newtype honors a `str(self) -> str` override (Stringable) like enum/struct; else it
            // renders `Name(inner)` (recursing so an inner struct/enum honors its own override).
            Value::NewType { type_key, inner } => {
                if let Some(def) = self.newtype_defs.get(type_key.as_ref()).cloned()
                    && let Some(decl) = def.methods.get("str").cloned()
                    && decl.params.len() == 1
                {
                    self.enter_call(span)?;
                    let res = self.call(&decl, &def.home, vec![v.clone()], span);
                    self.call_depth -= 1;
                    return self.stringify(&res?, span, depth);
                }
                let display = crate::compiler::bare_display(type_key.as_ref());
                Ok(format!(
                    "{display}({})",
                    self.stringify(inner, span, depth + 1)?
                ))
            }
            // Scalars, functions, modules — no protocol dispatch; reuse `Display`.
            other => Ok(other.to_string()),
        }
    }

    /// `stringify` each element and join with `, ` (shared by list/tuple/set/enum-payload).
    fn stringify_seq(
        &mut self,
        elems: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<String, RuntimeError> {
        let mut rendered = Vec::with_capacity(elems.len());
        for e in elems {
            rendered.push(self.stringify(e, span, depth)?);
        }
        Ok(rendered.join(", "))
    }

    /// Operator overloading for ordering (`< <= > >=`) on two structs: dispatch to the receiver's
    /// `compare(self, other) -> int` method and map the sign of the result to a boolean. The checker
    /// guarantees the struct satisfies `Comparable`, so `compare` exists and returns int.
    fn struct_ordering(
        &mut self,
        op: BinaryOp,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let ord = self.struct_compare(l, r, span)?;
        Ok(Value::Bool(match op {
            BinaryOp::Lt => ord.is_lt(),
            BinaryOp::LtEq => ord.is_le(),
            BinaryOp::Gt => ord.is_gt(),
            BinaryOp::GtEq => ord.is_ge(),
            _ => unreachable!(),
        }))
    }

    /// Arithmetic operator overloading: dispatch `+`/`-`/`*` on two structs to the receiver's
    /// `add`/`sub`/`mul(self, other) -> Self` method (the `Add`/`Sub`/`Mul` protocols). The checker
    /// has verified conformance, so the method exists and returns the same struct type.
    fn struct_arith(
        &mut self,
        op: BinaryOp,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let method = match op {
            BinaryOp::Add => "add",
            BinaryOp::Sub => "sub",
            BinaryOp::Mul => "mul",
            _ => unreachable!("struct_arith only handles + - *"),
        };
        let (decl, home) = self.resolve_overload_method(&l, method, span)?;
        self.call(&decl, &home, vec![l, r], span)
    }

    /// Resolve `(method decl, home globals)` for an operator-overload method on receiver `recv` — a
    /// struct (via `structs`) or an enum (via `enum_defs`). The shared dispatch core for the
    /// arithmetic and ordering overloads on both struct and enum values.
    #[allow(clippy::type_complexity)]
    fn resolve_overload_method(
        &self,
        recv: &Value,
        method: &str,
        span: Span,
    ) -> Result<(std::rc::Rc<FnDecl>, value::ModEnv), RuntimeError> {
        match recv {
            Value::Struct { name, .. } => {
                let def = self
                    .structs
                    .get(name.as_ref())
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!("unknown struct type '{name}'"),
                        span,
                    })?;
                let decl = def
                    .methods
                    .get(method)
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!(
                            "struct '{}' has no '{method}' method",
                            crate::compiler::bare_display(name)
                        ),
                        span,
                    })?;
                Ok((decl, def.home.clone()))
            }
            Value::Enum { ty, .. } => {
                let def = self
                    .enum_defs
                    .get(ty.as_ref())
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!("unknown enum type '{ty}'"),
                        span,
                    })?;
                let decl = def
                    .methods
                    .get(method)
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!(
                            "enum '{}' has no '{method}' method",
                            crate::compiler::bare_display(ty)
                        ),
                        span,
                    })?;
                Ok((decl, def.home.clone()))
            }
            // A newtype's overload/hook methods (`hash`/`str`/user methods) resolve via
            // `newtype_defs`, mirroring the enum path.
            Value::NewType { type_key, .. } => {
                let def = self
                    .newtype_defs
                    .get(type_key.as_ref())
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!("unknown newtype '{type_key}'"),
                        span,
                    })?;
                let decl = def
                    .methods
                    .get(method)
                    .cloned()
                    .ok_or_else(|| RuntimeError {
                        message: format!(
                            "newtype '{}' has no '{method}' method",
                            crate::compiler::bare_display(type_key)
                        ),
                        span,
                    })?;
                Ok((decl, def.home.clone()))
            }
            _ => unreachable!("overload receiver is a struct, enum, or newtype"),
        }
    }

    /// Call a struct's `compare(self, other) -> int` method and return the resulting `Ordering`.
    /// Shared by ordering operators (`struct_ordering`) and `list.sort()` over Comparable structs.
    fn struct_compare(
        &mut self,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        let (decl, home) = self.resolve_overload_method(&l, "compare", span)?;
        match self.call(&decl, &home, vec![l, r], span)? {
            Value::Int(n) => Ok(n.cmp(&0)),
            other => Err(RuntimeError {
                message: format!("compare() must return int, got {}", other.type_name()),
                span,
            }),
        }
    }

    /// A `u64` hash of `v` for map/set keys, upholding `values_equal(a,b) ⇒ hash(a)==hash(b)`.
    /// Numeric keys hash by canonical f64 bits (so `3` and `3.0` collide); str by content; a struct
    /// key dispatches its user `hash(self) -> int` (re-entrant via `self.call`, but the Rc heap
    /// never moves, so no rooting is needed — unlike the VM). Floats are rejected as keys by the
    /// checker, so only integral-valued floats reach here.
    fn hash_value(&mut self, v: &Value, span: Span) -> Result<u64, RuntimeError> {
        match v {
            Value::Struct { .. } => self.struct_hash(v, span),
            // An enum key dispatches its user `hash(self) -> int` via the shared enum-aware
            // resolver, mirroring the struct path.
            Value::Enum { .. } => self.enum_hash(v, span),
            // A newtype key dispatches its user `hash(self) -> int` (opt-in — the checker rejects a
            // newtype with no `hash` as a key, even over an intrinsically-hashable underlying).
            Value::NewType { .. } => self.newtype_hash(v, span),
            Value::Str(_)
            | Value::Bytes(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Bool(_)
            | Value::Nil => Ok(scalar_hash(v)),
            other => Err(RuntimeError {
                message: format!(
                    "{} is not hashable (cannot be a map/set key)",
                    other.type_name()
                ),
                span,
            }),
        }
    }

    /// Dispatch an enum key's user `hash(self) -> int` via the shared enum-aware
    /// [`resolve_overload_method`], mirroring [`struct_hash`].
    fn enum_hash(&mut self, v: &Value, span: Span) -> Result<u64, RuntimeError> {
        let (decl, home) = self.resolve_overload_method(v, "hash", span)?;
        match self.call(&decl, &home, vec![v.clone()], span)? {
            Value::Int(n) => Ok(n as u64),
            other => Err(RuntimeError {
                message: format!("hash() must return int, got {}", other.type_name()),
                span,
            }),
        }
    }

    /// Dispatch a newtype key's user `hash(self) -> int` via the shared resolver (mirrors `enum_hash`).
    fn newtype_hash(&mut self, v: &Value, span: Span) -> Result<u64, RuntimeError> {
        let (decl, home) = self.resolve_overload_method(v, "hash", span)?;
        match self.call(&decl, &home, vec![v.clone()], span)? {
            Value::Int(n) => Ok(n as u64),
            other => Err(RuntimeError {
                message: format!("hash() must return int, got {}", other.type_name()),
                span,
            }),
        }
    }

    /// Dispatch a struct key's user `hash(self) -> int`, returning its `i64` as a `u64`. Mirrors
    /// [`struct_compare`].
    fn struct_hash(&mut self, v: &Value, span: Span) -> Result<u64, RuntimeError> {
        let Value::Struct { name, .. } = v else {
            unreachable!()
        };
        let def = self
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| RuntimeError {
                message: format!("unknown struct type '{name}'"),
                span,
            })?;
        let decl = def
            .methods
            .get("hash")
            .cloned()
            .ok_or_else(|| RuntimeError {
                message: format!(
                    "struct '{}' has no 'hash' method (needed to use it as a map/set key)",
                    crate::compiler::bare_display(name)
                ),
                span,
            })?;
        match self.call(&decl, &def.home, vec![v.clone()], span)? {
            Value::Int(n) => Ok(n as u64),
            other => Err(RuntimeError {
                message: format!("hash() must return int, got {}", other.type_name()),
                span,
            }),
        }
    }

    /// `xs.sort()` over a list of Comparable structs: a stable merge sort that orders elements via
    /// each struct's `compare` method. (Primitive lists use the faster `builtins::value_order`.)
    fn eval_list_sort(
        &mut self,
        list: std::rc::Rc<std::cell::RefCell<Vec<Value>>>,
        elems: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let sorted = self.merge_sort_structs(elems, span)?;
        *list.borrow_mut() = sorted;
        Ok(Value::Nil)
    }

    fn merge_sort_structs(
        &mut self,
        mut xs: Vec<Value>,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        let n = xs.len();
        if n <= 1 {
            return Ok(xs);
        }
        let right = xs.split_off(n / 2);
        let left = self.merge_sort_structs(xs, span)?;
        let right = self.merge_sort_structs(right, span)?;
        let mut out = Vec::with_capacity(n);
        let mut li = left.into_iter().peekable();
        let mut ri = right.into_iter().peekable();
        loop {
            match (li.peek(), ri.peek()) {
                (Some(l), Some(r)) => {
                    // `<= Equal` keeps left first on ties → stable.
                    let ord = self.struct_compare(l.clone(), r.clone(), span)?;
                    if ord.is_le() {
                        out.push(li.next().unwrap());
                    } else {
                        out.push(ri.next().unwrap());
                    }
                }
                (Some(_), None) => out.push(li.next().unwrap()),
                (None, Some(_)) => out.push(ri.next().unwrap()),
                (None, None) => break,
            }
        }
        Ok(out)
    }

    /// Evaluate the higher-order list methods `map` / `filter` / `fold`. `elems` is the receiver
    /// list's elements (already cloned out so no `RefCell` borrow is held across `call_value`).
    fn eval_list_hof(
        &mut self,
        method: &str,
        elems: Vec<Value>,
        mut arg_vals: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "map" => {
                if arg_vals.len() != 1 {
                    return Err(RuntimeError {
                        message: format!("'map' expects 1 argument(s), got {}", arg_vals.len()),
                        span,
                    });
                }
                let f = arg_vals.swap_remove(0);
                let mut out = Vec::with_capacity(elems.len());
                for elem in elems {
                    out.push(self.call_value(f.clone(), vec![elem], span)?);
                }
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            "filter" => {
                if arg_vals.len() != 1 {
                    return Err(RuntimeError {
                        message: format!("'filter' expects 1 argument(s), got {}", arg_vals.len()),
                        span,
                    });
                }
                let p = arg_vals.swap_remove(0);
                let mut out = Vec::new();
                for elem in elems {
                    let keep = self.call_value(p.clone(), vec![elem.clone()], span)?;
                    match keep {
                        Value::Bool(true) => out.push(elem),
                        Value::Bool(false) => {}
                        other => {
                            return Err(RuntimeError {
                                message: format!(
                                    "filter predicate must return bool, got {}",
                                    other.type_name()
                                ),
                                span,
                            });
                        }
                    }
                }
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            "fold" => {
                if arg_vals.len() != 2 {
                    return Err(RuntimeError {
                        message: format!("'fold' expects 2 argument(s), got {}", arg_vals.len()),
                        span,
                    });
                }
                let f = arg_vals.swap_remove(1);
                let mut acc = arg_vals.swap_remove(0);
                for elem in elems {
                    acc = self.call_value(f.clone(), vec![acc, elem], span)?;
                }
                Ok(acc)
            }
            _ => unreachable!("eval_list_hof called with non-HOF method {method}"),
        }
    }

    /// `set()` → empty set; `set(it)` → a deduped hash set drained from ANY for-iterable
    /// (list/set/str/bytes/bytearray/map-keys/range/user-iterator). On `Interp` (not `builtins`)
    /// because a struct element's `hash()` and a user iterator's `next()` re-enter the engine.
    /// Mirrors `vm::Vm::builtin_set`.
    fn builtin_set(&mut self, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let src: Vec<Value> = match args.into_iter().next() {
            // The checker guarantees ≤1 arg; `set()` (0 args) is the empty set.
            None => Vec::new(),
            // Drain any for-iterable into single-element rows, then flatten to elements.
            Some(it) => self
                .drain_value_to_rows(it, 1, span)?
                .into_iter()
                .flatten()
                .collect(),
        };
        let mut set = SetData::default();
        for v in src {
            let hv = self.hash_value(&v, span)?;
            if !set
                .candidates(hv)
                .iter()
                .copied()
                .any(|p| values_equal(&set.entries[p].1, &v))
            {
                set.push(hv, v);
            }
        }
        Ok(Value::Set(std::rc::Rc::new(std::cell::RefCell::new(set))))
    }

    /// `list(it)` → a list drained from ANY for-iterable. On `Interp` because a user iterator's
    /// `next()` re-enters the engine. Mirrors `vm::Vm::builtin_list`.
    fn builtin_list(&mut self, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let it = args.into_iter().next().ok_or_else(|| RuntimeError {
            message: "list() takes exactly one iterable argument — use [] for an empty list"
                .to_string(),
            span,
        })?;
        let items: Vec<Value> = self
            .drain_value_to_rows(it, 1, span)?
            .into_iter()
            .flatten()
            .collect();
        Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(
            items,
        ))))
    }

    /// `map(it)` → a map from an iterable of 2-tuples `(k, v)` (last-wins on duplicate keys, like the
    /// `{k: v}` literal). On `Interp` because a struct key's `hash()` and a user iterator's `next()`
    /// re-enter the engine. Mirrors `vm::Vm::builtin_map`.
    fn builtin_map(&mut self, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let it = args.into_iter().next().ok_or_else(|| RuntimeError {
            message: "map() takes exactly one iterable argument — use {} for an empty map"
                .to_string(),
            span,
        })?;
        let rows = self.drain_value_to_rows(it, 1, span)?;
        let mut out = MapData::default();
        for row in rows {
            // Each drained element must be a 2-tuple (the checker enforces this statically; this guards
            // the `Unknown`-typed escape so a bad runtime shape faults recoverably, not panics).
            let (k, v) = match row.into_iter().next() {
                Some(Value::Tuple(parts)) if parts.len() == 2 => {
                    (parts[0].clone(), parts[1].clone())
                }
                Some(other) => {
                    return Err(RuntimeError {
                        message: format!(
                            "map() expects an iterable of (key, value) 2-tuples, got {}",
                            other.type_name()
                        ),
                        span,
                    });
                }
                None => unreachable!("drain_value_to_rows yields non-empty single-var rows"),
            };
            let hk = self.hash_value(&k, span)?;
            map_upsert(&mut out, hk, k, v);
        }
        Ok(Value::Map(std::rc::Rc::new(std::cell::RefCell::new(out))))
    }

    /// Collect raw bytes from a byte-sequence-shaped argument (a `bytes`, a `bytearray`, or a
    /// `list[int]` with each element 0..=255). Mirrors the VM's `collect_bytes_arg`. `what` names the
    /// constructor/method in error messages.
    fn collect_bytes_arg(
        &self,
        what: &str,
        v: &Value,
        span: Span,
    ) -> Result<Vec<u8>, RuntimeError> {
        match v {
            Value::Bytes(b) => Ok(b.to_vec()),
            Value::ByteArray(b) => Ok(b.borrow().clone()),
            Value::List(items) => {
                let mut out = Vec::with_capacity(items.borrow().len());
                for e in items.borrow().iter() {
                    match e {
                        Value::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                        Value::Int(n) => {
                            return Err(RuntimeError {
                                message: format!(
                                    "{what}() list element {n} out of range (must be 0..=255)"
                                ),
                                span,
                            });
                        }
                        other => {
                            return Err(RuntimeError {
                                message: format!(
                                    "{what}() expects a list of int, got an element of type {}",
                                    other.type_name()
                                ),
                                span,
                            });
                        }
                    }
                }
                Ok(out)
            }
            other => Err(RuntimeError {
                message: format!(
                    "{what}() expects a bytes, a bytearray, or a list[int], got {}",
                    other.type_name()
                ),
                span,
            }),
        }
    }

    /// `bytearray()` → empty; `bytearray(N)` → N zero bytes; `bytearray(b)`/`bytearray(ba)` → mutable
    /// copy; `bytearray([ints])` → from a list of ints (each 0..=255). Mirrors the VM's
    /// `builtin_bytearray` — the MUTABLE buffer (`Rc<RefCell<Vec<u8>>>`).
    fn builtin_bytearray(&mut self, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let bytes: Vec<u8> = match args.as_slice() {
            [] => Vec::new(),
            [Value::Int(n)] => {
                if *n < 0 {
                    return Err(RuntimeError {
                        message: format!("bytearray() size {n} must be non-negative"),
                        span,
                    });
                }
                // Bound the eager zero-fill: an unguarded `vec![0u8; n]` for a huge n aborts the
                // process (SIGABRT), uncatchable by `recover:`. `try_reserve` turns OOM into a
                // recoverable fault, matching the VM + range()/format-width "never a giant abort" rule.
                let n = *n as usize;
                let mut buf: Vec<u8> = Vec::new();
                if buf.try_reserve_exact(n).is_err() {
                    return Err(RuntimeError {
                        message: format!("bytearray() size {n} is too large to allocate"),
                        span,
                    });
                }
                buf.resize(n, 0u8);
                buf
            }
            [one] => self.collect_bytes_arg("bytearray", one, span)?,
            _ => {
                return Err(RuntimeError {
                    message: format!("bytearray() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                });
            }
        };
        Ok(Value::ByteArray(std::rc::Rc::new(std::cell::RefCell::new(
            bytes,
        ))))
    }

    /// `bytes(b)` → copy; `bytes(ba)` → immutable snapshot of a `bytearray`; `bytes([ints])` → from a
    /// list of ints. The conversion bridge to the IMMUTABLE form. Mirrors the VM's `builtin_bytes`.
    fn builtin_bytes(&mut self, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let bytes: Vec<u8> = match args.as_slice() {
            [one] => self.collect_bytes_arg("bytes", one, span)?,
            _ => {
                return Err(RuntimeError {
                    message: format!("bytes() expects 1 argument, got {}", args.len()),
                    span,
                });
            }
        };
        Ok(Value::Bytes(bytes.into()))
    }

    /// Built-in methods on a `bytearray`: `len`, `push(int 0..=255)`, `pop() -> Option[int]`,
    /// `extend(bytes|bytearray|list[int])`. Mirrors the VM's `bytearray_method` and the checker's
    /// `bytearray_method_sig`. Mutators write IN PLACE via the shared `Rc<RefCell>` (`borrow_mut`).
    fn eval_bytearray_method(
        &mut self,
        buf: &std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "len" => {
                builtins::arity("len", &args, 0, span)?;
                Ok(Value::Int(buf.borrow().len() as i64))
            }
            "push" => {
                builtins::arity("push", &args, 1, span)?;
                let byte = match args[0] {
                    Value::Int(n) if (0..=255).contains(&n) => n as u8,
                    Value::Int(n) => {
                        return Err(RuntimeError {
                            message: format!("byte value {n} out of range (must be 0..=255)"),
                            span,
                        });
                    }
                    ref other => {
                        return Err(RuntimeError {
                            message: format!("push() expects an int, got {}", other.type_name()),
                            span,
                        });
                    }
                };
                buf.borrow_mut().push(byte);
                Ok(Value::Nil)
            }
            "pop" => {
                builtins::arity("pop", &args, 0, span)?;
                let popped = buf.borrow_mut().pop();
                Ok(match popped {
                    Some(x) => enum_val("Option", "Some", vec![Value::Int(x as i64)]),
                    None => enum_val("Option", "None", vec![]),
                })
            }
            "extend" => {
                builtins::arity("extend", &args, 1, span)?;
                // Snapshot/validate the other side first (so `ba.extend(ba)` terminates).
                let appended = self.collect_bytes_arg("extend", &args[0], span)?;
                buf.borrow_mut().extend_from_slice(&appended);
                Ok(Value::Nil)
            }
            // `decode() -> str`: UTF-8 decode the current buffer. Invalid UTF-8 is a RECOVERABLE
            // fault (catchable by `recover:`), never a panic — mirrors the bytes path + the VM.
            "decode" => {
                builtins::arity("decode", &args, 0, span)?;
                let bytes = buf.borrow().clone();
                decode_utf8(&bytes, span)
            }
            _ => Err(RuntimeError {
                message: format!("type bytearray has no method '{method}'"),
                span,
            }),
        }
    }

    /// Built-in methods on `bytes` (immutable byte sequence): only `decode() -> str` (UTF-8). Mirrors
    /// the VM's `bytes_method` and the checker's `bytes_method_sig`.
    fn eval_bytes_method(
        &mut self,
        b: &std::rc::Rc<[u8]>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "decode" => {
                builtins::arity("decode", &args, 0, span)?;
                decode_utf8(b, span)
            }
            _ => Err(RuntimeError {
                message: format!("type bytes has no method '{method}'"),
                span,
            }),
        }
    }

    /// Built-in methods on `map[K, V]`. Mirrors the VM's `core_method` Map arm and the checker's
    /// `map_method_sig` (keep the three in lockstep, error strings included). Lives on `Interp` (not
    /// the pure `builtins` table) because a struct key's `hash()` re-enters the engine. `get`/`remove`
    /// return `Option[V]`.
    fn eval_map_method(
        &mut self,
        m: &std::rc::Rc<std::cell::RefCell<MapData>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let some = |v: Value| Value::Enum {
            ty: "Option".into(),
            variant: "Some".into(),
            payload: vec![v],
        };
        let none = || Value::Enum {
            ty: "Option".into(),
            variant: "None".into(),
            payload: vec![],
        };
        match method {
            "len" => {
                builtins::arity("len", &args, 0, span)?;
                Ok(Value::Int(m.borrow().entries.len() as i64))
            }
            "has" => {
                builtins::arity("has", &args, 1, span)?;
                let hk = self.hash_value(&args[0], span)?; // hash before borrowing (re-entrant)
                let mm = m.borrow();
                Ok(Value::Bool(
                    mm.candidates(hk)
                        .iter()
                        .any(|&p| values_equal(&mm.entries[p].1, &args[0])),
                ))
            }
            "get" => {
                builtins::arity("get", &args, 1, span)?;
                let hk = self.hash_value(&args[0], span)?;
                let found = {
                    let mm = m.borrow();
                    mm.candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| values_equal(&mm.entries[p].1, &args[0]))
                        .map(|p| mm.entries[p].2.clone())
                };
                Ok(found.map(some).unwrap_or_else(none))
            }
            "keys" => {
                builtins::arity("keys", &args, 0, span)?;
                let keys: Vec<Value> = m
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, k, _)| k.clone())
                    .collect();
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(keys))))
            }
            "values" => {
                builtins::arity("values", &args, 0, span)?;
                let vals: Vec<Value> = m
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, _, v)| v.clone())
                    .collect();
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(vals))))
            }
            "remove" => {
                builtins::arity("remove", &args, 1, span)?;
                let hk = self.hash_value(&args[0], span)?;
                let pos = {
                    let mm = m.borrow();
                    mm.candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| values_equal(&mm.entries[p].1, &args[0]))
                };
                match pos {
                    Some(i) => {
                        let (_, _, v) = m.borrow_mut().remove_at(i);
                        Ok(some(v))
                    }
                    None => Ok(none()),
                }
            }
            "merge" | "update" => {
                builtins::arity(method, &args, 1, span)?;
                let other = match &args[0] {
                    Value::Map(o) => o,
                    other => {
                        return Err(RuntimeError {
                            message: format!(
                                "{method}() expects a map argument, got {}",
                                other.type_name()
                            ),
                            span,
                        });
                    }
                };
                // Snapshot the incoming entries first so `m.merge(m)` / `m.update(m)` terminate.
                let incoming: Vec<(u64, Value, Value)> = other.borrow().entries.clone();
                if method == "merge" {
                    let mut out = m.borrow().clone();
                    for (h, k, v) in incoming {
                        map_upsert(&mut out, h, k, v);
                    }
                    Ok(Value::Map(std::rc::Rc::new(std::cell::RefCell::new(out))))
                } else {
                    let mut mm = m.borrow_mut();
                    for (h, k, v) in incoming {
                        map_upsert(&mut mm, h, k, v);
                    }
                    Ok(Value::Nil)
                }
            }
            _ => Err(RuntimeError {
                message: format!("type map has no method '{method}'"),
                span,
            }),
        }
    }

    /// Built-in methods on `set[T]`. Mirrors the VM's `core_method` Set arm and the checker's
    /// `set_method_sig`. On `Interp` (not `builtins`) for struct-key `hash()` access. Set algebra
    /// reuses each operand's per-element cached hash, so it never re-enters the engine.
    fn eval_set_method(
        &mut self,
        s: &std::rc::Rc<std::cell::RefCell<SetData>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "len" => {
                builtins::arity("len", &args, 0, span)?;
                Ok(Value::Int(s.borrow().entries.len() as i64))
            }
            "has" => {
                builtins::arity("has", &args, 1, span)?;
                let hx = self.hash_value(&args[0], span)?;
                let ss = s.borrow();
                Ok(Value::Bool(
                    ss.candidates(hx)
                        .iter()
                        .any(|&p| values_equal(&ss.entries[p].1, &args[0])),
                ))
            }
            "add" => {
                builtins::arity("add", &args, 1, span)?;
                let hx = self.hash_value(&args[0], span)?;
                let present = {
                    let ss = s.borrow();
                    ss.candidates(hx)
                        .iter()
                        .any(|&p| values_equal(&ss.entries[p].1, &args[0]))
                };
                if !present {
                    s.borrow_mut().push(hx, args[0].clone());
                }
                Ok(Value::Nil)
            }
            "remove" => {
                builtins::arity("remove", &args, 1, span)?;
                let hx = self.hash_value(&args[0], span)?;
                let pos = {
                    let ss = s.borrow();
                    ss.candidates(hx)
                        .iter()
                        .copied()
                        .find(|&p| values_equal(&ss.entries[p].1, &args[0]))
                };
                match pos {
                    Some(i) => {
                        s.borrow_mut().remove_at(i);
                        Ok(Value::Bool(true))
                    }
                    None => Ok(Value::Bool(false)),
                }
            }
            "union" | "intersection" | "difference" => {
                builtins::arity(method, &args, 1, span)?;
                let Value::Set(other) = &args[0] else {
                    return Err(RuntimeError {
                        message: format!(
                            "{method}() expects a set argument, got {}",
                            args[0].type_name()
                        ),
                        span,
                    });
                };
                // Both operands carry per-element cached hashes — no re-hashing, no re-entry.
                let mine = s.borrow().entries.clone();
                let other = other.borrow().clone();
                let mut out = SetData::default();
                let add = |out: &mut SetData, he: u64, e: &Value| {
                    if !out
                        .candidates(he)
                        .iter()
                        .any(|&p| values_equal(&out.entries[p].1, e))
                    {
                        out.push(he, e.clone());
                    }
                };
                match method {
                    "union" => {
                        for (he, e) in mine.iter().chain(other.entries.iter()) {
                            add(&mut out, *he, e);
                        }
                    }
                    m => {
                        let keep_when_present = m == "intersection";
                        for (he, e) in &mine {
                            let in_other = other
                                .candidates(*he)
                                .iter()
                                .any(|&p| values_equal(&other.entries[p].1, e));
                            if in_other == keep_when_present {
                                add(&mut out, *he, e);
                            }
                        }
                    }
                }
                Ok(Value::Set(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            _ => Err(RuntimeError {
                message: format!("type set has no method '{method}'"),
                span,
            }),
        }
    }

    /// `Channel[T]` methods (C2). The channel is a buffered, unbounded FIFO under the sequential
    /// executor: `send` never blocks; `recv` on an empty channel is a deadlock-detect fault (all
    /// tasks have finished and nothing is queued), *not* a hang — that preserves the C5 blocking
    /// surface. Values move across the airlock on `send` (deep-copied in) so the queue never shares
    /// mutable state with the sender.
    fn eval_channel_method(
        &mut self,
        q: &std::rc::Rc<std::cell::RefCell<value::ChanState>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "send" => {
                builtins::arity("send", &args, 1, span)?;
                let mut s = q.borrow_mut();
                if s.closed {
                    return Err(RuntimeError {
                        message: "send on a closed channel".to_string(),
                        span,
                    });
                }
                s.queue.push_back(deep_clone(&args[0]));
                Ok(Value::Nil)
            }
            // `try_send` is the safe partner of `send`: channels are unbounded, so its only failure is
            // a closed channel — returns `false` then (never faults), `true` once the value is queued.
            "try_send" => {
                builtins::arity("try_send", &args, 1, span)?;
                let mut s = q.borrow_mut();
                if s.closed {
                    return Ok(Value::Bool(false));
                }
                s.queue.push_back(deep_clone(&args[0]));
                Ok(Value::Bool(true))
            }
            "recv" => {
                builtins::arity("recv", &args, 0, span)?;
                let (popped, closed, timer, latch) = {
                    let mut s = q.borrow_mut();
                    (s.queue.pop_front(), s.closed, s.timer, s.done_latch)
                };
                if let Some(v) = popped {
                    return Ok(v);
                }
                // A tripped latch (`trip()`) delivers `true` immediately and forever.
                if latch {
                    return Ok(Value::Bool(true));
                }
                // A `timer(ms)` channel synthesises its one-shot `true` once the deadline passes:
                // inline-sleep to it (single-threaded interp), then yield. Checked before the closed /
                // deadlock arms — a timer channel is never closed and always eventually ready.
                if let Some(deadline) = timer {
                    let now = std::time::Instant::now();
                    if now < deadline {
                        std::thread::sleep(deadline - now);
                    }
                    return Ok(Value::Bool(true));
                }
                // Drain-before-done: only consult `closed` on an empty queue. A closed-and-empty
                // channel faults distinctly (not the deadlock fault) — there is no producer left.
                if closed {
                    return Err(RuntimeError {
                        message: "receive on a closed channel".to_string(),
                        span,
                    });
                }
                Err(RuntimeError {
                    message: "recv on an empty channel: deadlock — nothing is queued and the \
                              sequential executor cannot block waiting for a producer (a \
                              consumer that waits mid-flight on a live producer needs C5)"
                        .to_string(),
                    span,
                })
            }
            "try_recv" => {
                // A1: non-blocking poll. `Some(v)` if queued, `None` if empty — never the deadlock
                // fault `recv` raises (and never suspends; the VM twin mirrors this exactly). A closed
                // channel is indistinguishable here from an empty one (`None`) — by design.
                builtins::arity("try_recv", &args, 0, span)?;
                let popped = {
                    let mut s = q.borrow_mut();
                    // A `timer(ms)` channel reports ready (`Some(true)`) once its deadline passes, even
                    // with nothing queued — the level-triggered, non-blocking poll.
                    s.queue.pop_front().or_else(|| {
                        if s.done_latch {
                            return Some(Value::Bool(true));
                        }
                        s.timer
                            .filter(|d| std::time::Instant::now() >= *d)
                            .map(|_| Value::Bool(true))
                    })
                };
                Ok(match popped {
                    Some(v) => Value::Enum {
                        ty: "Option".into(),
                        variant: "Some".into(),
                        payload: vec![v],
                    },
                    None => Value::Enum {
                        ty: "Option".into(),
                        variant: "None".into(),
                        payload: vec![],
                    },
                })
            }
            // `close()` marks the channel closed (idempotent). In the sequential oracle there are no
            // parked receivers to wake — a later `recv`/`for` observes `closed` directly.
            "close" => {
                builtins::arity("close", &args, 0, span)?;
                q.borrow_mut().closed = true;
                Ok(Value::Nil)
            }
            // `trip()` flips the manual level-trigger latch: the channel is then permanently ready
            // (`recv`/`try_recv`/`wait` yield `true`). Idempotent. In the sequential oracle there are no
            // parked receivers to wake — a later `recv`/`wait` observes the latch directly.
            "trip" => {
                builtins::arity("trip", &args, 0, span)?;
                q.borrow_mut().done_latch = true;
                Ok(Value::Nil)
            }
            "len" => {
                builtins::arity("len", &args, 0, span)?;
                Ok(Value::Int(q.borrow().queue.len() as i64))
            }
            _ => Err(RuntimeError {
                message: format!("type Channel has no method '{method}'"),
                span,
            }),
        }
    }

    /// `Shared[T]` methods (C3). The box owns its value; `get` copies it out and `set` copies the
    /// new value in (so neither aliases the box's interior — the shared-nothing airlock). `update`
    /// is read-modify-write: it reads the current value out, drops the borrow, runs the user
    /// function (which may itself touch this box), then stores the result. Under the sequential
    /// executor a single thread serialises every write, so no lock is needed.
    fn eval_shared_method(
        &mut self,
        cell: &std::rc::Rc<std::cell::RefCell<Value>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                builtins::arity("get", &args, 0, span)?;
                let cur = cell.borrow();
                Ok(deep_clone(&cur))
            }
            "set" => {
                builtins::arity("set", &args, 1, span)?;
                *cell.borrow_mut() = deep_clone(&args[0]);
                Ok(Value::Nil)
            }
            "update" => {
                builtins::arity("update", &args, 1, span)?;
                let f = args.into_iter().next().expect("arity checked 1 arg");
                // Read the current value out and release the borrow *before* calling `f`: the
                // closure may re-enter this same box (`get`/`set`), which would panic on a held
                // `RefCell` borrow.
                let cur = {
                    let g = cell.borrow();
                    deep_clone(&g)
                };
                let next = self.call_value(f, vec![cur], span)?;
                *cell.borrow_mut() = deep_clone(&next);
                Ok(Value::Nil)
            }
            _ => Err(RuntimeError {
                message: format!("type Shared has no method '{method}'"),
                span,
            }),
        }
    }

    /// `Atomic[T]` methods. The box owns its value (copied in/out across the airlock, like `Shared`):
    /// `load` copies out, `store` copies in, `exchange` swaps (returns the old value), `cas(expected,
    /// new)` swaps iff the box equals `expected` (returns whether it did), and `add`/`sub` are numeric
    /// read-modify-write returning the new value. Under the sequential executor a single thread
    /// serialises every op, so no locking is needed. Mirrors `vm::atomic_method`.
    fn eval_atomic_method(
        &mut self,
        cell: &std::rc::Rc<std::cell::RefCell<Value>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "load" => {
                builtins::arity("load", &args, 0, span)?;
                Ok(deep_clone(&cell.borrow()))
            }
            "store" => {
                builtins::arity("store", &args, 1, span)?;
                *cell.borrow_mut() = deep_clone(&args[0]);
                Ok(Value::Nil)
            }
            "exchange" => {
                builtins::arity("exchange", &args, 1, span)?;
                let new = deep_clone(&args[0]);
                Ok(std::mem::replace(&mut *cell.borrow_mut(), new))
            }
            "cas" => {
                builtins::arity("cas", &args, 2, span)?;
                let mut g = cell.borrow_mut();
                let swapped = values_equal(&g, &args[0]);
                if swapped {
                    *g = deep_clone(&args[1]);
                }
                Ok(Value::Bool(swapped))
            }
            "add" | "sub" => {
                builtins::arity(method, &args, 1, span)?;
                let mut g = cell.borrow_mut();
                let new = match (&*g, &args[0]) {
                    (Value::Int(a), Value::Int(b)) => {
                        let (r, label) = if method == "add" {
                            (a.checked_add(*b), "Add")
                        } else {
                            (a.checked_sub(*b), "Sub")
                        };
                        Value::Int(r.ok_or(RuntimeError {
                            message: format!("integer overflow in {label}"),
                            span,
                        })?)
                    }
                    (Value::Float(a), Value::Float(b)) => {
                        Value::Float(if method == "add" { a + b } else { a - b })
                    }
                    // The checker gates `add`/`sub` to numeric element types, so this is unreachable.
                    _ => {
                        return Err(RuntimeError {
                            message: format!("type Atomic has no method '{method}'"),
                            span,
                        });
                    }
                };
                *g = new.clone();
                Ok(new)
            }
            _ => Err(RuntimeError {
                message: format!("type Atomic has no method '{method}'"),
                span,
            }),
        }
    }

    /// `Executor` methods (C5 escape hatch). `submit(f)` enqueues a detached zero-arg task closure
    /// (rejected once shut down); `shutdown()` is graceful — it takes the queue out (so a task that
    /// re-submits during the drain hits the shut gate), then runs each task FIFO to completion via
    /// the re-entrant call path, first error aborting the rest (mirrors the nursery); `shutdown_now()`
    /// discards pending work. Under the sequential executor submitted work runs at the reap point.
    fn eval_executor_method(
        &mut self,
        ex: &std::rc::Rc<std::cell::RefCell<value::ExecState>>,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "submit" => {
                builtins::arity("submit", &args, 1, span)?;
                let mut st = ex.borrow_mut();
                if st.shut {
                    return Err(RuntimeError {
                        message: "submit on a shut-down Executor (it no longer accepts work)"
                            .to_string(),
                        span,
                    });
                }
                let f = args.into_iter().next().expect("arity checked 1 arg");
                st.queue.push_back(f);
                Ok(Value::Nil)
            }
            "shutdown" => {
                builtins::arity("shutdown", &args, 0, span)?;
                // Mark shut, then drain the *live* queue one task at a time, popping under a tight
                // borrow and releasing it before `call_value` — a drained task may re-enter this
                // executor (it must see `shut`, and `call_value` would panic on a held `RefCell`
                // borrow). Popping from the live queue (not a taken snapshot) means a task that
                // faults leaves the not-yet-run siblings in place, and a re-entrant
                // `shutdown`/`shutdown_now` observes the same queue — matching the VM exactly.
                loop {
                    let task = {
                        let mut st = ex.borrow_mut();
                        st.shut = true;
                        st.queue.pop_front()
                    };
                    let Some(task) = task else { break };
                    self.call_value(task, Vec::new(), span)?;
                }
                Ok(Value::Nil)
            }
            "shutdown_now" => {
                builtins::arity("shutdown_now", &args, 0, span)?;
                let mut st = ex.borrow_mut();
                st.shut = true;
                st.queue.clear();
                Ok(Value::Nil)
            }
            _ => Err(RuntimeError {
                message: format!("type Executor has no method '{method}'"),
                span,
            }),
        }
    }

    /// At a clean program end, gracefully drain every `Executor` that was created but never
    /// explicitly `shutdown`/`shutdown_now`-ed — mirrors a top-level `defer ex.shutdown()`, so the
    /// submitted work runs instead of silently vanishing (C5 / A2). Executors drain in creation
    /// order; each reuses the shipped `shutdown` path (FIFO, first-fault-aborts-siblings). A hard
    /// `std.os.exit` is *not* drained (it skips `defer` too) — the caller gates on `pending_exit`,
    /// and a task that calls `os.exit` mid-drain stops the remaining drain here.
    fn drain_live_executors(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            return Ok(());
        }
        // Snapshot the handles: a drained task may itself create new executors; reap only those
        // alive at exit (a newly-created one is the caller's to reap, like nested `defer`).
        let execs: Vec<_> = self.executors.clone();
        for ex in execs {
            if ex.borrow().shut {
                continue;
            }
            self.eval_executor_method(&ex, "shutdown", Vec::new(), span)?;
            if self.pending_exit.is_some() {
                break; // a drained task called os.exit — hard halt, stop draining
            }
        }
        Ok(())
    }

    /// `xs.sort_by(cmp)` — sort `xs` in place using a Chezzi comparator `fn(T, T) -> int`
    /// (negative = a before b, positive = a after b, zero = equal). Uses a stable top-down merge
    /// sort: the comparator is fallible (re-enters the interpreter) so `slice::sort_by`, which
    /// demands an infallible total order, can't be used. Returns `nil`.
    fn eval_list_sort_by(
        &mut self,
        list: std::rc::Rc<std::cell::RefCell<Vec<Value>>>,
        elems: Vec<Value>,
        mut arg_vals: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if arg_vals.len() != 1 {
            return Err(RuntimeError {
                message: format!("'sort_by' expects 1 argument(s), got {}", arg_vals.len()),
                span,
            });
        }
        let cmp = arg_vals.swap_remove(0);
        let sorted = self.merge_sort_by(elems, &cmp, span)?;
        *list.borrow_mut() = sorted;
        Ok(Value::Nil)
    }

    /// `xs.sort_by_key(f)` — sort `xs` in place by a derived key `f: fn(T) -> K`. The key extractor
    /// is called once per element (re-entrant); keys are compared by their natural order (`order_key`:
    /// scalar ordering, or a Comparable struct key's `compare`). Stable. Returns `nil`.
    fn eval_list_sort_by_key(
        &mut self,
        list: std::rc::Rc<std::cell::RefCell<Vec<Value>>>,
        elems: Vec<Value>,
        mut arg_vals: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if arg_vals.len() != 1 {
            return Err(RuntimeError {
                message: format!(
                    "'sort_by_key' expects 1 argument(s), got {}",
                    arg_vals.len()
                ),
                span,
            });
        }
        let f = arg_vals.swap_remove(0);
        // Precompute keys once per element (Python `sorted(key=…)` / Rust `sort_by_key`).
        let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(elems.len());
        for e in elems {
            let k = self.call_value(f.clone(), vec![e.clone()], span)?;
            pairs.push((k, e));
        }
        let sorted = self.merge_sort_by_key(pairs, span)?;
        *list.borrow_mut() = sorted.into_iter().map(|(_, e)| e).collect();
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `(key, value)` pairs, ordering by `key` via [`order_key`].
    fn merge_sort_by_key(
        &mut self,
        mut xs: Vec<(Value, Value)>,
        span: Span,
    ) -> Result<Vec<(Value, Value)>, RuntimeError> {
        let n = xs.len();
        if n <= 1 {
            return Ok(xs);
        }
        let right = xs.split_off(n / 2);
        let left = self.merge_sort_by_key(xs, span)?;
        let right = self.merge_sort_by_key(right, span)?;
        let mut out = Vec::with_capacity(n);
        let mut li = left.into_iter().peekable();
        let mut ri = right.into_iter().peekable();
        loop {
            match (li.peek(), ri.peek()) {
                (Some(l), Some(r)) => {
                    // `<= Equal` keeps left first on ties → stable.
                    let ord = self.order_key(&l.0, &r.0, span)?;
                    if ord.is_le() {
                        out.push(li.next().unwrap());
                    } else {
                        out.push(ri.next().unwrap());
                    }
                }
                (Some(_), None) => out.push(li.next().unwrap()),
                (None, Some(_)) => out.push(ri.next().unwrap()),
                (None, None) => break,
            }
        }
        Ok(out)
    }

    /// Natural order over two `sort_by_key` keys: a Comparable struct key dispatches to its
    /// `compare`; scalar keys (int/float/str) use the built-in ordering. The checker has verified the
    /// key type is orderable, so any other shape is an internal invariant break.
    fn order_key(
        &mut self,
        a: &Value,
        b: &Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        if matches!(a, Value::Struct { .. }) && matches!(b, Value::Struct { .. }) {
            return self.struct_compare(a.clone(), b.clone(), span);
        }
        compare(a, b).ok_or_else(|| RuntimeError {
            message: format!(
                "sort_by_key keys are not comparable: {} vs {}",
                a.type_name(),
                b.type_name()
            ),
            span,
        })
    }

    /// Stable top-down merge sort driven by a Chezzi comparator value.
    fn merge_sort_by(
        &mut self,
        mut xs: Vec<Value>,
        cmp: &Value,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        let n = xs.len();
        if n <= 1 {
            return Ok(xs);
        }
        let right = xs.split_off(n / 2);
        let left = self.merge_sort_by(xs, cmp, span)?;
        let right = self.merge_sort_by(right, cmp, span)?;
        let mut out = Vec::with_capacity(n);
        let mut li = left.into_iter().peekable();
        let mut ri = right.into_iter().peekable();
        loop {
            match (li.peek(), ri.peek()) {
                (Some(l), Some(r)) => {
                    // `<= 0` keeps left first on ties → stable.
                    let ord = self.compare_with(cmp, l.clone(), r.clone(), span)?;
                    if ord <= 0 {
                        out.push(li.next().unwrap());
                    } else {
                        out.push(ri.next().unwrap());
                    }
                }
                (Some(_), None) => out.push(li.next().unwrap()),
                (None, Some(_)) => out.push(ri.next().unwrap()),
                (None, None) => break,
            }
        }
        Ok(out)
    }

    /// Run the comparator on `(a, b)` and return its int result (errors if it returns non-int).
    fn compare_with(
        &mut self,
        cmp: &Value,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<i64, RuntimeError> {
        match self.call_value(cmp.clone(), vec![a, b], span)? {
            Value::Int(n) => Ok(n),
            other => Err(RuntimeError {
                message: format!(
                    "sort_by comparator must return int, got {}",
                    other.type_name()
                ),
                span,
            }),
        }
    }

    /// Call a user function: bind params in a fresh local frame (lexical scoping — the callee
    /// sees only globals + its params), run the body, and surface a `return` value (or `Nil`).
    /// Re-establish the CALLEE's module context (`cur_module_idx` + `bare_types`) from its `home`
    /// globals env, returning the saved caller context to restore at frame teardown. Drives
    /// `enum_bare_key`/`type_key` so a disambiguated enum match inside the callee's declaring module
    /// resolves its pattern key against the RIGHT module — the runtime mirror of the compiler's
    /// per-module key resolution (Blocker C). A `home` with no recorded idx (e.g. a synthetic env)
    /// leaves the context unchanged.
    fn enter_module_ctx(&mut self, home: &value::ModEnv) -> (usize, ModuleCtxSaved) {
        let saved_idx = self.cur_module_idx;
        let ptr = std::rc::Rc::as_ptr(&home.0) as usize;
        if let Some(&idx) = self.home_module_idx.get(&ptr)
            && idx != saved_idx
        {
            let saved_bare = std::mem::replace(
                &mut self.bare_types,
                self.module_bare_types.get(idx).cloned().unwrap_or_default(),
            );
            self.cur_module_idx = idx;
            (saved_idx, Some(saved_bare))
        } else {
            // Same module (or unknown home) → no swap needed; restoration is a no-op.
            (saved_idx, None)
        }
    }

    /// Restore the caller's module context saved by [`enter_module_ctx`].
    fn restore_module_ctx(&mut self, saved: (usize, ModuleCtxSaved)) {
        let (saved_idx, saved_bare) = saved;
        if let Some(bare) = saved_bare {
            self.cur_module_idx = saved_idx;
            self.bare_types = bare;
        }
    }

    fn call(
        &mut self,
        decl: &FnDecl,
        home: &value::ModEnv,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != decl.params.len() {
            return Err(RuntimeError {
                message: format!(
                    "function '{}' expects {} argument(s), got {}",
                    decl.name,
                    decl.params.len(),
                    args.len()
                ),
                span,
            });
        }
        // One-way int→float widening: does this fn declare a `float` return type? (Used at the
        // return site below to coerce the returned value, mirroring the VM.)
        let ret_is_float = decl.ret.as_ref().is_some_and(crate::ast::is_float_ty);
        let mut frame = std::collections::HashMap::new();
        for (param, arg) in decl.params.iter().zip(args) {
            // One-way int→float widening at the callee prologue (mirrors the VM): coerce an int
            // argument to a `float`-typed param, so EVERY caller (incl. an int variable) widens.
            let arg = if param.ty.as_ref().is_some_and(crate::ast::is_float_ty) {
                coerce_float(arg)
            } else {
                arg
            };
            frame.insert(param.name.clone(), arg);
        }
        self.enter_call(span)?;
        self.deferred.push(Vec::new());
        // Record this frame for stack traces; popped below only on a successful return (an error
        // path leaves it so the driver can read the call chain at the fault).
        self.call_stack.push(TraceFrame {
            function: decl.name.clone(),
            span,
        });
        // Resolve the callee's top-level names against *its* module, not the caller's.
        let saved_globals = self.env.swap_globals(home.clone());
        // Re-establish the callee's MODULE context (key resolution) too, so a disambiguated enum
        // match inside the callee resolves its pattern key against the callee's module (Blocker C).
        let saved_ctx = self.enter_module_ctx(home);
        let saved = self.env.swap_locals(vec![frame]);
        // The function body's *direct* defers belong to the per-call list (pushed above, drained by
        // `finish_frame`); nested blocks open their own defer scopes via `exec_block`.
        // M-C: if the body has a bare `spawn` (not inside an explicit `parallel:`), it is an implicit
        // nursery — push a task list now and JOIN it at the body's `return`/`?`/end (before defers).
        // An inline-expr body (`fn a(): <expr>`) implicitly returns its single expression — exactly
        // like a closure `fn(x): expr` (see `call_closure`): evaluate the expr and treat its value as
        // the return. An inline body cannot hold a bare `spawn` (spawn is a statement), so the
        // implicit-nursery dance never applies. Mirrors the VM's `compile_fn` inline-expr branch.
        let result = if decl.inline_expr_body
            && let [
                Stmt {
                    kind: StmtKind::Expr(e),
                    ..
                },
            ] = decl.body.as_slice()
        {
            self.eval(e).map(Flow::Return)
        } else {
            let implicit = crate::compiler::block_has_bare_spawn(&decl.body);
            if implicit {
                self.nurseries.push(Vec::new());
            }
            let result = self.exec_block_inner(&decl.body);
            if implicit {
                self.leave_implicit_nursery(result)
            } else {
                result
            }
        };
        // Teardown (defer drain + scope restore + `?`/defer-fault selection) is a separate,
        // non-inlined frame so its locals don't enlarge `call`'s frame on the deep-recursion path.
        let outcome = match self.finish_frame(saved, saved_globals) {
            Err(e) => Err(e),
            Ok(Some(v)) => Ok(v),
            Ok(None) => match result {
                // One-way int→float widening (mirrors the VM's `Op::CoerceFloat` before `Op::Return`,
                // incl. the inline-expr body's implicit return): a `-> float` fn coerces its returned
                // value. Only the explicit/implicit RETURN value is coerced — a `?`-propagated value
                // (`Ok(Some(v))` above) is a Result/Option, never a bare numeric in a `-> float` fn.
                Ok(Flow::Return(v)) if ret_is_float => Ok(coerce_float(v)),
                Ok(Flow::Return(v)) => Ok(v),
                // A function body falling off the end (or a stray break/continue the checker would
                // have rejected) yields nil.
                Ok(Flow::Normal) | Ok(Flow::Break) | Ok(Flow::Continue) => Ok(Value::Nil),
                Err(e) => Err(e),
            },
        };
        self.restore_module_ctx(saved_ctx);
        if outcome.is_ok() {
            self.call_stack.pop();
        }
        outcome
    }

    /// Shared call-frame teardown for `call` / `call_closure`: drain this frame's deferred calls
    /// (LIFO), restore the caller's scope, and decrement the depth. Returns `Err` if a deferred call
    /// faulted (it supersedes the frame's result), `Ok(Some(v))` if a `?` propagated a value out of
    /// the body, or `Ok(None)` to use the body's own result. Kept out of the callers' frames
    /// (`#[inline(never)]`) so deep recursion stays within the stack budget.
    #[inline(never)]
    fn finish_frame(
        &mut self,
        saved: Vec<std::collections::HashMap<String, Value>>,
        saved_globals: value::ModEnv,
    ) -> Result<Option<Value>, RuntimeError> {
        let defer_err = self.drain_frame_defers();
        self.env.swap_locals(saved);
        self.env.swap_globals(saved_globals);
        self.call_depth -= 1;
        if let Some(e) = defer_err {
            self.propagating = None;
            return Err(e);
        }
        Ok(self.propagating.take())
    }

    /// Pop the current frame's deferred-call list and drain it (LIFO). Skipped on a hard
    /// `std.os.exit` (Go: `os.Exit` does not run deferred calls). Returns a fault from a deferred
    /// call, if any.
    fn drain_frame_defers(&mut self) -> Option<RuntimeError> {
        let frame_defers = self.deferred.pop().unwrap_or_default();
        if self.pending_exit.is_some() {
            return None;
        }
        self.run_deferred(frame_defers).err()
    }

    /// Drive a single-file module (the `run_program` test path): the whole program shares one set
    /// of globals and runs top-to-bottom (no auto-`main`). Errors surface with output preserved.
    #[cfg(test)]
    fn execute(&mut self, stmts: &[Stmt]) -> Result<(), RuntimeError> {
        // Single-source path (no module graph): every top-level type is locally declared, hence
        // bare-visible under its own (bare) key. Populate `module_types`/`bare_types`/`type_names`
        // so the bare struct/enum constructor and the from-import skip behave as in the graph path.
        self.module_types = vec![std::collections::HashSet::new()];
        self.cur_module_idx = 0;
        // SINGLE-RESOLVER: extern C types come from the checker's standalone pass — the SAME resolver
        // the multi-file path uses (no second backend resolver exists). Consumed verbatim below.
        self.extern_sigs = crate::checker::resolve_extern_signatures_standalone(stmts);
        for s in stmts {
            if let StmtKind::Struct { name, .. }
            | StmtKind::Enum { name, .. }
            | StmtKind::NewType { name, .. }
            | StmtKind::TypeAlias { name, .. } = &s.kind
            {
                // ROOT REDESIGN — single-source types are keyed `<main>::Name` (the standalone module
                // key), identical to the compiler's standalone path, so identity keys agree (parity).
                let key = format!("{}::{name}", crate::compiler::STANDALONE_MODULE_KEY);
                self.module_types[0].insert(name.clone());
                self.type_names.insert(name.clone());
                self.type_keys.insert((0, name.clone()), key.clone());
                self.bare_types.insert(name.clone(), key);
            }
        }
        self.eval_top_level(stmts)
    }

    /// Hoist declarations and run top-level statements. There is **no** automatic entry point —
    /// `main` is an ordinary function the program calls itself (scripting-language model). An
    /// `Err`/`None` propagated to the top by a top-level `?` is an unhandled error and exits.
    fn eval_top_level(&mut self, stmts: &[Stmt]) -> Result<(), RuntimeError> {
        self.hoist_declarations(stmts)?;
        // M-C: the module top level is an implicit nursery that joins at program exit (here, before
        // the run driver drains live executors). A bare top-level `spawn` registers on it. JOIN only
        // on a clean run to the end; on a top-level fault or unhandled `?` the nursery is abandoned —
        // matching the VM, where an uncaught top-level error never reaches the toplevel `Return`'s
        // join and the pending tasks are silently dropped as the program errors out.
        let implicit = crate::compiler::block_has_bare_spawn(stmts);
        if implicit {
            self.nurseries.push(Vec::new());
        }
        let result = self.exec_block(stmts);
        if implicit {
            let tasks = self.nurseries.pop().unwrap_or_default();
            if matches!(result, Ok(Flow::Normal)) {
                for task in tasks {
                    self.run_task(task)?;
                }
            }
        }
        // A `?` that propagated to the top (no enclosing function) is an unhandled error. The
        // propagation marker still carries the `?`'s `expr.span`, so report at the real location
        // (matching the bare-expr path and the VM) rather than a hard-coded line 1.
        if let Some(value) = self.propagating.take() {
            let span = result
                .as_ref()
                .err()
                .map(|e| e.span)
                .unwrap_or(Span { line: 1, col: 1 });
            return Err(
                top_level_error(&value, span).unwrap_or_else(|| RuntimeError {
                    message: format!("unhandled error: {value}"),
                    span,
                }),
            );
        }
        result.map(|_| ())
    }

    /// Evaluate one module of a multi-file program into its own fresh globals, then snapshot those
    /// globals as the module's namespace (cached for importers). Run-once: each module is evaluated
    /// exactly once, in dependency order. No module auto-runs `main` (it's a normal function).
    fn eval_module(&mut self, lm: &crate::resolver::LoadedModule) -> Result<(), RuntimeError> {
        // A native std module (std.math/io/os) has no AST: build its namespace from the Rust member
        // table + float constants and cache it. Mirrors the VM's `run_module` native arm.
        if let Some(name) = lm.native {
            let members = value::ModEnv::new();
            {
                let mut m = members.0.borrow_mut();
                for (mname, func) in crate::native::native_members(name) {
                    m.insert(
                        mname.to_string(),
                        Value::Native(value::NativeFnEntry {
                            name: (*mname).into(),
                            func: *func,
                        }),
                    );
                }
                for (cname, cval) in crate::native::native_consts(name) {
                    m.insert(cname.to_string(), Value::Float(*cval));
                }
            }
            self.namespaces.insert(
                lm.id.clone(),
                std::rc::Rc::new(value::ModuleNamespace {
                    name: name.into(),
                    members,
                }),
            );
            return Ok(());
        }

        let mod_globals = value::ModEnv::new();
        let saved = self.env.swap_globals(mod_globals.clone());
        // Module-scoped types: record this module's index + its bound module bindings (for qualified
        // construction). Must match the compiler's per-module state so the runtime keys agree.
        self.cur_module_idx = self.module_idx_of.get(&lm.id).copied().unwrap_or(0);
        self.imported_module_idx.clear();
        self.bare_types.clear();
        let cur_idx = self.cur_module_idx;
        if let Some(own) = self.module_types.get(cur_idx) {
            for name in own.clone() {
                let key = self.type_key(cur_idx, &name);
                self.bare_types.insert(name, key);
            }
        }
        for imp in &lm.imports {
            match &imp.import {
                Import::Module { path, alias } => {
                    let bind = alias
                        .clone()
                        .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                    if let Some(&tidx) = self.module_idx_of.get(&imp.target) {
                        self.imported_module_idx.insert(bind, tidx);
                        // Std whole-module import exposes its types bare too (mirrors the checker).
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
                    if let Some(&tidx) = self.module_idx_of.get(&imp.target) {
                        for (member, alias) in names {
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
        // Cache this module's fully-built `bare_types` + record its `home` env address → idx, so a
        // cross-module runtime call can re-establish the callee's module context (Blocker C: a
        // disambiguated enum match inside this module, called from elsewhere, must resolve its pattern
        // key against THIS module's bare-types).
        if self.module_bare_types.len() <= cur_idx {
            self.module_bare_types
                .resize_with(cur_idx + 1, Default::default);
        }
        self.module_bare_types[cur_idx] = self.bare_types.clone();
        self.home_module_idx
            .insert(std::rc::Rc::as_ptr(&mod_globals.0) as usize, cur_idx);
        // Bind this module's imports before its body runs (dependencies are already evaluated, so
        // their namespaces are in `self.namespaces`).
        let bind = lm.imports.iter().try_for_each(|imp| self.bind_import(imp));
        let result = bind.and_then(|()| self.eval_top_level(&lm.ast.stmts));
        self.env.swap_globals(saved);
        self.namespaces.insert(
            lm.id.clone(),
            std::rc::Rc::new(value::ModuleNamespace {
                name: lm.label().into(),
                members: mod_globals,
            }),
        );
        result
    }

    /// Bind a resolved import's names into the current module's scope.
    fn bind_import(&mut self, imp: &crate::resolver::ResolvedImport) -> Result<(), RuntimeError> {
        use crate::ast::Import;
        let ns = self
            .namespaces
            .get(&imp.target)
            .cloned()
            .ok_or_else(|| RuntimeError {
                message: "internal: imported module not evaluated before importer".to_string(),
                span: imp.span,
            })?;
        match &imp.import {
            Import::Module { path, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.env.define(&name, Value::Module(ns));
            }
            Import::From { path: _, names } => {
                for (member, alias) in names {
                    // `std.ffi`'s exported fixed-width integer TYPE names (`import int32 from std.ffi`)
                    // carry NO runtime value — they are compile-time type imports the checker resolves.
                    // Skip them here (the namespace has no such member by design); any other missing
                    // member is a genuine error.
                    if &*ns.name == "std.ffi"
                        && crate::native::ffi::TYPE_NAMES.contains(&member.as_str())
                    {
                        continue;
                    }
                    // Bind the member's runtime value if the target module exports one (a fn/value).
                    // A `from`-imported USER type (struct/enum/alias) carries NO runtime value — it
                    // resolves through the program-global type tables by name — so a member with no
                    // value that IS a known type name is skipped (not an error). The value bind is
                    // tried FIRST so a fn named like a type IN ANOTHER MODULE is still bound here.
                    let value = ns.members.0.borrow().get(member).cloned();
                    match value {
                        Some(v) => {
                            self.env.define(alias.as_ref().unwrap_or(member), v);
                        }
                        None if self.type_names.contains(member) => {}
                        None => {
                            return Err(RuntimeError {
                                message: format!("module '{}' has no member '{member}'", ns.name),
                                span: imp.span,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Pre-register top-level `fn` declarations as globals so functions can call each other
    /// regardless of source order (mutual / forward references).
    fn hoist_declarations(&mut self, stmts: &[Stmt]) -> Result<(), RuntimeError> {
        let home = self.env.globals_rc();
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Fn(decl) => {
                    self.env.define(
                        &decl.name,
                        Value::Func(std::rc::Rc::new(decl.clone()), home.clone()),
                    );
                }
                StmtKind::Struct {
                    name,
                    fields,
                    methods,
                    ..
                } => {
                    // Module-scoped types: register under this module's runtime KEY (bare unless a
                    // genuine cross-module clash disambiguated it). A duplicate KEY is a real
                    // redefinition (same module, same name) — still rejected.
                    let key = self.type_key(self.cur_module_idx, name);
                    if self.structs.contains_key(&key) {
                        return Err(RuntimeError {
                            message: format!("type '{name}' is already defined"),
                            span: stmt.span,
                        });
                    }
                    let def = StructDef {
                        fields: fields.iter().map(|f| f.name.clone()).collect(),
                        methods: methods
                            .iter()
                            .map(|m| (m.name.clone(), std::rc::Rc::new(m.clone())))
                            .collect(),
                        home: home.clone(),
                    };
                    self.structs.insert(key.clone(), std::rc::Rc::new(def));
                    self.struct_module.insert(key.clone(), self.cur_module_idx);
                    self.struct_fields.insert(key, fields.clone());
                }
                // Type-erased: type parameters are checker-only (identical runtime per instantiation).
                StmtKind::Enum {
                    name,
                    variants,
                    methods,
                    ..
                } => {
                    let key = self.type_key(self.cur_module_idx, name);
                    for v in variants {
                        self.register_variant(&v.name, &key, v.payload.len());
                    }
                    if !methods.is_empty() {
                        let def = EnumDef {
                            methods: methods
                                .iter()
                                .map(|m| (m.name.clone(), std::rc::Rc::new(m.clone())))
                                .collect(),
                            home: home.clone(),
                        };
                        self.enum_defs.insert(key, std::rc::Rc::new(def));
                    }
                }
                // A newtype registers its methods (name-keyed, like an enum) under its runtime key.
                StmtKind::NewType { name, methods, .. } => {
                    let key = self.type_key(self.cur_module_idx, name);
                    let def = EnumDef {
                        methods: methods
                            .iter()
                            .map(|m| (m.name.clone(), std::rc::Rc::new(m.clone())))
                            .collect(),
                        home: home.clone(),
                    };
                    self.newtype_defs.insert(key, std::rc::Rc::new(def));
                }
                StmtKind::Extern { lib, fns } => {
                    // Eager `dlopen` + `dlsym` at module init (like the VM's `MakeCffi`): a missing
                    // library/symbol surfaces as a startup error. Each extern fn binds a `Cffi` value
                    // into the module's globals, exactly where a top-level `fn` binds.
                    //
                    // SINGLE-RESOLVER (fix5): each param/return CType comes VERBATIM from the
                    // CHECKER-RESOLVED `extern_sigs` table — the ONLY extern-type resolver (the
                    // multi-file path fills it via `resolve_extern_signatures`, the standalone path via
                    // `resolve_extern_signatures_standalone`). The interp does ZERO alias/qualified/
                    // struct resolution of its own, so a second resolver cannot exist or drift.
                    let cur_idx = self.cur_module_idx;
                    #[allow(clippy::type_complexity)]
                    let mut lowered: Vec<(
                        String,
                        Vec<crate::native::cffi::CType>,
                        Option<crate::native::cffi::CType>,
                    )> = Vec::with_capacity(fns.len());
                    for ef in fns {
                        let sig = self.extern_sigs.get(&(cur_idx, ef.name.clone()));
                        let params: Vec<crate::native::cffi::CType> = ef
                            .params
                            .iter()
                            .enumerate()
                            .map(|(i, p)| {
                                // The checker is the real gate; this is the never-panic backstop.
                                sig.and_then(|s| s.params.get(i).cloned().flatten())
                                    .ok_or_else(|| RuntimeError {
                                        message: format!(
                                            "type '{}' is not C-marshallable in extern fn '{}' \
                                             (v1 supports only int, float, bool, str, ptr, and a \
                                             flat struct of those)",
                                            ffi_type_display(p.ty.as_ref()),
                                            ef.name
                                        ),
                                        span: stmt.span,
                                    })
                            })
                            .collect::<Result<_, _>>()?;
                        // `None` ⇒ void: no annotation, or one resolving to `nil` (incl. an alias to
                        // `nil`). The checker guarantees a non-void return is a scalar.
                        let ret = sig.and_then(|s| s.ret.clone());
                        lowered.push((ef.name.clone(), params, ret));
                    }
                    for (name, params, ret) in lowered {
                        let cffi = crate::native::cffi::Cffi::new(lib, &name, params, ret)
                            .map_err(|e| RuntimeError {
                                message: e.message,
                                span: stmt.span,
                            })?;
                        self.env
                            .define(&name, Value::Cffi(std::sync::Arc::new(cffi)));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Execute a sequence of statements in the current scope, stopping early on `return`.
    /// `recover: <block>` — run the block; convert any genuine runtime fault occurring transitively
    /// beneath it into `Err(<message>)` (a `str`, which is an `Error`), otherwise wrap the block's
    /// trailing-expression value in `Ok`. A `?` Err/None in flight short-circuits to this boundary
    /// (try-block style): its value becomes the result directly.
    ///
    /// No execution state is restored here: a fault unwinds through `call`/`call_closure`/loops/
    /// scoped blocks, each of which restores caller locals + globals + call-depth + scope on its own
    /// error path. So side effects performed before the fault (e.g. an outer-variable assignment)
    /// persist — matching the VM, which mutates locals in place. Only the recover's own scope is
    /// balanced here.
    fn eval_recover(&mut self, block: &[Stmt]) -> Result<Value, RuntimeError> {
        let saved_prop = self.propagating.take();
        // A caught fault leaves the faulted call chain on `call_stack` (frames pop only on success);
        // restore it to this depth so a later fault's trace isn't polluted by recovered frames.
        let stack_depth = self.call_stack.len();
        self.env.push();
        let result = self.eval_recover_body(block);
        self.env.pop();
        match result {
            Ok(v) => {
                self.propagating = saved_prop;
                Ok(enum_val("Result", "Ok", vec![v]))
            }
            Err(e) => {
                if self.pending_exit.is_some() {
                    // `std.os.exit(code)` is a hard halt: it unwinds past `recover:` uncaught.
                    self.propagating = saved_prop;
                    Err(e)
                } else if let Some(propagated) = self.propagating.take() {
                    // A `?` Err/None in the block short-circuits here: it becomes the result.
                    self.propagating = saved_prop;
                    self.call_stack.truncate(stack_depth);
                    Ok(propagated)
                } else {
                    // A genuine runtime fault: its message (a `str`, i.e. an `Error`) becomes Err.
                    self.propagating = saved_prop;
                    self.call_stack.truncate(stack_depth);
                    Ok(enum_val(
                        "Result",
                        "Err",
                        vec![Value::Str(e.message.into())],
                    ))
                }
            }
        }
    }

    /// Evaluate a `recover` block to its value, with the block as a **defer scope**: its defers run
    /// at the recover boundary on every path (Ok, `?` short-circuit, or genuine fault), before the
    /// recovered value is produced. A fault in a deferred call supersedes the in-flight result —
    /// clearing `propagating` so `eval_recover` reports it as the recover's `Err`, not the `?` value.
    fn eval_recover_body(&mut self, block: &[Stmt]) -> Result<Value, RuntimeError> {
        if !block_has_defer(block) {
            return self.eval_recover_body_inner(block);
        }
        self.deferred.push(Vec::new());
        let result = self.eval_recover_body_inner(block);
        if let Some(e) = self.drain_block_defers() {
            self.propagating = None; // a defer fault supersedes any in-flight `?` value
            return Err(e);
        }
        result
    }

    /// The recover body proper: run the leading statements for effect (rejecting control flow), then
    /// evaluate the trailing expression statement (or `nil`).
    fn eval_recover_body_inner(&mut self, block: &[Stmt]) -> Result<Value, RuntimeError> {
        let Some((last, init)) = block.split_last() else {
            return Ok(Value::Nil);
        };
        for stmt in init {
            if !matches!(self.exec_stmt(stmt)?, Flow::Normal) {
                return Err(RuntimeError {
                    message:
                        "control flow (return/break/continue) is not allowed inside a recover block"
                            .to_string(),
                    span: stmt.span,
                });
            }
        }
        match &last.kind {
            StmtKind::Expr(e) => self.eval(e),
            _ => {
                if !matches!(self.exec_stmt(last)?, Flow::Normal) {
                    return Err(RuntimeError {
                        message: "control flow (return/break/continue) is not allowed inside a recover block".to_string(),
                        span: last.span,
                    });
                }
                Ok(Value::Nil)
            }
        }
    }

    /// Execute a block as a **defer scope**: any `defer` directly inside it runs when the block
    /// exits, on *every* path (fall-through, return/break/continue flow signal, or an `Err`/`?`
    /// fault), LIFO — the tree-walk analogue of a per-block `finally`. Blocks with no direct
    /// `defer` skip the push/drain entirely, so defer-free code is unaffected.
    fn exec_block(&mut self, stmts: &[Stmt]) -> Result<Flow, RuntimeError> {
        if !block_has_defer(stmts) {
            return self.exec_block_inner(stmts);
        }
        self.deferred.push(Vec::new());
        let result = self.exec_block_inner(stmts);
        // Drain this block's defers before propagating whatever the body produced. A fault in a
        // deferred call supersedes the body's result (Go semantics).
        if let Some(e) = self.drain_block_defers() {
            return Err(e);
        }
        result
    }

    /// The raw statement loop, without a defer scope. Used directly for the function body (whose
    /// defers are owned by the per-call list drained in `finish_frame`) and by `exec_block`.
    fn exec_block_inner(&mut self, stmts: &[Stmt]) -> Result<Flow, RuntimeError> {
        for stmt in stmts {
            match self.exec_stmt(stmt)? {
                Flow::Normal => {}
                // Any non-normal flow (return/break/continue) short-circuits the block and
                // propagates up to the enclosing loop (or function, for `return`).
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    /// Pop and drain (LIFO) the innermost block's deferred-call list. Skipped on a hard
    /// `std.os.exit`. Returns the latest fault from a deferred call, if any.
    fn drain_block_defers(&mut self) -> Option<RuntimeError> {
        let block_defers = self.deferred.pop().unwrap_or_default();
        if self.pending_exit.is_some() {
            return None;
        }
        self.run_deferred(block_defers).err()
    }

    /// Execute a block in a fresh child scope (locals don't leak), popping it even on error.
    fn exec_scoped_block(&mut self, stmts: &[Stmt]) -> Result<Flow, RuntimeError> {
        self.env.push();
        let result = self.exec_block(stmts);
        self.env.pop();
        result
    }

    /// Execute one statement.
    fn exec_stmt(&mut self, stmt: &Stmt) -> Result<Flow, RuntimeError> {
        match &stmt.kind {
            StmtKind::Let { names, value, ty, .. } => {
                let mut v = self.eval(value)?;
                // One-way int→float widening — coerce from the binding's annotation, mirroring the
                // VM's `Op::CoerceFloat` at the typed-`let` sink. A scalar `float` coerces the value
                // unconditionally (the VM coerces after the value expr, whatever its shape). A
                // `list[float]`/`map[_, float]` annotation coerces int ELEMENTS only when the value is
                // a LITERAL collection — the VM's element coercion is emitted at the literal-compile
                // site, so a non-literal value (`xs: list[float] = f()`) is NOT element-coerced by
                // either engine (it stays whatever `f()` returned).
                if names.len() == 1
                    && let Some(t) = ty
                {
                    let value_is_collection_literal =
                        matches!(value.kind, ExprKind::List(_) | ExprKind::Map(_));
                    coerce_value_to_annotation(&mut v, t, value_is_collection_literal);
                }
                if names.len() > 1 {
                    // destructuring let `a, b := expr` — `expr` must be a tuple of matching arity.
                    let Value::Tuple(items) = &v else {
                        return Err(RuntimeError {
                            message: format!("cannot destructure {}", v.type_name()),
                            span: stmt.span,
                        });
                    };
                    if items.len() != names.len() {
                        return Err(RuntimeError {
                            message: format!(
                                "destructuring binds {} name(s), but the tuple has {} element(s)",
                                names.len(),
                                items.len()
                            ),
                            span: stmt.span,
                        });
                    }
                    let items = items.clone();
                    for (name, elem) in names.iter().zip(items.iter()) {
                        self.env.define(name, elem.clone());
                    }
                } else {
                    self.env.define(&names[0], v);
                }
                Ok(Flow::Normal)
            }
            StmtKind::Assign { target, op, value } => {
                self.exec_assign(target, *op, value, stmt.span)?;
                Ok(Flow::Normal)
            }
            StmtKind::Expr(expr) => {
                let v = self.eval(expr)?;
                // An `Err`/`None` left unhandled at the top level (`call_depth == 0`, i.e. not inside
                // any function/closure call) exits the program. Inside a function a bare expression's
                // value is discarded as usual.
                if self.call_depth == 0
                    && let Some(e) = top_level_error(&v, expr.span)
                {
                    return Err(e);
                }
                Ok(Flow::Normal)
            }
            StmtKind::Assert { cond, msg } => {
                // Mirror the VM (`Op::Assert`): fault with `stmt.span` and the custom message (or
                // `"assertion failed"`) when `cond` is false. Both engines must be byte-identical.
                let c = self.eval(cond)?;
                if matches!(c, Value::Bool(false)) {
                    let message = match msg {
                        Some(m) => match self.eval(m)? {
                            Value::Str(s) => s.to_string(),
                            _ => "assertion failed".to_string(),
                        },
                        None => "assertion failed".to_string(),
                    };
                    return Err(RuntimeError { message, span: stmt.span });
                }
                Ok(Flow::Normal)
            }
            StmtKind::Fn(decl) => {
                let home = self.env.globals_rc();
                self.env
                    .define(&decl.name, Value::Func(std::rc::Rc::new(decl.clone()), home));
                Ok(Flow::Normal)
            }
            // Type declarations are registered up-front by `hoist_declarations`; imports are bound
            // before the body runs (see `eval_module`). Nothing to do when execution reaches them.
            StmtKind::Struct { .. }
            | StmtKind::Enum { .. }
            | StmtKind::NewType { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Extern { .. } // bound by hoist_declarations, like top-level fn
            | StmtKind::TypeAlias { .. }
            | StmtKind::Import(_) => Ok(Flow::Normal),
            StmtKind::Match { scrutinee, arms } => self.exec_match(scrutinee, arms),
            StmtKind::Return(value) => {
                let v = match value {
                    Some(e) => self.eval(e)?,
                    None => Value::Nil,
                };
                Ok(Flow::Return(v))
            }
            // Generators (`yield`) are an experimental VM-only feature; the frozen tree-walk
            // interpreter cannot suspend a native Rust call mid-body, so it rejects them outright.
            StmtKind::Yield(_) => Err(RuntimeError {
                message: "generators (`yield`) are not supported by the interpreter (VM-only)"
                    .to_string(),
                span: stmt.span,
            }),
            // Kept out of this match (its locals are large) so `exec_stmt`'s frame stays small for
            // deep recursion — same reason as `eval_slice`.
            StmtKind::Defer(target) => self.exec_defer(target, stmt.span),
            // Kept out of this match (their locals are large) so `exec_stmt`'s frame stays small.
            StmtKind::Parallel { body } => self.exec_parallel(body),
            StmtKind::Spawn(target) => self.exec_spawn(target, stmt.span),
            StmtKind::Wait { arms, else_block } => self.exec_wait(arms, else_block.as_deref(), stmt.span),
            StmtKind::Break => Ok(Flow::Break),
            StmtKind::Continue => Ok(Flow::Continue),
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    if as_bool(self.eval(cond)?, cond.span)? {
                        return self.exec_scoped_block(body);
                    }
                }
                match else_block {
                    Some(body) => self.exec_scoped_block(body),
                    None => Ok(Flow::Normal),
                }
            }
            StmtKind::For { vars, iter, body } => self.exec_for(vars, iter, body),
            StmtKind::While { cond, body } => {
                while as_bool(self.eval(cond)?, cond.span)? {
                    match self.exec_scoped_block(body)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        // `break` stops the loop; the loop itself completes normally.
                        Flow::Break => break,
                        // `continue` re-evaluates the condition (the natural top of this `while`).
                        Flow::Continue | Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
        }
    }

    /// Execute a `for vars in iter:` loop. `iter` is a `start..end` range, a list, or a map
    /// (`for k in m` binds the key; `for k, v in m` binds key+value). Each iteration runs the body
    /// in a fresh scope so the loop variables don't leak. Ranges are iterated **lazily** (never
    /// materialized) so `for i in 0..huge:` can't exhaust memory.
    fn exec_for(
        &mut self,
        vars: &[String],
        iter: &Expr,
        body: &[Stmt],
    ) -> Result<Flow, RuntimeError> {
        if let ExprKind::Range { start, end } = &iter.kind {
            let lo = self.eval_int(start)?;
            let hi = self.eval_int(end)?;
            let mut i = lo;
            while i < hi {
                match self.run_for_body(vars, vec![Value::Int(i)], body)? {
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    Flow::Break => break,
                    // `continue` falls through to the `i += 1` increment below — it must NOT skip
                    // the increment, or the loop never advances and hangs.
                    Flow::Continue | Flow::Normal => {}
                }
                i += 1; // increment: `continue` lands here (falls through), never skips it.
            }
            return Ok(Flow::Normal);
        }
        let iter_val = self.eval(iter)?;
        // PURE-`Iterable` struct (has `iter`, lacks `next`): drive it through a ONE-TIME `.iter()`
        // call to get the cursor, which `iter_rows_from_value` then drains. A struct with BOTH `iter`
        // and `next` is NOT converted here — it keeps the `next()` fast path below (back-compat
        // precedence). Checked first so the rest of `exec_for` sees a cursor, not the struct.
        let iter_val = match &iter_val {
            Value::Struct { name, .. }
                if self.structs.get(name.as_ref()).is_some_and(|d| {
                    !d.methods.contains_key("next") && d.methods.contains_key("iter")
                }) =>
            {
                self.dispatch_method(iter_val.clone(), "iter", Vec::new(), iter.span)?
            }
            _ => iter_val,
        };
        // Struct iterator protocol: a struct with `next(self) -> Option[T]` is iterated LAZILY —
        // call `next()` each step until it returns `None`, so an infinite iterator with an early
        // `break` terminates. The struct advances by mutating its own fields in place (its fields
        // are `Rc<RefCell<…>>`), so re-cloning the receiver handle each step reads the new state.
        if let Value::Struct { name, .. } = &iter_val
            && self
                .structs
                .get(name.as_ref())
                .is_some_and(|d| d.methods.contains_key("next"))
        {
            let name = name.clone();
            // Dispatch `next(self)` the same way `eval_method_call` does for a struct method.
            let def = self
                .structs
                .get(name.as_ref())
                .cloned()
                .ok_or_else(|| RuntimeError {
                    message: format!("unknown struct type '{name}'"),
                    span: iter.span,
                })?;
            let decl = def
                .methods
                .get("next")
                .cloned()
                .ok_or_else(|| RuntimeError {
                    message: format!("struct '{name}' has no method 'next'"),
                    span: iter.span,
                })?;
            loop {
                let result = self.call(&decl, &def.home, vec![iter_val.clone()], iter.span)?;
                match result {
                    Value::Enum {
                        variant, payload, ..
                    } if variant.as_ref() == "Some" => {
                        let item = payload.into_iter().next().ok_or_else(|| RuntimeError {
                            message: "iterator next() returned Some with no payload".to_string(),
                            span: iter.span,
                        })?;
                        match self.run_for_body(vars, vec![item], body)? {
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Break => break,
                            Flow::Continue | Flow::Normal => {}
                        }
                    }
                    Value::Enum { variant, .. } if variant.as_ref() == "None" => break,
                    other => {
                        return Err(RuntimeError {
                            message: format!(
                                "iterator next() must return Option, found {}",
                                other.type_name()
                            ),
                            span: iter.span,
                        });
                    }
                }
            }
            return Ok(Flow::Normal);
        }
        // `for v in ch:` over a Channel — drain buffered + future values, end cleanly when closed.
        // The sequential oracle cannot block, so an open-and-empty channel faults like bare `recv`
        // (a valid program runs the producer to completion + closes before the consumer iterates).
        if let Value::Channel(state) = &iter_val {
            let state = state.clone();
            loop {
                let next = {
                    let mut s = state.borrow_mut();
                    match s.queue.pop_front() {
                        Some(v) => Some(v),
                        None if s.closed => None, // closed + drained ⇒ clean exit
                        None => {
                            return Err(RuntimeError {
                                message: "recv on an empty channel: deadlock — nothing is queued and \
                                          the sequential executor cannot block waiting for a producer \
                                          (a consumer that waits mid-flight on a live producer needs C5)"
                                    .to_string(),
                                span: iter.span,
                            });
                        }
                    }
                };
                match next {
                    Some(v) => match self.run_for_body(vars, vec![v], body)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                    },
                    None => break,
                }
            }
            return Ok(Flow::Normal);
        }
        // Materialize the per-iteration value tuples up front (clone out) so a body that mutates the
        // collection doesn't disturb iteration, and no borrow is held across the body.
        let rows = iter_rows_from_value(&iter_val, vars.len(), iter.span)?;
        for row in rows {
            match self.run_for_body(vars, row, body)? {
                Flow::Return(v) => return Ok(Flow::Return(v)),
                Flow::Break => break,
                // `continue` falls through to the next element (the `for` advances naturally).
                Flow::Continue | Flow::Normal => {}
            }
        }
        Ok(Flow::Normal)
    }

    /// Run one `for` iteration: bind each name in `vars` to the matching value in a fresh scope,
    /// execute the body, pop the scope. `vars` and `vals` are zipped (equal length in practice).
    fn run_for_body(
        &mut self,
        vars: &[String],
        vals: Vec<Value>,
        body: &[Stmt],
    ) -> Result<Flow, RuntimeError> {
        self.env.push();
        for (name, val) in vars.iter().zip(vals) {
            self.env.define(name, val);
        }
        let flow = self.exec_block(body);
        self.env.pop();
        flow
    }

    /// Evaluate a comprehension into a fresh list / set / map. Iteration reuses the same lazy paths
    /// as a `for` loop (range, struct iterator via `next()`, or materialized list/map/set/str — see
    /// `eval_comp_clauses`). Multiple `for` clauses nest (first clause outermost, last innermost):
    /// `eval_comp_clauses` recurses left-to-right, binding each clause's vars in a fresh scope so a
    /// later clause's `iter`/guards see earlier bindings — exactly the order the compiler's nested
    /// `compile_for` produces (parity). Each clause's guards are tested in turn; at the innermost
    /// clause the element (plus key, for maps) is collected into the accumulator. Set elements and
    /// map keys dedupe exactly like their literals.
    fn eval_comprehension(
        &mut self,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        clauses: &[CompClause],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let mut acc = match kind {
            CompKind::List => CompAcc::List(Vec::new()),
            CompKind::Set => CompAcc::Set(SetData::default()),
            CompKind::Map => CompAcc::Map(MapData::default()),
        };
        self.eval_comp_clauses(clauses, 0, key, elem, span, &mut acc)?;
        Ok(match acc {
            CompAcc::List(out) => Value::List(std::rc::Rc::new(std::cell::RefCell::new(out))),
            CompAcc::Set(set) => Value::Set(std::rc::Rc::new(std::cell::RefCell::new(set))),
            CompAcc::Map(map) => Value::Map(std::rc::Rc::new(std::cell::RefCell::new(map))),
        })
    }

    /// Recurse over comprehension clauses starting at `idx`. At `clauses[idx]`, drive that clause's
    /// iterable LAZILY (one element at a time, in the CURRENT env, which already has earlier clauses'
    /// vars bound), and for each element push a scope, bind the vars, test the clause's guards (skip
    /// the element if any is false), and either recurse to `idx+1` or — if this is the last clause —
    /// evaluate the key/element into `acc`. Each element's scope is popped even on error.
    ///
    /// Laziness matters for parity: a stateful struct iterator advances its fields with each `next()`,
    /// and the VM (`compile_for`) interleaves `next()` with the element/guard evaluation. So the
    /// element/guard must run BEFORE the next `next()`, never after the whole iterator is drained.
    /// `for x in struct_iter` (the statement) is already lazy this way; this mirrors it exactly so a
    /// comprehension over the same iterator agrees byte-for-byte. List/map/set/str/range iterables are
    /// stateless, so materializing-then-iterating them is observationally identical to driving them
    /// one element at a time — order and semantics are unchanged.
    fn eval_comp_clauses(
        &mut self,
        clauses: &[CompClause],
        idx: usize,
        key: Option<&Expr>,
        elem: &Expr,
        span: Span,
        acc: &mut CompAcc,
    ) -> Result<(), RuntimeError> {
        let iter = &clauses[idx].iter;
        // Lazy range: never materialized (mirrors `exec_for`).
        if let ExprKind::Range { start, end } = &iter.kind {
            let lo = self.eval_int(start)?;
            let hi = self.eval_int(end)?;
            let mut i = lo;
            while i < hi {
                self.run_comp_element(clauses, idx, key, elem, span, acc, vec![Value::Int(i)])?;
                i += 1;
            }
            return Ok(());
        }
        let iter_val = self.eval(iter)?;
        // PURE-`Iterable` struct (has `iter`, lacks `next`): convert to its cursor via a one-time
        // `.iter()`, then drive the cursor's `next()` below — identical to `exec_for`.
        let iter_val = match &iter_val {
            Value::Struct { name, .. }
                if self.structs.get(name.as_ref()).is_some_and(|d| {
                    !d.methods.contains_key("next") && d.methods.contains_key("iter")
                }) =>
            {
                self.dispatch_method(iter_val.clone(), "iter", Vec::new(), span)?
            }
            _ => iter_val,
        };
        // Struct iterator protocol: drive `next(self) -> Option[T]` LAZILY — call it, run the body
        // (which reads the iterator's just-advanced state), then call it again. This is the path the
        // eager `collect_iter_rows` used to break: it drained to `None` before any body ran, so the
        // element saw the fully-advanced state instead of the per-step state the VM observes.
        if let Value::Struct { name, .. } = &iter_val
            && self
                .structs
                .get(name.as_ref())
                .is_some_and(|d| d.methods.contains_key("next"))
        {
            let name = name.clone();
            let def = self
                .structs
                .get(name.as_ref())
                .cloned()
                .ok_or_else(|| RuntimeError {
                    message: format!("unknown struct type '{name}'"),
                    span,
                })?;
            let decl = def
                .methods
                .get("next")
                .cloned()
                .ok_or_else(|| RuntimeError {
                    message: format!("struct '{name}' has no method 'next'"),
                    span,
                })?;
            loop {
                let result = self.call(&decl, &def.home, vec![iter_val.clone()], span)?;
                match result {
                    Value::Enum {
                        variant, payload, ..
                    } if variant.as_ref() == "Some" => {
                        let item = payload.into_iter().next().ok_or_else(|| RuntimeError {
                            message: "iterator next() returned Some with no payload".to_string(),
                            span,
                        })?;
                        self.run_comp_element(clauses, idx, key, elem, span, acc, vec![item])?;
                    }
                    Value::Enum { variant, .. } if variant.as_ref() == "None" => break,
                    other => {
                        return Err(RuntimeError {
                            message: format!(
                                "iterator next() must return Option, found {}",
                                other.type_name()
                            ),
                            span,
                        });
                    }
                }
            }
            return Ok(());
        }
        // Stateless built-in iterables (list/map/set/str): materialize rows then drive them one at a
        // time — order/semantics unchanged vs. the old eager path.
        let rows = iter_rows_from_value(&iter_val, clauses[idx].vars.len(), span)?;
        for row in rows {
            self.run_comp_element(clauses, idx, key, elem, span, acc, row)?;
        }
        Ok(())
    }

    /// Run one comprehension element for `clauses[idx]`: push a scope, bind the clause's vars to
    /// `row`, run the guard/recurse/collect step, and pop the scope (even on error).
    #[allow(clippy::too_many_arguments)]
    fn run_comp_element(
        &mut self,
        clauses: &[CompClause],
        idx: usize,
        key: Option<&Expr>,
        elem: &Expr,
        span: Span,
        acc: &mut CompAcc,
        row: Vec<Value>,
    ) -> Result<(), RuntimeError> {
        self.env.push();
        for (name, val) in clauses[idx].vars.iter().zip(row) {
            self.env.define(name, val);
        }
        let r = self.eval_comp_row_step(clauses, idx, key, elem, span, acc);
        self.env.pop();
        r
    }

    /// Body of one comprehension row (scope already pushed/bound): test this clause's guards, then
    /// recurse to the next clause or collect the element. Split out so the caller can always pop the
    /// scope, even on error.
    fn eval_comp_row_step(
        &mut self,
        clauses: &[CompClause],
        idx: usize,
        key: Option<&Expr>,
        elem: &Expr,
        span: Span,
        acc: &mut CompAcc,
    ) -> Result<(), RuntimeError> {
        for g in &clauses[idx].guards {
            match self.eval(g)? {
                Value::Bool(true) => {}
                Value::Bool(false) => return Ok(()),
                // The checker guarantees a bool guard; this is a defensive fallback.
                other => {
                    return Err(RuntimeError {
                        message: format!(
                            "comprehension guard must be bool, found {}",
                            other.type_name()
                        ),
                        span,
                    });
                }
            }
        }
        if idx + 1 < clauses.len() {
            return self.eval_comp_clauses(clauses, idx + 1, key, elem, span, acc);
        }
        // Innermost clause: evaluate the key (map only) then the element, and collect.
        let k = match key {
            Some(k) => Some(self.eval(k)?),
            None => None,
        };
        let v = self.eval(elem)?;
        match acc {
            CompAcc::List(out) => out.push(v),
            CompAcc::Set(set) => {
                let hx = self.hash_value(&v, span)?;
                if !set
                    .candidates(hx)
                    .iter()
                    .any(|&p| values_equal(&set.entries[p].1, &v))
                {
                    set.push(hx, v);
                }
            }
            CompAcc::Map(map) => {
                let k = k.expect("a map comprehension evaluates a key per row");
                let hk = self.hash_value(&k, span)?;
                match map
                    .candidates(hk)
                    .iter()
                    .copied()
                    .find(|&p| values_equal(&map.entries[p].1, &k))
                {
                    Some(i) => map.entries[i].2 = v,
                    None => map.push(hk, k, v),
                }
            }
        }
        Ok(())
    }

    /// Materialize an already-evaluated iterable VALUE into per-iteration rows (the post-eval body of
    /// `eval_comp_clauses`' struct-iterator path, factored out so the `list()`/`set()`/`map()` builtin constructors reuse the
    /// SAME "drain any for-iterable" logic — no duplicated iteration semantics). Handles (a) a user
    /// struct with `next(self) -> Option[T]` (call lazily until `None`, may run user code) and (b) the
    /// built-in collections via `iter_rows_from_value`. `vars` is the loop-variable count (1 for the
    /// constructors; map() post-processes the single-element rows into (k, v) pairs itself).
    fn drain_value_to_rows(
        &mut self,
        iter_val: Value,
        vars: usize,
        span: Span,
    ) -> Result<Vec<Vec<Value>>, RuntimeError> {
        // PURE-`Iterable` struct (has `iter`, lacks `next`): convert to its cursor via a one-time
        // `.iter()`, then drain the cursor below. Mirrors `exec_for` (so `list(w)`/`for x in w` agree).
        let iter_val = match &iter_val {
            Value::Struct { name, .. }
                if self.structs.get(name.as_ref()).is_some_and(|d| {
                    !d.methods.contains_key("next") && d.methods.contains_key("iter")
                }) =>
            {
                self.dispatch_method(iter_val.clone(), "iter", Vec::new(), span)?
            }
            _ => iter_val,
        };
        if let Value::Struct { name, .. } = &iter_val
            && self
                .structs
                .get(name.as_ref())
                .is_some_and(|d| d.methods.contains_key("next"))
        {
            let name = name.clone();
            let def = self
                .structs
                .get(name.as_ref())
                .cloned()
                .ok_or_else(|| RuntimeError {
                    message: format!("unknown struct type '{name}'"),
                    span,
                })?;
            let decl = def
                .methods
                .get("next")
                .cloned()
                .ok_or_else(|| RuntimeError {
                    message: format!("struct '{name}' has no method 'next'"),
                    span,
                })?;
            let mut rows = Vec::new();
            loop {
                let result = self.call(&decl, &def.home, vec![iter_val.clone()], span)?;
                match result {
                    Value::Enum {
                        variant, payload, ..
                    } if variant.as_ref() == "Some" => {
                        let item = payload.into_iter().next().ok_or_else(|| RuntimeError {
                            message: "iterator next() returned Some with no payload".to_string(),
                            span,
                        })?;
                        rows.push(vec![item]);
                    }
                    Value::Enum { variant, .. } if variant.as_ref() == "None" => break,
                    other => {
                        return Err(RuntimeError {
                            message: format!(
                                "iterator next() must return Option, found {}",
                                other.type_name()
                            ),
                            span,
                        });
                    }
                }
            }
            return Ok(rows);
        }
        iter_rows_from_value(&iter_val, vars, span)
    }

    /// Evaluate an expression expected to be an integer (range bounds, list index).
    fn eval_int(&mut self, expr: &Expr) -> Result<i64, RuntimeError> {
        match self.eval(expr)? {
            Value::Int(n) => Ok(n),
            other => Err(RuntimeError {
                message: format!("expected int, found {}", other.type_name()),
                span: expr.span,
            }),
        }
    }

    /// Execute an assignment (`=`, `+=`, `-=`). Only simple identifier targets for now.
    fn exec_assign(
        &mut self,
        target: &Expr,
        op: AssignOp,
        value: &Expr,
        span: Span,
    ) -> Result<(), RuntimeError> {
        match &target.kind {
            ExprKind::Ident(name) => {
                let rhs = self.eval(value)?;
                let new_val = match op.to_binop() {
                    None => rhs,
                    Some(bin) => {
                        let cur = self.env.get(name).ok_or_else(|| RuntimeError {
                            message: format!("undefined name '{name}'"),
                            span,
                        })?;
                        eval_binary(bin, cur, rhs, span)?
                    }
                };
                if !self.env.assign(name, new_val) {
                    return Err(RuntimeError {
                        message: format!("cannot assign to undefined name '{name}'"),
                        span,
                    });
                }
                Ok(())
            }
            // `xs[i] = v` / `m[k] = v` — mutate a list element or upsert a map entry in place
            // (both are `Rc<RefCell<…>>`). Evaluate the object once, then branch on its kind.
            ExprKind::Index { obj, index } => {
                let target_val = self.eval(obj)?;
                // Map upsert: the key is an arbitrary value (str/bool/int), not an int index.
                if let Value::Map(entries) = &target_val {
                    let key = self.eval(index)?;
                    // Hash before borrowing (a struct key's hash() may re-read this map).
                    let hk = self.hash_value(&key, span)?;
                    let new_val = match op.to_binop() {
                        None => self.eval(value)?,
                        Some(bin) => {
                            // Compound on a missing key is an error (consistent with read-missing).
                            let cur = {
                                let m = entries.borrow();
                                m.candidates(hk)
                                    .iter()
                                    .copied()
                                    .find(|&p| values_equal(&m.entries[p].1, &key))
                                    .map(|p| m.entries[p].2.clone())
                            };
                            let Some(cur) = cur else {
                                return Err(RuntimeError {
                                    message: "key not found".to_string(),
                                    span,
                                });
                            };
                            let rhs = self.eval(value)?;
                            eval_binary(bin, cur, rhs, span)?
                        }
                    };
                    let pos = {
                        let m = entries.borrow();
                        m.candidates(hk)
                            .iter()
                            .copied()
                            .find(|&p| values_equal(&m.entries[p].1, &key))
                    };
                    match pos {
                        Some(i) => entries.borrow_mut().entries[i].2 = new_val,
                        None => entries.borrow_mut().push(hk, key, new_val),
                    }
                    return Ok(());
                }
                // Struct index-assign dispatches `obj[k] = v` to `set_index(self, k, v)`; a compound
                // `+=`/`-=` reads the current element via `index` first.
                if let Value::Struct { .. } = &target_val {
                    let key = self.eval(index)?;
                    let new_val = match op.to_binop() {
                        None => self.eval(value)?,
                        Some(bin) => {
                            let cur = self.call_struct_method(
                                target_val.clone(),
                                "index",
                                vec![key.clone()],
                                span,
                            )?;
                            let rhs = self.eval(value)?;
                            eval_binary(bin, cur, rhs, span)?
                        }
                    };
                    self.call_struct_method(
                        target_val.clone(),
                        "set_index",
                        vec![key, new_val],
                        span,
                    )?;
                    return Ok(());
                }
                // Validate an int index at the assignment `span` (matches the VM's `SetIndex` span).
                let idx = match self.eval(index)? {
                    Value::Int(n) => n,
                    other => {
                        return Err(RuntimeError {
                            message: format!("expected int, found {}", other.type_name()),
                            span,
                        });
                    }
                };
                // `ba[i] = x` — the mutable byte buffer. Value must be an int 0..=255 (else a
                // recoverable fault); index must be in range. Mutation flows through the shared
                // `Rc<RefCell>` so a second binding observes it. Mirrors the VM's `set_index` arm.
                if let Value::ByteArray(buf) = &target_val {
                    let bounds_check = |idx: i64, len: usize| {
                        crate::slice::norm_index(idx, len).ok_or_else(|| RuntimeError {
                            message: format!("index {idx} out of bounds (len {len})"),
                            span,
                        })
                    };
                    let new_val = match op.to_binop() {
                        None => self.eval(value)?,
                        Some(bin) => {
                            let i = bounds_check(idx, buf.borrow().len())?;
                            let cur = Value::Int(buf.borrow()[i] as i64);
                            let rhs = self.eval(value)?;
                            eval_binary(bin, cur, rhs, span)?
                        }
                    };
                    let byte = match new_val {
                        Value::Int(n) if (0..=255).contains(&n) => n as u8,
                        Value::Int(n) => {
                            return Err(RuntimeError {
                                message: format!("byte value {n} out of range (must be 0..=255)"),
                                span,
                            });
                        }
                        other => {
                            return Err(RuntimeError {
                                message: format!("expected int, found {}", other.type_name()),
                                span,
                            });
                        }
                    };
                    let i = bounds_check(idx, buf.borrow().len())?;
                    buf.borrow_mut()[i] = byte;
                    return Ok(());
                }
                let Value::List(items) = &target_val else {
                    return Err(RuntimeError {
                        message: format!("cannot index {}", target_val.type_name()),
                        span,
                    });
                };
                // Bounds-checked index. Eval order mirrors the VM exactly: plain `=` pushes `value`
                // before `SetIndex` bounds-checks; compound `+=`/`-=` reads the current element
                // (`Dup2`+`GetIndex`, which bounds-checks) BEFORE `value`.
                let bounds_check = |idx: i64, len: usize| {
                    crate::slice::norm_index(idx, len).ok_or_else(|| RuntimeError {
                        message: format!("index {idx} out of bounds (len {len})"),
                        span,
                    })
                };
                let new_val = match op.to_binop() {
                    None => self.eval(value)?,
                    Some(bin) => {
                        let i = bounds_check(idx, items.borrow().len())?;
                        let cur = items.borrow()[i].clone();
                        let rhs = self.eval(value)?;
                        eval_binary(bin, cur, rhs, span)?
                    }
                };
                let i = bounds_check(idx, items.borrow().len())?;
                items.borrow_mut()[i] = new_val;
                Ok(())
            }
            // `p.x = v` — mutate a struct field in place (fields are `Rc<RefCell<…>>`).
            ExprKind::Field { obj, name } => {
                let target_val = self.eval(obj)?;
                let rhs = self.eval(value)?;
                let Value::Struct { fields, .. } = &target_val else {
                    return Err(RuntimeError {
                        message: format!(
                            "cannot assign field '{name}' of {}",
                            target_val.type_name()
                        ),
                        span,
                    });
                };
                let pos = fields.borrow().iter().position(|(k, _)| k == name);
                let Some(pos) = pos else {
                    return Err(RuntimeError {
                        message: format!("no field '{name}' on {target_val}"),
                        span,
                    });
                };
                let new_val = match op.to_binop() {
                    None => rhs,
                    Some(bin) => eval_binary(bin, fields.borrow()[pos].1.clone(), rhs, span)?,
                };
                fields.borrow_mut()[pos].1 = new_val;
                Ok(())
            }
            // `a, b = b, a` — multi-target tuple assignment (op is always `Eq`; the parser
            // enforces it). Evaluate the FULL RHS tuple FIRST into a temp `Vec` (Python semantics —
            // so a swap whose index appears on both sides is correct), then store each value into
            // its target left-to-right.
            ExprKind::Tuple(targets) => {
                // `value` is either a tuple literal (`b, a`) or any tuple-valued expression
                // (`f()` returning a tuple). Evaluate it FULLY first (Python semantics), then store
                // each element into its target left-to-right.
                let rhs = self.eval(value)?;
                let Value::Tuple(items) = &rhs else {
                    return Err(RuntimeError {
                        message: format!("cannot destructure {} in assignment", rhs.type_name()),
                        span,
                    });
                };
                if items.len() != targets.len() {
                    return Err(RuntimeError {
                        message: format!(
                            "assignment has {} target(s) but the value has {} element(s)",
                            targets.len(),
                            items.len()
                        ),
                        span,
                    });
                }
                let items: Vec<Value> = items.iter().cloned().collect();
                for (t, v) in targets.iter().zip(items) {
                    self.store_plain(t, v, span)?;
                }
                Ok(())
            }
            _ => Err(RuntimeError {
                message: "invalid assignment target".to_string(),
                span,
            }),
        }
    }

    /// Store an already-evaluated `value` into `target` with plain `=` semantics (no re-evaluation,
    /// no compound op). Used by multi-target tuple assignment, where the RHS is evaluated up front.
    fn store_plain(&mut self, target: &Expr, value: Value, span: Span) -> Result<(), RuntimeError> {
        match &target.kind {
            ExprKind::Ident(name) => {
                if !self.env.assign(name, value) {
                    return Err(RuntimeError {
                        message: format!("cannot assign to undefined name '{name}'"),
                        span,
                    });
                }
                Ok(())
            }
            ExprKind::Index { obj, index } => {
                let target_val = self.eval(obj)?;
                if let Value::Map(entries) = &target_val {
                    let key = self.eval(index)?;
                    let hk = self.hash_value(&key, span)?;
                    let pos = {
                        let m = entries.borrow();
                        m.candidates(hk)
                            .iter()
                            .copied()
                            .find(|&p| values_equal(&m.entries[p].1, &key))
                    };
                    match pos {
                        Some(i) => entries.borrow_mut().entries[i].2 = value,
                        None => entries.borrow_mut().push(hk, key, value),
                    }
                    return Ok(());
                }
                if let Value::Struct { .. } = &target_val {
                    let key = self.eval(index)?;
                    self.call_struct_method(
                        target_val.clone(),
                        "set_index",
                        vec![key, value],
                        span,
                    )?;
                    return Ok(());
                }
                let idx = match self.eval(index)? {
                    Value::Int(n) => n,
                    other => {
                        return Err(RuntimeError {
                            message: format!("expected int, found {}", other.type_name()),
                            span,
                        });
                    }
                };
                let Value::List(items) = &target_val else {
                    return Err(RuntimeError {
                        message: format!("cannot index {}", target_val.type_name()),
                        span,
                    });
                };
                let i = crate::slice::norm_index(idx, items.borrow().len()).ok_or_else(|| {
                    RuntimeError {
                        message: format!(
                            "index {idx} out of bounds (len {})",
                            items.borrow().len()
                        ),
                        span,
                    }
                })?;
                items.borrow_mut()[i] = value;
                Ok(())
            }
            ExprKind::Field { obj, name } => {
                let target_val = self.eval(obj)?;
                let Value::Struct { fields, .. } = &target_val else {
                    return Err(RuntimeError {
                        message: format!(
                            "cannot assign field '{name}' of {}",
                            target_val.type_name()
                        ),
                        span,
                    });
                };
                let pos = fields.borrow().iter().position(|(k, _)| k == name);
                let Some(pos) = pos else {
                    return Err(RuntimeError {
                        message: format!("no field '{name}' on {target_val}"),
                        span,
                    });
                };
                fields.borrow_mut()[pos].1 = value;
                Ok(())
            }
            _ => Err(RuntimeError {
                message: "invalid assignment target".to_string(),
                span,
            }),
        }
    }
}

/// Stack size for the interpreter thread. The tree-walk interpreter recurses on the host stack
/// (several frames per Chezzi call), so it runs on a dedicated large-stack thread; this decouples
/// the recursion limit from the caller's (possibly small, e.g. 2 MB test) thread stack. Sized so the
/// `MAX_CALL_DEPTH` (10_000) guard fires *before* the host stack overflows in **release** builds —
/// with headroom for per-frame growth (e.g. the `defer` bookkeeping added a few % per frame). Debug
/// frames are far larger and can still overflow at the limit (a pre-existing property, not specific
/// to `defer`).
const INTERP_STACK_BYTES: usize = 384 * 1024 * 1024;

/// A reserved, un-lexable binding name `wait`'s `=` arm uses to hand the received value to the shared
/// `exec_assign` path (so `wait` `=` semantics match a plain assignment). The NUL bytes guarantee no
/// user identifier can collide.
const WAIT_RECV_TMP: &str = "\u{0}wait-recv\u{0}";

/// ROOT REDESIGN — the interpreter's [`crate::json_decode::DecodeEnv`]: resolves a decode target and
/// its nested field struct types to qualified identity keys via the interp's per-module type tables.
/// Mirrors the compiler's `CompilerDecodeEnv` (parity — identical key derivation, identical errors).
struct InterpDecodeEnv<'a> {
    i: &'a Interp,
}

impl crate::json_decode::DecodeEnv for InterpDecodeEnv<'_> {
    fn resolve_bare(&self, module_idx: usize, name: &str) -> Option<String> {
        // In the CALL module, a bare name may be local / from-imported / std — use `bare_types`.
        // In any other (declaring) module, a nested field type resolves to that module's own type.
        if module_idx == self.i.cur_module_idx
            && let Some(k) = self.i.bare_types.get(name)
        {
            return Some(k.clone());
        }
        self.i
            .type_keys
            .get(&(module_idx, name.to_string()))
            .cloned()
    }

    fn resolve_qualified(&self, _module_idx: usize, binder: &str, name: &str) -> Option<String> {
        let tidx = *self.i.imported_module_idx.get(binder)?;
        if self
            .i
            .module_types
            .get(tidx)
            .is_some_and(|t| t.contains(name))
        {
            self.i.type_keys.get(&(tidx, name.to_string())).cloned()
        } else {
            None
        }
    }

    fn struct_def(&self, key: &str) -> Option<(usize, &[crate::ast::Field])> {
        let module_idx = *self.i.struct_module.get(key)?;
        let fields = self.i.struct_fields.get(key)?;
        Some((module_idx, fields.as_slice()))
    }

    fn display_of(&self, key: &str) -> String {
        crate::compiler::bare_display(key)
    }
}

/// Run a whole program from a source string, returning the output produced **so far** alongside
/// the outcome. The single-file test entry point — the CLI uses [`run_file`] so multi-file
/// programs work. Runs on a dedicated large-stack thread (see [`INTERP_STACK_BYTES`]).
#[cfg(test)]
pub fn run_program(src: &str) -> (String, Result<(), RuntimeError>) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(INTERP_STACK_BYTES)
        .spawn(move || run_program_inner(&src))
        .expect("failed to spawn interpreter thread")
        .join()
        .expect("interpreter thread panicked")
}

#[cfg(test)]
fn run_program_inner(src: &str) -> (String, Result<(), RuntimeError>) {
    let parsed = lexer::tokenize(src)
        .map_err(|e| RuntimeError {
            message: e.to_string(),
            span: Span { line: 1, col: 1 },
        })
        .and_then(|tokens| {
            parser::parse(tokens).map_err(|e| RuntimeError {
                message: e.message,
                span: e.span,
            })
        });
    let mut module = match parsed {
        Ok(m) => m,
        Err(e) => return (String::new(), Err(e)),
    };
    // Mirror the file-backed path: normalize named/default call arguments before evaluating.
    if let Err(e) = crate::desugar::run_standalone(&mut module) {
        return (
            String::new(),
            Err(RuntimeError {
                message: e.message,
                span: e.span,
            }),
        );
    }

    let mut interp = Interp::new();
    let result = interp
        .execute(&module.stmts)
        .and_then(|()| interp.drain_live_executors(Span { line: 1, col: 1 }));
    (interp.out, result)
}

/// Run a multi-file program from its entry path: resolve the dependency graph, evaluate each
/// module once in dependency order, then run the entry's `main()`. Output produced so far is
/// preserved alongside the outcome (so the CLI can print partial output before an error).
/// Convenience wrapper with the default (inert) host config. Test-only — the CLI uses
/// [`run_file_with`] to pass a process-backed config.
#[cfg(test)]
pub fn run_file(entry: &std::path::Path) -> RunOutput {
    run_file_with(entry, crate::native::HostConfig::default())
}

/// A finished run: captured `(stdout, stderr, outcome, exit_code)`. Stderr holds `std.io.eprint`
/// output. `exit_code` is `Some(n)` only when the program called `std.os.exit(n)` (a clean halt,
/// so `outcome` is `Ok`); `None` for a normal end or a runtime error.
pub type RunOutput = (String, String, Result<(), RunError>, Option<i32>);

/// Like [`run_file`], but with an explicit [`crate::native::HostConfig`] (args/env/stdin) for the
/// native std modules. The CLI passes a process-backed config; tests inject a deterministic one.
pub fn run_file_with(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(INTERP_STACK_BYTES)
        .spawn(move || run_file_inner(&entry, cfg))
        .expect("failed to spawn interpreter thread")
        .join()
        .expect("interpreter thread panicked")
}

fn run_file_inner(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    let graph = match crate::resolver::build_graph(entry) {
        Ok(g) => g,
        Err(e) => {
            return (
                String::new(),
                String::new(),
                Err(RunError::plain(RuntimeError {
                    message: e.message,
                    span: e.span,
                })),
                None,
            );
        }
    };
    let mut interp = Interp::new();
    interp.host = cfg;
    // FFI ROOT FIX (fix4): resolve every extern fn's C signature ONCE in the checker (module-scoped,
    // every alias spelling) and consume it at extern lowering — the same table + key the VM uses, so
    // the C ABI width is byte-identical across engines by construction. Keyed by graph module index.
    interp.extern_sigs = crate::checker::resolve_extern_signatures(&graph);
    // Gather every module's user type names (struct/enum/alias) so a `from`-imported type — which
    // carries no runtime value — is skipped at `bind_import` rather than erroring (module-scoped
    // types resolve through the program-global tables by name). ROOT REDESIGN — assign each user type
    // the canonical IDENTITY KEY `<module-key>::Name` (via the shared `resolver::module_keys`, the
    // SAME derivation the compiler + checker use), so all three engines agree on every key (parity).
    interp.module_types = vec![std::collections::HashSet::new(); graph.modules.len()];
    let mkeys = crate::resolver::module_keys(&graph);
    for (idx, lm) in graph.modules.iter().enumerate() {
        interp.module_idx_of.insert(lm.id.clone(), idx);
        if lm.native.is_some() {
            continue;
        }
        // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (skip the
        // qualified key → `type_key` falls back to bare), so `Ref`/`Iterator`/FFI widths resolve bare.
        let is_std = lm.is_std();
        for s in &lm.ast.stmts {
            if let StmtKind::Struct { name, .. }
            | StmtKind::Enum { name, .. }
            | StmtKind::NewType { name, .. }
            | StmtKind::TypeAlias { name, .. } = &s.kind
            {
                interp.type_names.insert(name.clone());
                interp.module_types[idx].insert(name.clone());
                if !is_std {
                    interp
                        .type_keys
                        .insert((idx, name.clone()), format!("{}::{name}", mkeys[idx]));
                }
            }
        }
    }
    // Modules are in load order: dependencies first, entry last.
    for lm in &graph.modules {
        if let Err(e) = interp.eval_module(lm) {
            // A pending exit means the `Err` is the `exit()` unwind sentinel, not a fault: report
            // the requested code as a clean halt.
            if let Some(code) = interp.pending_exit {
                return (interp.out, interp.stderr, Ok(()), Some(code));
            }
            // On an uncaught fault, `call_stack` holds the chain (outermost first, frames pop only on
            // success); reverse to innermost-first for the trace.
            let trace: Vec<TraceFrame> = interp.call_stack.iter().rev().cloned().collect();
            return (
                interp.out,
                interp.stderr,
                Err(RunError::from_error(e, trace)),
                None,
            );
        }
    }
    // Clean end: gracefully reap any Executor never explicitly shut down (C5 / A2). Skipped on a
    // hard `std.os.exit` (handled inside `drain_live_executors`).
    if let Err(e) = interp.drain_live_executors(Span { line: 1, col: 1 }) {
        if let Some(code) = interp.pending_exit {
            return (interp.out, interp.stderr, Ok(()), Some(code));
        }
        let trace: Vec<TraceFrame> = interp.call_stack.iter().rev().cloned().collect();
        return (
            interp.out,
            interp.stderr,
            Err(RunError::from_error(e, trace)),
            None,
        );
    }
    (interp.out, interp.stderr, Ok(()), None)
}

/// Run a whole program and return its complete stdout, or the error if it didn't finish.
/// Test helper; the CLI uses [`run_program`] so it can print partial output before an error.
#[cfg(test)]
pub fn run_capture(src: &str) -> Result<String, RuntimeError> {
    let (out, result) = run_program(src);
    result.map(|()| out)
}

/// Evaluate a single Chezzi expression from source (test helper).
#[cfg(test)]
pub fn eval_str(src: &str) -> Result<Value, RuntimeError> {
    let expr = parse_expr_str(src)?;
    Interp::new().eval(&expr)
}

/// Lex + parse one expression from source.
fn parse_expr_str(src: &str) -> Result<Expr, RuntimeError> {
    let tokens = lexer::tokenize(src).map_err(|e| RuntimeError {
        message: e.to_string(),
        span: Span { line: 1, col: 1 },
    })?;
    let mut expr = parser::parse_expr(tokens).map_err(|e| RuntimeError {
        message: e.message,
        span: e.span,
    })?;
    // Fragments bypass the module-wide desugar pass; lower `?.`/`??` carriers here (both engines do).
    crate::desugar::lower_carriers(&mut expr);
    Ok(expr)
}

/// The interpreter's [`crate::native::Host`] adapter: lets a native fn read the evaluated `Value`
/// arguments and write to the captured output buffers. Borrows only the fields it needs so it can
/// be built inside an `&mut self` method. (Stdin / args / env / cooperative-exit are wired in a
/// later milestone; the unwired methods return inert defaults — empty stdin/env, real cwd.)
struct InterpHost<'a> {
    args: Vec<Value>,
    out: &'a mut String,
    stderr: &'a mut String,
    cfg: &'a mut crate::native::HostConfig,
    exit: &'a mut Option<i32>,
}

/// The callback-capable host for `call_cffi` (callbacks #4): unlike [`InterpHost`]'s split borrows,
/// it holds `&mut Interp`, so a `fn(...)`-typed extern param's trampoline can RE-ENTER the engine via
/// `Interp::call_value` to run the Chezzi closure. All other `Host` methods route through the same
/// Interp fields `InterpHost` borrows (`out`/`stderr`/`host`/`pending_exit`), so behavior is
/// byte-identical (the two-engine parity tests assert it). `span` is the extern call site (used to
/// attribute a callback runtime error).
struct InterpCallbackHost<'a> {
    interp: &'a mut Interp,
    args: Vec<Value>,
    span: Span,
}

impl crate::native::Host for InterpCallbackHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "int",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        matches!(self.args.get(i), Some(Value::Int(_)))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Float(f)) => Ok(*f),
            Some(Value::Int(n)) => Ok(*n as f64),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "float",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "bool",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Ptr(a)) => Ok(*a),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "ptr",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_struct_fields(
        &mut self,
        i: usize,
    ) -> Result<Vec<crate::native::NativeRet>, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Struct { fields, .. }) => {
                let fields = fields.borrow();
                let mut out = Vec::with_capacity(fields.len());
                for (_, v) in fields.iter() {
                    out.push(value_to_native_scalar(v, i)?);
                }
                Ok(out)
            }
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "struct",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn invoke_callback(
        &mut self,
        arg_index: usize,
        args: &[crate::native::NativeRet],
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        // Re-enter the engine to run the closure passed as extern arg `arg_index` on the C scalars,
        // then lower its result back to a NativeRet. Synchronous, same-thread: plain Rust-stack
        // recursion through `call_value` (the dispatch map/closure calls already use).
        let callee = self
            .args
            .get(arg_index)
            .cloned()
            .ok_or_else(|| crate::native::HostError {
                message: format!("callback argument {arg_index} is missing"),
            })?;
        let vals: Vec<Value> = args.iter().cloned().map(lower_native).collect();
        let result = self
            .interp
            .call_value(callee, vals, self.span)
            .map_err(|e| crate::native::HostError { message: e.message })?;
        // A callback return is checker-restricted to a C scalar; a non-scalar is a defended case.
        Ok(value_to_native_scalar(&result, arg_index).unwrap_or(crate::native::NativeRet::Int(0)))
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Str(s)) => Ok(s.to_string()),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "str",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Map(m)) => {
                let m = m.borrow();
                let mut pairs = Vec::with_capacity(m.entries.len());
                for (_, k, v) in &m.entries {
                    let (Value::Str(ks), Value::Str(vs)) = (k, v) else {
                        return Err(crate::native::HostError::arg_type(
                            i,
                            "map[str, str]",
                            "other",
                        ));
                    };
                    pairs.push((ks.to_string(), vs.to_string()));
                }
                Ok(pairs)
            }
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "map[str, str]",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn write_stdout(&mut self, s: &str) {
        self.interp.out.push_str(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.interp.stderr.push_str(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.interp.host.stdin.read_line()
    }
    fn os_args(&self) -> Vec<String> {
        self.interp.host.args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        self.interp.host.env.get(key).cloned()
    }
    fn os_getcwd(&self) -> Result<String, crate::native::HostError> {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .map_err(|e| crate::native::HostError {
                message: e.to_string(),
            })
    }
    fn request_exit(&mut self, code: i64) {
        self.interp.pending_exit = Some(code.clamp(0, 255) as i32);
    }
}

/// Map a scalar [`Value`] to an engine-neutral [`crate::native::NativeRet`] scalar — the field
/// values of a by-value struct extern arg. Only the C-marshallable scalar variants are reachable
/// (the checker rejects str/nested-struct fields), so a non-scalar is a defended type error.
fn value_to_native_scalar(
    v: &Value,
    i: usize,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    use crate::native::NativeRet as N;
    match v {
        Value::Int(n) => Ok(N::Int(*n)),
        Value::Float(f) => Ok(N::Float(*f)),
        Value::Bool(b) => Ok(N::Bool(*b)),
        Value::Ptr(a) => Ok(N::Ptr(*a)),
        other => Err(crate::native::HostError::arg_type(
            i,
            "struct scalar field",
            other.type_name(),
        )),
    }
}

impl crate::native::Host for InterpHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "int",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        matches!(self.args.get(i), Some(Value::Int(_)))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Float(f)) => Ok(*f),
            Some(Value::Int(n)) => Ok(*n as f64),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "float",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "bool",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Ptr(a)) => Ok(*a),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "ptr",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_struct_fields(
        &mut self,
        i: usize,
    ) -> Result<Vec<crate::native::NativeRet>, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Struct { fields, .. }) => {
                // Fields are in declaration order; map each scalar field value to a NativeRet so the
                // cffi layer can cast it to its C field width. (The checker guarantees flat scalars.)
                let fields = fields.borrow();
                let mut out = Vec::with_capacity(fields.len());
                for (_, v) in fields.iter() {
                    out.push(value_to_native_scalar(v, i)?);
                }
                Ok(out)
            }
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "struct",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Str(s)) => Ok(s.to_string()),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "str",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Map(m)) => {
                // Iterate `entries` (insertion order) so header order matches the VM + off-heap
                // hosts exactly. Every key/value must be a str.
                let m = m.borrow();
                let mut pairs = Vec::with_capacity(m.entries.len());
                for (_, k, v) in &m.entries {
                    let (Value::Str(ks), Value::Str(vs)) = (k, v) else {
                        return Err(crate::native::HostError::arg_type(
                            i,
                            "map[str, str]",
                            "other",
                        ));
                    };
                    pairs.push((ks.to_string(), vs.to_string()));
                }
                Ok(pairs)
            }
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "map[str, str]",
                other.type_name(),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn write_stdout(&mut self, s: &str) {
        self.out.push_str(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.stderr.push_str(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.cfg.stdin.read_line()
    }
    fn os_args(&self) -> Vec<String> {
        self.cfg.args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        self.cfg.env.get(key).cloned()
    }
    fn os_getcwd(&self) -> Result<String, crate::native::HostError> {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .map_err(|e| crate::native::HostError {
                message: e.to_string(),
            })
    }
    fn request_exit(&mut self, code: i64) {
        *self.exit = Some(code.clamp(0, 255) as i32);
    }
}

/// Materialize a (non-range, non-struct-iterator) iterable into per-row binding tuples: one value
/// per row for list/set/str, and `[k]` or `[k, v]` for a map depending on the loop-variable count.
/// Shared by `exec_for`, the comprehension driver, and the collection constructors so all iterate
/// every collection identically.
fn iter_rows_from_value(
    iter_val: &Value,
    vars: usize,
    span: Span,
) -> Result<Vec<Vec<Value>>, RuntimeError> {
    Ok(match iter_val {
        // Over a list with >1 loop var, each element is a tuple to destructure into a row (the
        // checker guarantees the element is a tuple of matching arity). One var → whole-element row.
        Value::List(items) => items
            .borrow()
            .iter()
            .map(|v| match v {
                // A tuple with FEWER elements than loop vars is an arity error — matching the VM,
                // which fails on the first missing `GetField`. (The checker catches this statically
                // unless the element type was `Unknown`, e.g. an empty/unannotated list.)
                Value::Tuple(t) if vars > 1 && t.len() < vars => Err(RuntimeError {
                    message: format!("tuple has no element '.{}' (len {})", t.len(), t.len()),
                    span,
                }),
                Value::Tuple(t) if vars > 1 => Ok(t.iter().cloned().collect()),
                _ => Ok(vec![v.clone()]),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Value::Map(m) => m
            .borrow()
            .entries
            .iter()
            .map(|(_, k, v)| {
                if vars == 2 {
                    vec![k.clone(), v.clone()]
                } else {
                    vec![k.clone()]
                }
            })
            .collect(),
        Value::Set(s) => s
            .borrow()
            .entries
            .iter()
            .map(|(_, e)| vec![e.clone()])
            .collect(),
        // Strings iterate as 1-char strings (Python-style; the checker binds a single str var).
        Value::Str(s) => s
            .chars()
            .map(|c| vec![Value::Str(c.to_string().into())])
            .collect(),
        // `bytes`/`bytearray` iterate as `int`s (0–255). The `bytearray` snapshots its bytes so a
        // mutation during the loop does not change the iteration sequence (matches the VM).
        Value::Bytes(b) => b.iter().map(|&x| vec![Value::Int(x as i64)]).collect(),
        Value::ByteArray(b) => b
            .borrow()
            .iter()
            .map(|&x| vec![Value::Int(x as i64)])
            .collect(),
        // A cursor yields its REMAINING snapshot (`items[pos..]`) as single-var rows — drains a
        // `for x in pure_iterable_struct` (lowered to a cursor) and `list(xs.iter())`.
        Value::Iter(c) => {
            let c = c.borrow();
            c.items[c.pos.min(c.items.len())..]
                .iter()
                .map(|v| vec![v.clone()])
                .collect()
        }
        other => {
            return Err(RuntimeError {
                message: format!("cannot iterate over {}", other.type_name()),
                span,
            });
        }
    })
}

/// UTF-8 decode a byte slice into a new `str` value. Invalid UTF-8 maps to a RECOVERABLE
/// RuntimeError (catchable by `recover:`), not a panic — the message is byte-identical to the VM's
/// `decode_utf8` so the two engines stay parity-equal. UTF-8 only (no encoding-name argument).
fn decode_utf8(bytes: &[u8], span: Span) -> Result<Value, RuntimeError> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(Value::Str(s.into())),
        Err(_) => Err(RuntimeError {
            message: "invalid UTF-8 in decode()".to_string(),
            span,
        }),
    }
}

/// Render an extern param/return source `Type` for a marshallability error message (the surface
/// spelling the user wrote). Twin of the compiler's `ffi_type_display` (parity); used only by the
/// never-panic backstop in the extern marshal loop.
fn ffi_type_display(ty: Option<&crate::ast::Type>) -> String {
    match ty {
        Some(crate::ast::Type::Named(n)) => n.clone(),
        Some(crate::ast::Type::Qualified { module, name, .. }) => format!("{module}.{name}"),
        Some(crate::ast::Type::Generic(n, _)) => n.clone(),
        Some(_) => "<unsupported>".to_string(),
        None => "nil".to_string(),
    }
}

// The interp's standalone-path `ctype_of`/`ctype_of_visiting` second resolver was DELETED in fix5
// (the single-resolver redesign); extern C types now come VERBATIM from the checker's
// `resolve_extern_signatures{,_standalone}`, the ONLY extern-type resolver.

/// Lower a native fn's engine-neutral [`crate::native::NativeRet`] into an interpreter `Value`.
/// `Ok`/`Err`/`Some`/`None` become the built-in `Result` / `Option` enum values.
fn lower_native(ret: crate::native::NativeRet) -> Value {
    use crate::native::NativeRet as N;
    match ret {
        N::Int(n) => Value::Int(n),
        N::Float(f) => Value::Float(f),
        N::Bool(b) => Value::Bool(b),
        N::Ptr(a) => Value::Ptr(a),
        N::Str(s) => Value::Str(s.into()),
        N::List(items) => {
            let vs = items.into_iter().map(lower_native).collect();
            Value::List(std::rc::Rc::new(std::cell::RefCell::new(vs)))
        }
        N::Struct { name, fields } => {
            let fs = fields
                .into_iter()
                .map(|(k, v)| (k, lower_native(v)))
                .collect();
            Value::Struct {
                name: name.into(),
                fields: std::rc::Rc::new(std::cell::RefCell::new(fs)),
            }
        }
        N::Map(entries) => {
            // Native maps have unique scalar (str) keys — hash directly, no dedup needed.
            let mut map = MapData::default();
            for (k, v) in entries {
                let lk = lower_native(k);
                let hk = scalar_hash(&lk);
                map.push(hk, lk, lower_native(v));
            }
            Value::Map(std::rc::Rc::new(std::cell::RefCell::new(map)))
        }
        N::Ok(inner) => enum_val("Result", "Ok", vec![lower_native(*inner)]),
        N::Err(msg) => enum_val("Result", "Err", vec![Value::Str(msg.into())]),
        N::Some(inner) => enum_val("Option", "Some", vec![lower_native(*inner)]),
        N::None => enum_val("Option", "None", Vec::new()),
        N::Nil => Value::Nil,
    }
}

fn enum_val(ty: &str, variant: &str, payload: Vec<Value>) -> Value {
    Value::Enum {
        ty: ty.into(),
        variant: variant.into(),
        payload,
    }
}

/// The `f64` of a JSON `Num` variant's payload, else `None` (used by `json.decode` coercion).
fn json_num(variant: &str, payload: &[Value]) -> Option<f64> {
    if variant == "Num" {
        match payload.first() {
            Some(Value::Float(f)) => Some(*f),
            Some(Value::Int(n)) => Some(*n as f64),
            _ => None,
        }
    } else {
        None
    }
}

/// Apply a binary operator to two already-evaluated operands.
fn eval_binary(op: BinaryOp, l: Value, r: Value, span: Span) -> Result<Value, RuntimeError> {
    use BinaryOp::*;
    use Value::{Float, Int, Str};

    let type_err = |op: BinaryOp, l: &Value, r: &Value| RuntimeError {
        message: format!(
            "cannot apply {op:?} to {} and {}",
            l.type_name(),
            r.type_name()
        ),
        span,
    };

    match op {
        Add | Sub | Mul | Div | Mod => match (&l, &r) {
            (Int(a), Int(b)) => {
                let v = match op {
                    Add => a.checked_add(*b),
                    Sub => a.checked_sub(*b),
                    Mul => a.checked_mul(*b),
                    Div | Mod if *b == 0 => {
                        return Err(RuntimeError {
                            message: format!(
                                "{} by zero",
                                if op == Div { "division" } else { "modulo" }
                            ),
                            span,
                        });
                    }
                    Div => a.checked_div(*b),
                    Mod => a.checked_rem(*b),
                    _ => unreachable!(),
                };
                v.map(Int).ok_or(RuntimeError {
                    message: format!("integer overflow in {op:?}"),
                    span,
                })
            }
            // promote to float if either side is float
            (Int(_) | Float(_), Int(_) | Float(_)) => {
                let a = as_f64(&l);
                let b = as_f64(&r);
                // Like the integer path (and Python), division/modulo by zero is an error rather
                // than a silent `inf`/`nan`.
                if matches!(op, Div | Mod) && b == 0.0 {
                    return Err(RuntimeError {
                        message: format!(
                            "{} by zero",
                            if op == Div { "division" } else { "modulo" }
                        ),
                        span,
                    });
                }
                Ok(Float(match op {
                    Add => a + b,
                    Sub => a - b,
                    Mul => a * b,
                    Div => a / b,
                    Mod => a % b,
                    _ => unreachable!(),
                }))
            }
            (Str(a), Str(b)) if op == Add => Ok(Str(format!("{a}{b}").into())),
            _ => Err(type_err(op, &l, &r)),
        },
        Lt | LtEq | Gt | GtEq => {
            let ord = compare(&l, &r).ok_or_else(|| type_err(op, &l, &r))?;
            Ok(Value::Bool(match op {
                Lt => ord.is_lt(),
                LtEq => ord.is_le(),
                Gt => ord.is_gt(),
                GtEq => ord.is_ge(),
                _ => unreachable!(),
            }))
        }
        // Bitwise/shift ops — int-only (gap #13). The checker rejects non-int operands; this is the
        // runtime fallback.
        BitAnd | BitOr | BitXor | Shl | Shr => match (&l, &r) {
            (Int(a), Int(b)) => {
                let v = match op {
                    BitAnd => a & b,
                    BitOr => a | b,
                    BitXor => a ^ b,
                    Shl | Shr => {
                        if *b < 0 || *b >= 64 {
                            return Err(RuntimeError {
                                message: format!("shift amount {b} out of range (0..64)"),
                                span,
                            });
                        }
                        if op == Shl {
                            // Left shift can overflow (drop high bits) like `+ - * / %`; treat
                            // it as a recoverable fault, not a silent wrap. Round-trip test:
                            // `(a << b) >> b == a` holds iff no significant bit was shifted out
                            // (correct for negative operands too — `-1 << 63` round-trips).
                            let v = a << (*b as u32);
                            if (v >> (*b as u32)) != *a {
                                return Err(RuntimeError {
                                    message: format!("integer overflow in {op:?}"),
                                    span,
                                });
                            }
                            v
                        } else {
                            a >> (*b as u32)
                        }
                    }
                    _ => unreachable!(),
                };
                Ok(Int(v))
            }
            _ => Err(type_err(op, &l, &r)),
        },
        Eq => Ok(Value::Bool(values_equal_guarded(&l, &r, 0, span)?)),
        NotEq => Ok(Value::Bool(!values_equal_guarded(&l, &r, 0, span)?)),
        // `x in container` — membership. Type-directed by the checker; this is the runtime. A
        // free fn (no `&self`/hasher), so containers are scanned linearly with the same
        // `values_equal_guarded` the VM's `candidates` scan ultimately uses — identical result.
        In => match &r {
            Value::List(items) => {
                let items = items.borrow();
                for e in items.iter() {
                    if values_equal_guarded(e, &l, 0, span)? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            Value::Set(set) => {
                let set = set.borrow();
                for (_, e) in set.entries.iter() {
                    if values_equal_guarded(e, &l, 0, span)? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            // Map `in` tests KEY membership (Python-style).
            Value::Map(map) => {
                let map = map.borrow();
                for (_, k, _) in map.entries.iter() {
                    if values_equal_guarded(k, &l, 0, span)? {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            Str(hay) => match &l {
                Str(sub) => Ok(Value::Bool(hay.contains(sub.as_ref()))),
                _ => Err(RuntimeError {
                    message: format!(
                        "substring `in` requires a str on the left, found {}",
                        l.type_name()
                    ),
                    span,
                }),
            },
            _ => Err(RuntimeError {
                message: format!("cannot use `in` on {}", r.type_name()),
                span,
            }),
        },
        And | Or => Err(RuntimeError {
            message: "logical operators are handled before evaluation".to_string(),
            span,
        }),
    }
}

/// Require a boolean value (operand of `and`/`or`/`not`, condition of `if`/`while`).
fn as_bool(v: Value, span: Span) -> Result<bool, RuntimeError> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(RuntimeError {
            message: format!("expected bool, found {}", other.type_name()),
            span,
        }),
    }
}

fn as_int_val(v: Value, span: Span) -> Result<i64, RuntimeError> {
    match v {
        Value::Int(n) => Ok(n),
        other => Err(RuntimeError {
            message: format!("expected int, found {}", other.type_name()),
            span,
        }),
    }
}

/// If `v` is an unhandled error (`Err(..)` or `None`) reaching the top level, build the runtime
/// error that exits the program. Mirrors the VM's `top_level_error` — keep the message identical.
fn top_level_error(v: &Value, span: Span) -> Option<RuntimeError> {
    let Value::Enum {
        ty,
        variant,
        payload,
    } = v
    else {
        return None;
    };
    // Builtin `Result`/`Option` only — a user enum that shadows `Err`/`None` is a normal value.
    let unhandled = (ty.as_ref() == "Result" && variant.as_ref() == "Err")
        || (ty.as_ref() == "Option" && variant.as_ref() == "None");
    if !unhandled {
        return None;
    }
    let detail = match payload.first() {
        Some(p) => p.to_string(),
        None => v.to_string(),
    };
    Some(RuntimeError {
        message: format!("unhandled error: {detail}"),
        span,
    })
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(n) => *n as f64,
        Value::Float(f) => *f,
        _ => unreachable!("as_f64 on non-numeric"),
    }
}

/// Ordering for `< <= > >=`. `None` ⇒ operands aren't comparable.
fn compare(l: &Value, r: &Value) -> Option<std::cmp::Ordering> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        // `bytes`/`bytearray` order lexicographically by byte (Python parity), incl. cross-type.
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
        (Value::ByteArray(a), Value::ByteArray(b)) => Some(a.borrow().cmp(&b.borrow())),
        (Value::Bytes(a), Value::ByteArray(b)) => Some(a.as_ref().cmp(b.borrow().as_slice())),
        (Value::ByteArray(a), Value::Bytes(b)) => Some(a.borrow().as_slice().cmp(b.as_ref())),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            as_f64(l).partial_cmp(&as_f64(r))
        }
        _ => None,
    }
}

/// Structural equality for `==` / `!=`. Cross-type compares are simply unequal.
/// Infallible hash for scalar keys (int/float/bool/nil/str). Numeric values hash by canonical f64
/// bits so `3` and `3.0` collide; str by content. Non-scalar values fall back to `0` (a
/// correctness-safe degenerate hash — `values_equal` still confirms each probe). Mirrors
/// `vm::Vm::scalar_hash`.
pub(super) fn scalar_hash(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    match v {
        // Normalise zero so `Int(0)`, `+0.0`, and `-0.0` (all `values_equal`) hash identically —
        // `(-0.0).to_bits() != (0.0).to_bits()` would otherwise break the hash invariant.
        Value::Int(n) => (if *n == 0 { 0.0 } else { *n as f64 }).to_bits(),
        Value::Float(f) => (if *f == 0.0 { 0.0 } else { *f }).to_bits(),
        Value::Bool(b) => *b as u64,
        Value::Nil => 0,
        Value::Str(s) => {
            let mut hr = std::collections::hash_map::DefaultHasher::new();
            s.as_bytes().hash(&mut hr);
            hr.finish()
        }
        // `bytes` is Hashable (immutable, value-compared). Hash the raw slice.
        Value::Bytes(b) => {
            let mut hr = std::collections::hash_map::DefaultHasher::new();
            b.as_ref().hash(&mut hr);
            hr.finish()
        }
        _ => 0,
    }
}

/// Insert-or-overwrite `(h, k, v)` into `map`: if `k` already exists, replace its value (last write
/// wins — used by `merge`/`update` so the incoming map wins on a key clash); else append. The hash
/// `h` is the key's cached hash (engine-wide consistent, so reusing the source map's hash is sound).
fn map_upsert(map: &mut MapData, h: u64, k: Value, v: Value) {
    let pos = map
        .candidates(h)
        .iter()
        .copied()
        .find(|&p| values_equal(&map.entries[p].1, &k));
    match pos {
        Some(p) => map.entries[p].2 = v,
        None => map.push(h, k, v),
    }
}

pub(super) fn values_equal(l: &Value, r: &Value) -> bool {
    values_equal_guarded(l, r, 0, Span { line: 1, col: 1 }).unwrap_or(false)
}

/// Structural equality with a contained recursion-depth guard (Bug A). Recursive container kinds are
/// handled EXPLICITLY and recurse via `values_equal_guarded(.., depth + 1, span)?`; a cyclic structure
/// trips `MAX_STRUCTURAL_DEPTH` and returns a recoverable `RuntimeError` instead of overflowing the
/// host stack. Only the language `==`/`!=` op site surfaces the error (`?`); every other caller goes
/// through the boolean `values_equal` wrapper (which maps the error to `false`), so the invariant
/// `values_equal(a, b) ⇒ hash(a) == hash(b)` is preserved for non-pathological data. Map and Set arms
/// are order-INDEPENDENT (Bug B).
pub(super) fn values_equal_guarded(
    l: &Value,
    r: &Value,
    depth: usize,
    span: Span,
) -> Result<bool, RuntimeError> {
    if depth > MAX_STRUCTURAL_DEPTH {
        return Err(RuntimeError {
            message: "maximum structural depth (10000) exceeded (cyclic data structure?)"
                .to_string(),
            span,
        });
    }
    match (l, r) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            Ok(as_f64(l) == as_f64(r))
        }
        (Value::List(a), Value::List(b)) => {
            if std::rc::Rc::ptr_eq(a, b) {
                return Ok(true); // identity fast-path (mirrors the VM's `ha == hb`)
            }
            let (a, b) = (a.borrow(), b.borrow());
            if a.len() != b.len() {
                return Ok(false);
            }
            for (x, y) in a.iter().zip(b.iter()) {
                if !values_equal_guarded(x, y, depth + 1, span)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Tuple(a), Value::Tuple(b)) => {
            if std::rc::Rc::ptr_eq(a, b) {
                return Ok(true); // identity fast-path (mirrors the VM's `ha == hb`)
            }
            if a.len() != b.len() {
                return Ok(false);
            }
            for (x, y) in a.iter().zip(b.iter()) {
                if !values_equal_guarded(x, y, depth + 1, span)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        // Maps are unordered: equal iff same size and every (key, value) of one has a matching
        // (key, value) in the other (order-INDEPENDENT — Bug B).
        (Value::Map(a), Value::Map(b)) => {
            if std::rc::Rc::ptr_eq(a, b) {
                return Ok(true); // identity fast-path (mirrors the VM's `ha == hb`)
            }
            let (a, b) = (a.borrow(), b.borrow());
            if a.entries.len() != b.entries.len() {
                return Ok(false);
            }
            for (_, ka, va) in a.entries.iter() {
                let mut found = false;
                for (_, kb, vb) in b.entries.iter() {
                    if values_equal_guarded(ka, kb, depth + 1, span)?
                        && values_equal_guarded(va, vb, depth + 1, span)?
                    {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        // Sets are unordered: equal iff same size and every element of one is in the other.
        (Value::Set(a), Value::Set(b)) => {
            if std::rc::Rc::ptr_eq(a, b) {
                return Ok(true); // identity fast-path (mirrors the VM's `ha == hb`)
            }
            let (a, b) = (a.borrow(), b.borrow());
            if a.entries.len() != b.entries.len() {
                return Ok(false);
            }
            for (_, x) in a.entries.iter() {
                let mut found = false;
                for (_, y) in b.entries.iter() {
                    if values_equal_guarded(x, y, depth + 1, span)? {
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (
            Value::Struct {
                name: na,
                fields: fa,
            },
            Value::Struct {
                name: nb,
                fields: fb,
            },
        ) => {
            if na == nb && std::rc::Rc::ptr_eq(fa, fb) {
                return Ok(true); // identity fast-path (mirrors the VM's `ha == hb`)
            }
            if na != nb {
                return Ok(false);
            }
            let (fa, fb) = (fa.borrow(), fb.borrow());
            if fa.len() != fb.len() {
                return Ok(false);
            }
            for ((ka, va), (kb, vb)) in fa.iter().zip(fb.iter()) {
                if ka != kb || !values_equal_guarded(va, vb, depth + 1, span)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (
            Value::Enum {
                ty: ta,
                variant: va,
                payload: pa,
            },
            Value::Enum {
                ty: tb,
                variant: vb,
                payload: pb,
            },
        ) => {
            if ta != tb || va != vb || pa.len() != pb.len() {
                return Ok(false);
            }
            for (x, y) in pa.iter().zip(pb.iter()) {
                if !values_equal_guarded(x, y, depth + 1, span)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        // Two newtypes are equal iff SAME key and structurally-equal inner (recurse so numeric
        // unification — Int(3)==Float(3.0) — matches the VM). A different key is a distinct type.
        (
            Value::NewType {
                type_key: ka,
                inner: ia,
            },
            Value::NewType {
                type_key: kb,
                inner: ib,
            },
        ) => {
            if ka != kb {
                return Ok(false);
            }
            values_equal_guarded(ia, ib, depth + 1, span)
        }
        // Cross-type `bytes == bytearray` is content-equal (Python parity: `b"a" == bytearray(b"a")`
        // is true). Same-type `Bytes`/`ByteArray` equality falls through to the derived `PartialEq`
        // in the default arm (which compares contents).
        (Value::Bytes(a), Value::ByteArray(b)) => Ok(a.as_ref() == b.borrow().as_slice()),
        (Value::ByteArray(a), Value::Bytes(b)) => Ok(a.borrow().as_slice() == b.as_ref()),
        // Non-recursive kinds (Bool, Str, Nil, Func, Closure, Module, Native, Channel/Shared/Executor
        // handles) — derived `PartialEq` does not recurse through user data here.
        _ => Ok(l == r),
    }
}

/// The "no arm matched" runtime error message (a checker-prevented, non-exhaustive case). For an
/// enum it names the variant — matching the VM's `MatchNoArm` wording for parity; otherwise the
/// scrutinee's type.
fn no_match_arm_message(value: &Value) -> String {
    match value {
        Value::Enum { variant, .. } => format!("no match arm for variant '{variant}'"),
        other => format!("no match arm for {}", other.type_name()),
    }
}

/// Does a literal pattern match a runtime value, by value (int/str/bool)?
fn literal_matches(lit: &LitPattern, value: &Value) -> bool {
    match (lit, value) {
        (LitPattern::Int(n), Value::Int(v)) => n == v,
        (LitPattern::Str(s), Value::Str(v)) => s.as_str() == v.as_ref(),
        (LitPattern::Bool(b), Value::Bool(v)) => b == v,
        _ => false,
    }
}

/// Whether a top-level arm pattern requires the scrutinee to be an enum (so a non-enum value is a
/// clean "cannot match on …" error rather than silently falling through). Only a variant *with a
/// payload* (`Some(x)`) qualifies; literals/wildcards/tuples don't, and a bare identifier is
/// ambiguous (it may be a binding capturing a literal value), so it doesn't force the enum guard.
fn pattern_needs_enum(pattern: &Pattern) -> bool {
    // Only a payload-bearing variant unambiguously requires an enum scrutinee. A bare identifier
    // (empty bindings) is ambiguous — it may be a binding capturing a literal value — so it doesn't
    // force the enum guard (`try_bind` handles a bare ident against a non-enum as a binding).
    matches!(pattern, Pattern::Variant { bindings, .. } if !bindings.is_empty())
}

/// Try to match `value` against `pattern`, returning the name→value bindings to install on success,
/// or `None` on a mismatch. Recurses through nested tuple/variant patterns (gap #15). The program is
/// type-checked, so a shape mismatch here is a genuine value mismatch (a different variant / a
/// non-matching literal / a different tuple shape), not a type error.
/// Resolve a variant reference to its `(enum, variant)` registry key. The enum is the explicit
/// qualifier when present (user variants are always qualified post-check); a bare name resolves only
/// as a built-in (`Ok`/`Err`→`Result`, `Some`/`None`→`Option`). `None` for any other bare name.
fn variant_pair(enum_name: Option<&str>, name: &str) -> Option<(String, String)> {
    let en = match enum_name {
        Some(en) => en,
        None => match name {
            "Ok" | "Err" => "Result",
            "Some" | "None" => "Option",
            _ => return None,
        },
    };
    Some((en.to_string(), name.to_string()))
}

impl Interp {
    /// Attempt to bind `pattern` against `value`, returning the captured bindings on a match. A
    /// method (not a free fn) so a *qualified* variant pattern can resolve its bare written enum name
    /// to the module-scoped runtime key (`enum_bare_key`, against the CURRENT module context which the
    /// callee's frame established) — so a disambiguated enum's variants MATCH (Blocker C). In the
    /// no-collision common case the key resolves back to the bare name, unchanged.
    fn try_bind(&self, pattern: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
        let variants = &self.variants;
        match pattern {
            Pattern::Wildcard => Some(Vec::new()),
            Pattern::Ident(name) => {
                // A bare nested identifier naming a *built-in* NULLARY variant (`None`) is a
                // refutable variant match, binding nothing: it matches iff the value IS that variant
                // of that built-in enum (mirrors the VM's pure-int variant_id routing, which keys on
                // the enum-scoped id). A non-variant name is a binding capturing the whole sub-value.
                if let Some((en, _)) = variant_pair(None, name)
                    .filter(|k| variants.get(k).is_some_and(|d| d.arity == 0))
                {
                    return match value {
                        Value::Enum {
                            ty,
                            variant,
                            payload,
                        } if ty.as_ref() == en
                            && variant.as_ref() == name
                            && payload.is_empty() =>
                        {
                            Some(Vec::new())
                        }
                        _ => None,
                    };
                }
                Some(vec![(name.clone(), value.clone())])
            }
            // An or-pattern matches the first alternative that matches (first match wins).
            Pattern::Or(alts) => alts.iter().find_map(|a| self.try_bind(a, value)),
            Pattern::Literal(lit) => literal_matches(lit, value).then(Vec::new),
            Pattern::Range { start, end } => match value {
                Value::Int(v) => (*start <= *v && *v < *end).then(Vec::new),
                _ => None,
            },
            Pattern::Tuple(subs) => {
                let Value::Tuple(elems) = value else {
                    return None;
                };
                if elems.len() != subs.len() {
                    return None;
                }
                let mut out = Vec::new();
                for (sub, v) in subs.iter().zip(elems.iter()) {
                    out.extend(self.try_bind(sub, v)?);
                }
                Some(out)
            }
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                ..
            } => {
                let Value::Enum {
                    ty,
                    variant,
                    payload,
                } = value
                else {
                    // A bare top-level identifier (no payload) against a non-enum value is a binding
                    // capturing the whole value — the checker permits this only for literal
                    // scrutinees.
                    if bindings.is_empty() {
                        return Some(vec![(name.clone(), value.clone())]);
                    }
                    return None;
                };
                if name != variant.as_ref() || bindings.len() != payload.len() {
                    return None;
                }
                // A *qualified* pattern only matches a value of that same enum — two enums may share
                // a variant name, so the variant string alone is not enough (mirrors the VM's
                // enum-scoped variant_id compare). SCRUTINEE-DRIVEN: the value carries its own
                // qualified enum identity key (`ty`, e.g. `a::Color`) — the very enum the checker
                // resolved the scrutinee to. Match iff its BARE form equals the pattern's written
                // qualifier (`Color`). This is deterministic and engine-independent; NEVER iterate
                // the (RandomState-seeded) import map to re-guess which module's enum it is — that
                // ignored the scrutinee and picked nondeterministically (often the wrong enum). The
                // bare built-in form (`Some`/`None`) carries no `enum_name` and keys on variant only.
                if let Some(en) = enum_name
                    && crate::compiler::bare_display(ty.as_ref()).as_str() != en.as_str()
                {
                    return None;
                }
                let mut out = Vec::new();
                for (sub, v) in bindings.iter().zip(payload.iter()) {
                    out.extend(self.try_bind(sub, v)?);
                }
                Some(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> Value {
        eval_str(src).expect("eval should succeed")
    }

    fn run(src: &str) -> String {
        run_capture(src).expect("run should succeed")
    }

    #[test]
    fn interp_raw_string_is_verbatim_no_interpolation() {
        // A raw string is NOT interpolated even with `x` in scope — braces stay literal.
        assert_eq!(run("x := 5\nprint(r\"{x}\")\n"), "{x}\n");
        // Backslashes are verbatim (no escape processing).
        assert_eq!(run("print(r\"\\d+\")\n"), "\\d+\n");
        // ...while a NORMAL string still interpolates, proving raw is opt-in.
        assert_eq!(run("x := 5\nprint(\"{x}\")\n"), "5\n");
    }

    #[test]
    fn interp_inline_expr_body_implicitly_returns() {
        // `fn a(): 10` returns 10 (not nil) on the interpreter — mirrors the VM's compile_fn branch.
        assert_eq!(run("fn ten(): 10\nprint(ten())\n"), "10\n");
        // an inline-expr body used as a `.map` argument.
        assert_eq!(
            run("fn dbl(x: int): x * 2\nprint([1, 2, 3].map(dbl))\n"),
            "[2, 4, 6]\n"
        );
    }

    // ---- assert (Phase A) — interp half + cross-engine parity ----

    #[test]
    fn interp_assert_true_does_not_fault() {
        assert_eq!(run("assert true\nprint(\"ok\")\n"), "ok\n");
    }

    #[test]
    fn interp_assert_false_faults_with_msg_and_line() {
        let err = run_capture("print(\"a\")\nassert false, \"boom\"\n").unwrap_err();
        assert_eq!(err.message, "boom");
        assert_eq!(err.span.line, 2);
    }

    #[test]
    fn assert_fault_is_byte_identical_across_engines() {
        // The headline parity guarantee: VM and interp produce the SAME message + span for a
        // failed assert (both default and custom-message paths).
        for src in [
            "print(\"a\")\nassert false\n",
            "print(\"a\")\nassert 1 == 2, \"mismatch\"\n",
        ] {
            let vm_err = crate::vm::run_capture(src).unwrap_err();
            let interp_err = run_capture(src).unwrap_err();
            assert_eq!(
                vm_err.message, interp_err.message,
                "message parity for {src:?}"
            );
            assert_eq!(
                vm_err.span.line, interp_err.span.line,
                "line parity for {src:?}"
            );
        }
    }

    #[test]
    fn assert_passing_msg_is_lazy_and_byte_identical_across_engines() {
        // Regression (eager-vs-lazy parity): the message of a *passing* assert must NOT be evaluated
        // by either engine. The VM used to compile `msg` unconditionally before the check, so a
        // passing `assert true, loud()` printed in the VM but stayed silent in the interp, and a
        // passing `assert true, xs[5]` faulted in the VM but passed in the interp. Both forms must
        // now produce identical stdout and neither evaluate the message.
        let side_effect = "fn loud() -> str:\n    print(\"msg-evaluated\")\n    return \"m\"\nprint(\"a\")\nassert true, loud()\nprint(\"b\")\n";
        let vm_out =
            crate::vm::run_capture(side_effect).expect("vm: passing assert must not fault");
        let interp_out = run_capture(side_effect).expect("interp: passing assert must not fault");
        assert_eq!(
            vm_out, interp_out,
            "stdout parity for a passing assert with a side-effecting msg"
        );
        assert_eq!(
            vm_out, "a\nb\n",
            "a passing assert must not evaluate its message"
        );

        // A passing assert whose message expression would FAULT must not fault in either engine.
        let faulting = "xs := [\"only\"]\nprint(\"a\")\nassert true, xs[5]\nprint(\"b\")\n";
        let vm_out =
            crate::vm::run_capture(faulting).expect("vm: faulting msg must not be evaluated");
        let interp_out = run_capture(faulting).expect("interp: faulting msg must not be evaluated");
        assert_eq!(
            vm_out, interp_out,
            "stdout parity for a passing assert with a faulting msg expr"
        );
        assert_eq!(vm_out, "a\nb\n");
    }

    /// `InterpHost::arg_str_map` reads a `map[str, str]` Value in insertion order; a non-map arg
    /// errors. Parity twin of `vm::tests::vm_host_arg_str_map_reads_live_map` — both iterate
    /// `MapData.entries` so header order is identical across engines.
    #[test]
    fn interp_host_arg_str_map_reads_map() {
        use crate::native::Host;
        let mut map = MapData::default();
        map.push(1, Value::Str("one".into()), Value::Str("1".into()));
        map.push(2, Value::Str("two".into()), Value::Str("2".into()));
        let m = Value::Map(std::rc::Rc::new(std::cell::RefCell::new(map)));

        let (mut out, mut stderr) = (String::new(), String::new());
        let mut cfg = crate::native::HostConfig::default();
        let mut exit = None;
        let mut host = InterpHost {
            args: vec![m, Value::Int(3)],
            out: &mut out,
            stderr: &mut stderr,
            cfg: &mut cfg,
            exit: &mut exit,
        };
        assert_eq!(
            host.arg_str_map(0).unwrap(),
            vec![("one".into(), "1".into()), ("two".into(), "2".into())]
        );
        assert!(host.arg_str_map(1).is_err(), "a non-map arg must error");
    }

    /// M-C implicit nurseries (interp side of the cross-engine parity twins in `vm::tests`): a bare
    /// `spawn` at function scope joins at the body's end; `return` is a join point; the module top
    /// level joins at program exit.
    #[test]
    fn implicit_nursery_basic_interp() {
        let src = "fn w():\n    print(\"w\")\nfn main():\n    print(\"a\")\n    spawn w()\n    print(\"b\")\nmain()\n";
        assert_eq!(run(src), "a\nb\nw\n");
    }

    #[test]
    fn implicit_nursery_return_joins_interp() {
        let src = "fn w(n: int):\n    print(\"w{n}\")\nfn f() -> int:\n    spawn w(1)\n    spawn w(2)\n    print(\"x\")\n    return 0\nfn main():\n    print(f())\nmain()\n";
        assert_eq!(run(src), "x\nw1\nw2\n0\n");
    }

    #[test]
    fn implicit_nursery_toplevel_interp() {
        let src = "fn w():\n    print(\"w\")\nprint(\"end\")\nspawn w()\n";
        assert_eq!(run(src), "end\nw\n");
    }

    #[test]
    fn for_tuple_arity_mismatch_errors_at_runtime() {
        // An empty list (Unknown element type) bypasses the checker's arity guard; the interp must
        // still error like the VM rather than silently under-binding (cross-engine parity).
        let err = run_capture("xs := []\nxs.push((1, 2))\nfor a, b, c in xs:\n    print(a)\n")
            .expect_err("arity mismatch should error");
        assert!(
            err.message.contains("tuple has no element '.2' (len 2)"),
            "{}",
            err.message
        );
    }

    #[test]
    fn optchain_in_interpolation_lowers_and_evaluates() {
        // `?.`/`??` inside `{…}` are re-parsed after the module desugar pass; they must still lower.
        assert_eq!(
            run("o := Some(7)\nn: Option[int] = None\nprint(\"{o ?? 0}/{n ?? -1}\")\n"),
            "7/-1\n"
        );
    }

    #[test]
    fn list_comprehension_maps_and_filters() {
        assert_eq!(run("print([x * 2 for x in [1, 2, 3]])\n"), "[2, 4, 6]\n");
        assert_eq!(
            run("print([x for x in [1, 2, 3, 4] if x % 2 == 0])\n"),
            "[2, 4]\n"
        );
    }

    #[test]
    fn interp_or_pattern_matches() {
        // Literal or-pattern (first-match-wins), enum or-pattern, and a binding or-pattern.
        assert_eq!(
            run(
                "fn f(n: int) -> str:\n    return match n:\n        1 | 2 | 3: \"low\"\n        _: \"high\"\nprint(f(2))\nprint(f(9))\n"
            ),
            "low\nhigh\n"
        );
        assert_eq!(
            run(
                "enum E:\n    A(int)\n    B(int)\nfn v(e: E) -> int:\n    return match e:\n        E.A(a) | E.B(a): a\nprint(v(E.A(4)))\nprint(v(E.B(6)))\n"
            ),
            "4\n6\n"
        );
    }

    #[test]
    fn interp_nested_nullary_matches() {
        // A bare nested `None` is a refutable variant match (not a binding): `Some(None)` matches only
        // the inner-none case; `Some(Some(7))` falls through to `_`. Single outer `Some` arm + `_` keeps
        // this CLI-valid (one arm per outer variant), so it mirrors a runnable program.
        assert_eq!(
            run(
                "fn f(oo: Option[Option[int]]) -> str:\n    return match oo:\n        Some(None): \"in\"\n        _: \"out\"\nx: Option[Option[int]] = Some(None)\ny: Option[Option[int]] = Some(Some(7))\nprint(f(x))\nprint(f(y))\n"
            ),
            "in\nout\n"
        );
    }

    #[test]
    fn list_comprehension_over_range() {
        assert_eq!(run("print([x * x for x in 0..5])\n"), "[0, 1, 4, 9, 16]\n");
    }

    #[test]
    fn set_comprehension_dedupes() {
        // Squaring mod-collapses duplicates; the set keeps insertion order of first sight.
        assert_eq!(
            run("print({x % 3 for x in [0, 1, 2, 3, 4, 5]})\n"),
            "{0, 1, 2}\n"
        );
    }

    #[test]
    fn map_comprehension_builds_entries() {
        assert_eq!(
            run("print({x: x * x for x in [1, 2, 3]})\n"),
            "{1: 1, 2: 4, 3: 9}\n"
        );
    }

    #[test]
    fn map_comprehension_over_map_keys_and_values() {
        assert_eq!(
            run("m := {\"a\": 1, \"b\": 2}\nprint({k: v * 10 for k, v in m})\n"),
            "{a: 10, b: 20}\n"
        );
    }

    #[test]
    fn interp_two_clause_list_comprehension() {
        assert_eq!(
            run("print([x + y for x in [1, 2] for y in [10, 20]])\n"),
            "[11, 21, 12, 22]\n"
        );
    }

    #[test]
    fn interp_nested_comp_guard_and_later_var() {
        assert_eq!(
            run("print([x * y for x in 1..4 if x % 2 == 1 for y in [10, 20]])\n"),
            "[10, 20, 30, 60]\n"
        );
        assert_eq!(
            run("print([y for xs in [[1, 2], [3], [4, 5]] for y in xs])\n"),
            "[1, 2, 3, 4, 5]\n"
        );
    }

    #[test]
    fn interp_nested_set_and_map_comprehension() {
        assert_eq!(
            run("print({x + y for x in [0, 3] for y in [0, 3]})\n"),
            "{0, 3, 6}\n"
        );
        assert_eq!(
            run("print({x * 10 + y: x + y for x in [1, 2] for y in [3]})\n"),
            "{13: 4, 23: 5}\n"
        );
    }

    /// A comprehension over a STATEFUL struct iterator drives `next()` lazily (interleaved with the
    /// element/guard), so the element reads the iterator's just-advanced field — NOT the value after
    /// the whole iterator is drained. This matches the `for`-statement and the VM (regression for the
    /// eager-drain divergence). Both single-clause and nested forms are checked.
    #[test]
    fn interp_comprehension_stateful_iterator_is_lazy() {
        let src = "\
struct Counter:
    n: int
    fn next(self) -> Option[int]:
        v := self.n
        self.n = self.n + 1
        if v >= 3:
            return None
        return Some(v)

fn main():
    c := Counter(0)
    print([x * 100 + c.n for x in c])
    c2 := Counter(0)
    print([x * 100 + c2.n for x in c2 for y in [0, 1]])

main()
";
        assert_eq!(run(src), "[1, 102, 203]\n[1, 1, 102, 102, 203, 203]\n");
    }

    // ----- M6c: native function values -----

    #[test]
    fn calls_native_fn_value() {
        use crate::native::{Host, HostError, NativeRet};
        fn add(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            crate::native::expect_args(h, "add", 2)?;
            Ok(NativeRet::Int(h.arg_int(0)? + h.arg_int(1)?))
        }
        let mut interp = Interp::new();
        interp.env.define(
            "add",
            Value::Native(value::NativeFnEntry {
                name: "add".into(),
                func: add,
            }),
        );
        let expr = parse_expr_str("add(40, 2)").unwrap();
        assert_eq!(interp.eval(&expr), Ok(Value::Int(42)));
    }

    #[test]
    fn lowers_native_struct_to_struct_value() {
        use crate::native::NativeRet as N;
        let ret = N::Struct {
            name: "Match".into(),
            fields: vec![
                ("text".into(), N::Str("hi".into())),
                ("start".into(), N::Int(0)),
            ],
        };
        match lower_native(ret) {
            Value::Struct { name, fields } => {
                assert_eq!(&*name, "Match");
                let f = fields.borrow();
                assert_eq!(f[0].0, "text");
                assert_eq!(f[0].1, Value::Str("hi".into()));
                assert_eq!(f[1], ("start".to_string(), Value::Int(0)));
            }
            other => panic!("expected Struct, got {other:?}"),
        }
    }

    #[test]
    fn lowers_native_map_to_map_value() {
        use crate::native::NativeRet as N;
        let ret = N::Map(vec![(N::Str("k".into()), N::Str("v".into()))]);
        match lower_native(ret) {
            Value::Map(m) => {
                let m = m.borrow();
                assert_eq!(m.entries.len(), 1);
                assert_eq!(m.entries[0].1, Value::Str("k".into()));
                assert_eq!(m.entries[0].2, Value::Str("v".into()));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn native_fn_error_carries_span() {
        use crate::native::{Host, HostError, NativeRet};
        fn boom(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            crate::native::expect_args(h, "boom", 0)?;
            Ok(NativeRet::Nil)
        }
        let mut interp = Interp::new();
        interp.env.define(
            "boom",
            Value::Native(value::NativeFnEntry {
                name: "boom".into(),
                func: boom,
            }),
        );
        let expr = parse_expr_str("boom(1)").unwrap();
        let err = interp.eval(&expr).unwrap_err();
        assert_eq!(err.message, "boom() expects 0 argument(s), got 1");
    }

    #[test]
    fn let_infers_and_prints() {
        assert_eq!(run("x := 5\nprint(x)\n"), "5\n");
    }

    #[test]
    fn typed_let() {
        assert_eq!(run("x: int = 7\nprint(x)\n"), "7\n");
    }

    #[test]
    fn compound_assignment() {
        assert_eq!(run("c := 0\nc += 5\nc -= 2\nprint(c)\n"), "3\n");
    }

    #[test]
    fn plain_reassignment() {
        assert_eq!(run("x := 1\nx = 9\nprint(x)\n"), "9\n");
    }

    #[test]
    fn assign_to_undefined_errors() {
        assert!(run_capture("x = 5\n").is_err());
    }

    #[test]
    fn print_joins_args_with_space() {
        assert_eq!(run(r#"print(1, "a", true)"#), "1 a true\n");
    }

    const IF_CHAIN: &str = "\
if n > 0:
    print(\"pos\")
else if n == 0:
    print(\"zero\")
else:
    print(\"neg\")
";

    #[test]
    fn if_selects_first_true_branch() {
        assert_eq!(run(&format!("n := 5\n{IF_CHAIN}")), "pos\n");
    }

    #[test]
    fn if_selects_else_if_branch() {
        assert_eq!(run(&format!("n := 0\n{IF_CHAIN}")), "zero\n");
    }

    #[test]
    fn if_falls_through_to_else() {
        assert_eq!(run(&format!("n := -3\n{IF_CHAIN}")), "neg\n");
    }

    #[test]
    fn if_block_scopes_locals() {
        // a local declared inside an if-body must not leak to the enclosing scope.
        assert!(run_capture("if true:\n    tmp := 1\nprint(tmp)\n").is_err());
    }

    #[test]
    fn if_condition_must_be_bool() {
        assert!(run_capture("if 1:\n    print(1)\n").is_err());
    }

    #[test]
    fn for_range_is_end_exclusive() {
        // 0..3 yields 0, 1, 2 — NOT 3. Guards the classic off-by-one.
        assert_eq!(run("for i in 0..3:\n    print(i)\n"), "0\n1\n2\n");
    }

    #[test]
    fn for_range_accumulates() {
        let src =
            "total := 0\nfor i in 0..10:\n    if i % 2 == 0:\n        total += i\nprint(total)\n";
        assert_eq!(run(src), "20\n");
    }

    #[test]
    fn for_iterates_a_list() {
        assert_eq!(
            run("for x in [10, 20, 30]:\n    print(x)\n"),
            "10\n20\n30\n"
        );
    }

    #[test]
    fn for_loop_var_does_not_leak() {
        assert!(run_capture("for i in 0..3:\n    print(i)\nprint(i)\n").is_err());
    }

    #[test]
    fn while_loops_until_false() {
        assert_eq!(
            run("n := 3\nwhile n > 0:\n    print(n)\n    n -= 1\n"),
            "3\n2\n1\n"
        );
    }

    #[test]
    fn call_function_with_params() {
        assert_eq!(
            run("fn add(a: int, b: int) -> int:\n    return a + b\nprint(add(2, 3))\n"),
            "5\n"
        );
    }

    #[test]
    fn early_return_skips_rest() {
        let src = "fn f(x: int) -> int:\n    if x > 0:\n        return 1\n    return 2\nprint(f(5))\nprint(f(-1))\n";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn recursion_factorial() {
        let src = "fn fact(n: int) -> int:\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nprint(fact(5))\n";
        assert_eq!(run(src), "120\n");
    }

    #[test]
    fn forward_reference_between_top_level_fns() {
        let src = "fn a() -> int:\n    return b()\nfn b() -> int:\n    return 7\nprint(a())\n";
        assert_eq!(run(src), "7\n");
    }

    #[test]
    fn function_does_not_see_callers_locals() {
        // Lexical, not dynamic, scoping: f() must NOT see g()'s `local`.
        let src = "fn f():\n    print(local)\nfn g():\n    local := 5\n    f()\ng()\n";
        assert!(run_capture(src).is_err());
    }

    #[test]
    fn arity_mismatch_errors() {
        assert!(run_capture("fn f(a: int):\n    print(a)\nf(1, 2)\n").is_err());
    }

    #[test]
    fn closure_called_inline() {
        assert_eq!(
            run("double := fn(x: int) -> int: x * 2\nprint(double(5))\n"),
            "10\n"
        );
    }

    #[test]
    fn closure_captures_environment() {
        // The returned closure must capture `n` from adder's frame even after adder returns.
        let src = "fn adder(n: int):\n    return fn(x): x + n\nadd5 := adder(5)\nprint(add5(10))\n";
        assert_eq!(run(src), "15\n");
    }

    #[test]
    fn closure_passed_as_argument() {
        let src = "fn apply(f, v: int) -> int:\n    return f(v)\nprint(apply(fn(x): x + 1, 41))\n";
        assert_eq!(run(src), "42\n");
    }

    const POINT: &str = "\
struct Point:
    x: int
    y: int

    fn sum(self) -> int:
        return self.x + self.y
";

    #[test]
    fn struct_construct_and_field_read() {
        assert_eq!(
            run(&format!(
                "{POINT}p := Point(3, 4)\nprint(p.x)\nprint(p.y)\n"
            )),
            "3\n4\n"
        );
    }

    #[test]
    fn struct_method_binds_self() {
        assert_eq!(
            run(&format!("{POINT}p := Point(3, 4)\nprint(p.sum())\n")),
            "7\n"
        );
    }

    #[test]
    fn struct_wrong_field_count_errors() {
        assert!(run_capture(&format!("{POINT}p := Point(1)\n")).is_err());
    }

    #[test]
    fn struct_unknown_field_errors() {
        assert!(run_capture(&format!("{POINT}p := Point(3, 4)\nprint(p.z)\n")).is_err());
    }

    const SHAPE: &str = "\
enum Shape:
    Circle(int)
    Square(int)

fn area(s: Shape) -> int:
    match s:
        Shape.Circle(r): return r * r
        Shape.Square(n): return n * n
";

    #[test]
    fn enum_variant_with_payload_and_match() {
        let src = format!("{SHAPE}print(area(Shape.Circle(3)))\nprint(area(Shape.Square(4)))\n");
        assert_eq!(run(&src), "9\n16\n");
    }

    #[test]
    fn enum_variant_and_match() {
        let src = "enum Color:\n    Red\n    Green\nfn name(c: Color) -> str:\n    match c:\n        Color.Red: return \"r\"\n        Color.Green: return \"g\"\nprint(name(Color.Red))\nprint(name(Color.Green))\n";
        assert_eq!(run(src), "r\ng\n");
    }

    #[test]
    fn match_without_matching_arm_errors() {
        let src = format!(
            "{SHAPE}fn f(s: Shape):\n    match s:\n        Shape.Circle(r): print(r)\nf(Shape.Square(2))\n"
        );
        assert!(run_capture(&src).is_err());
    }

    #[test]
    fn enum_variant_arity_mismatch_errors() {
        assert!(run_capture(&format!("{SHAPE}print(area(Shape.Circle(1, 2)))\n")).is_err());
    }

    const SAFE_DIV: &str = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"divide by zero\")
    return Ok(a / b)
";

    #[test]
    fn try_unwraps_ok() {
        let src = format!(
            "{SAFE_DIV}fn calc() -> Result[int]:\n    x := safe_div(10, 2)?\n    return Ok(x)\nmatch calc():\n    Ok(v): print(v)\n    Err(e): print(e)\n"
        );
        assert_eq!(run(&src), "5\n");
    }

    #[test]
    fn try_propagates_err_and_skips_rest() {
        // safe_div(x, 0)? yields Err; calc must return that Err immediately and never reach
        // `return Ok(x + y)`.
        let src = format!(
            "{SAFE_DIV}fn calc() -> Result[int]:\n    x := safe_div(10, 2)?\n    y := safe_div(x, 0)?\n    return Ok(x + y)\nmatch calc():\n    Ok(v): print(v)\n    Err(e): print(e)\n"
        );
        assert_eq!(run(&src), "divide by zero\n");
    }

    #[test]
    fn try_works_on_option() {
        let src = "fn first() -> Option[int]:\n    return Some(42)\nfn get() -> Option[int]:\n    v := first()?\n    return Some(v + 1)\nmatch get():\n    Some(n): print(n)\n    None: print(-1)\n";
        assert_eq!(run(src), "43\n");
    }

    #[test]
    fn interpolation_inserts_variable() {
        assert_eq!(
            run("name := \"chezzi\"\nprint(\"hi {name}\")\n"),
            "hi chezzi\n"
        );
    }

    #[test]
    fn interpolation_evaluates_expression() {
        assert_eq!(run("a := 2\nb := 3\nprint(\"sum: {a + b}\")\n"), "sum: 5\n");
    }

    #[test]
    fn interpolation_escapes_double_braces() {
        assert_eq!(
            run("print(\"brace: {{not}} {1 + 1}\")\n"),
            "brace: {not} 2\n"
        );
    }

    #[test]
    fn interpolation_calls_method() {
        let src = POINT.to_owned() + "p := Point(3, 4)\nprint(\"d {{p}} {p.sum()}\")\n";
        assert_eq!(run(&src), "d {p} 7\n");
    }

    #[test]
    fn interpolation_unterminated_brace_errors() {
        assert!(run_capture("print(\"oops {x\")\n").is_err());
    }

    #[test]
    fn builtin_len() {
        assert_eq!(run("print(len([1, 2, 3]))\n"), "3\n");
        assert_eq!(run("print(len(\"abcd\"))\n"), "4\n");
    }

    // ===== M6a: core-type methods on str / list =====

    #[test]
    fn str_method_len() {
        assert_eq!(run("print(\"abcd\".len())\n"), "4\n");
    }

    #[test]
    fn str_method_upper_lower() {
        assert_eq!(run("print(\"Hi There\".upper())\n"), "HI THERE\n");
        assert_eq!(run("print(\"Hi There\".lower())\n"), "hi there\n");
    }

    #[test]
    fn str_method_trim() {
        assert_eq!(run("print(\"  hi  \".trim())\n"), "hi\n");
    }

    #[test]
    fn str_conforms_to_error_message_returns_self() {
        // `str` is an `Error` (Go-style): `.message()` yields the string itself.
        assert_eq!(run("print(\"boom\".message())\n"), "boom\n");
    }

    #[test]
    fn recover_catches_index_oob() {
        // A runtime fault beneath a `recover:` boundary becomes `Err`, not a process kill.
        let out = run(
            "fn main():\n    r := recover:\n        [1, 2][9]\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()\n",
        );
        assert!(out.starts_with("recovered: "), "got: {out:?}");
        assert!(out.contains("out of bounds"), "got: {out:?}");
    }

    #[test]
    fn recover_ok_path_wraps_value() {
        // No fault ⇒ the block's trailing expression is the `Ok` value.
        let out = run(
            "fn main():\n    r := recover:\n        2 + 3\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
        );
        assert_eq!(out, "ok 5\n");
    }

    #[test]
    fn recover_catches_question_mark_err() {
        // try-block: a `?` Err inside recover lands in `r` (not propagated out of the function).
        let out = run(
            "fn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(7)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        x := risky(false)?\n        x + 1\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"caught: {e.message()}\")\nmain()\n",
        );
        assert_eq!(out, "caught: boom\n");
    }

    #[test]
    fn recover_keeps_side_effects_before_a_fault() {
        // A mutation made before a caught fault persists (keep semantics; matches the VM).
        let out = run(
            "fn main():\n    x := 1\n    r := recover:\n        x = 99\n        [1][9]\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered\")\n    print(\"x={x}\")\nmain()\n",
        );
        assert_eq!(out, "recovered\nx=99\n");
    }

    #[test]
    fn recover_question_mark_ok_unwraps() {
        let out = run(
            "fn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(7)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        x := risky(true)?\n        x + 1\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"caught: {e.message()}\")\nmain()\n",
        );
        assert_eq!(out, "ok 8\n");
    }

    #[test]
    fn str_method_split() {
        assert_eq!(run("print(\"a,b,c\".split(\",\"))\n"), "[a, b, c]\n");
    }

    #[test]
    fn str_method_join() {
        assert_eq!(run("print(\",\".join([\"a\", \"b\", \"c\"]))\n"), "a,b,c\n");
    }

    #[test]
    fn str_method_starts_with_and_contains() {
        assert_eq!(run("print(\"abc\".starts_with(\"ab\"))\n"), "true\n");
        assert_eq!(run("print(\"abc\".starts_with(\"x\"))\n"), "false\n");
        assert_eq!(run("print(\"abc\".contains(\"b\"))\n"), "true\n");
        assert_eq!(run("print(\"abc\".contains(\"z\"))\n"), "false\n");
    }

    #[test]
    fn list_method_push_mutates_in_place() {
        assert_eq!(
            run("xs := [1, 2]\nxs.push(3)\nprint(xs)\nprint(xs.len())\n"),
            "[1, 2, 3]\n3\n"
        );
    }

    #[test]
    fn list_map_doubles() {
        assert_eq!(
            run("xs := [1,2,3]\nys := xs.map(fn(x: int) -> int: x * 2)\nprint(ys)\n"),
            "[2, 4, 6]\n"
        );
    }

    #[test]
    fn list_map_changes_type_to_str() {
        assert_eq!(
            run(
                "xs := [1,2,3]\nys := xs.map(fn(x: int) -> str: \"n{x}\")\nfor y in ys:\n    print(y)\n"
            ),
            "n1\nn2\nn3\n"
        );
    }

    #[test]
    fn list_filter_evens() {
        assert_eq!(
            run("xs := [1,2,3,4]\nys := xs.filter(fn(x: int) -> bool: x % 2 == 0)\nprint(ys)\n"),
            "[2, 4]\n"
        );
    }

    #[test]
    fn list_fold_sum() {
        assert_eq!(
            run("print([1,2,3,4].fold(0, fn(a: int, x: int) -> int: a + x))\n"),
            "10\n"
        );
    }

    #[test]
    fn list_fold_string_acc() {
        assert_eq!(
            run(
                "xs := [\"a\",\"b\",\"c\"]\ns := xs.fold(\"\", fn(a: str, x: str) -> str: a + x)\nprint(s)\n"
            ),
            "abc\n"
        );
    }

    #[test]
    fn str_method_wrong_arity_errors() {
        let e = run_capture("print(\"hi\".upper(\"extra\"))\n").unwrap_err();
        assert!(
            e.message.contains("upper() expects 0 argument(s), got 1"),
            "{}",
            e.message
        );
    }

    #[test]
    fn unknown_str_method_errors() {
        let e = run_capture("print(\"hi\".frobnicate())\n").unwrap_err();
        assert_eq!(e.message, "type str has no method 'frobnicate'");
    }

    #[test]
    fn join_on_non_str_element_errors() {
        let e = run_capture("print(\",\".join([1, 2]))\n").unwrap_err();
        assert!(e.message.contains("join"), "{}", e.message);
    }

    #[test]
    fn builtin_range() {
        assert_eq!(run("print(range(3))\n"), "[0, 1, 2]\n");
        assert_eq!(run("print(range(2, 5))\n"), "[2, 3, 4]\n");
        assert_eq!(run("for i in range(3):\n    print(i)\n"), "0\n1\n2\n");
    }

    #[test]
    fn builtin_casts() {
        assert_eq!(run("print(int(3.9))\n"), "3\n");
        assert_eq!(run("print(int(\"42\"))\n"), "42\n");
        assert_eq!(run("print(float(5))\n"), "5.0\n");
        assert_eq!(run("print(str(42) + \"!\")\n"), "42!\n");
    }

    #[test]
    fn builtin_wrong_arity_errors() {
        assert!(run_capture("print(len())\n").is_err());
        assert!(run_capture("print(int())\n").is_err());
    }

    // ----- review-panel hardening regressions -----

    #[test]
    fn neg_of_int_min_does_not_panic() {
        // unary `-` on i64::MIN must error, not abort the process.
        assert!(eval_str("-(-9223372036854775807 - 1)").is_err());
    }

    #[test]
    fn try_on_redeclared_nullary_variant_errors_not_panics() {
        // A user enum can shadow the builtin `Ok` as a nullary variant; `Ok?` must not index an
        // empty payload and panic.
        let src = "enum E:\n    Ok\nfn f():\n    x := Ok?\n    print(x)\nf()\n";
        assert!(run_capture(src).is_err());
    }

    #[test]
    fn deep_recursion_errors_instead_of_aborting() {
        let src = "fn f(n: int) -> int:\n    return f(n + 1)\nprint(f(0))\n";
        assert!(run_capture(src).is_err());
    }

    /// M10-G1: a self-referential `Stringable` `str` (`return str(self)`) must hit the call-depth
    /// guard gracefully, not overflow the host stack — `stringify` adds native frames per cycle.
    #[test]
    fn self_referential_stringable_errors_instead_of_aborting() {
        let src = "struct Loop:\n    n: int\n    fn str(self) -> str:\n        return str(self)\nprint(Loop(1))\n";
        assert!(run_capture(src).is_err());
    }

    /// A cyclic data structure (`list[Self]` field forming a cycle) must error gracefully on
    /// `print` rather than overflowing the host stack (SIGABRT), via the structural-depth guard.
    #[test]
    fn cyclic_print_errors_not_crashes() {
        let src = "struct Node:\n    next: list[Node]\na := Node([])\nb := Node([])\na.next.push(b)\nb.next.push(a)\nprint(a)\n";
        let err = run_capture(src).unwrap_err();
        assert!(
            err.message.contains("maximum structural depth"),
            "{}",
            err.message
        );
    }

    /// `==` between two separate equal cycles must error via the structural-depth guard rather than
    /// recursing forever on the host stack.
    #[test]
    fn cyclic_equality_errors_not_crashes() {
        let src = "struct Node:\n    next: list[Node]\na := Node([])\nb := Node([])\na.next.push(b)\nb.next.push(a)\nc := Node([])\nd := Node([])\nc.next.push(d)\nd.next.push(c)\nprint(a == c)\n";
        let err = run_capture(src).unwrap_err();
        assert!(
            err.message.contains("maximum structural depth"),
            "{}",
            err.message
        );
    }

    /// The structural-depth fault is recoverable (a `RuntimeError`, not a SIGABRT): wrapping the
    /// offending `print` in `recover:` catches it.
    #[test]
    fn cyclic_print_is_recoverable() {
        let src = "struct Node:\n    next: list[Node]\na := Node([])\nb := Node([])\na.next.push(b)\nb.next.push(a)\nr := recover:\n    print(a)\nmatch r:\n    Ok(v): print(\"ok\")\n    Err(e): print(\"caught: {e.message()}\")\n";
        let out = run(src);
        assert!(out.contains("caught: maximum structural depth"), "{out}");
    }

    /// Map `==` is order-INDEPENDENT: the same entries in a different insertion order compare equal.
    #[test]
    fn map_equality_is_order_independent() {
        assert_eq!(run("print({1: 10, 2: 20} == {2: 20, 1: 10})\n"), "true\n");
    }

    /// Map `==` still distinguishes differing values / differing sizes.
    #[test]
    fn map_equality_distinguishes_values() {
        assert_eq!(run("print({1: 10} == {1: 99})\n"), "false\n");
        assert_eq!(run("print({1: 10} == {1: 10, 2: 20})\n"), "false\n");
    }

    /// A deep but ACYCLIC structure (well under the guard limit) prints and compares without error.
    #[test]
    fn deep_acyclic_structure_ok() {
        let mut src = String::from("x := 0\n");
        for _ in 0..100 {
            src.push_str("x = [x]\n");
        }
        src.push_str("y := 0\n");
        for _ in 0..100 {
            src.push_str("y = [y]\n");
        }
        src.push_str("print(x == y)\n");
        assert_eq!(run(&src), "true\n");
    }

    #[test]
    fn list_indexing() {
        assert_eq!(eval("[10, 20, 30][1]"), Value::Int(20));
        assert_eq!(run("xs := [1, 2, 3]\nprint(xs[2])\n"), "3\n");
    }

    #[test]
    fn list_index_out_of_bounds_errors() {
        assert!(run_capture("print([1, 2, 3][5])\n").is_err());
        // `-1` now indexes the last element (Python); only out-of-range negatives fault.
        assert_eq!(run("print([1, 2, 3][-1])\n"), "3\n");
        assert!(run_capture("print([1, 2, 3][-4])\n").is_err());
    }

    #[test]
    fn string_indexing() {
        assert_eq!(run("s := \"abc\"\nprint(s[1])\n"), "b\n");
    }

    #[test]
    fn index_assign_mutates_in_place() {
        assert_eq!(
            run("xs := [1, 2, 3]\nxs[1] = 9\nprint(xs)\n"),
            "[1, 9, 3]\n"
        );
    }

    #[test]
    fn index_compound_assign() {
        assert_eq!(
            run("xs := [1, 2, 3]\nxs[0] += 5\nxs[2] -= 1\nprint(xs)\n"),
            "[6, 2, 2]\n"
        );
    }

    #[test]
    fn index_assign_out_of_bounds_errors() {
        assert!(run_capture("xs := [1, 2, 3]\nxs[5] = 0\n").is_err());
    }

    #[test]
    fn slices_list_and_str() {
        assert_eq!(run("print([1, 2, 3, 4, 5][1:3])\n"), "[2, 3]\n");
        assert_eq!(run("print(\"hello\"[0:2])\n"), "he\n");
        // Optional bounds (Python open slices).
        assert_eq!(run("print([1, 2, 3, 4, 5][2:])\n"), "[3, 4, 5]\n");
        assert_eq!(run("print([1, 2, 3, 4, 5][:2])\n"), "[1, 2]\n");
        assert_eq!(run("print([1, 2, 3, 4, 5][:])\n"), "[1, 2, 3, 4, 5]\n");
    }

    #[test]
    fn slice_step_and_reverse() {
        assert_eq!(run("print([1, 2, 3, 4, 5][0:5:2])\n"), "[1, 3, 5]\n");
        assert_eq!(run("print([1, 2, 3, 4, 5][::-1])\n"), "[5, 4, 3, 2, 1]\n");
        assert_eq!(run("print(\"hello\"[::-1])\n"), "olleh\n");
        // Zero step is a fault, message byte-identical to the VM.
        let e = run_capture("print([1, 2, 3][::0])\n").unwrap_err();
        assert!(
            e.message.contains("slice step cannot be zero"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn slice_negative_bounds() {
        // Negative bounds count from the end (Python), they do NOT fault on slices.
        assert_eq!(run("print([1, 2, 3, 4, 5][-2:])\n"), "[4, 5]\n");
        assert_eq!(run("print([1, 2, 3, 4, 5][:-1])\n"), "[1, 2, 3, 4]\n");
        assert_eq!(run("print([1, 2, 3, 4, 5][-100:])\n"), "[1, 2, 3, 4, 5]\n"); // clamps, no fault
    }

    #[test]
    fn slice_bounds_are_clamped() {
        assert_eq!(run("print([1, 2, 3][1:99])\n"), "[2, 3]\n");
        assert_eq!(run("print([1, 2, 3][0:0])\n"), "[]\n");
        assert_eq!(run("print([1, 2, 3][2:1])\n"), "[]\n"); // start > end → empty
        assert_eq!(run("print(\"hello\"[3:99])\n"), "lo\n");
    }

    #[test]
    fn negative_index_read_and_assign() {
        assert_eq!(run("print([10, 20, 30][-1])\n"), "30\n");
        assert_eq!(run("print([10, 20, 30][-3])\n"), "10\n");
        assert_eq!(run("xs := [1, 2, 3]\nxs[-1] = 99\nprint(xs[2])\n"), "99\n");
        assert_eq!(run("print(\"hello\"[-1])\n"), "o\n");
        // Plain negative index out of range FAULTS (Python asymmetry vs slice clamping).
        assert!(run_capture("print([1, 2, 3][-100])\n").is_err());
    }

    const BUF_PROG: &str = "\
struct Buf:
    xs: list[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> list[int]:
        # Forward each component straight to the backing list's slice; an omitted
        # component stays omitted so the built-in Python defaults apply per direction.
        match (start, end, step):
            (Some(s), Some(e), Some(c)): return self.xs[s:e:c]
            (Some(s), Some(e), None): return self.xs[s:e]
            (Some(s), None, Some(c)): return self.xs[s::c]
            (Some(s), None, None): return self.xs[s:]
            (None, Some(e), Some(c)): return self.xs[:e:c]
            (None, Some(e), None): return self.xs[:e]
            (None, None, Some(c)): return self.xs[::c]
            (None, None, None): return self.xs[:]
b := Buf([10, 20, 30])
";

    #[test]
    fn struct_index_read_dispatch() {
        assert_eq!(run(&format!("{BUF_PROG}print(b[0])\n")), "10\n");
    }

    #[test]
    fn struct_index_assign_dispatch() {
        assert_eq!(run(&format!("{BUF_PROG}b[1] = 99\nprint(b[1])\n")), "99\n");
    }

    #[test]
    fn struct_slice_dispatch() {
        assert_eq!(run(&format!("{BUF_PROG}print(b[0:2])\n")), "[10, 20]\n");
        // Optional bounds + reverse route through the protocol's None-aware body.
        assert_eq!(run(&format!("{BUF_PROG}print(b[:])\n")), "[10, 20, 30]\n");
        assert_eq!(
            run(&format!("{BUF_PROG}print(b[::-1])\n")),
            "[30, 20, 10]\n"
        );
    }

    #[test]
    fn struct_compound_index_assign_dispatch() {
        // `b[0] += 5` reads via `index`, then writes via `set_index`.
        assert_eq!(run(&format!("{BUF_PROG}b[0] += 5\nprint(b[0])\n")), "15\n");
    }

    #[test]
    fn slice_non_sliceable_errors() {
        assert!(run_capture("print((5)[1..2])\n").is_err());
    }

    #[test]
    fn slice_non_int_bound_errors() {
        assert!(run_capture("print([1, 2, 3][\"a\"..2])\n").is_err());
    }

    #[test]
    fn field_assign_mutates_in_place() {
        assert_eq!(
            run(
                "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\nprint(p.x)\nprint(p.y)\n"
            ),
            "9\n2\n"
        );
    }

    #[test]
    fn field_compound_assign() {
        assert_eq!(
            run("struct P:\n    x: int\np := P(10)\np.x += 5\np.x -= 3\nprint(p.x)\n"),
            "12\n"
        );
    }

    #[test]
    fn partial_output_is_kept_when_a_later_statement_errors() {
        let (out, res) = run_program("print(\"before\")\nx := 1 / 0\nprint(\"after\")\n");
        assert_eq!(out, "before\n");
        assert!(res.is_err());
    }

    #[test]
    fn float_division_by_zero_errors() {
        assert!(eval_str("1.0 / 0.0").is_err());
        assert!(eval_str("1.0 % 0.0").is_err());
    }

    #[test]
    fn int_cast_rejects_out_of_range_float() {
        assert!(run_capture("print(int(float(\"1e40\")))\n").is_err());
    }

    #[test]
    fn float_cast_accepts_bool() {
        assert_eq!(run("print(float(true))\n"), "1.0\n");
    }

    #[test]
    fn for_over_huge_range_with_early_return_is_lazy() {
        // A 10-billion range must NOT be materialized; the early return exits immediately.
        // (Eager collection would allocate ~80 GB and abort the process.)
        let src = "fn f() -> int:\n    for i in 0..10000000000:\n        if i == 3:\n            return i\n    return -1\nprint(f())\n";
        assert_eq!(run(src), "3\n");
    }

    #[test]
    fn range_builtin_rejects_absurd_length() {
        assert!(run_capture("print(range(0, 100000000000))\n").is_err());
    }

    #[test]
    fn top_level_try_in_block_reports_unhandled_error() {
        // A `?` in a top-level block (still call_depth 0) whose Err reaches the top is unhandled.
        let src = format!("{SAFE_DIV}if true:\n    x := safe_div(1, 0)?\n    print(x)\n");
        let err = run_capture(&src).unwrap_err();
        assert_eq!(err.message, "unhandled error: divide by zero");
    }

    #[test]
    fn struct_whole_value_display_is_field_ordered() {
        // Display must follow declaration order, not HashMap iteration order.
        assert_eq!(
            run(&format!("{POINT}print(Point(3, 4))\n")),
            "Point(x=3, y=4)\n"
        );
    }

    #[test]
    fn string_escapes_reach_output() {
        assert_eq!(run(r#"print("tab\there")"#), "tab\there\n");
        assert_eq!(run(r#"print("quote: \"x\"")"#), "quote: \"x\"\n");
        assert_eq!(run(r#"print("back\\slash")"#), "back\\slash\n");
    }

    #[test]
    fn escapes_and_interpolation_coexist() {
        // `\t` is a lex-time escape; `{{a}}` and `{1+1}` are eval-time interpolation.
        assert_eq!(run(r#"print("{{a}}\t{1 + 1}")"#), "{a}\t2\n");
    }

    #[test]
    fn numeric_underscores_have_normal_value() {
        assert_eq!(run("print(10_000_000)\n"), "10000000\n");
        assert_eq!(run("print(1_000 == 1000)\n"), "true\n");
    }

    /// Golden end-to-end: the touchstone program must produce exactly its expected output.
    /// `examples/hello.expected` is the regression baseline (the M5 VM must match it too).
    #[test]
    fn golden_hello_chz() {
        let source = include_str!("../../examples/hello.chz");
        let expected = include_str!("../../examples/hello.expected");
        assert_eq!(run_capture(source).expect("hello.chz should run"), expected);
    }

    /// Slicing golden: list/str slicing (clamped) + a struct satisfying the `Index`/`IndexSet`/
    /// `Slice` protocols + a generic bounded by `Index[int, V]` over both.
    #[test]
    fn golden_slicing_chz() {
        let source = include_str!("../../examples/slicing.chz");
        let expected = include_str!("../../examples/slicing.expected");
        assert_eq!(
            run_capture(source).expect("slicing.chz should run"),
            expected
        );
    }

    /// M8-M4 golden: the set type (literals, membership, algebra, iteration).
    #[test]
    fn golden_set_chz() {
        let source = include_str!("../../examples/set.chz");
        let expected = include_str!("../../examples/set.expected");
        assert_eq!(run_capture(source).expect("set.chz should run"), expected);
    }

    /// Additive std.math trig/exp/log intrinsics: interp-side twin of `golden_math_more_via_run_file`.
    /// Imports (`std.io`/`std.math`) require the module-graph path, so this drives `run_file` (not
    /// `run_capture`, which skips resolution) and asserts the interp stdout byte-matches `.expected`.
    #[test]
    fn golden_math_more_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/math_more.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/math_more.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("math_more.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// `Channel.trip()` latch + `std.cancel` (interp twins of the VM `*_via_run_file` goldens). Each
    /// imports a std module, so they drive `run_file` (the module-graph path) and byte-match `.expected`.
    #[test]
    fn golden_channel_trip_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/channel_trip.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/channel_trip.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("channel_trip.chz should run on the interp");
        assert_eq!(out, expected);
    }

    #[test]
    fn golden_cancel_manual_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cancel_manual.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/cancel_manual.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("cancel_manual.chz should run on the interp");
        assert_eq!(out, expected);
    }

    #[test]
    fn golden_cancel_timeout_wait_chz() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("examples/cancel_timeout_wait.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/cancel_timeout_wait.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("cancel_timeout_wait.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// `std.cancel` tree propagation (interp twin of the VM `golden_cancel_tree_via_run_file`): the
    /// parent/child `derive()` cascade runs as pure Chezzi on the interp and byte-matches the SAME
    /// `.expected` the VM produces — proving the feature is two-engine identical (no interp change).
    #[test]
    fn golden_cancel_tree_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cancel_tree.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/cancel_tree.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("cancel_tree.chz should run on the interp");
        assert_eq!(out, expected);
    }

    // The interp's `ctype_of` second resolver + its parity-twin tests were DELETED in fix5 (the
    // single-resolver redesign). Both engines now read the checker's `resolve_extern_signatures`
    // table verbatim, so the two-engine FFI-type parity is structural, not a duplicated-resolver
    // hazard; the behavioral coverage lives in `checker::tests::resolve_extern_ctype`.

    /// C-ABI FFI golden (interp side): an `extern "lib":` block calls `cos`/`sqrt` (libm) and
    /// `strlen` (libc) via dlopen+libffi. Deterministic by design (cos(0.0)=1.0, sqrt(4.0)=2.0,
    /// strlen("hello")=5 — no ULP drift). Drives `run_file` (extern decls need the module-graph +
    /// MakeCffi/hoist path). Linux-only (needs libm.so.6/libc.so.6). The VM twin asserts parity.
    #[test]
    #[cfg(target_os = "linux")]
    fn golden_ffi_chz() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("ffi.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// C-ABI opaque `ptr` handle golden (interp side): an `extern "lib":` block opens a `FILE*` via
    /// libc `fopen` (`-> ptr`), hands the handle back to `fclose` (`ptr ->` int), and detects a NULL
    /// handle via `std.ffi.is_null` / `== std.ffi.null()`. Deterministic on any Linux: `/dev/null` is
    /// always readable (fclose ⇒ 0), the bad path is always NULL. The VM twin asserts parity.
    #[test]
    #[cfg(target_os = "linux")]
    fn golden_ffi_ptr_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_ptr.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_ptr.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("ffi_ptr.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// C-ABI deeper `str` returns golden (interp side): `strdup(str) -> owned_str` (the owned `char*`
    /// is copied into a Chezzi `str` AND freed) and `getenv(str) -> str?` (NULL → `None`, set → `Some`).
    /// Deterministic on any Linux: `strdup` is pure, `PATH` is set, the junk var is unset. VM twin asserts parity.
    #[test]
    #[cfg(target_os = "linux")]
    fn golden_ffi_str_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_str.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_str.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("ffi_str.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// C-ABI fixed-width integer marshalling golden (interp side): `atoi -> int32` (sign-extends a
    /// negative), `htonl(uint32) -> uint32` (zero-extends, high-bit stays positive), `abs(int8) ->
    /// int8` (signed round-trip + param truncation per a C cast). Deterministic on any little-endian
    /// Linux: atoi/htonl/abs are pure + always present. VM twin asserts parity.
    #[test]
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    fn golden_ffi_int_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_int.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_int.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("ffi_int.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// C struct BY VALUE golden (interp side): an `extern "lib":` block binds `div_t div(int, int)` —
    /// a libc fn taking two scalars and returning a small POD struct BY VALUE — to a Chezzi
    /// `struct DivT{quot, rem}` (two int32 fields). Deterministic on any Linux (`div(17, 5) == {3, 2}`,
    /// pure + always present). The VM twin asserts parity.
    #[test]
    #[cfg(target_os = "linux")]
    fn golden_ffi_struct_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_struct.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/ffi_struct.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("ffi_struct.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// CAPSTONE C-buffer FFI golden (interp side): sort a Chezzi list with libc `qsort`, composing the
    /// WHOLE C-buffer surface — `ffi.alloc` lays out an int64[n] buffer, `store_int64_at` fills it from
    /// the list, `qsort` sorts it in place CALLING BACK into a Chezzi comparator that `load_int64`s both
    /// `const void*` sides, `load_int64_at` reads the sorted values back, `defer ffi.free` releases it.
    /// Proves callbacks + deref + alloc all compose. Deterministic on any LP64 Linux (qsort is pure +
    /// always present; the fixed 8-byte stride matches the comparator). The VM twin asserts parity.
    #[test]
    #[cfg(target_os = "linux")]
    fn golden_ffi_qsort_chz() {
        let path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/ffi_qsort.chz");
        let expected = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/ffi_qsort.expected"),
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&path);
        res.expect("ffi_qsort.chz should run on the interp");
        assert_eq!(out, expected);
    }

    /// Regression (blocker): an extern fn declared with an explicit `-> nil` (void) return must RUN,
    /// not panic. `ctype_of("nil")` is `None`, which means *void* here — not an unresolvable type — so
    /// the return slot must use `and_then` (None ⇒ void), never `.expect`. Linux-only (needs libc).
    #[test]
    #[cfg(target_os = "linux")]
    fn extern_explicit_nil_return_runs() {
        let src = "extern \"libc.so.6\":\n    fn srand(seed: int) -> nil\n\nsrand(1)\nprint(42)\n";
        let path = std::env::temp_dir().join(format!(
            "chezzi_interp_ffi_nilret_{}.chz",
            std::process::id()
        ));
        std::fs::write(&path, src).unwrap();
        let (out, _err, res, _) = run_file(&path);
        let _ = std::fs::remove_file(&path);
        res.expect("extern with `-> nil` return should run on the interp");
        assert_eq!(out, "42\n");
    }

    /// SINGLE-RESOLVER guard (fix5): the source-string standalone path (`run_capture`/`run_program`,
    /// NOT `run_file`) must lower an extern by-value RETURN STRUCT via the checker's
    /// `resolve_extern_signatures_standalone` — the ONLY extern-type resolver. After the backend's
    /// `ctype_of` fallback is deleted there is no other path, so a void/None return here would be a
    /// regression. `div(17,5)` must read `quot` 3 (libc `div`). Linux-only.
    #[test]
    #[cfg(target_os = "linux")]
    fn extern_standalone_source_string_struct_return_runs() {
        let src = "import int32 from std.ffi\n\nstruct DivT:\n    quot: int32\n    rem: int32\n\n\
                   extern \"libc.so.6\":\n    fn div(numer: int32, denom: int32) -> DivT\n\n\
                   r := div(17, 5)\nprint(r.quot)\n";
        let out =
            run_capture(src).expect("standalone extern struct return should run on the interp");
        assert_eq!(out, "3\n");
    }

    /// Regression (blocker): a type alias defined in an IMPORTED module, `from`-imported and used at
    /// an extern signature, must lower to its underlying C type — the interp consumes the checker's
    /// module-scoped resolution (a cross-module alias is reachable via `import Size from sizes`, not
    /// bare), else it returns `None` and silently drops the return (would-be void). Linux-only (needs
    /// libc).
    #[test]
    #[cfg(target_os = "linux")]
    fn extern_cross_module_alias_runs() {
        let dir =
            std::env::temp_dir().join(format!("chezzi_interp_ffi_xmod_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sizes.chz"), "type Size = int\n").unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(
            &entry,
            "import Size from sizes\n\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Size\n\nprint(strlen(\"hello\"))\n",
        )
        .unwrap();
        let (out, _err, res, _) = run_file(&entry);
        let _ = std::fs::remove_dir_all(&dir);
        res.expect("extern with a cross-module alias should run on the interp");
        assert_eq!(out, "5\n");
    }

    /// `defer` golden: LIFO cleanup across every frame-exit path (normal return, `?`, panic), with
    /// args evaluated at the defer statement. Cross-engine parity is asserted in `vm`'s twin test.
    #[test]
    fn golden_defer_chz() {
        let source = include_str!("../../examples/defer.chz");
        let expected = include_str!("../../examples/defer.expected");
        assert_eq!(run_capture(source).expect("defer.chz should run"), expected);
    }

    /// Comprehensions golden: list/set/map comprehensions, a guard, and a range source.
    #[test]
    fn golden_comprehensions_chz() {
        let source = include_str!("../../examples/comprehensions.chz");
        let expected = include_str!("../../examples/comprehensions.expected");
        assert_eq!(
            run_capture(source).expect("comprehensions.chz should run"),
            expected
        );
    }

    /// C1 concurrency golden: `parallel:` nursery + `spawn` (both forms) on the sequential
    /// executor — tasks join FIFO at the dedent; inline statements run before spawned bodies. No
    /// VM twin until C4 (concurrency runs on `--interp` only for now).
    #[test]
    fn golden_parallel_chz() {
        let source = include_str!("../../examples/parallel.chz");
        let expected = include_str!("../../examples/parallel.expected");
        assert_eq!(
            run_capture(source).expect("parallel.chz should run"),
            expected
        );
    }

    /// C2 concurrency golden: the canonical `Channel[T]` fan-out worker — spawned workers `send`
    /// results into a shared mailbox that the parent collects after the join (FIFO). Interp-only
    /// until C4.
    #[test]
    fn golden_channel_chz() {
        let source = include_str!("../../examples/channel.chz");
        let expected = include_str!("../../examples/channel.expected");
        assert_eq!(
            run_capture(source).expect("channel.chz should run"),
            expected
        );
    }

    /// A1 concurrency golden: `Channel[T].try_recv()` — a non-blocking poll. Workers `send` at the
    /// dedent; the parent drains the mailbox with `try_recv` (`Some` per value, `None` once empty)
    /// instead of guarding every `recv` with `len()`. Non-blocking, so byte-identical on both engines.
    #[test]
    fn golden_try_recv_chz() {
        let source = include_str!("../../examples/try_recv.chz");
        let expected = include_str!("../../examples/try_recv.expected");
        assert_eq!(
            run_capture(source).expect("try_recv.chz should run"),
            expected
        );
    }

    #[test]
    fn channel_send_after_close_faults() {
        let err = run_capture(
            "fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.send(1)\nmain()\n",
        )
        .expect_err("send on a closed channel should fault");
        assert!(
            err.message.contains("send on a closed channel"),
            "{}",
            err.message
        );
    }

    #[test]
    fn channel_recv_on_closed_empty_faults() {
        let err = run_capture(
            "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.recv())\nmain()\n",
        )
        .expect_err("recv on a closed empty channel should fault");
        assert!(
            err.message.contains("receive on a closed channel"),
            "{}",
            err.message
        );
    }

    #[test]
    fn channel_drains_buffered_after_close() {
        // close() must not drop buffered values — recv drains them first, then faults distinctly.
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.close()\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn channel_try_send_false_when_closed() {
        let src = "fn main():\n    ch := Channel[int]()\n    print(ch.try_send(1))\n    ch.close()\n    print(ch.try_send(2))\nmain()\n";
        assert_eq!(run(src), "true\nfalse\n");
    }

    #[test]
    fn channel_double_close_ok() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.close()\n    print(1)\nmain()\n";
        assert_eq!(run(src), "1\n");
    }

    #[test]
    fn channel_close_then_len_zero() {
        // close() must not fault len(); a closed-and-empty channel still reports 0.
        let src =
            "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.len())\nmain()\n";
        assert_eq!(run(src), "0\n");
    }

    #[test]
    fn channel_try_recv_closed_empty_is_none() {
        // try_recv stays non-blocking + non-faulting on a closed channel — None, indistinguishable
        // from empty (by design; only blocking recv / `for v in ch:` detect close).
        let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"none\")\nmain()\n";
        assert_eq!(run(src), "none\n");
    }

    #[test]
    fn for_over_channel_drains_then_exits() {
        // Producer-first (sequential oracle): send 1,2,3, close, then the consumer `for` drains and
        // exits cleanly — no deadlock fault.
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.send(3)\n    ch.close()\n    total := 0\n    for v in ch:\n        total = total + v\n    print(total)\nmain()\n";
        assert_eq!(run(src), "6\n");
    }

    #[test]
    fn for_over_open_empty_channel_deadlocks() {
        // The sequential oracle cannot block: a `for` over an open-and-empty channel faults (it can
        // never receive a value), mirroring bare `recv`.
        let err = run_capture(
            "fn main():\n    ch := Channel[int]()\n    for v in ch:\n        print(v)\nmain()\n",
        )
        .expect_err("for over an open empty channel should deadlock-fault");
        assert!(err.message.contains("deadlock"), "{}", err.message);
    }

    /// C3 concurrency golden: the canonical `Shared[T]` cross-task counter — three spawned tasks
    /// each `update` the same box through a copied handle; the parent reads `3` after the join, then
    /// `set`/`get` round-trips. Interp-only until C4.
    #[test]
    fn golden_shared_chz() {
        let source = include_str!("../../examples/shared.chz");
        let expected = include_str!("../../examples/shared.expected");
        assert_eq!(
            run_capture(source).expect("shared.chz should run"),
            expected
        );
    }

    #[test]
    fn shared_get_set_round_trip() {
        let src = "fn main():\n    s := Shared(1)\n    print(s.get())\n    s.set(42)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "1\n42\n");
    }

    #[test]
    fn shared_update_read_modify_write() {
        let src = "fn main():\n    s := Shared(10)\n    s.update(fn(x): x * 2)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "21\n");
    }

    /// C5 concurrency golden: the `Executor` escape hatch — `submit` enqueues detached work that
    /// runs at `shutdown()` (FIFO), `defer ex.shutdown()` reaps on function exit, and `shutdown_now`
    /// discards. Cross-engine parity is asserted in `vm`'s twin test.
    #[test]
    fn golden_executor_chz() {
        let source = include_str!("../../examples/executor.chz");
        let expected = include_str!("../../examples/executor.expected");
        assert_eq!(
            run_capture(source).expect("executor.chz should run"),
            expected
        );
    }

    #[test]
    fn executor_submit_runs_fifo_at_shutdown() {
        // Submitted tasks do not run when submitted — they drain FIFO at shutdown().
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\n    ex.shutdown()\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n");
    }

    #[test]
    fn executor_submit_after_shutdown_errors() {
        let src = "fn main():\n    ex := Executor()\n    ex.shutdown()\n    ex.submit(fn(): print(1))\nmain()\n";
        let err = run_capture(src).expect_err("submit after shutdown should fault");
        assert!(
            err.message.contains("shut-down Executor"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn executor_shutdown_now_discards_pending() {
        let src = "fn j():\n    print(99)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    ex.shutdown_now()\n    print(0)\nmain()\n";
        assert_eq!(run(src), "0\n");
    }

    // ----- C5 (A2): program-exit auto-drain of an Executor never explicitly shut down -----

    #[test]
    fn golden_executor_autodrain_chz() {
        let source = include_str!("../../examples/executor_autodrain.chz");
        let expected = include_str!("../../examples/executor_autodrain.expected");
        assert_eq!(
            run_capture(source).expect("executor_autodrain.chz should run"),
            expected
        );
    }

    #[test]
    fn executor_autodrain_runs_unshut_at_exit() {
        // An Executor submitted to but never shut down is gracefully drained at program exit — its
        // queued work runs FIFO after main() returns (mirrors a top-level `defer ex.shutdown()`).
        // Without the auto-drain this work would silently never run.
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n");
    }

    #[test]
    fn executor_autodrain_fifo_across_executors() {
        // Multiple un-shut executors drain in creation order at exit.
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    a := Executor()\n    b := Executor()\n    a.submit(fn(): j(1))\n    b.submit(fn(): j(2))\nmain()\n";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn executor_autodrain_not_redrained_after_explicit_shutdown() {
        // An explicitly shut-down executor is not re-drained at exit (its `shut` flag is set, so the
        // auto-drain skips it) — its work runs exactly once, at the explicit shutdown.
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.shutdown()\n    print(0)\nmain()\n";
        assert_eq!(run(src), "1\n0\n");
    }

    #[test]
    fn executor_autodrain_fault_surfaces() {
        // A fault inside an auto-drained task surfaces as the program's runtime error (the drain
        // uses the same re-entrant call path + first-fault-aborts semantics as `shutdown()`).
        let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn main():\n    ex := Executor()\n    ex.submit(fn(): boom())\nmain()\n";
        let err = run_capture(src).expect_err("auto-drain fault should surface");
        assert!(
            err.message.contains("out of bounds") || err.message.contains("index"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn shared_get_does_not_alias_box() {
        // `get` copies out: mutating the returned list must not change what the box holds.
        let src = "fn main():\n    s := Shared([1, 2])\n    xs := s.get()\n    xs.push(3)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "[1, 2]\n");
    }

    #[test]
    fn channel_recv_on_empty_is_deadlock_error() {
        let err =
            run_capture("fn main():\n    ch := Channel[int]()\n    print(ch.recv())\nmain()\n")
                .unwrap_err();
        assert!(err.message.contains("deadlock"), "got: {}", err.message);
    }

    /// A1: `try_recv` on an empty channel returns `None` — never the deadlock fault `recv` raises
    /// (contrast `channel_recv_on_empty_is_deadlock_error`). The non-blocking poll.
    #[test]
    fn channel_try_recv_on_empty_returns_none() {
        let src = "fn main():\n    ch := Channel[int]()\n    match ch.try_recv():\n        Some(v): print(\"got {v}\")\n        None: print(\"empty\")\nmain()\n";
        assert_eq!(run(src), "empty\n");
    }

    /// A1: `try_recv` on a non-empty channel returns `Some(v)` (FIFO, like `recv`).
    #[test]
    fn channel_try_recv_with_value_returns_some() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nmain()\n";
        assert_eq!(run(src), "42\n");
    }

    /// Parity-gap pin (B1/B2 is VM-only for now): a mid-flight blocking `recv` that the VM resolves
    /// via cooperative fibers still faults `deadlock` under the tree-walking interpreter, which has
    /// no suspendable execution yet. The VM twin `golden_channel_block_chz_matches_expected` asserts
    /// the working behavior. When the interp gains B1, this test flips to a golden.
    #[test]
    fn channel_block_chz_faults_deadlock_on_interp() {
        let source = include_str!("../../examples/channel_block.chz");
        let err = run_capture(source).unwrap_err();
        assert!(err.message.contains("deadlock"), "got: {}", err.message);
    }

    // ----- §6d: `wait` (select) — interp reference semantics -----

    #[test]
    fn wait_picks_first_ready_arm_in_source_order() {
        let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.send(1)\n    b.send(2)\n    wait:\n        v := a.recv(): print(10 + v)\n        w := b.recv(): print(20 + w)\nmain()\n";
        assert_eq!(run(src), "11\n");
    }

    #[test]
    fn wait_skips_closed_empty_arm() {
        let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.close()\n    b.send(9)\n    wait:\n        v := a.recv(): print(100)\n        w := b.recv(): print(w)\nmain()\n";
        assert_eq!(run(src), "9\n");
    }

    #[test]
    fn wait_runs_else_when_nothing_ready() {
        let src = "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\n        else: print(0)\nmain()\n";
        assert_eq!(run(src), "0\n");
    }

    #[test]
    fn wait_assign_arm_mutates_outer_lvalue() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(5)\n    n := 0\n    wait:\n        n = ch.recv(): print(n)\n    print(n)\nmain()\n";
        assert_eq!(run(src), "5\n5\n");
    }

    #[test]
    fn wait_timer_arm_fires() {
        let src = "fn main():\n    t := timer(1)\n    wait:\n        _ := t.recv(): print(\"tick\")\nmain()\n";
        assert_eq!(run(src), "tick\n");
    }

    #[test]
    fn wait_all_closed_no_else_faults() {
        let err = run_capture("fn main():\n    ch := Channel[int]()\n    ch.close()\n    wait:\n        v := ch.recv(): print(v)\nmain()\n")
            .unwrap_err();
        assert!(
            err.message.contains("all channels closed"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn wait_live_empty_no_else_deadlocks() {
        let err = run_capture("fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\nmain()\n")
            .unwrap_err();
        assert!(err.message.contains("deadlock"), "got: {}", err.message);
    }

    #[test]
    fn channel_send_recv_fifo() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn channel_send_deep_copies_value() {
        // Mutating the original list after send must NOT change what the channel holds (airlock).
        let src = "fn main():\n    ch := Channel[list[int]]()\n    xs := [1, 2]\n    ch.send(xs)\n    xs.push(3)\n    print(ch.recv())\nmain()\n";
        assert_eq!(run(src), "[1, 2]\n");
    }

    // ----- golden coverage for the formerly-orphaned examples + the comprehensive torture
    // programs. Cross-engine parity for each is asserted in `vm`'s twin `golden_*` test.

    #[test]
    fn golden_hof_chz() {
        let source = include_str!("../../examples/hof.chz");
        let expected = include_str!("../../examples/hof.expected");
        assert_eq!(run_capture(source).expect("hof.chz should run"), expected);
    }

    #[test]
    fn golden_list_hof_chz() {
        let source = include_str!("../../examples/list_hof.chz");
        let expected = include_str!("../../examples/list_hof.expected");
        assert_eq!(
            run_capture(source).expect("list_hof.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_list_methods_chz() {
        let source = include_str!("../../examples/list_methods.chz");
        let expected = include_str!("../../examples/list_methods.expected");
        assert_eq!(
            run_capture(source).expect("list_methods.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_loops_chz() {
        let source = include_str!("../../examples/loops.chz");
        let expected = include_str!("../../examples/loops.expected");
        assert_eq!(run_capture(source).expect("loops.chz should run"), expected);
    }

    #[test]
    fn golden_match_value_chz() {
        let source = include_str!("../../examples/match_value.chz");
        let expected = include_str!("../../examples/match_value.expected");
        assert_eq!(
            run_capture(source).expect("match_value.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_pair_chz() {
        let source = include_str!("../../examples/pair.chz");
        let expected = include_str!("../../examples/pair.expected");
        assert_eq!(run_capture(source).expect("pair.chz should run"), expected);
    }

    #[test]
    fn golden_method_default_args_chz() {
        let source = include_str!("../../examples/method_default_args.chz");
        let expected = include_str!("../../examples/method_default_args.expected");
        assert_eq!(
            run_capture(source).expect("method_default_args.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_method_type_params_chz() {
        let source = include_str!("../../examples/method_type_params.chz");
        let expected = include_str!("../../examples/method_type_params.expected");
        assert_eq!(
            run_capture(source).expect("method_type_params.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_param_protocol_chz() {
        let source = include_str!("../../examples/param_protocol.chz");
        let expected = include_str!("../../examples/param_protocol.expected");
        assert_eq!(
            run_capture(source).expect("param_protocol.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_edge_cases_chz() {
        let source = include_str!("../../examples/edge_cases.chz");
        let expected = include_str!("../../examples/edge_cases.expected");
        assert_eq!(
            run_capture(source).expect("edge_cases.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_evaluator_chz() {
        let source = include_str!("../../examples/evaluator.chz");
        let expected = include_str!("../../examples/evaluator.expected");
        assert_eq!(
            run_capture(source).expect("evaluator.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_ledger_chz() {
        let source = include_str!("../../examples/ledger.chz");
        let expected = include_str!("../../examples/ledger.expected");
        assert_eq!(
            run_capture(source).expect("ledger.chz should run"),
            expected
        );
    }

    /// Match-guard golden: `pattern if cond:` arms (expr + stmt forms) produce the expected output.
    #[test]
    fn golden_match_guard_chz() {
        let source = include_str!("../../examples/match_guard.chz");
        let expected = include_str!("../../examples/match_guard.expected");
        assert_eq!(
            run_capture(source).expect("match_guard.chz should run"),
            expected
        );
    }

    /// Range-pattern golden: half-open `start..end` int patterns produce the expected output.
    #[test]
    fn golden_match_range_chz() {
        let source = include_str!("../../examples/match_range.chz");
        let expected = include_str!("../../examples/match_range.expected");
        assert_eq!(
            run_capture(source).expect("match_range.chz should run"),
            expected
        );
    }

    /// M1 (tier-1) golden: Python-style char handling — `s.chars()` + iterable strings.
    #[test]
    fn golden_string_iter_chz() {
        let source = include_str!("../../examples/string_iter.chz");
        let expected = include_str!("../../examples/string_iter.expected");
        assert_eq!(
            run_capture(source).expect("string_iter.chz should run"),
            expected
        );
    }

    /// M6 golden: core-type methods + pipe produce exactly the expected output on the interp.
    #[test]
    fn golden_methods_chz() {
        let source = include_str!("../../examples/methods.chz");
        let expected = include_str!("../../examples/methods.expected");
        assert_eq!(
            run_capture(source).expect("methods.chz should run"),
            expected
        );
    }

    /// Gap #5 golden: map literal, keyed get/set, methods, and iteration on the interp.
    #[test]
    fn golden_map_chz() {
        let source = include_str!("../../examples/map.chz");
        let expected = include_str!("../../examples/map.expected");
        assert_eq!(run_capture(source).expect("map.chz should run"), expected);
    }

    /// G1 golden: generics + structural `Comparable` protocol on the interp.
    #[test]
    fn golden_generics_chz() {
        let source = include_str!("../../examples/generics.chz");
        let expected = include_str!("../../examples/generics.expected");
        assert_eq!(
            run_capture(source).expect("generics.chz should run"),
            expected
        );
    }

    /// G2 golden: generic structs (Pair / Stack) on the interp.
    #[test]
    fn golden_generic_structs_chz() {
        let source = include_str!("../../examples/generic_structs.chz");
        let expected = include_str!("../../examples/generic_structs.expected");
        assert_eq!(
            run_capture(source).expect("generic_structs.chz should run"),
            expected
        );
    }

    /// Tier-2 golden: generic enums (Tree[T] / Either[A, B]) on the interp.
    #[test]
    fn golden_generic_enum_chz() {
        let source = include_str!("../../examples/generic_enum.chz");
        let expected = include_str!("../../examples/generic_enum.expected");
        assert_eq!(
            run_capture(source).expect("generic_enum.chz should run"),
            expected
        );
    }

    /// Golden: real hash-table map/set with Hashable struct keys, on the interp.
    #[test]
    fn golden_hashmap_keys_chz() {
        let source = include_str!("../../examples/hashmap_keys.chz");
        let expected = include_str!("../../examples/hashmap_keys.expected");
        assert_eq!(
            run_capture(source).expect("hashmap_keys.chz should run"),
            expected
        );
    }

    /// M10-G1 golden: the `Stringable` protocol — `str(self)` overrides print/str()/interpolation.
    #[test]
    fn golden_stringable_chz() {
        let source = include_str!("../../examples/stringable.chz");
        let expected = include_str!("../../examples/stringable.expected");
        assert_eq!(
            run_capture(source).expect("stringable.chz should run"),
            expected
        );
    }

    /// M10-G3 golden: operator overloading (`Add`/`Sub`/`Mul`) + multi-bound `T: Add + Mul`.
    #[test]
    fn golden_operators_chz() {
        let source = include_str!("../../examples/operators.chz");
        let expected = include_str!("../../examples/operators.expected");
        assert_eq!(
            run_capture(source).expect("operators.chz should run"),
            expected
        );
    }

    /// M10-G3 golden: transparent type aliases (`type UserId = int`).
    #[test]
    fn golden_type_alias_chz() {
        let source = include_str!("../../examples/type_alias.chz");
        let expected = include_str!("../../examples/type_alias.expected");
        assert_eq!(
            run_capture(source).expect("type_alias.chz should run"),
            expected
        );
    }

    #[test]
    fn golden_recover_chz() {
        let source = include_str!("../../examples/recover.chz");
        let expected = include_str!("../../examples/recover.expected");
        assert_eq!(
            run_capture(source).expect("recover.chz should run"),
            expected
        );
    }

    /// `panic` golden (interp twin): recover-catches `panic(msg)`, a defer runs during the unwind,
    /// and bottom-typed `panic` type-checks in if-expression value position. (The VM half asserts the
    /// byte-for-byte cross-engine parity.)
    #[test]
    fn golden_panic_chz() {
        let source = include_str!("../../examples/panic.chz");
        let expected = include_str!("../../examples/panic.expected");
        assert_eq!(run_capture(source).expect("panic.chz should run"), expected);
    }

    // ----- struct iterator protocol (`for x in s` driven by `next(self) -> Option[T]`) -----

    /// A `Counter` struct yields 0..limit lazily via `next`; iteration advances by mutating `self.n`.
    #[test]
    fn for_over_struct_iterator_counts() {
        let src = "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n3\n4\n");
    }

    /// A `break` stops an *infinite* iterator early — proving iteration is lazy (next() per step).
    #[test]
    fn for_over_struct_iterator_break_lazy() {
        let src = "struct Fib:\n    a: int\n    b: int\n    fn next(self) -> Option[int]:\n        v := self.a\n        nb := self.a + self.b\n        self.a = self.b\n        self.b = nb\n        return Some(v)\nfn main():\n    for x in Fib(0, 1):\n        if x > 10:\n            break\n        print(x)\nmain()\n";
        assert_eq!(run(src), "0\n1\n1\n2\n3\n5\n8\n");
    }

    /// Golden: the iterator example runs on the interp with exactly the expected output.
    #[test]
    fn golden_iterator_chz() {
        let source = include_str!("../../examples/iterator.chz");
        let expected = include_str!("../../examples/iterator.expected");
        assert_eq!(
            run_capture(source).expect("iterator.chz should run"),
            expected
        );
    }

    /// G3 golden: std.cmp (generic min/max/clamp) + Comparable sort on the interp. (Imports a std
    /// module → must go through the file/graph path, so it's the run_file golden in the VM suite;
    /// here the source is embedded and run via the standard interp entry.)
    #[test]
    fn stdlib_cmp_sort_works() {
        // Covers cmp over primitives + struct, and sort() over structs (without imports, inline).
        let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        return self.c - o.c
xs := [M(3), M(1), M(2)]
xs.sort()
out := \"\"
for m in xs:
    out = out + str(m.c)
print(out)
";
        assert_eq!(run(src), "123\n");
    }

    #[test]
    fn generic_struct_is_erased_at_runtime() {
        let src = "struct Box[T]:\n    val: T\n    fn get(self) -> T:\n        return self.val\nb := Box(42)\nprint(b.get())\nprint(b.val)\n";
        assert_eq!(run(src), "42\n42\n");
    }

    #[test]
    fn generic_max_over_int_runs() {
        let src = "fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\nprint(max(3, 9))\nprint(max(9, 3))\n";
        assert_eq!(run(src), "9\n9\n");
    }

    #[test]
    fn sort_over_comparable_structs_is_stable() {
        // n=1 ties keep original order ("a" before "z") → stable.
        let src = "\
struct P:
    n: int
    t: str
    fn compare(self, o: P) -> int:
        return self.n - o.n
    fn show(self) -> str:
        return self.t + str(self.n)
xs := [P(3, \"c\"), P(1, \"a\"), P(2, \"b\"), P(1, \"z\")]
xs.sort()
for x in xs:
    print(x.show())
";
        assert_eq!(run(src), "a1\nz1\nb2\nc3\n");
    }

    #[test]
    fn struct_ordering_dispatches_to_compare() {
        let src = "\
struct P:
    n: int
    fn compare(self, other: P) -> int:
        return self.n - other.n
print(P(1) < P(2))
print(P(2) < P(1))
print(P(5) >= P(5))
";
        assert_eq!(run(src), "true\nfalse\ntrue\n");
    }

    #[test]
    fn primitive_compare_method_returns_sign() {
        // Reachable via an erased generic body (`a.compare(b)` on a concrete primitive).
        let src = "fn c[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nprint(c(2, 5))\nprint(c(5, 2))\nprint(c(4, 4))\n";
        assert_eq!(run(src), "-1\n1\n0\n");
    }

    // ===== entry model: no auto-main; unhandled top-level Err/None exits =====

    #[test]
    fn main_is_not_auto_called() {
        // No automatic entry point — a defined-but-uncalled main produces no output.
        assert_eq!(run("fn main():\n    print(1)\n"), "");
    }

    #[test]
    fn explicit_main_call_runs() {
        assert_eq!(run("fn main():\n    print(1)\nmain()\n"), "1\n");
    }

    #[test]
    fn bare_top_level_err_exits() {
        let e = run_capture("Err(\"boom\")\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: boom");
    }

    #[test]
    fn bare_main_propagating_err_exits() {
        let e = run_capture("fn main():\n    x := Err(\"boom\")?\n    print(\"after\")\nmain()\n")
            .unwrap_err();
        assert_eq!(e.message, "unhandled error: boom");
    }

    #[test]
    fn main_propagating_err_prints_nothing_after() {
        // partial output before the error is preserved, but "after" never prints
        let (out, err) = crate::interp::run_program(
            "fn main():\n    print(\"before\")\n    x := Err(\"boom\")?\n    print(\"after\")\nmain()\n",
        );
        assert_eq!(out, "before\n");
        assert!(err.is_err());
    }

    #[test]
    fn handled_err_does_not_exit() {
        // binding the Err = handled; the program keeps running
        assert_eq!(
            run("fn f() -> Result[int]:\n    return Err(\"x\")\nr := f()\nprint(\"handled\")\n"),
            "handled\n"
        );
    }

    #[test]
    fn bare_top_level_ok_does_not_exit() {
        assert_eq!(run("Ok(5)\nprint(\"ok\")\n"), "ok\n");
    }

    #[test]
    fn bare_main_returning_nil_does_not_exit() {
        assert_eq!(run("fn main():\n    print(\"hi\")\nmain()\n"), "hi\n");
    }

    #[test]
    fn top_level_question_err_exits_with_unified_message() {
        let e = run_capture("x := Err(\"oops\")?\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: oops");
    }

    #[test]
    fn top_level_question_err_reports_real_line() {
        // The `?` is on line 3 — the error must point there, not at a hard-coded line 1.
        let e =
            run_capture("fn d() -> Result[int]:\n    return Err(\"x\")\nx := d()?\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: x");
        assert_eq!(e.span.line, 3, "expected the `?` line, got {}", e.span.line);
    }

    #[test]
    fn bare_top_level_none_exits() {
        let e = run_capture("fn g() -> Option[int]:\n    return None\ng()\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: None");
    }

    #[test]
    fn user_enum_named_err_is_not_a_top_level_error() {
        // A user enum that reuses the `Err` name is a normal value — its qualified `Signal.Err` must
        // NOT exit at top level (only the builtin Result's `Err` does). Gating is on the type.
        assert_eq!(
            run("enum Signal:\n    Err(int)\n    Quiet\nSignal.Err(5)\nprint(\"made it\")\n"),
            "made it\n"
        );
    }

    #[test]
    fn try_on_user_enum_named_err_is_type_error_not_propagation() {
        // `?` must reject a user `Signal.Err` (not of type Result/Option), not silently propagate it.
        let e = run_capture(
            "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := Signal.Err(5)?\n    return x\nf()\n",
        )
        .unwrap_err();
        assert!(
            e.message.contains("expects Result or Option"),
            "{}",
            e.message
        );
    }

    #[test]
    fn int_division_truncates() {
        assert_eq!(eval("7 / 2"), Value::Int(3));
        assert_eq!(eval("10 / 2"), Value::Int(5));
    }

    #[test]
    fn float_promotion() {
        assert_eq!(eval("7.0 / 2"), Value::Float(3.5));
        assert_eq!(eval("1 + 2.0"), Value::Float(3.0));
    }

    // `math.abs(i64::MIN)` overflow is covered cross-engine by `vm::tests::math_abs_min_overflows`
    // (file-based, so `import std.math` resolves; asserts interp==vm error text).

    #[test]
    fn int_min_neg_and_div_overflow() {
        let neg = "fn main():\n    x := -9223372036854775807 - 1\n    print(-x)\nmain()\n";
        assert!(
            run_capture(neg)
                .expect_err("neg overflow")
                .message
                .contains("integer overflow")
        );
        let div = "fn main():\n    x := -9223372036854775807 - 1\n    print(x / -1)\nmain()\n";
        assert!(
            run_capture(div)
                .expect_err("div overflow")
                .message
                .contains("integer overflow")
        );
    }

    #[test]
    fn modulo_keeps_rust_remainder_sign() {
        assert_eq!(eval("-7 % 3"), Value::Int(-1));
        assert_eq!(eval("7 % 3"), Value::Int(1));
    }

    #[test]
    fn arithmetic_precedence() {
        assert_eq!(eval("2 + 3 * 4"), Value::Int(14));
        assert_eq!(eval("(2 + 3) * 4"), Value::Int(20));
    }

    #[test]
    fn string_concat() {
        assert_eq!(eval(r#""a" + "b""#), Value::Str("ab".into()));
    }

    #[test]
    fn and_short_circuits_before_rhs_error() {
        // RHS `1 / 0` would raise a runtime error if evaluated; `and` must not touch it.
        assert_eq!(eval("false and (1 / 0 == 0)"), Value::Bool(false));
        assert_eq!(eval("true and 2 > 1"), Value::Bool(true));
    }

    #[test]
    fn or_short_circuits_before_rhs_error() {
        assert_eq!(eval("true or (1 / 0 == 0)"), Value::Bool(true));
        assert_eq!(eval("false or 2 > 1"), Value::Bool(true));
    }

    #[test]
    fn logical_requires_bool_operands() {
        assert!(eval_str("1 and true").is_err());
    }

    #[test]
    fn comparisons_and_equality() {
        assert_eq!(eval("3 < 5"), Value::Bool(true));
        assert_eq!(eval("3 == 3"), Value::Bool(true));
        assert_eq!(eval("3 != 3"), Value::Bool(false));
        assert_eq!(eval(r#""x" == "x""#), Value::Bool(true));
    }

    // ----- gap #8: tuples + multi-return + destructuring -----

    #[test]
    fn tuple_literal_and_element_access() {
        assert_eq!(
            run("t := (1, 2)\nprint(t.0)\nprint(t.1)\nprint(t)\n"),
            "1\n2\n(1, 2)\n"
        );
    }

    #[test]
    fn destructuring_binds_elements() {
        assert_eq!(run("a, b := (10, 20)\nprint(a + b)\n"), "30\n");
    }

    #[test]
    fn tuple_element_out_of_range_is_runtime_error() {
        let err = run_capture("t := (1, 2)\nprint(t.2)\n").expect_err("out-of-range should error");
        assert!(
            err.message.contains("has no element '.2'"),
            "{}",
            err.message
        );
    }

    /// Robustness for `--interp` (the checker normally prevents this): destructuring a non-tuple is
    /// a clean runtime error, not a panic.
    #[test]
    fn destructuring_non_tuple_is_runtime_error() {
        let err = run_capture("a, b := 5\n").expect_err("non-tuple destructure should error");
        assert!(
            err.message.contains("cannot destructure"),
            "{}",
            err.message
        );
    }

    // ---- user-callable panic(msg) builtin (interp half + cross-engine parity) ----

    /// `panic(msg)` beneath `recover:` materializes as `Err(e)` whose `.message()` == msg — the same
    /// recoverable fault the runtime raises for overflow/OOB.
    #[test]
    fn interp_panic_under_recover_yields_err_with_message() {
        let src = "fn main():\n    r := recover:\n        panic(\"boom\")\n    match r:\n        Ok(v): print(\"ok: {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()\n";
        assert_eq!(run(src), "recovered: boom\n");
    }

    /// An uncaught `panic(msg)` aborts the program with that message (run_capture returns Err).
    #[test]
    fn interp_panic_uncaught_returns_runtime_error() {
        let src = "fn main():\n    panic(\"kaboom\")\nmain()\n";
        let err = run_capture(src).expect_err("uncaught panic should fault");
        assert_eq!(err.message, "kaboom");
    }

    /// `defer`s run during the panic unwind (same path overflow takes today).
    #[test]
    fn interp_panic_runs_defers_during_unwind() {
        let src = "fn log(m: str):\n    print(m)\nfn risky():\n    defer log(\"cleanup ran\")\n    panic(\"kaboom\")\nfn main():\n    r := recover:\n        risky()\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()\n";
        assert_eq!(run(src), "cleanup ran\nrecovered: kaboom\n");
    }
}

#[cfg(test)]
mod module_tests {
    //! M4.5 multi-file program tests, driven through `run_file` over real tempdir fixtures. Each
    //! pins a distinct cross-module bug class.
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("chezzi_interp_{}_{}", std::process::id(), n));
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

    /// Run an entry file, asserting success and returning its stdout.
    fn run_ok(entry: &std::path::Path) -> String {
        let (out, _err, res, _) = run_file(entry);
        res.expect("run_file should succeed");
        out
    }

    /// Run an entry file, asserting it fails, returning the error message.
    fn run_err(entry: &std::path::Path) -> String {
        let (_out, _err, res, _) = run_file(entry);
        res.expect_err("run_file should fail").message
    }

    // 8. Import a module and call a member (Value::Module + Field dispatch, not the struct path).
    #[test]
    fn import_module_and_call_member() {
        let t = TmpDir::new();
        t.write("util.chz", "fn greet(): print(\"hi from util\")\n");
        let entry = t.write("main.chz", "import util\nfn main(): util.greet()\nmain()\n");
        assert_eq!(run_ok(&entry), "hi from util\n");
    }

    // 13. Cross-module globals: an imported fn resolves K against ITS module, not the caller's.
    // (Written right after the basic import to force the architecture correct early.)
    #[test]
    fn imported_fn_sees_its_own_module_globals() {
        let t = TmpDir::new();
        t.write("b.chz", "K := 100\nfn helper() -> int: return K\n");
        let entry = t.write(
            "main.chz",
            "import helper from b\nK := 1\nfn main(): print(helper())\nmain()\n",
        );
        // Must print B's K (100), not the entry's K (1).
        assert_eq!(run_ok(&entry), "100\n");
    }

    // 14. Same fix, second failure mode: the importer defines no K at all.
    #[test]
    fn imported_fn_globals_work_when_caller_has_none() {
        let t = TmpDir::new();
        t.write("b.chz", "K := 42\nfn helper() -> int: return K\n");
        let entry = t.write(
            "main.chz",
            "import helper from b\nfn main(): print(helper())\nmain()\n",
        );
        assert_eq!(run_ok(&entry), "42\n");
    }

    // 9. `import a as m` binds the alias; the un-aliased last segment is NOT bound.
    #[test]
    fn import_module_alias() {
        let t = TmpDir::new();
        t.write("util.chz", "fn f(): print(\"f\")\n");
        let entry = t.write("main.chz", "import util as m\nfn main(): m.f()\nmain()\n");
        assert_eq!(run_ok(&entry), "f\n");

        let bad = t.write("bad.chz", "import util as m\nfn main(): util.f()\nmain()\n");
        assert!(run_err(&bad).contains("undefined name 'util'"));
    }

    // 10. `import f, g from a` pulls names into the importer's scope.
    #[test]
    fn from_named_import() {
        let t = TmpDir::new();
        t.write("lib.chz", "fn f(): print(\"f\")\nfn g(): print(\"g\")\n");
        let entry = t.write(
            "main.chz",
            "import f, g from lib\nfn main():\n    f()\n    g()\nmain()\n",
        );
        assert_eq!(run_ok(&entry), "f\ng\n");
    }

    // 11. `import f as h from a`: the alias is bound, the original name is not.
    #[test]
    fn from_named_import_alias() {
        let t = TmpDir::new();
        t.write("lib.chz", "fn f(): print(\"f\")\n");
        let entry = t.write(
            "main.chz",
            "import f as h from lib\nfn main(): h()\nmain()\n",
        );
        assert_eq!(run_ok(&entry), "f\n");

        let bad = t.write(
            "bad.chz",
            "import f as h from lib\nfn main(): f()\nmain()\n",
        );
        assert!(run_err(&bad).contains("undefined name 'f'"));
    }

    // 12. Run-once across a diamond: a module-level statement runs exactly once.
    #[test]
    fn run_once_diamond_side_effect() {
        let t = TmpDir::new();
        t.write("c.chz", "print(\"init C\")\nfn fc(): print(\"fc\")\n");
        t.write("a.chz", "import c\nfn fa(): c.fc()\n");
        t.write("b.chz", "import c\nfn fb(): c.fc()\n");
        let entry = t.write(
            "main.chz",
            "import a\nimport b\nfn main(): print(\"done\")\nmain()\n",
        );
        let out = run_ok(&entry);
        assert_eq!(
            out.matches("init C").count(),
            1,
            "C init ran more than once: {out:?}"
        );
        assert_eq!(out, "init C\ndone\n");
    }

    // 15. Accessing an undefined member is a clean error.
    #[test]
    fn missing_member_access_errors() {
        let t = TmpDir::new();
        t.write("util.chz", "fn f(): print(\"f\")\n");
        let entry = t.write("main.chz", "import util\nfn main(): util.nope()\nmain()\n");
        assert!(run_err(&entry).contains("has no member 'nope'"));
    }

    // 16. `main` is an ordinary function: nothing auto-runs. The entry calls its own main; a `main`
    // defined in an imported module is never invoked.
    #[test]
    fn main_in_imported_module_does_not_autorun() {
        let t = TmpDir::new();
        t.write(
            "lib.chz",
            "fn main(): print(\"lib main\")\nfn f(): print(\"f\")\n",
        );
        let entry = t.write("main.chz", "import f from lib\nfn main(): f()\nmain()\n");
        let out = run_ok(&entry);
        assert!(!out.contains("lib main"), "imported main ran: {out:?}");
        assert_eq!(out, "f\n");
    }

    // 17. An import cycle surfaces as a clean error through the real entry point.
    #[test]
    fn import_cycle_errors_end_to_end() {
        let t = TmpDir::new();
        let entry = t.write("a.chz", "import b\nfn main(): print(1)\n");
        t.write("b.chz", "import a\nfn f(): print(2)\n");
        assert!(run_err(&entry).contains("cycle"));
    }

    // 22. Golden end-to-end: the committed multi-file fixture (chezzi.toml root, whole-module +
    // from imports, cross-module global) runs to its expected stdout. The M5 VM must match it too.
    #[test]
    fn golden_multi_file_project() {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proj/main.chz");
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/proj/main.expected"),
        )
        .expect("fixture expected output should exist");
        assert_eq!(run_ok(&entry), expected);
    }

    // std.str helpers golden (interp side): `examples/str_more.chz` imports `std.str` and exercises
    // the additive ends_with/index_of/count/replace/strip_prefix/strip_suffix funcs. The frozen
    // interpreter must produce exactly the captured `.expected` (so VM==expected==interp).
    #[test]
    fn golden_str_more_chz() {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/str_more.chz");
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/str_more.expected"),
        )
        .expect("str_more.expected should exist");
        assert_eq!(run_ok(&entry), expected);
    }

    // std.iter helpers golden (interp side): `examples/iter_more.chz` imports `std.iter` and
    // exercises the additive take/drop/any/all/find/flatten funcs. The frozen interpreter must
    // produce exactly the captured `.expected` (so VM==expected==interp).
    #[test]
    fn golden_iter_more_chz() {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/iter_more.chz");
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/iter_more.expected"),
        )
        .expect("iter_more.expected should exist");
        assert_eq!(run_ok(&entry), expected);
    }

    // bytes golden (interp side): `examples/bytes.chz` — the frozen interpreter must produce exactly
    // the captured `.expected` (so VM==expected==interp, the three-engine parity gate).
    #[test]
    fn golden_bytes_chz() {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/bytes.chz");
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/bytes.expected"),
        )
        .expect("bytes.expected should exist");
        assert_eq!(run_ok(&entry), expected);
    }

    // ===== bytes type (interp side — must match the VM byte-for-byte) =====

    #[test]
    fn interp_bytes_ops() {
        let src = concat!(
            "fn main():\n",
            "    b := b\"\\x01\\x02\\x03\"\n",
            "    print(b[0])\n",
            "    print(b[-1])\n",
            "    print(b[::-1])\n",
            "    s := 0\n",
            "    for x in b:\n",
            "        s = s + x\n",
            "    print(s)\n",
            "    print(len(b))\n",
            "    print(b\"ab\" == b\"ab\")\n",
            "    print(b\"ab\" == b\"ac\")\n",
            "    print(b\"ab\" != b\"ac\")\n",
            "main()\n"
        );
        assert_eq!(
            run_capture(src).expect("interp"),
            "1\n3\nb'\\x03\\x02\\x01'\n6\n3\ntrue\nfalse\ntrue\n"
        );
    }

    #[test]
    fn interp_bytes_repr_and_map_key() {
        let src = concat!(
            "fn main():\n",
            "    print(b\"hi\\n\")\n",
            "    print(str(b\"\\xFF\"))\n",
            "    m := {b\"a\": 1, b\"b\": 2}\n",
            "    print(m[b\"a\"])\n",
            "    print(m[b\"b\"])\n",
            "main()\n"
        );
        assert_eq!(
            run_capture(src).expect("interp"),
            "b'hi\\n'\nb'\\xff'\n1\n2\n"
        );
    }

    // ===== bytearray type (interp side — must match the VM byte-for-byte) =====

    #[test]
    fn interp_bytearray_ops() {
        let src = concat!(
            "fn main():\n",
            "    print(bytearray())\n",
            "    print(bytearray(3))\n",
            "    print(bytearray(b\"\\x01\\x02\"))\n",
            "    ba := bytearray([1, 2, 3])\n",
            "    print(ba)\n",
            "    print(ba[0])\n",
            "    print(ba[-1])\n",
            "    print(ba[::-1])\n",
            "    s := 0\n",
            "    for x in ba:\n",
            "        s = s + x\n",
            "    print(s)\n",
            "    print(len(ba))\n",
            "    ba.push(4)\n",
            "    print(ba)\n",
            "    print(ba.pop())\n",
            "    ba.extend(b\"\\xFF\")\n",
            "    ba.extend([7, 8])\n",
            "    print(ba)\n",
            "    print(bytearray([1]) == bytearray([1]))\n",
            "    print(bytearray([1]) != bytearray([2]))\n",
            "main()\n"
        );
        assert_eq!(
            run_capture(src).expect("interp"),
            "bytearray(b'')\nbytearray(b'\\x00\\x00\\x00')\nbytearray(b'\\x01\\x02')\nbytearray(b'\\x01\\x02\\x03')\n1\n3\nbytearray(b'\\x03\\x02\\x01')\n6\n3\nbytearray(b'\\x01\\x02\\x03\\x04')\nSome(4)\nbytearray(b'\\x01\\x02\\x03\\xff\\x07\\x08')\ntrue\ntrue\n"
        );
    }

    #[test]
    fn interp_bytearray_shared_mutation() {
        // Two bindings to the same bytearray observe each other's in-place writes (Rc<RefCell>).
        let src = concat!(
            "fn main():\n",
            "    ba := bytearray([1, 2, 3])\n",
            "    ba2 := ba\n",
            "    ba[0] = 65\n",
            "    print(ba2[0])\n",
            "    ba2[1] = 66\n",
            "    print(ba[1])\n",
            "main()\n"
        );
        assert_eq!(run_capture(src).expect("interp"), "65\n66\n");
    }

    // bytearray golden (interp side): `examples/bytearray.chz` must produce exactly `.expected`.
    #[test]
    fn golden_bytearray_chz() {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/bytearray.chz");
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/bytearray.expected"),
        )
        .expect("bytearray.expected should exist");
        assert_eq!(run_ok(&entry), expected);
    }

    // conversions golden (interp side): `examples/conversions.chz` (str<->bytes UTF-8 methods +
    // list()/set()/map() constructors over any for-iterable) must produce exactly `.expected`.
    #[test]
    fn golden_conversions_chz() {
        let entry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/conversions.chz");
        let expected = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/conversions.expected"),
        )
        .expect("conversions.expected should exist");
        assert_eq!(run_ok(&entry), expected);
    }
}
