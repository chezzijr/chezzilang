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
    /// Require the top of stack to be a bool (operand of `and`/`or`/`not`, `if`/`while` condition).
    AsBool,
    /// Require the top of stack to be an int (range bounds, list index).
    AsInt,

    // ----- control flow (absolute jump targets) -----
    Jump(usize),
    /// Pop; jump if the popped value is `false`.
    JumpIfFalse(usize),
    /// Peek; jump if top is `false`, leaving it on the stack (`and` short-circuit).
    JumpIfFalseKeep(usize),
    /// Peek; jump if top is `true`, leaving it on the stack (`or` short-circuit).
    JumpIfTrueKeep(usize),

    // ----- calls -----
    /// Stack: `[callee, arg0, …]`; pops `argc + 1`, pushes the result.
    Call(usize),
    /// `obj.method(args)` — stack `[recv, arg0, …]`. Resolves a struct method (binds `self`) or a
    /// module member (plain call, no `self`).
    CallMethod(String, usize),
    CallBuiltin(String, usize),
    CallPrint(usize),
    Return,
    /// `?` — unwrap `Ok`/`Some`, else propagate `Err`/`None` out of the enclosing function.
    Try,

    // ----- construction -----
    NewList(usize),
    /// Build a map from `n` entries. Stack layout `[k0, v0, k1, v1, …]` (2n values); last key wins.
    NewMap(usize),
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
