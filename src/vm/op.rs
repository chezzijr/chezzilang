//! Bytecode instruction set + compiled program shapes (M5).
//!
//! A `Program` is the output of the compiler: a pool of function prototypes (`Proto`) plus the
//! program-global struct / enum-variant tables (type names are program-global in M4.5) and the
//! per-module metadata the run driver needs (toplevel proto, resolved imports, identity).
//!
//! Instructions are a `Vec<Op>` of typed operands (not packed bytes) — a flat instruction array
//! addressed by an instruction pointer, with jumps as absolute indices. Each instruction has a
//! parallel `Span` in `Proto::lines` so a runtime error can recover the source location the
//! opcode lost.

use crate::ast::Span;
use crate::resolver::{ModuleId, ResolvedImport};
use std::collections::HashMap;

/// Index of a `Proto` in `Program::protos`.
pub type ProtoId = usize;

/// Where one snapshotted name's value comes from in the *enclosing* frame, at the moment a closure
/// is created. The interpreter snapshots all in-scope locals by value; we mirror that — a closure
/// captures every visible enclosing binding by name (snapshot-by-value, not by reference).
#[derive(Debug, Clone, Copy)]
pub enum CapSrc {
    /// Read the enclosing frame's local slot.
    Slot(usize),
    /// Read the enclosing closure's captured value for this same name (closure nested in closure).
    Captured,
}

/// One entry of a closure's captured environment: a name + where to snapshot its value from.
#[derive(Debug, Clone)]
pub struct CapEntry {
    pub name: String,
    pub src: CapSrc,
}

/// The binary operator carried by a superinstruction (`BinLocalLocal` / `BinLocalConst`). Covers
/// arithmetic and *ordered* comparison — the ops that route through `Vm::arith` / `Vm::compare_op`.
/// `Eq`/`NotEq` are excluded (they use `values_equal_guarded`, a separate path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// A single VM instruction. Operands are inline (typed), so there is no separate constant pool —
/// strings and numbers live in the op. Jump targets are absolute indices into the proto's `code`.
#[derive(Debug, Clone)]
pub enum Op {
    // ----- literals / stack -----
    ConstInt(i64),
    ConstFloat(f64),
    ConstStr(String),
    True,
    False,
    Nil,
    Pop,
    /// Pop the value of a *statement-level* expression. If the current frame is the module top
    /// level and the value is an unhandled `Err`/`None`, the program exits with that error;
    /// otherwise it is discarded like `Pop`. (Emitted for expression statements.)
    PopExprStmt,

    // ----- variables -----
    GetLocal(usize),
    SetLocal(usize),
    /// Resolve a name against the current frame's home-module globals.
    GetGlobal(String),
    /// Declare (`:=` at top level / `fn` hoist) into the current module's globals.
    DefineGlobal(String),
    /// Assign (`=`/`+=`/`-=`) an existing global; runtime error if undefined.
    SetGlobal(String),
    /// Resolve a name against the current closure's captured env, falling back to globals.
    GetCaptured(String),

    // ----- arithmetic / logic (dispatch on runtime types, mirroring the interpreter) -----
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Not,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Eq,
    NotEq,
    // Bitwise / shift (int-only) — gap #13.
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    /// Require the top of stack to be a bool (operand of `and`/`or`/`not`, `if`/`while` condition).
    AsBool,
    /// Require the top of stack to be an int (range bounds, list index).
    AsInt,

    // ----- superinstructions (M19 perf): peephole-fused windows of the hot numeric paths.
    // Each carries a `BinKind` (arith + ordered-compare; not `Eq`/`NotEq`, which use a different
    // VM path). The fast path is `Int`-only and inlined; any other operand type falls back to the
    // exact unfused behaviour (`arith`/`compare_op`), so struct overloading / string concat / float
    // promotion / fiber parking all stay identical. -----
    /// `GetLocal(a), GetLocal(b), <binop>` fused — push `local[a] <op> local[b]`.
    BinLocalLocal { a: usize, b: usize, kind: BinKind },
    /// `GetLocal(slot), ConstInt(val), <binop>` fused — push `local[slot] <op> val`.
    BinLocalConst { slot: usize, val: i64, kind: BinKind },
    /// `GetLocal(s), ConstInt(d), Add, SetLocal(s)` fused — in-place `local[s] += d` with no stack
    /// traffic. (Only `Add`; `-=` keeps the two-op `BinLocalConst{Sub} + SetLocal` form to avoid
    /// negating the immediate and to preserve `Sub`'s error message for non-numeric operands.)
    IncLocal { slot: usize, delta: i64 },

    // ----- control flow (absolute jump targets) -----
    Jump(usize),
    /// Pop; jump if the popped value is `false`.
    JumpIfFalse(usize),
    /// Peek; jump if top is `false`, leaving it on the stack (`and` short-circuit).
    JumpIfFalseKeep(usize),
    /// Peek; jump if top is `true`, leaving it on the stack (`or` short-circuit).
    JumpIfTrueKeep(usize),

    // ----- panic recovery (`recover:` boundary) -----
    /// Install a handler covering the following region. On a runtime fault before the matching
    /// `PopHandler`, the VM unwinds the operand stack / call frames / call-depth to this point,
    /// pushes the fault message (a `str`) as the operand, and jumps to the given catch target.
    PushHandler(usize),
    /// Remove the most-recently installed handler (the protected region completed without faulting).
    PopHandler,

    // ----- calls -----
    /// Stack: `[callee, arg0, …]`; pops `argc + 1`, pushes the result.
    Call(usize),
    /// `obj.method(args)` — stack `[recv, arg0, …]`. Resolves a struct method (binds `self`) or a
    /// module member (plain call, no `self`).
    CallMethod(String, usize),
    CallBuiltin(String, usize),
    CallPrint(usize),
    Return,
    /// `defer f(args)` — stack `[callee, arg0, …]`; pops `argc + 1` and records a deferred call on
    /// the current frame. Drained LIFO when the frame exits (return / `?` / panic).
    DeferCall(usize),
    /// `defer recv.name(args)` — stack `[recv, arg0, …]`; pops `argc + 1` and records a deferred
    /// method call on the current frame.
    DeferMethod(String, usize),
    /// Enter a lexical block that contains a `defer`: push a scope marker (the current count of
    /// pending defers) on the frame. Emitted by the compiler only for blocks that statically hold a
    /// `defer`, so defer-free code is unchanged.
    EnterDeferScope,
    /// Leave such a block: run (LIFO) and discard the defers registered since the matching
    /// `EnterDeferScope`, then pop the marker.
    LeaveDeferScope,
    /// Drain (LIFO) the defers a `recover:` block registered, down to the live handler's snapshot.
    /// Emitted on the recover Ok path (before wrapping the value in `Ok`) so the block's cleanup
    /// runs at the recover boundary, like the fault and `?`-short-circuit paths. Keyed off the
    /// top handler's marker, so it touches no `EnterDeferScope` bookkeeping.
    DrainHandlerDefers,
    /// `?` — unwrap `Ok`/`Some`, else propagate `Err`/`None` out of the enclosing function.
    Try,
    /// `json.decode[T](s)` coercion step. Stack: `[result_json]` where `result_json` is the
    /// `Result[Json]` produced by `json.parse(s)`. Pops it and pushes `Result[T]`: if the parse
    /// errored, the `Err` passes through; otherwise the inner `Json` is coerced against the
    /// descriptor (→ `Ok(value)` or `Err(msg)`).
    JsonDecode(crate::json_decode::TypeDescriptor),

    // ----- construction -----
    NewList(usize),
    /// Build a tuple from the top `n` values (`n` ≥ 2). Mirrors `NewList`.
    NewTuple(usize),
    /// Build a map from `n` entries. Stack layout `[k0, v0, k1, v1, …]` (2n values); last key wins.
    NewMap(usize),
    /// Build a set from the top `n` values (deduped, insertion order kept). Mirrors `NewList`.
    NewSet(usize),
    NewStruct(String, usize),
    /// `ty`, `variant`, `argc`.
    NewEnum(String, String, usize),
    /// Build a `Func` value over `ProtoId`, capturing the current frame's home module.
    MakeFunc(ProtoId),
    /// Build a `Closure`: snapshot each `CapEntry`'s value from the enclosing frame into the new
    /// closure's captured env, and capture the current frame's home module.
    MakeClosure(ProtoId, Vec<CapEntry>),

    // ----- access -----
    GetField(String),
    /// Stack `[obj, index]` (index already `AsInt`-checked).
    GetIndex,
    /// Stack `[obj, start, end]` → `[slice]` — half-open slice of a list/str, or a struct's `slice`.
    GetSlice,
    /// Stack `[obj, value]` → `[]` — mutate a struct field in place.
    SetField(String),
    /// Stack `[obj, index, value]` (index already `AsInt`-checked) → `[]` — mutate a list element.
    SetIndex,
    /// `[a]` → `[a, a]` — duplicate the top (compound field assignment).
    Dup,
    /// `[a, b]` → `[a, b, a, b]` — duplicate the top two (compound index assignment).
    Dup2,

    // ----- strings -----
    /// Pop a value, push its `Display` form as a `Str` (interpolation chunk).
    ToStr,
    /// Concatenate the top `n` `Str` values into one.
    BuildStr(usize),

    // ----- iteration helpers -----
    /// Pop an iterable; push a *clone* of its list contents (matches the interpreter snapshotting
    /// `items.borrow().clone()`), erroring if not a list.
    ListClone,
    /// Pop a list; push its length as an int.
    ArrLen,
    /// Pop a value; push `true` if it is a struct instance, else `false`. Used by `for` to pick the
    /// struct-iterator path (`next(self) -> Option[T]`) vs the sequence path at runtime, since the
    /// compiler is type-erased and can't decide statically.
    IsStruct,
    /// Pop a value; push `true` if it is a map, else `false`. Used by the multi-name `for` to pick
    /// the map (key, value) path vs the list-of-tuples destructuring path at runtime, since the
    /// compiler is type-erased and can't decide statically.
    IsMap,
    /// Pop a value; push `true` if it is a `Channel`, else `false`. Used by `for v in ch:` to pick
    /// the blocking channel-iteration path vs the struct/sequence paths at runtime (type-erased).
    IsChannel,
    /// Pop a `Channel` handle; push `Option[T]`: `Some(v)` once a value is available (parking the
    /// fiber on an empty-open channel exactly like `recv`), or `None` when the channel is
    /// closed-and-drained. The lazy step driving `for v in ch:` — `None` ends the loop cleanly.
    ChanRecvOrClosed,

    // ----- match -----
    /// Require the scrutinee in `slot` to be an enum, else "cannot match on …".
    EnsureEnum(usize),
    /// If the enum in `scrut` matches `variant`: bind its payload into locals
    /// `bind_start..bind_start+nbind` and fall through; else jump to `next`. A matching variant
    /// with the wrong payload arity is a runtime error.
    MatchArm {
        scrut: usize,
        variant: String,
        nbind: usize,
        bind_start: usize,
        next: usize,
    },
    /// No arm matched the enum in `slot` — runtime error.
    MatchNoArm(usize),

    // ----- concurrency (C4: sequential, run-to-completion executor) -----
    /// `parallel:` entry — push a fresh, empty task list on the VM's nursery stack. The block body
    /// follows; spawned tasks register on this list; the matching `JoinNursery` drains it.
    EnterNursery,
    /// `parallel:` dedent (the join barrier) — drain the innermost nursery FIFO, running each
    /// registered task to completion (results discarded). The first task to fault aborts the
    /// remaining siblings and propagates (composing with `recover:` / `defer`).
    JoinNursery,
    /// TASK B — emitted on the `break`/`continue` jump path for each `parallel:` scope the jump leaves
    /// before its `JoinNursery` runs (mirrors the `LeaveDeferScope` drain `break`/`continue` emit for
    /// defer scopes). Pops the innermost nursery and CANCELS-AND-REPORTS its unstarted tasks via
    /// `drain_escaped_nursery` (one report line when non-empty) — the block-scoped reclaim for the
    /// net-new in-frame escape site (`do_return` covers the whole-frame return; this covers in-frame
    /// loop exits). Reclaims exactly one level so nested parallels each report their own count.
    ReclaimNursery,
    /// `spawn f(args)` — stack `[callee, arg0, …]`; pops `argc + 1`, deep-copies the args across the
    /// airlock (the callee passes by handle, like `defer`), and registers the task on the innermost
    /// nursery. Mirrors `DeferCall`.
    SpawnCall(usize),
    /// `spawn recv.name(args)` — stack `[recv, arg0, …]`; pops `argc + 1`, deep-copies the receiver
    /// AND the args across the airlock, and registers the method task. Mirrors `DeferMethod`.
    SpawnMethod(String, usize),
    /// `spawn:` block — snapshot each `CapEntry`'s value from the enclosing frame (like
    /// `MakeClosure`), deep-copy the captured values across the airlock, build a zero-arg closure
    /// over `ProtoId`, and register it as a `Call` task. (Form 2; the block was compiled to a
    /// synthetic zero-arg proto.)
    SpawnBlock(ProtoId, Vec<CapEntry>),
    /// `Channel[T]()` — push a fresh empty mailbox (`Obj::Channel`).
    NewChannel,
    /// `Shared(v)` — stack `[init]`; pop it, deep-copy across the airlock, push `Obj::Shared(init)`.
    NewShared,
    /// `Executor()` — push a fresh, empty, explicitly-owned work queue (`Obj::Executor`). C5.
    NewExecutor,
}

/// A compiled function (or the synthetic module-toplevel) — its code, parallel spans, arity, and
/// local-slot high-water mark (the operand stack reserves `n_slots` slots for the frame).
#[derive(Debug, Clone)]
pub struct Proto {
    pub name: String,
    pub arity: usize,
    pub n_slots: usize,
    pub code: Vec<Op>,
    pub lines: Vec<Span>,
}

/// A struct type's runtime shape (program-global). `module_idx` identifies the module that defined
/// it, so a method resolves its top-level names against that module's globals (home-globals).
#[derive(Debug, Clone)]
pub struct StructDef {
    pub fields: Vec<String>,
    pub methods: HashMap<String, ProtoId>,
    pub module_idx: usize,
}

/// An enum variant's runtime shape: which enum it belongs to and how many payload values it holds.
#[derive(Debug, Clone)]
pub struct VariantDef {
    pub enum_name: String,
    pub arity: usize,
}

/// Per-module metadata the run driver consumes: its label, toplevel proto, resolved imports, and
/// stable id (for the run-once namespace cache + import target resolution).
#[derive(Debug, Clone)]
pub struct ModuleProto {
    pub id: ModuleId,
    pub label: String,
    pub toplevel: ProtoId,
    pub imports: Vec<ResolvedImport>,
    /// `Some(name)` for a native std module (`std.math` etc., M6c): its globals are Rust
    /// `NativeFn`s injected at run time, and its (empty) `toplevel` is never executed.
    pub native: Option<&'static str>,
}

/// The whole compiled program.
#[derive(Debug, Clone)]
pub struct Program {
    pub protos: Vec<Proto>,
    pub structs: HashMap<String, StructDef>,
    pub variants: HashMap<String, VariantDef>,
    /// Modules in dependency order (deps first, entry last) — the run order.
    pub modules: Vec<ModuleProto>,
}

impl Program {
    /// Map a module's stable id to its index in `modules` (for resolving import targets).
    pub fn module_index(&self, id: &ModuleId) -> Option<usize> {
        self.modules.iter().position(|m| &m.id == id)
    }
}
