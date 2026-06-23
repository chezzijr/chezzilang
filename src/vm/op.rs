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
    /// Read the enclosing closure's captured value at the given positional slot (closure nested in
    /// closure). The slot is the index of this same name in the *enclosing* proto's `capture_names`,
    /// stamped at compile time so `MakeClosure`/`do_spawn_block` read `captured[parent_slot]` with no
    /// string hash. (M19 lever #3: positional captures.)
    Captured(u32),
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

/// Sentinel `ic` id meaning "no inline cache for this field op" (tuple/numeric element access).
/// See [`Op::GetField`]. Real ids are dense `0..Program::field_ic_sites`.
pub const NO_IC: u32 = u32::MAX;

/// Sentinel struct type id (`StructDef::tid` / `Obj::Struct.tid`) for a struct whose name is not a
/// registered type. The field IC never treats it as a cache hit (distinct unregistered layouts would
/// otherwise share it), so such a struct always falls to the name-probe. Real tids are dense `0..n`.
pub const TID_NONE: u32 = u32::MAX;

/// M19 memory-layout lever #2 — sentinel enum variant id (`VariantDef::variant_id` /
/// `Obj::Enum.variant_id`) for an enum whose variant is not a registered variant. `match_arm` never
/// matches it (no real arm id equals it) and the cold-path name resolver falls back to a placeholder.
/// The language cannot construct an unregistered variant (enums are program-global, compiler- /
/// native-registered), so this is a defensive fallback. Real ids are dense `0..n`.
pub const VID_NONE: u32 = u32::MAX;

/// M19 lever #2 — the FIXED low variant ids of the built-in `Result`/`Option` variants, assigned
/// first in `Compiler::new()` so `?` (`do_try`) and top-level-error gating compare against these
/// compile-time constants (the constant / jump-table dispatch the future Cranelift codegen + match-
/// on-enum reuse). User variants follow at `4..` in declaration order.
pub const VID_OK: u32 = 0;
pub const VID_ERR: u32 = 1;
pub const VID_SOME: u32 = 2;
pub const VID_NONE_VARIANT: u32 = 3;

/// A single VM instruction. Operands are inline (typed), so there is no separate constant pool —
/// strings and numbers live in the op. Jump targets are absolute indices into the proto's `code`.
#[derive(Debug, Clone)]
pub enum Op {
    // ----- literals / stack -----
    ConstInt(i64),
    ConstFloat(f64),
    ConstStr(String),
    /// Push a fresh `bytes` heap object from a `b"..."` literal's raw bytes. Parallel to
    /// `ConstStr`; not interned in v1 (allocates per execution, like a list literal).
    ConstBytes(Box<[u8]>),
    True,
    False,
    Nil,
    Pop,
    /// Pop the value of a *statement-level* expression. If the current frame is the module top
    /// level and the value is an unhandled `Err`/`None`, the program exits with that error;
    /// otherwise it is discarded like `Pop`. (Emitted for expression statements.)
    PopExprStmt,
    /// The failing tail of `assert cond[, msg]`. The compiler tests `cond` with a preceding
    /// `JumpIfFalse` and only reaches this op when `cond` was false, so it *always* faults: pop
    /// `msg` (a str, if `has_msg`) and fault at this op's span with that message (or
    /// `"assertion failed"`). `msg` is evaluated lazily on the failing path only — byte-identical to
    /// the interpreter, which likewise evaluates `msg` solely when the assertion fails.
    Assert {
        has_msg: bool,
    },

    // ----- variables -----
    GetLocal(usize),
    SetLocal(usize),
    /// Read the current frame's home-module global at compile-time `slot` (M19 Phase 2b: a
    /// bounds-checked `Vec` index, no name hash). Slots are assigned per module by the compiler and
    /// recorded in [`ModuleProto::global_slots`].
    GetGlobalSlot(u32),
    /// Declare (`:=` at top level / `fn` hoist) into the current module's global `slot`.
    DefineGlobalSlot(u32),
    /// Assign (`=`/`+=`/`-=`) the current module's global `slot` (checker guarantees it is defined).
    SetGlobalSlot(u32),
    /// Read the current closure's captured value at compile-time `slot` (M19 lever #3: positional
    /// captures — `captured[slot]`, no string hash on the hot path). The slot indexes the closure's
    /// `captured` Vec, which is populated in the same snapshot order as the proto's `capture_names`.
    /// On the cold path (a missing/Nil slot, e.g. the home-global fallback) the name is recovered
    /// from `Proto::capture_names[slot]`.
    GetCaptured(u32),

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
    /// `x in xs` — membership test. Pops `[x, container]`, pushes a `Bool`. Dispatches on the
    /// container: list/set element, map KEY, str substring.
    Contains,
    /// Require the top of stack to be a bool (operand of `and`/`or`/`not`, `if`/`while` condition).
    AsBool,
    /// Require the top of stack to be an int (range bounds, list index).
    AsInt,
    /// One-way int→float coercion (C-like implicit widening). Replaces the top of stack: `Int(n)` →
    /// `Float(n as f64)`; a `Float` is left unchanged (idempotent — so a re-coerced float param or a
    /// double-coerce is a harmless no-op); any other type is a runtime error (the checker guarantees
    /// the operand is numeric at every emit site). Emitted by the compiler at value-DEFINITION
    /// boundaries whose static annotation is `float` (typed `let`, float params, float returns, float
    /// struct fields, annotated/all-literal float collections) — a cheap inline op, not a builtin call.
    CoerceFloat,

    // ----- superinstructions (M19 perf): peephole-fused windows of the hot numeric paths.
    // Each carries a `BinKind` (arith + ordered-compare; not `Eq`/`NotEq`, which use a different
    // VM path). The fast path is `Int`-only and inlined; any other operand type falls back to the
    // exact unfused behaviour (`arith`/`compare_op`), so struct overloading / string concat / float
    // promotion / fiber parking all stay identical. -----
    /// `GetLocal(a), GetLocal(b), <binop>` fused — push `local[a] <op> local[b]`.
    BinLocalLocal {
        a: usize,
        b: usize,
        kind: BinKind,
    },
    /// `GetLocal(slot), ConstInt(val), <binop>` fused — push `local[slot] <op> val`.
    BinLocalConst {
        slot: usize,
        val: i64,
        kind: BinKind,
    },
    /// `GetLocal(s), ConstInt(d), Add, SetLocal(s)` fused — in-place `local[s] += d` with no stack
    /// traffic. (Only `Add`; `-=` keeps the two-op `BinLocalConst{Sub} + SetLocal` form to avoid
    /// negating the immediate and to preserve `Sub`'s error message for non-numeric operands.)
    IncLocal {
        slot: usize,
        delta: i64,
    },

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
    /// module member (plain call, no `self`). `ic`: per-call-site method inline-cache id (dense
    /// `0..Program::method_ic_sites`). Every compiler-emitted `CallMethod` gets a real `ic` (including
    /// the synthetic iterator-protocol `next`/`values` sites); [`NO_IC`] is passed ONLY by the
    /// VM-internal native-re-entry callers (`spawn`/`defer`/fiber-start method tasks), never at compile
    /// time — a real `ic` is exactly the "flatten-safe, called-from-the-dispatch-loop" signal.
    CallMethod {
        name: String,
        argc: usize,
        ic: u32,
    },
    CallBuiltin(String, usize),
    CallPrint(usize),
    /// `print(..., sep=, end=)` — stack `[arg0, …, argN-1, sep, end]`: the positional args, then the
    /// `sep` and `end` strings (user exprs or their `" "` / `"\n"` defaults), pushed last. Pops
    /// `argc + 2`, joins the positional args with `sep`, appends `end`, writes to stdout. Plain
    /// `print(...)` (no kwargs) stays on `CallPrint` and is byte-identical to before.
    CallPrintSep {
        argc: usize,
    },
    Return,
    /// `yield <expr>` (experimental generators) — stack top holds the yielded value. Suspends the
    /// running generator: returns control out of the generator's private `run_until` to the host's
    /// `.next()` call, leaving the frames/stack intact to resume on the next `.next()`. Only emitted
    /// inside a generator proto (`Proto::is_generator`); never reached on the cooperative host stack.
    Yield,
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
    /// Construct a `newtype` wrapper: pop ONE inner value off the stack, allocate an
    /// `Obj::NewType { type_key, inner }`. The newtype analogue of a single-field `NewStruct`.
    NewType(String),
    /// Construct an enum variant from the top `argc` stack values. `variant_id` (M19 lever #2) is the
    /// dense, compile-time id stamped onto the instance — the enum analogue of `NewStruct`'s `tid`;
    /// `variant` is kept only for the cold arity-mismatch error message. `VID_NONE` for an
    /// unregistered variant (never constructible from source).
    NewEnum {
        variant: String,
        variant_id: u32,
        argc: usize,
    },
    /// Build a `Func` value over `ProtoId`, capturing the current frame's home module.
    MakeFunc(ProtoId),
    /// Build a `Cffi` value from `Program.cffi_defs[id]`: `dlopen` the library + resolve the symbol
    /// at module init (eager — a missing library/symbol fails here). Pushed onto the stack, then
    /// bound to its global slot by the following `DefineGlobalSlot`.
    MakeCffi(u32),
    /// Build a `Closure`: snapshot each `CapEntry`'s value from the enclosing frame into the new
    /// closure's captured env, and capture the current frame's home module.
    MakeClosure(ProtoId, Vec<CapEntry>),

    // ----- access -----
    /// Read field `name`. `ic` is a per-call-site inline-cache id into the VM's `field_ic` vector
    /// (M19 Phase 4): a monomorphic, name-verified cache of the field's index, collapsing the
    /// struct name-probe to one verify-compare on a hit. `ic == NO_IC` ⇒ no cache (tuple `.0`/`.1`
    /// element access, which dispatches to the tuple arm and never touches the IC).
    GetField {
        name: String,
        ic: u32,
    },
    /// Stack `[obj, index]` (index already `AsInt`-checked).
    GetIndex,
    /// Stack `[obj, start, end, step]` → `[slice]` — Python-style slice of a list/str, or a
    /// struct's `slice`. Each of `start`/`end`/`step` is `Nil` when its component was omitted.
    GetSlice,
    /// Stack `[obj, value]` → `[]` — mutate a struct field in place. `ic`: see [`Op::GetField`].
    SetField {
        name: String,
        ic: u32,
    },
    /// Stack `[obj, index, value]` (index already `AsInt`-checked) → `[]` — mutate a list element.
    SetIndex,
    /// `[a]` → `[a, a]` — duplicate the top (compound field assignment).
    Dup,
    /// `[a, b]` → `[a, b, a, b]` — duplicate the top two (compound index assignment).
    Dup2,

    // ----- strings -----
    /// Pop a value, push its `Display` form as a `Str` (interpolation chunk).
    ToStr,
    /// Pop a value, push it formatted per the parsed format spec as a `Str` (`{expr:spec}` chunk).
    /// The spec was parsed at compile time; type/value mismatches surface as runtime errors.
    ToStrFmt(Box<crate::fmtspec::FormatSpec>),
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
    /// Pop a value; push `true` if it is a generator (experimental), else `false`. Used by `for x in
    /// g():` to route a generator result into the same lazy `next()` step as a struct iterator
    /// (type-erased: the compiler can't tell a generator result from a struct statically).
    IsGenerator,
    /// `for`-loop one-time conversion for a PURE-`Iterable` struct (one with `iter(self)` but NO
    /// `next`). Pop a value: if it is a struct that has `iter` but not `next`, call `iter()` once and
    /// push the resulting cursor (an `Obj::Iter`, which the seq path then drains like any sequence);
    /// otherwise push the value back UNCHANGED. Emitted once before the loop so a struct-with-`next`
    /// and a generator stay on their existing fast paths byte-for-byte (this op is a no-op for them).
    IterableToCursor,
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
        /// M19 lever #2 — the dense, compile-time id of `variant`. Match dispatch compares the
        /// scrutinee's stamped `Obj::Enum.variant_id` against this u32 (no per-arm variant-name string
        /// compare — the JIT jump-table groundwork). `variant` is kept only for the cold error message
        /// ("pattern '…' binds …"). `VID_NONE` for an unregistered variant (never matches).
        variant_id: u32,
        /// SCRUTINEE-DRIVEN fallback key: the BARE written enum qualifier of a user pattern
        /// (`Color` from `Color.Red`), or `None` for a built-in/nullary arm (`Some`/`None`). On an
        /// id-compare MISS, the VM checks the scrutinee's own qualified enum key's bare form against
        /// this name — so two whole-imported same-named enums resolve from the value, not from a
        /// nondeterministic import-map guess. `None` ⇒ pure-int dispatch only (zero behavior change).
        enum_name: Option<String>,
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
    /// `wait:` — Chezzi's `select` (§6d). The N arm channel handles are on the operand stack
    /// (source order; arm 0 deepest). Poll source order: the first channel with a queued value (or a
    /// fired timer) wins → pop the N handles, push the value, and jump to that arm's body target. A
    /// closed+empty arm is skipped. Nothing ready → jump to `else_target` if present; else inline-
    /// sleep to the soonest live timer and take it; else fault (all-closed) or block (cooperative
    /// multi-channel park: keep the handles, rewind to re-poll on wake). See `Vm::op_wait_poll`.
    WaitPoll(Box<WaitMeta>),
    /// `Channel[T]()` — push a fresh empty mailbox (`Obj::Channel`).
    NewChannel,
    /// `Shared(v)` — stack `[init]`; pop it, deep-copy across the airlock, push `Obj::Shared(init)`.
    NewShared,
    /// `Atomic(v)` — stack `[init]`; pop it, deep-copy across the airlock, push `Obj::Atomic(init)`.
    NewAtomic,
    /// `timer(ms)` — stack `[ms]`; pop it, push a fresh `Channel[bool]` whose deadline is `now + ms`.
    /// Delivery happens at `recv` time (in the receiver's own scheduler), NOT here — see
    /// `chan_recv_step`'s timer branch.
    NewTimer,
    /// `Executor()` — push a fresh, empty, explicitly-owned work queue (`Obj::Executor`). C5.
    NewExecutor,
}

/// Static layout for an [`Op::WaitPoll`]: the arm count `n` (== channel handles on the stack), each
/// arm's body target ip (the bind/assign/discard prologue), and the optional `else` block target.
#[derive(Debug, Clone)]
pub struct WaitMeta {
    pub n: usize,
    pub arm_targets: Vec<usize>,
    pub else_target: Option<usize>,
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
    /// M-C implicit nurseries: `true` when this body (a function, or the module top level) contains a
    /// bare `spawn` not already inside an explicit `parallel:`, so the compiler opened an implicit
    /// nursery (`Op::EnterNursery`) at body entry. `do_return` joins it at the body's `return`/end
    /// (vs. cancelling an *inner* escaped `parallel:`). `false` ⇒ zero-overhead, byte-identical to
    /// pre-M-C bytecode.
    pub has_implicit_nursery: bool,
    /// True if this proto is a generator body (its source fn uses `yield`). Calling it does not run
    /// the body — the VM allocates a suspendable `Obj::Generator` instead. Experimental, VM-only.
    pub is_generator: bool,
    /// True if this proto is a `test fn` body (free test or suite method). `chezzi test` discovers
    /// runnable tests by this tag (set by the compiler from `FnDecl::is_test`); ordinary `run` never
    /// inspects it.
    pub is_test: bool,
    /// M19 lever #3 — for a closure proto, the names of its captured environment in slot order
    /// (`capture_names[i]` is the name read by `Op::GetCaptured(i)`). Cold-path metadata only (the
    /// `GetCaptured` home-global fallback + closure error messages); the hot read is a pure
    /// `captured[slot]` index. Empty for non-closure protos. Mirrors [`StructDef::fields`].
    pub capture_names: Vec<String>,
}

/// A struct type's runtime shape (program-global). `module_idx` identifies the module that defined
/// it, so a method resolves its top-level names against that module's globals (home-globals).
#[derive(Debug, Clone)]
pub struct StructDef {
    pub fields: Vec<String>,
    pub methods: HashMap<String, ProtoId>,
    pub module_idx: usize,
    /// ROOT REDESIGN — the BARE user-facing name (`Point`), separate from the program-global IDENTITY
    /// KEY (`<module-key>::Point`) this def is stored under. Display/print/stringify/error/json-encode
    /// render this, so output stays byte-identical regardless of module (and two colliding `Point`s
    /// both print `Point`). The key is identity; this is display.
    pub display_name: String,
    /// M19 Phase 5b — a dense, declaration-order numeric id (unique per struct type, hence per field
    /// layout). Stamped onto every `Obj::Struct` instance so the field inline cache can guard on a
    /// pure-int `tid` compare instead of re-verifying the field-name string. See [`super::mod`]'s
    /// `field_ic` / `IcCell`.
    pub tid: u32,
    /// Names of this struct's `test fn` methods (declaration order). Non-empty ⇒ this struct is a
    /// test suite (`chezzi test`); empty for ordinary structs. Set by the compiler from
    /// `FnDecl::is_test`.
    pub test_methods: Vec<String>,
}

/// An enum variant's runtime shape: which enum it belongs to and how many payload values it holds.
#[derive(Debug, Clone)]
pub struct VariantDef {
    pub enum_name: String,
    /// This variant's own name (also the `Program::variants` map key — kept here so the cold path can
    /// recover it from a `variant_id` via `Program::variants_by_id` without re-scanning the map).
    pub name: String,
    pub arity: usize,
    /// M19 memory-layout lever #2 — a dense, global variant id (`0..n`, one namespace across all
    /// enums) stamped onto every `Obj::Enum` instance so match dispatch / equality / `?` are pure-int
    /// compares (the JIT jump-table groundwork). Unique per `(enum-type, variant)` pair — `Program::
    /// variants` is keyed by that pair, so two enums may share a variant name yet still get distinct
    /// ids. Native `Result`/`Option` variants get the fixed ids `VID_OK`/`VID_ERR`/`VID_SOME`/
    /// `VID_NONE_VARIANT`; user variants follow in declaration order.
    pub variant_id: u32,
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
    /// M19 Phase 2b — global names in compile-time slot order (slot `i` ⇒ `global_slots[i]`). The
    /// run driver pre-sizes the module's `slots` vector to this length and builds its name→slot
    /// index from it. Empty for native modules (their members are populated by name at run time).
    pub global_slots: Vec<String>,
}

/// The whole compiled program.
#[derive(Debug, Clone)]
pub struct Program {
    pub protos: Vec<Proto>,
    pub structs: HashMap<String, StructDef>,
    /// Enum methods, keyed by the enum's module-scoped runtime key → (method name → its proto). The
    /// enum analogue of `StructDef::methods` (enums are type-erased — no `StructDef`/`tid`). Looked up
    /// by `do_method_call` (via `variants_by_id[id].enum_name`) and the arith/compare overload paths.
    pub enum_methods: HashMap<String, HashMap<String, ProtoId>>,
    /// Enum runtime key → the index of the module that declared it, so an enum method resolves its
    /// top-level names against that module's globals (home-globals), mirroring `StructDef::module_idx`.
    pub enum_home: HashMap<String, usize>,
    /// Newtype methods, keyed by the newtype's runtime key → (method name → its proto). The newtype
    /// analogue of `enum_methods` (a newtype is a 1-field nominal wrapper). Looked up by
    /// `do_method_call`, `resolve_overload_method` (str/hash), and the stringify/hash paths.
    pub newtype_methods: HashMap<String, HashMap<String, ProtoId>>,
    /// Newtype runtime key → the index of the module that declared it (home-globals for its methods),
    /// mirroring `enum_home`.
    pub newtype_home: HashMap<String, usize>,
    pub variants: HashMap<(String, String), VariantDef>,
    /// M19 lever #2 — variants indexed by their dense `variant_id` (`variants_by_id[id]` ⇒ that
    /// variant's `VariantDef`, carrying its `enum_name` + `name`). The reverse of `variants`: O(1)
    /// cold-path id→names resolution for Display / stringify / error / wire / snap, where the instance
    /// no longer carries the strings. Dense and gap-free (ids are assigned `0..n`).
    pub variants_by_id: Vec<VariantDef>,
    /// Modules in dependency order (deps first, entry last) — the run order.
    pub modules: Vec<ModuleProto>,
    /// M19 Phase 4 — number of struct-field inline-cache sites (dense ids `0..field_ic_sites`
    /// baked into `GetField`/`SetField` ops). The VM pre-sizes its per-`Vm` `field_ic` vector to
    /// this length. Carries no heap state, so it is never snapshotted or swapped.
    pub field_ic_sites: u32,
    /// M19 Phase 6 — number of method-call inline-cache sites (dense ids `0..method_ic_sites` baked
    /// into `CallMethod` ops). The VM pre-sizes its per-`Vm` `method_ic` vector to this length. Holds
    /// proto ids + module indices, not `GcRef`s, so it carries no heap state (never snapshotted/swapped).
    pub method_ic_sites: u32,
    /// C-ABI FFI — one entry per `extern "lib":` function, referenced by `Op::MakeCffi(id)`. The
    /// resolved symbol address is *not* stored here (it is per-process, resolved at `MakeCffi` via
    /// `dlopen`+`dlsym`); only the library path, name, and marshalling signature are.
    pub cffi_defs: Vec<CffiDef>,
    /// `chezzi test` discovery — free (top-level) `test fn`s in the entry module, as `(name, proto)`
    /// in declaration order. Empty for an ordinary `run`. Populated only for the entry module.
    pub tests: Vec<(String, ProtoId)>,
    /// `chezzi test` discovery — every test suite (a struct with ≥1 `test fn` method) in the entry
    /// module, with its zero-arg constructor thunk + test methods + present lifecycle hooks.
    pub suites: Vec<SuiteInfo>,
    /// User type names (struct / enum / type-alias) declared in ANY module. Module-scoped types: a
    /// `from`-imported TYPE name carries no runtime value (types resolve through `structs`/`variants`
    /// by name), so `bind_import` skips a from-member in this set — like `std.ffi` width imports.
    pub type_names: std::collections::HashSet<String>,
}

/// Discovery metadata for one test suite (a struct containing `test fn` methods). The runner builds
/// the instance once via `new_thunk`, then drives each test method with the present lifecycle hooks.
#[derive(Debug, Clone)]
pub struct SuiteInfo {
    /// The suite struct's name (for the report).
    pub name: String,
    /// A synthetic zero-arg constructor proto that returns `Suite()` (default field exprs applied),
    /// so the runner builds the instance without Rust knowing the field values.
    pub new_thunk: ProtoId,
    /// Test methods, as `(method_name, proto)` in declaration order.
    pub tests: Vec<(String, ProtoId)>,
    /// Present lifecycle hooks, by canonical name → its proto (subset of `LIFECYCLE_HOOKS`).
    pub hooks: HashMap<String, ProtoId>,
}

/// The four recognized suite lifecycle hook names (detected by exact name on a suite struct). A
/// name-matched method is signature-validated by the checker (`fn name(self)` returning nothing).
pub const LIFECYCLE_HOOKS: [&str; 4] = ["before_all", "after_all", "before_each", "after_each"];

/// A compile-time description of one `extern` C function: enough to `dlopen`+`dlsym` and build the
/// runtime [`crate::native::cffi::Cffi`] at module init. The symbol address is resolved at runtime
/// (per-process), so it is deliberately absent here.
#[derive(Debug, Clone, PartialEq)]
pub struct CffiDef {
    pub lib: String,
    pub name: String,
    pub params: Vec<crate::native::cffi::CType>,
    pub ret: Option<crate::native::cffi::CType>,
}

impl Program {
    /// Map a module's stable id to its index in `modules` (for resolving import targets).
    pub fn module_index(&self, id: &ModuleId) -> Option<usize> {
        self.modules.iter().position(|m| &m.id == id)
    }
}
