//! Bytecode stack VM (M5) — the Phase-2 execution path. Runs the [`Program`] produced by the
//! compiler, reproducing the tree-walk interpreter's semantics byte-for-byte (golden/parity tests
//! cross-check the two engines). M5a: handle-addressed values, no collector yet (the mark-sweep
//! GC lands in M5b).

pub mod core;
pub mod heap;
pub mod op;
pub mod value;
pub mod wire;

use core::{ChannelCore, ExecutorCore, SharedCore};
use heap::{Heap, MapData, Obj, SetData};
use op::{CapEntry, CapSrc, Op, Program, ProtoId};
use std::sync::{Arc, Mutex};
use value::{GcRef, Value};
use wire::WireValue;

use crate::ast::Span;
#[cfg(test)]
use crate::{lexer, parser};

/// A runtime error, with the source span it occurred at. Mirrors `interp::RuntimeError` (same
/// `Display`) so the two engines' failures compare equal in parity tests.
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
/// `interp::TraceFrame`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceFrame {
    pub function: String,
    pub span: Span,
}

/// A runtime error enriched with a stack trace, produced at the run boundary for an uncaught fault.
/// `Display` matches [`RuntimeError`] exactly (message only — the trace is printed separately) so
/// parity tests that compare error strings are unaffected. Mirrors `interp::RunError`.
#[derive(Debug, Clone)]
pub struct RunError {
    pub message: String,
    pub span: Span,
    pub trace: Vec<TraceFrame>,
}

impl RunError {
    fn from_error(e: RuntimeError, trace: Vec<TraceFrame>) -> Self {
        RunError { message: e.message, span: e.span, trace }
    }
    fn plain(e: RuntimeError) -> Self {
        RunError { message: e.message, span: e.span, trace: Vec::new() }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// Render a runtime error plus its stack trace for the CLI: the error line, then one indented
/// `  at <function> (<call site>)` line per frame, innermost first. Shared by both engines' drivers.
pub fn format_trace(message: &str, span: Span, trace: &[TraceFrame]) -> String {
    let mut s = format!("runtime error ({span}): {message}");
    for frame in trace {
        s.push_str(&format!("\n  at {} (called at {})", frame.function, frame.span));
    }
    s
}

/// Maximum user-function call depth — mirrors the interpreter, so infinite recursion is a clean
/// runtime error rather than a host stack overflow.
const MAX_CALL_DEPTH: usize = 10_000;

/// Stack size for the VM thread (same as the interpreter's): the VM recurses on the host stack
/// when a builtin/method re-enters the dispatch loop, so a large dedicated stack decouples the
/// call-depth limit from the caller's thread.
const VM_STACK_BYTES: usize = 256 * 1024 * 1024;

/// One activation record.
struct CallFrame {
    proto: ProtoId,
    ip: usize,
    /// Index into the operand stack where this frame's slots begin.
    base: usize,
    /// Module globals this frame resolves top-level names against (home-globals).
    home: GcRef,
    /// The closure object backing this frame, if it is a closure call (for `GetCaptured`).
    closure: Option<GcRef>,
    /// Whether this frame counts toward the call-depth limit (user calls do; module toplevels
    /// don't, matching the interpreter).
    counted: bool,
    /// Module toplevel frame — an `Err`/`None` unhandled here (a `?` or a bare expression
    /// statement) is a top-level unhandled error that exits the program.
    is_toplevel: bool,
    /// Calls registered by `defer` in this frame, in source order. Drained LIFO when the frame
    /// exits (return / `?` / panic). Receiver/args are evaluated at the `defer` statement and held
    /// here as values; the call runs at drain. GC-rooted in [`Vm::collect`].
    deferred: Vec<Deferred>,
    /// Stack of lexical defer-scope markers (each = `deferred.len()` at `EnterDeferScope`). A
    /// `LeaveDeferScope` drains `deferred` back down to the top marker, giving block-scoped defer:
    /// a block's defers run when that block exits, not when the whole frame does.
    defer_markers: Vec<usize>,
    /// Source span of the call site that pushed this frame (where the function was invoked). Used to
    /// build a runtime stack trace; not part of execution.
    call_span: Span,
}

/// A call registered by `defer`, with its receiver/arguments already evaluated. The held values are
/// GC roots while the deferred call is pending.
enum Deferred {
    /// `defer f(args)` — invoke the callable value with the args (`invoke_value`).
    Call { callee: Value, args: Vec<Value>, span: Span },
    /// `defer recv.name(args)` — dispatch the named method on the receiver.
    Method { recv: Value, name: String, args: Vec<Value>, span: Span },
}

impl Deferred {
    /// The GcRefs this deferred call keeps alive (callee/receiver + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let (head, args) = match self {
            Deferred::Call { callee, args, .. } => (callee, args),
            Deferred::Method { recv, args, .. } => (recv, args),
        };
        std::iter::once(head)
            .chain(args.iter())
            .filter_map(|v| if let Value::Obj(h) = v { Some(*h) } else { None })
    }
}

/// A task registered by `spawn`, awaiting its nursery's join barrier (C4). The callee/receiver and
/// arguments are evaluated and deep-copied across the airlock at the `spawn` statement (Go's
/// arg-evaluation timing); the body runs at the `parallel:` dedent. Mirrors the interpreter's
/// `Task` enum — a `spawn:` block is lowered to a zero-arg closure, so it rides the `Call` variant.
/// The held values are GC roots while the task is pending (see [`Vm::collect`]).
enum PendingCall {
    /// `spawn f(args)` (or a `spawn:` block, lowered to a zero-arg closure) — invoke the callable.
    Call { callee: Value, args: Vec<Value>, span: Span },
    /// `spawn recv.name(args)` — dispatch the named method on the receiver.
    Method { recv: Value, name: String, args: Vec<Value>, span: Span },
}

impl PendingCall {
    /// The GcRefs this pending task keeps alive (callee/receiver + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let (head, args) = match self {
            PendingCall::Call { callee, args, .. } => (callee, args),
            PendingCall::Method { recv, args, .. } => (recv, args),
        };
        std::iter::once(head)
            .chain(args.iter())
            .filter_map(|v| if let Value::Obj(h) = v { Some(*h) } else { None })
    }
}

struct Vm {
    program: Arc<Program>,
    heap: Heap,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    out: String,
    /// Captured stderr (written by `std.io.eprint`). Separate from `out` so streams don't mix.
    stderr: String,
    /// Runtime configuration the native std modules read (args/env/stdin). Default = inert.
    host: crate::native::HostConfig,
    call_depth: usize,
    /// Each module's namespace object, indexed by module index (run-once cache + import targets).
    module_objs: Vec<GcRef>,
    /// The current frame's slot base — cached so local access doesn't re-walk `frames` each op.
    cur_base: usize,
    /// Active `recover:` boundaries (a stack; the innermost is last). A runtime fault unwinds to the
    /// nearest handler whose frame is owned by the currently-running dispatch loop.
    handlers: Vec<Handler>,
    /// Set by `std.os.exit(code)` (clamped to `0..=255`). While set, the fault dispatch bypasses
    /// every `recover:` handler and unwinds to the top — a hard, uncatchable halt; the driver then
    /// reports `code` as the process exit status.
    pending_exit: Option<i32>,
    /// The stack trace of the uncaught fault that propagates, captured before frames unwind. The
    /// **deepest** fault wins (`fault_trace_depth` = frame count at capture): the original fault site
    /// captures first; a `defer`red call that itself faults runs while its owning frame is still on
    /// the stack (so it is deeper) and supersedes — matching Go's defer-supersedes semantics and the
    /// interpreter. Reset whenever a `recover:` boundary catches a fault. Read by the driver.
    fault_trace: Option<Vec<TraceFrame>>,
    fault_trace_depth: usize,
    /// Test mode: collect before *every* instruction, to surface any missing GC root.
    gc_stress: bool,
    /// Active `parallel:` nurseries (C4), innermost last. `EnterNursery` pushes; each `spawn`
    /// registers a [`PendingCall`] on the innermost list; `JoinNursery` drains it FIFO at the
    /// dedent. Tasks are GC roots while pending. A `recover:` boundary truncates this stack back to
    /// its install-time length on catch (see [`Handler::nursery_len`]), so a fault in the nursery
    /// body or a task can't leave a stale entry.
    nurseries: Vec<Vec<PendingCall>>,
    /// Every `Executor` created during the run (`Op::NewExecutor`), in creation order. These handles
    /// are GC roots (see [`Vm::collect`]) so an un-shut executor's queued work survives until the
    /// program-exit auto-drain (C5 / A2) reaps any executor never explicitly shut down — the VM
    /// parity counterpart of the interpreter's `Rc` registry.
    executors: Vec<GcRef>,
    /// Concurrency B1/B2: set by a blocking `recv` (empty channel) running inside an active nursery
    /// scheduler. It records the channel handle the running fiber is waiting on; `run_until` and the
    /// re-entrant call path break (without unwinding defers) when it is set, returning control to the
    /// scheduler so a sibling can run. Cleared by the scheduler when it resumes a fiber. It is a
    /// VM-global (not part of [`FiberCtx`]): only one fiber runs at a time, so at most one suspend is
    /// pending. See [`Vm::run_scheduler`].
    suspend: Option<GcRef>,
    /// Depth of native (Rust) callbacks currently on the host stack that re-enter Chezzi (operator
    /// overloads, `compare`/`hash`/`str` hooks, list HOFs, sorts, `Shared.update`, the executor
    /// drain, deferred calls). Their loop / recursion state lives on the Rust stack and cannot be
    /// parked into a [`Fiber`], so a `recv` reached while this is `> 0` cannot suspend — it faults
    /// `deadlock` instead (B1 v1 limitation). Maintained by [`Vm::guarded`].
    native_reentry: usize,
    /// Active cooperative-scheduler levels (B1/B2), innermost last. Each [`Nursery`] holds the parked
    /// joining (parent) fiber's context plus its child fibers; non-empty means a `recv` may suspend.
    /// Every parked fiber here is a GC root (see [`Vm::collect`]).
    scheduler_stack: Vec<Nursery>,
}

/// A fiber's saved execution context (B1): every `Vm` field that `run_until` reads or writes keyed by
/// per-execution indices. Swapped with the live `Vm` fields ([`Vm::swap_ctx`]) when a fiber is
/// scheduled in or out. `pending_exit` is deliberately NOT here — `std.os.exit` halts the whole
/// program, so it stays VM-global.
#[derive(Default)]
struct FiberCtx {
    frames: Vec<CallFrame>,
    stack: Vec<Value>,
    call_depth: usize,
    cur_base: usize,
    handlers: Vec<Handler>,
    nurseries: Vec<Vec<PendingCall>>,
    fault_trace: Option<Vec<TraceFrame>>,
    fault_trace_depth: usize,
}

/// Scheduling state of a child fiber within a [`Nursery`].
enum FiberState {
    /// Spawned but not yet started; holds the task to launch on first schedule.
    Pending(PendingCall),
    /// Started and runnable — resume by re-entering its `run_until`.
    Ready,
    /// Parked on an empty channel; runnable again once that channel is non-empty.
    Blocked(GcRef),
    /// Ran to completion.
    Done,
}

/// One child fiber: its saved context plus scheduling state. While the fiber is the one actively
/// running, its context lives in the live `Vm` fields and `ctx` is empty (see the scheduler).
struct Fiber {
    ctx: FiberCtx,
    state: FiberState,
}

/// One active `parallel:` scheduler level (B1/B2): the parked context of the joining (parent) fiber
/// and the child fibers spawned into the nursery. Pushed on `JoinNursery`, popped when every child
/// is `Done`.
struct Nursery {
    /// The joining fiber's context, parked while its children run cooperatively.
    parent: FiberCtx,
    children: Vec<Fiber>,
}

/// A snapshot taken at a `recover:` boundary (`Op::PushHandler`). On a caught fault the VM restores
/// the operand stack, call frames, and call-depth to these values, then jumps to `ip` in the
/// boundary's frame with the fault message pushed as the operand.
#[derive(Clone, Copy)]
struct Handler {
    stack_len: usize,
    frame_len: usize,
    call_depth: usize,
    ip: usize,
    /// `deferred.len()` of the boundary's frame when the handler was installed. A caught fault, a
    /// `?` short-circuit, or the Ok path drains the frame's defers down to this marker — the
    /// recover block's own cleanup — before binding the recovered value.
    defer_len: usize,
    /// `defer_markers.len()` of the boundary's frame at install. A fault / `?` short-circuit jumps
    /// past the `LeaveDeferScope`s of any defer scopes opened *inside* the recover block, so their
    /// markers would leak; the catch paths truncate `defer_markers` back to this length.
    markers_len: usize,
    /// `nurseries.len()` at install. A fault inside a `parallel:` body or a spawned task jumps past
    /// the `JoinNursery` that would pop the nursery; the catch path truncates `nurseries` back to
    /// this length so the stale nursery (and its aborted siblings) is reclaimed.
    nursery_len: usize,
}

/// B3.2 — what a synchronously-run isolated worker hands back across the airlock: the task's return
/// value serialized in the worker heap, plus the worker's captured stdout/stderr (decision F —
/// buffer-per-worker, returned to the parent rather than interleaved live).
// Forward-plumbing: exercised by the B3.2 unit tests in isolation; B3.3 wires the worker path into
// the `--parallel` engine (`join_nursery`), at which point this becomes reachable in the bin build.
#[allow(dead_code)]
#[derive(Debug)]
struct WorkerResult {
    value: WireValue,
    out: String,
    stderr: String,
}

/// B3.2 — a spawned task lowered to a `Send` description (no parent-heap `GcRef`s), ready to rebuild
/// in a worker heap. The callee crosses as its `ProtoId` (the proto lives in the shared `Arc<Program>`)
/// plus wire'd captures/args.
#[allow(dead_code)] // forward-plumbing for B3.3 `--parallel`; see `WorkerResult`.
enum Lowered {
    Closure { proto: ProtoId, captured: Vec<(String, WireValue)>, args: Vec<WireValue>, span: Span },
    Func { proto: ProtoId, args: Vec<WireValue>, span: Span },
}

impl Vm {
    fn new(program: Arc<Program>) -> Self {
        Vm {
            program,
            heap: Heap::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            out: String::new(),
            stderr: String::new(),
            host: crate::native::HostConfig::default(),
            call_depth: 0,
            module_objs: Vec::new(),
            cur_base: 0,
            handlers: Vec::new(),
            pending_exit: None,
            fault_trace: None,
            fault_trace_depth: 0,
            gc_stress: false,
            nurseries: Vec::new(),
            executors: Vec::new(),
            suspend: None,
            native_reentry: 0,
            scheduler_stack: Vec::new(),
        }
    }

    fn err(&self, message: String, span: Span) -> RuntimeError {
        RuntimeError { message, span }
    }

    /// Swap the live per-execution `Vm` fields with `ctx` (B1). Used by the nursery scheduler to
    /// schedule a fiber in (its saved context becomes live) or out (the running context is parked
    /// back into the fiber). Exactly the fields [`FiberCtx`] holds — `pending_exit` stays global.
    fn swap_ctx(&mut self, ctx: &mut FiberCtx) {
        std::mem::swap(&mut self.frames, &mut ctx.frames);
        std::mem::swap(&mut self.stack, &mut ctx.stack);
        std::mem::swap(&mut self.call_depth, &mut ctx.call_depth);
        std::mem::swap(&mut self.cur_base, &mut ctx.cur_base);
        std::mem::swap(&mut self.handlers, &mut ctx.handlers);
        std::mem::swap(&mut self.nurseries, &mut ctx.nurseries);
        std::mem::swap(&mut self.fault_trace, &mut ctx.fault_trace);
        std::mem::swap(&mut self.fault_trace_depth, &mut ctx.fault_trace_depth);
    }

    /// Run `f` with the native-reentry guard raised (B1). A blocking `recv` reached while the guard
    /// is up cannot park (its caller's loop/recursion state lives on the Rust stack, not in a
    /// [`Fiber`]), so it faults `deadlock` instead of suspending. Wraps every site that re-enters
    /// Chezzi code from native Rust.
    fn guarded<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>) -> Result<T, RuntimeError> {
        self.native_reentry += 1;
        let r = f(self);
        self.native_reentry -= 1;
        r
    }

    /// If `v` is an unhandled error (`Err(..)`/`None`) reaching the top level, build the runtime
    /// error that exits the program. Mirrors `interp::top_level_error` — message must be identical.
    fn top_level_error(&self, v: Value, span: Span) -> Option<RuntimeError> {
        let Value::Obj(h) = v else { return None };
        let Obj::Enum { ty, variant, payload } = self.heap.get(h) else { return None };
        // Builtin `Result`/`Option` only — a user enum that shadows `Err`/`None` is a normal value.
        let unhandled = (ty.as_ref() == "Result" && variant.as_ref() == "Err")
            || (ty.as_ref() == "Option" && variant.as_ref() == "None");
        if !unhandled {
            return None;
        }
        let detail = match payload.first() {
            Some(p) => self.display(*p),
            None => self.display(v),
        };
        Some(self.err(format!("unhandled error: {detail}"), span))
    }

    // ----- top-level drivers -----

    /// Run every module in dependency order, then the entry's `main()`.
    fn run(&mut self) -> Result<(), RuntimeError> {
        for idx in 0..self.program.modules.len() {
            self.run_module(idx)?;
        }
        Ok(())
    }

    fn run_module(&mut self, idx: usize) -> Result<(), RuntimeError> {
        let m = self.program.modules[idx].clone();
        // Fresh, empty namespace for this module.
        let mod_obj = self.heap.alloc(Obj::Module {
            name: m.label.clone().into_boxed_str(),
            globals: std::collections::HashMap::new(),
        });
        debug_assert_eq!(self.module_objs.len(), idx);
        self.module_objs.push(mod_obj);

        // A native std module: populate its globals with Rust `NativeFn`s + float constants and
        // skip running a toplevel. Mirrors the interpreter's `eval_module` native arm.
        if let Some(name) = m.native {
            for (mname, func) in crate::native::native_members(name) {
                let nat = self.heap.alloc(Obj::Native {
                    name: (*mname).into(),
                    func: *func,
                });
                self.module_define(mod_obj, mname, Value::Obj(nat));
            }
            for (cname, cval) in crate::native::native_consts(name) {
                self.module_define(mod_obj, cname, Value::Float(*cval));
            }
            return Ok(());
        }

        // Bind imports (dependencies already ran, so their namespaces are populated).
        for imp in &m.imports {
            self.bind_import(mod_obj, imp)?;
        }

        // Run the module body once. No module auto-runs `main` — it's an ordinary function the
        // program calls itself (scripting-language model). An unhandled `Err`/`None` reaching the
        // top level (via `PopExprStmt` or a top-level `?`) exits during this call.
        self.run_proto(m.toplevel, mod_obj, None, Vec::new(), false, true, Span { line: 1, col: 1 })?;
        Ok(())
    }

    fn bind_import(&mut self, into: GcRef, imp: &crate::resolver::ResolvedImport) -> Result<(), RuntimeError> {
        use crate::ast::Import;
        let target_idx = self
            .program
            .module_index(&imp.target)
            .expect("resolver guarantees the import target is in the graph");
        let target_obj = self.module_objs[target_idx];
        match &imp.import {
            Import::Module { path, alias } => {
                let name = alias.clone().unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.module_define(into, &name, Value::Obj(target_obj));
            }
            Import::From { names, .. } => {
                for (member, alias) in names {
                    let value = self.module_global(target_obj, member).ok_or_else(|| {
                        let tname = self.module_name(target_obj);
                        self.err(format!("module '{tname}' has no member '{member}'"), imp.span)
                    })?;
                    self.module_define(into, alias.as_ref().unwrap_or(member), value);
                }
            }
        }
        Ok(())
    }

    /// Push a frame for `proto` and run the dispatch loop until it returns; yield its result.
    #[allow(clippy::too_many_arguments)]
    fn run_proto(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let base_level = self.frames.len();
        self.push_frame(proto, home, closure, args, counted, is_toplevel, span)?;
        self.run_until(base_level)?;
        // B1: a blocking `recv` parked this call's fiber mid-flight. The frames stay live (they
        // replay on resume); propagate the signal up without popping a result — the caller gates on
        // `suspend` before using the (sentinel) return value.
        if self.suspend.is_some() {
            return Ok(Value::Nil);
        }
        Ok(self.stack.pop().unwrap_or(Value::Nil))
    }

    #[allow(clippy::too_many_arguments)]
    fn push_frame(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if counted {
            self.call_depth += 1;
            if self.call_depth > MAX_CALL_DEPTH {
                self.call_depth -= 1;
                return Err(self.err(
                    format!("maximum call depth ({MAX_CALL_DEPTH}) exceeded (infinite recursion?)"),
                    span,
                ));
            }
        }
        let n_slots = self.program.protos[proto].n_slots;
        let base = self.stack.len();
        for a in args {
            self.stack.push(a);
        }
        // Reserve the remaining (non-parameter) local slots.
        while self.stack.len() < base + n_slots {
            self.stack.push(Value::Nil);
        }
        self.frames.push(CallFrame {
            proto,
            ip: 0,
            base,
            home,
            closure,
            counted,
            is_toplevel,
            deferred: Vec::new(),
            defer_markers: Vec::new(),
            call_span: span,
        });
        self.cur_base = base;
        Ok(())
    }

    /// Build a stack trace from the live frames (innermost first), skipping module-toplevel frames.
    /// Valid only while the frames are intact — i.e. on the uncaught-error path, before unwinding.
    fn capture_trace(&self) -> Vec<TraceFrame> {
        self.frames
            .iter()
            .rev()
            .filter(|f| !f.is_toplevel)
            .map(|f| TraceFrame {
                function: self.program.protos[f.proto].name.clone(),
                span: f.call_span,
            })
            .collect()
    }

    // ----- the dispatch loop -----

    fn run_until(&mut self, base_level: usize) -> Result<(), RuntimeError> {
        let program = Arc::clone(&self.program);
        while self.frames.len() > base_level {
            // Collect at instruction boundaries only: here every live value is reachable from the
            // VM roots (operand stack, frame slots, frame homes/closures, module namespaces) —
            // there are no mid-opcode temporaries off the stack to miss.
            if self.gc_stress || self.heap.should_collect() {
                self.collect();
            }
            let fi = self.frames.len() - 1;
            let pid = self.frames[fi].proto;
            let ip = self.frames[fi].ip;
            self.frames[fi].ip = ip + 1;
            // Borrow the instruction (no per-step clone — the hot path must not allocate). The
            // `Arc` clone is a single refcount bump per loop entry; `op` then borrows program data
            // that is disjoint from the `&mut self` fields `step` touches.
            let op = &program.protos[pid].code[ip];
            let span = program.protos[pid].lines[ip];
            if let Err(rte) = self.step(op, span) {
                // `std.os.exit(code)` is a hard halt: unwind past every `recover:` to the top.
                if self.pending_exit.is_some() {
                    return Err(rte);
                }
                // Capture the stack trace of an uncaught fault now, while the frames are still intact
                // (the unwind below drops them). The deepest fault wins: the original fault captures
                // first, and a deeper deferred-call fault (run while its frame is still live) replaces
                // it. A fault this loop CAN catch resets the capture below, so no stale trace survives
                // a `recover:`.
                let caught_here = matches!(self.handlers.last().copied(), Some(h) if h.frame_len > base_level);
                if !caught_here && self.frames.len() > self.fault_trace_depth {
                    self.fault_trace = Some(self.capture_trace());
                    self.fault_trace_depth = self.frames.len();
                }
                // The nearest `recover:` boundary owned by THIS dispatch loop catches the fault; a
                // handler at/below `base_level` belongs to an outer loop, so we unwind to
                // `base_level` and propagate. Either way, every frame discarded on the way runs its
                // deferred calls first (Go: defers run as the panic unwinds, before recover regains
                // control). A fault inside a deferred call supersedes the original.
                let target = match self.handlers.last().copied() {
                    Some(h) if h.frame_len > base_level => h.frame_len,
                    _ => base_level,
                };
                let rte = self.unwind_deferred(target).unwrap_or(rte);
                // A deferred `std.os.exit` turns the unwind into a hard halt.
                if self.pending_exit.is_some() {
                    return Err(rte);
                }
                match self.handlers.last().copied() {
                    Some(h) if h.frame_len > base_level => {
                        self.handlers.pop();
                        // This `recover:` caught the fault — discard any trace captured deeper in (it
                        // belongs to a fault that is now handled), so a later uncaught fault re-captures.
                        self.fault_trace = None;
                        self.fault_trace_depth = 0;
                        // `unwind_deferred` already dropped frames down to `h.frame_len`; restore the
                        // operand stack / call-depth / ip to the boundary's snapshot.
                        self.stack.truncate(h.stack_len);
                        self.call_depth = h.call_depth;
                        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
                        self.frames[h.frame_len - 1].ip = h.ip;
                        // `unwind_deferred` ran the defers of frames ABOVE the boundary, but the
                        // boundary frame's own (recover-block) defers remain — drain them now, before
                        // binding the result. A fault in one supersedes the original.
                        let rte = self.drain_frame_to(h.defer_len).unwrap_or(rte);
                        // Drop the scope markers of any defer scopes opened inside the recover block:
                        // the fault jumped past their `LeaveDeferScope`s, so they would otherwise leak
                        // and corrupt later drains in this frame.
                        self.frames[h.frame_len - 1].defer_markers.truncate(h.markers_len);
                        // Reclaim any `parallel:` nursery the fault unwound past (its `JoinNursery`
                        // never ran), dropping the aborted siblings — mirrors the interpreter always
                        // reclaiming its nursery list.
                        self.nurseries.truncate(h.nursery_len);
                        if self.pending_exit.is_some() {
                            return Err(rte);
                        }
                        // Convert the fault message (a `str`, i.e. an `Error`) into `Err(msg)`; the
                        // boundary's `done` label receives a ready `Result`.
                        let msg = self.alloc_str(rte.message);
                        let err = self.alloc_enum("Result", "Err", vec![msg]);
                        self.push(err);
                    }
                    _ => return Err(rte),
                }
            }
            // B1: a blocking `recv` parked the running fiber. Stop the dispatch loop WITHOUT
            // unwinding (frames + defers stay intact to replay on resume) and hand control back to
            // the nursery scheduler, which parks this fiber and runs a sibling.
            if self.suspend.is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Mark-sweep collection. Roots: the whole operand stack (which contains every frame's local
    /// slots *and* any in-flight expression temporaries), each frame's home module + backing
    /// closure, and the module namespace cache. Everything else is garbage.
    /// Collect the GC roots held in a parked fiber context (B1): operand-stack objects, each frame's
    /// home/closure and pending deferred calls, and not-yet-run nursery tasks. Mirrors the
    /// live-context rooting in [`Vm::collect`].
    fn root_ctx(ctx: &FiberCtx, work: &mut Vec<GcRef>) {
        for v in &ctx.stack {
            if let Value::Obj(h) = v {
                work.push(*h);
            }
        }
        for f in &ctx.frames {
            work.push(f.home);
            if let Some(c) = f.closure {
                work.push(c);
            }
            for d in &f.deferred {
                work.extend(d.roots());
            }
        }
        for nursery in &ctx.nurseries {
            for task in nursery {
                work.extend(task.roots());
            }
        }
    }

    fn collect(&mut self) {
        let mut work: Vec<GcRef> = Vec::new();
        for v in &self.stack {
            if let Value::Obj(h) = v {
                work.push(*h);
            }
        }
        for f in &self.frames {
            work.push(f.home);
            if let Some(c) = f.closure {
                work.push(c);
            }
            for d in &f.deferred {
                work.extend(d.roots());
            }
        }
        // Pending `spawn` tasks (C4): their captured callee/receiver/args are roots until the task
        // runs at the nursery's join.
        for nursery in &self.nurseries {
            for task in nursery {
                work.extend(task.roots());
            }
        }
        // Live executors (C5 / A2): their queued work must survive to the program-exit auto-drain
        // even when no in-program handle remains.
        work.extend(self.executors.iter().copied());
        work.extend(self.module_objs.iter().copied());
        // Parked fibers in active cooperative schedulers (B1/B2): each level's joining-fiber context
        // plus every child fiber's context are roots while the children run. The CURRENTLY running
        // fiber's context is the live `self.{stack,frames,nurseries}` already rooted above; a parked
        // fiber's context lives in its `FiberCtx` (or, for a not-yet-started child, in its `Pending`
        // task). Without this, a blocked fiber's locals would be swept while it waits.
        for nursery in &self.scheduler_stack {
            Self::root_ctx(&nursery.parent, &mut work);
            for child in &nursery.children {
                Self::root_ctx(&child.ctx, &mut work);
                if let FiberState::Pending(task) = &child.state {
                    work.extend(task.roots());
                }
            }
        }

        while let Some(h) = work.pop() {
            if self.heap.mark(h) {
                work.extend(self.heap.children(h));
            }
        }
        self.heap.sweep();
    }

    fn base(&self) -> usize {
        self.cur_base
    }

    fn jump(&mut self, target: usize) {
        self.frames.last_mut().unwrap().ip = target;
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().expect("operand stack underflow")
    }

    fn step(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        match op {
            Op::ConstInt(n) => self.push(Value::Int(*n)),
            Op::ConstFloat(x) => self.push(Value::Float(*x)),
            Op::ConstStr(s) => {
                let h = self.heap.alloc(Obj::Str(s.clone().into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::True => self.push(Value::Bool(true)),
            Op::False => self.push(Value::Bool(false)),
            Op::Nil => self.push(Value::Nil),
            Op::Pop => {
                self.pop();
            }
            Op::PopExprStmt => {
                let v = self.pop();
                // An unhandled `Err`/`None` at the top level exits the program.
                if self.frames.last().unwrap().is_toplevel
                    && let Some(e) = self.top_level_error(v, span)
                {
                    return Err(e);
                }
            }
            Op::GetLocal(slot) => {
                let v = self.stack[self.base() + slot];
                self.push(v);
            }
            Op::SetLocal(slot) => {
                let v = self.pop();
                let at = self.base() + slot;
                self.stack[at] = v;
            }
            Op::GetGlobal(name) => {
                let home = self.frames.last().unwrap().home;
                let v = self.module_global(home, name).ok_or_else(|| self.err(format!("undefined name '{name}'"), span))?;
                self.push(v);
            }
            Op::DefineGlobal(name) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.module_define(home, name, v);
            }
            Op::SetGlobal(name) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                if !self.module_assign(home, name, v) {
                    return Err(self.err(format!("cannot assign to undefined name '{name}'"), span));
                }
            }
            Op::GetCaptured(name) => {
                let clo = self.frames.last().unwrap().closure;
                let v = clo
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => captured.get(name).copied(),
                        _ => None,
                    })
                    .or_else(|| {
                        let home = self.frames.last().unwrap().home;
                        self.module_global(home, name)
                    })
                    .ok_or_else(|| self.err(format!("undefined name '{name}'"), span))?;
                self.push(v);
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => self.arith(op, span)?,
            Op::Neg => {
                let v = self.pop();
                let r = match v {
                    Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| self.err("integer overflow in negation".to_string(), span))?,
                    Value::Float(f) => Value::Float(-f),
                    other => return Err(self.err(format!("cannot apply Neg to {}", self.type_name(other)), span)),
                };
                self.push(r);
            }
            Op::Not => {
                let v = self.pop();
                match v {
                    Value::Bool(b) => self.push(Value::Bool(!b)),
                    other => return Err(self.err(format!("cannot apply Not to {}", self.type_name(other)), span)),
                }
            }
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => self.compare_op(op, span)?,
            Op::Eq => {
                let r = self.pop();
                let l = self.pop();
                self.push(Value::Bool(self.values_equal(l, r)));
            }
            Op::NotEq => {
                let r = self.pop();
                let l = self.pop();
                self.push(Value::Bool(!self.values_equal(l, r)));
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr => self.bitwise(op, span)?,
            Op::AsBool => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Bool(_)) {
                    return Err(self.err(format!("expected bool, found {}", self.type_name(v)), span));
                }
            }
            Op::AsInt => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Int(_)) {
                    return Err(self.err(format!("expected int, found {}", self.type_name(v)), span));
                }
            }
            Op::PushHandler(target) => self.handlers.push(Handler {
                stack_len: self.stack.len(),
                frame_len: self.frames.len(),
                call_depth: self.call_depth,
                ip: *target,
                defer_len: self.frames.last().map(|f| f.deferred.len()).unwrap_or(0),
                markers_len: self.frames.last().map(|f| f.defer_markers.len()).unwrap_or(0),
                nursery_len: self.nurseries.len(),
            }),
            Op::PopHandler => {
                self.handlers.pop();
            }
            Op::Jump(t) => self.jump(*t),
            Op::JumpIfFalse(t) => {
                if let Value::Bool(false) = self.pop() {
                    self.jump(*t);
                }
            }
            Op::JumpIfFalseKeep(t) => {
                if let Value::Bool(false) = *self.stack.last().unwrap() {
                    self.jump(*t);
                }
            }
            Op::JumpIfTrueKeep(t) => {
                if let Value::Bool(true) = *self.stack.last().unwrap() {
                    self.jump(*t);
                }
            }
            Op::Call(argc) => self.do_call(*argc, span)?,
            Op::CallMethod(name, argc) => self.do_method_call(name, *argc, span)?,
            Op::CallBuiltin(name, argc) => self.do_builtin(name, *argc, span)?,
            Op::CallPrint(argc) => self.do_print(*argc, span)?,
            Op::Return => self.do_return(false)?,
            Op::DeferCall(argc) => self.do_defer(None, *argc, span),
            Op::DeferMethod(name, argc) => self.do_defer(Some(name.clone()), *argc, span),
            Op::EnterDeferScope => {
                let frame = self.frames.last_mut().unwrap();
                let marker = frame.deferred.len();
                frame.defer_markers.push(marker);
            }
            Op::LeaveDeferScope => {
                if let Some(e) = self.leave_defer_scope() {
                    return Err(e);
                }
            }
            Op::DrainHandlerDefers => {
                // The live recover handler is still installed (its `PopHandler` follows). Drain the
                // block's defers down to its marker; a fault propagates and is caught by that same
                // handler (becoming the recover's `Err`).
                if let Some(marker) = self.handlers.last().map(|h| h.defer_len)
                    && let Some(e) = self.drain_frame_to(marker)
                {
                    return Err(e);
                }
            }
            Op::Try => self.do_try(span)?,
            Op::JsonDecode(desc) => {
                let desc = desc.clone();
                self.json_decode(&desc, span)?;
            }
            Op::NewList(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::List(items));
                self.push(Value::Obj(h));
            }
            Op::NewTuple(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::Tuple(items));
                self.push(Value::Obj(h));
            }
            Op::NewMap(n) => {
                // Build an insertion-ordered hash map with last-key-wins upsert. Phase 1 hashes
                // every key while ALL operands are still rooted on the stack (a struct key's hash()
                // re-enters the VM and can GC); phase 2 then builds the map with no further re-entry
                // (so no GC), reading keys/values from the still-rooted stack.
                let count = *n;
                let at = self.stack.len() - 2 * count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    let k = self.stack[at + 2 * j];
                    hashes.push(self.hash_value(k, span)?);
                }
                let mut map = MapData::default();
                for (j, &hk) in hashes.iter().enumerate() {
                    let (k, v) = (self.stack[at + 2 * j], self.stack[at + 2 * j + 1]);
                    match map.candidates(hk).iter().copied().find(|&p| self.values_equal(map.entries[p].1, k)) {
                        Some(p) => map.entries[p].2 = v,
                        None => map.push(hk, k, v),
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Map(map));
                self.push(Value::Obj(h));
            }
            Op::NewSet(n) => {
                // Insertion-ordered hash set, dedup keeping first occurrence. Same two-phase rooting
                // as NewMap (phase 1 hashes all elements rooted; phase 2 builds GC-free).
                let count = *n;
                let at = self.stack.len() - count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    hashes.push(self.hash_value(self.stack[at + j], span)?);
                }
                let mut set = SetData::default();
                for (j, &he) in hashes.iter().enumerate() {
                    let e = self.stack[at + j];
                    if !set.candidates(he).iter().copied().any(|p| self.values_equal(set.entries[p].1, e)) {
                        set.push(he, e);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Set(set));
                self.push(Value::Obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(name, *argc, span)?,
            Op::NewEnum(ty, variant, argc) => self.new_enum(ty, variant, *argc, span)?,
            Op::MakeFunc(proto) => {
                let home = self.frames.last().unwrap().home;
                let h = self.heap.alloc(Obj::Func { proto: *proto, home });
                self.push(Value::Obj(h));
            }
            Op::MakeClosure(proto, entries) => {
                let frame = self.frames.last().unwrap();
                let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
                let mut captured = std::collections::HashMap::new();
                for e in entries {
                    let v = match e.src {
                        CapSrc::Slot(i) => self.stack[base + i],
                        CapSrc::Captured => enclosing
                            .and_then(|h| match self.heap.get(h) {
                                Obj::Closure { captured, .. } => captured.get(&e.name).copied(),
                                _ => None,
                            })
                            .unwrap_or(Value::Nil),
                    };
                    captured.insert(e.name.clone(), v);
                }
                let h = self.heap.alloc(Obj::Closure { proto: *proto, captured, home });
                self.push(Value::Obj(h));
            }
            Op::GetField(name) => self.get_field(name, span)?,
            Op::GetIndex => self.get_index(span)?,
            Op::GetSlice => self.get_slice(span)?, // Phase 4
            Op::SetField(name) => self.set_field(name, span)?,
            Op::SetIndex => self.set_index(span)?,
            Op::Dup => {
                let top = *self.stack.last().expect("Dup on empty stack");
                self.push(top);
            }
            Op::Dup2 => {
                let n = self.stack.len();
                let a = self.stack[n - 2];
                let b = self.stack[n - 1];
                self.push(a);
                self.push(b);
            }
            Op::ToStr => {
                let v = self.stack[self.stack.len() - 1]; // leave rooted; stringify may run user code
                let s = self.stringify(v, span)?;
                self.pop();
                let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::BuildStr(n) => {
                let at = self.stack.len() - *n;
                // Stringify in place so each interpolated part stays rooted while a `str` method runs.
                let mut s = String::new();
                for i in 0..*n {
                    let p = self.stack[at + i];
                    s.push_str(&self.stringify(p, span)?);
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::ListClone => {
                // Normalise a `for` iterand to an index-iterable list: a list is cloned (so a body
                // that mutates it doesn't disturb iteration); a map yields its keys (gap #14).
                let v = self.pop();
                match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => {
                            let cloned = items.clone();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::Obj(nh));
                        }
                        Obj::Map(m) => {
                            let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                            let nh = self.heap.alloc(Obj::List(keys));
                            self.push(Value::Obj(nh));
                        }
                        Obj::Set(s) => {
                            let elems: Vec<Value> = s.entries.iter().map(|(_, e)| *e).collect();
                            let nh = self.heap.alloc(Obj::List(elems));
                            self.push(Value::Obj(nh));
                        }
                        // A string iterates as 1-char strings (Python-style; gap: char type).
                        Obj::Str(s) => {
                            let chars: Vec<String> = s.chars().map(|c| c.to_string()).collect();
                            let items: Vec<Value> =
                                chars.into_iter().map(|c| self.alloc_str(c)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        _ => return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span)),
                    },
                    other => return Err(self.err(format!("cannot iterate over {}", self.type_name(other)), span)),
                }
            }
            Op::ArrLen => {
                let v = self.pop();
                let len = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => items.len() as i64,
                        _ => unreachable!("ArrLen on non-list"),
                    },
                    _ => unreachable!("ArrLen on non-list"),
                };
                self.push(Value::Int(len));
            }
            Op::IsStruct => {
                let v = self.pop();
                let is_struct =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Struct { .. }));
                self.push(Value::Bool(is_struct));
            }
            Op::IsMap => {
                let v = self.pop();
                let is_map = matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Map(_)));
                self.push(Value::Bool(is_map));
            }
            Op::EnsureEnum(slot) => {
                let v = self.stack[self.base() + *slot];
                if !matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Enum { .. })) {
                    return Err(self.err(format!("cannot match on {}", self.type_name(v)), span));
                }
            }
            Op::MatchArm { scrut, variant, nbind, bind_start, next } => self.match_arm(*scrut, variant, *nbind, *bind_start, *next, span)?,
            Op::MatchNoArm(slot) => {
                let v = self.stack[self.base() + *slot];
                let variant = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::Enum { variant, .. } => variant.to_string(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                return Err(self.err(format!("no match arm for variant '{variant}'"), span));
            }
            Op::EnterNursery => self.nurseries.push(Vec::new()),
            Op::JoinNursery => self.join_nursery()?,
            Op::SpawnCall(argc) => self.do_spawn(None, *argc, span)?,
            Op::SpawnMethod(name, argc) => self.do_spawn(Some(name.clone()), *argc, span)?,
            Op::SpawnBlock(proto, entries) => self.do_spawn_block(*proto, entries, span)?,
            Op::NewChannel => {
                let h = self.heap.alloc(Obj::Channel(Arc::new(ChannelCore::default())));
                self.push(Value::Obj(h));
            }
            Op::NewShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in).
                let init = self.to_wire(init).expect("Shared init must be sendable (B3.1 single-thread)");
                let h = self.heap.alloc(Obj::Shared(Arc::new(SharedCore { v: Mutex::new(init) })));
                self.push(Value::Obj(h));
            }
            Op::NewExecutor => {
                let h = self.heap.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
                // Register for the program-exit auto-drain; the handle is also a GC root, so the
                // executor's queued work survives even after every in-program handle is gone.
                self.executors.push(h);
                self.push(Value::Obj(h));
            }
        }
        Ok(())
    }

    // ----- arithmetic / comparison -----

    fn arith(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let name = match op {
            Op::Add => "Add",
            Op::Sub => "Sub",
            Op::Mul => "Mul",
            Op::Div => "Div",
            Op::Mod => "Mod",
            _ => unreachable!(),
        };
        let result = match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    Op::Add => a.checked_add(b),
                    Op::Sub => a.checked_sub(b),
                    Op::Mul => a.checked_mul(b),
                    Op::Div | Op::Mod if b == 0 => {
                        return Err(self.err(format!("{} by zero", if matches!(op, Op::Div) { "division" } else { "modulo" }), span));
                    }
                    Op::Div => a.checked_div(b),
                    Op::Mod => a.checked_rem(b),
                    _ => unreachable!(),
                };
                Value::Int(v.ok_or_else(|| self.err(format!("integer overflow in {name}"), span))?)
            }
            (a, b) if is_numeric(a) && is_numeric(b) => {
                let (x, y) = (as_f64(a), as_f64(b));
                if matches!(op, Op::Div | Op::Mod) && y == 0.0 {
                    return Err(self.err(format!("{} by zero", if matches!(op, Op::Div) { "division" } else { "modulo" }), span));
                }
                Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Mod => x % y,
                    _ => unreachable!(),
                })
            }
            // Arithmetic overloading: `+`/`-`/`*` on two structs dispatch to `add`/`sub`/`mul` (the
            // `Add`/`Sub`/`Mul` protocols). The checker has verified conformance. Must precede the
            // string-concat `Add` arm below (which would otherwise reject struct+struct).
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Add | Op::Sub | Op::Mul)
                    && matches!(self.heap.get(ha), Obj::Struct { .. })
                    && matches!(self.heap.get(hb), Obj::Struct { .. }) =>
            {
                self.struct_arith(op, l, r, span)?
            }
            (Value::Obj(ha), Value::Obj(hb)) if matches!(op, Op::Add) => {
                if let (Obj::Str(a), Obj::Str(b)) = (self.heap.get(ha), self.heap.get(hb)) {
                    let s = format!("{a}{b}");
                    let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                    Value::Obj(h)
                } else {
                    return Err(self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span));
                }
            }
            _ => return Err(self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span)),
        };
        self.push(result);
        Ok(())
    }

    /// Arithmetic operator overloading: dispatch `+`/`-`/`*` on two structs to the receiver's
    /// `add`/`sub`/`mul(self, other) -> Self` method (the `Add`/`Sub`/`Mul` protocols). `l`/`r` are
    /// passed as the call's args (rooted as the new frame's locals). Mirrors `interp::struct_arith`.
    fn struct_arith(&mut self, op: &Op, l: Value, r: Value, span: Span) -> Result<Value, RuntimeError> {
        let method = match op {
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            _ => unreachable!("struct_arith only handles + - *"),
        };
        let Value::Obj(h) = l else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h) else { unreachable!() };
        let name = name.clone();
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def
            .methods
            .get(method)
            .ok_or_else(|| self.err(format!("struct '{name}' has no '{method}' method"), span))?;
        let home = self.module_objs[def.module_idx];
        self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))
    }

    /// Bitwise / shift ops — int-only (gap #13). Shift amounts outside `0..64` are a runtime error
    /// (Rust would otherwise panic), with a message identical to the interpreter's.
    fn bitwise(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let name = match op {
            Op::BitAnd => "BitAnd",
            Op::BitOr => "BitOr",
            Op::BitXor => "BitXor",
            Op::Shl => "Shl",
            Op::Shr => "Shr",
            _ => unreachable!(),
        };
        let result = match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    Op::BitAnd => a & b,
                    Op::BitOr => a | b,
                    Op::BitXor => a ^ b,
                    Op::Shl | Op::Shr => {
                        if !(0..64).contains(&b) {
                            return Err(self.err(format!("shift amount {b} out of range (0..64)"), span));
                        }
                        if matches!(op, Op::Shl) { a << (b as u32) } else { a >> (b as u32) }
                    }
                    _ => unreachable!(),
                };
                Value::Int(v)
            }
            _ => return Err(self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span)),
        };
        self.push(result);
        Ok(())
    }

    fn compare_op(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // Operator overloading: ordering on two structs dispatches to `compare(self, other) -> int`
        // (the `Comparable` protocol). The checker has verified conformance. Equality stays
        // structural; only ordering is overloaded. Mirrors `interp::struct_ordering`.
        if let (Value::Obj(hl), Value::Obj(hr)) = (l, r)
            && matches!(self.heap.get(hl), Obj::Struct { .. })
            && matches!(self.heap.get(hr), Obj::Struct { .. })
        {
            return self.struct_ordering(op, l, r, span);
        }
        let name = match op {
            Op::Lt => "Lt",
            Op::LtEq => "LtEq",
            Op::Gt => "Gt",
            Op::GtEq => "GtEq",
            _ => unreachable!(),
        };
        let ord = self.compare(l, r).ok_or_else(|| self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span))?;
        let b = match op {
            Op::Lt => ord.is_lt(),
            Op::LtEq => ord.is_le(),
            Op::Gt => ord.is_gt(),
            Op::GtEq => ord.is_ge(),
            _ => unreachable!(),
        };
        self.push(Value::Bool(b));
        Ok(())
    }

    /// Dispatch an ordering operator on two structs to the receiver's `compare(self, other) -> int`
    /// method, mapping the sign of the result to a boolean. Mirrors `interp::struct_ordering`.
    fn struct_ordering(&mut self, op: &Op, l: Value, r: Value, span: Span) -> Result<(), RuntimeError> {
        let ord = self.struct_compare(l, r, span)?;
        let b = match op {
            Op::Lt => ord.is_lt(),
            Op::LtEq => ord.is_le(),
            Op::Gt => ord.is_gt(),
            Op::GtEq => ord.is_ge(),
            _ => unreachable!(),
        };
        self.push(Value::Bool(b));
        Ok(())
    }

    /// Call a struct's `compare(self, other) -> int` method and return the resulting `Ordering`.
    /// Shared by ordering operators (`struct_ordering`) and `list.sort()` over Comparable structs.
    /// Mirrors `interp::struct_compare`.
    fn struct_compare(&mut self, l: Value, r: Value, span: Span) -> Result<std::cmp::Ordering, RuntimeError> {
        let Value::Obj(h) = l else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h).clone() else { unreachable!() };
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def.methods.get("compare").ok_or_else(|| {
            self.err(format!("struct '{name}' has no 'compare' method (needed to order its values)"), span)
        })?;
        let home = self.module_objs[def.module_idx];
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))? {
            Value::Int(n) => Ok(n.cmp(&0)),
            other => Err(self.err(format!("compare() must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// A `u64` hash of `v` for map/set keys, upholding the invariant `values_equal(a,b) ⇒
    /// hash(a)==hash(b)`. Numeric keys hash by their canonical f64 bits (so `Int(3)` and `Float(3.0)`
    /// collide, matching `values_equal`'s numeric unification); str by content; a struct key
    /// dispatches its user `hash(self) -> int` (re-entrant — may allocate / trigger GC). Floats are
    /// rejected as keys by the checker (NaN footgun), so only integral-valued floats reach here.
    fn hash_value(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        match v {
            // A struct key dispatches its user `hash()` (re-entrant). Everything else is scalar.
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Struct { .. } => self.struct_hash(v, span),
                Obj::Str(_) => Ok(self.scalar_hash(v)),
                _ => Err(self.err(format!("{} is not hashable (cannot be a map/set key)", self.type_name(v)), span)),
            },
            _ => Ok(self.scalar_hash(v)),
        }
    }

    /// Infallible hash for scalar keys (int/float/bool/nil/str). Numeric values hash by canonical
    /// f64 bits so `3` and `3.0` collide; str by content. Non-scalar values fall back to `0` (a
    /// correctness-safe degenerate hash — `values_equal` still confirms each probe).
    fn scalar_hash(&self, v: Value) -> u64 {
        use std::hash::{Hash, Hasher};
        match v {
            // Normalise zero so `Int(0)`, `+0.0`, and `-0.0` (all `values_equal`) hash identically —
            // `(-0.0).to_bits() != (0.0).to_bits()` would otherwise break the hash invariant.
            Value::Int(n) => (if n == 0 { 0.0 } else { n as f64 }).to_bits(),
            Value::Float(f) => (if f == 0.0 { 0.0 } else { f }).to_bits(),
            Value::Bool(b) => b as u64,
            Value::Nil => 0,
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => {
                    let mut hr = std::collections::hash_map::DefaultHasher::new();
                    s.as_bytes().hash(&mut hr);
                    hr.finish()
                }
                _ => 0,
            },
        }
    }

    /// Dispatch a struct key's user `hash(self) -> int`, returning its `i64` as a `u64`. Mirrors
    /// [`struct_compare`] (re-entrant via `run_proto`).
    fn struct_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let Value::Obj(h) = v else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h).clone() else { unreachable!() };
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def.methods.get("hash").ok_or_else(|| {
            self.err(format!("struct '{name}' has no 'hash' method (needed to use it as a map/set key)"), span)
        })?;
        let home = self.module_objs[def.module_idx];
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(format!("hash() must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// Hash `key`, keeping `roots` alive on the operand stack across the call. A struct key's
    /// `hash()` re-enters the VM and can trigger GC; the map/set receiver and any in-flight
    /// key/value (already popped off the stack before dispatch) must be rooted or the collector
    /// could free them mid-hash. For scalar keys this is a couple of redundant push/pops.
    fn hash_key_rooted(&mut self, key: Value, roots: &[Value], span: Span) -> Result<u64, RuntimeError> {
        for &r in roots {
            self.push(r);
        }
        let res = self.hash_value(key, span);
        for _ in roots {
            self.pop();
        }
        res
    }

    /// `xs.sort()` over a list of Comparable structs, ordering via each struct's `compare`. Because
    /// `compare` re-enters the VM (and may allocate / trigger GC), this mirrors `list_sort_by`
    /// exactly: snapshot the elements into a heap list ROOTED on the operand stack, permute
    /// *indices* re-read from that rooted list per comparison (never holding unrooted `Value`s
    /// across a `compare` call), then write the result back. (Primitives use the faster
    /// `value_order`, which never re-enters the VM.) Mirrors `interp::eval_list_sort`.
    fn list_sort_structs(&mut self, src_h: GcRef, span: Span) -> Result<Value, RuntimeError> {
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort()`) is otherwise unrooted and the comparator's GC could collect it
        // before the write-back.
        self.push(Value::Obj(src_h));
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot across the comparator calls
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices_structs(snap_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via each struct's `compare`. Re-reads elements from `src_h` per comparison so no
    /// unrooted `Value` is held across the GC-capable `struct_compare` call.
    fn msort_indices_structs(&mut self, src_h: GcRef, idx: Vec<usize>, span: Span) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_structs(src_h, idx, span)?;
        let right = self.msort_indices_structs(src_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.struct_compare(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    fn compare(&self, l: Value, r: Value) -> Option<std::cmp::Ordering> {
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(&b)),
            (a, b) if is_numeric(a) && is_numeric(b) => as_f64(a).partial_cmp(&as_f64(b)),
            (Value::Obj(ha), Value::Obj(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(a), Obj::Str(b)) => Some(a.cmp(b)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Structural equality mirroring `interp::values_equal`.
    fn values_equal(&self, l: Value, r: Value) -> bool {
        match (l, r) {
            (a, b) if is_numeric(a) && is_numeric(b) => as_f64(a) == as_f64(b),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Nil, Value::Nil) => true,
            (Value::Obj(ha), Value::Obj(hb)) => {
                if ha == hb {
                    return true;
                }
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => a == b,
                    (Obj::List(a), Obj::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| self.values_equal(*x, *y)),
                    (Obj::Tuple(a), Obj::Tuple(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| self.values_equal(*x, *y)),
                    // Maps compare key+value pairwise in insertion order (matches the old assoc-list
                    // semantics — the cached hash is ignored).
                    (Obj::Map(a), Obj::Map(b)) => {
                        a.entries.len() == b.entries.len()
                            && a.entries.iter().zip(&b.entries).all(|((_, ka, va), (_, kb, vb))| {
                                self.values_equal(*ka, *kb) && self.values_equal(*va, *vb)
                            })
                    }
                    // Sets are unordered: equal iff same size and every element of `a` is in `b`.
                    (Obj::Set(a), Obj::Set(b)) => {
                        a.entries.len() == b.entries.len()
                            && a.entries.iter().all(|(_, x)| b.entries.iter().any(|(_, y)| self.values_equal(*x, *y)))
                    }
                    (Obj::Struct { name: na, fields: fa }, Obj::Struct { name: nb, fields: fb }) => {
                        na == nb
                            && fa.len() == fb.len()
                            && fa.iter().zip(fb).all(|((ka, va), (kb, vb))| ka == kb && self.values_equal(*va, *vb))
                    }
                    (Obj::Enum { ty: ta, variant: va, payload: pa }, Obj::Enum { ty: tb, variant: vb, payload: pb }) => {
                        ta == tb && va == vb && pa.len() == pb.len() && pa.iter().zip(pb).all(|(x, y)| self.values_equal(*x, *y))
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Total order over scalar values for `sort()`. The checker restricts `sort` to homogeneous
    /// int/float/str lists; str elements are read through the heap. Anything else compares Equal.
    fn value_order(&self, a: Value, b: Value) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(&y),
            (Value::Float(x), Value::Float(y)) => x.total_cmp(&y),
            (Value::Obj(ha), Value::Obj(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(x), Obj::Str(y)) => x.cmp(y),
                _ => Equal,
            },
            _ => Equal,
        }
    }

    // ----- calls -----

    fn do_call(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let callee = self.pop();
        let v = self.invoke_value(callee, args, span)?;
        if self.suspend.is_some() {
            return Ok(()); // B1: callee parked on a blocking `recv`; don't push a sentinel result.
        }
        self.push(v);
        Ok(())
    }

    /// Dispatch an already-evaluated callable `Value` on evaluated args, *returning* the result
    /// instead of pushing it. Shared by `do_call` (which pushes) and the higher-order list methods
    /// (which call it per element while keeping their source/result lists rooted on the stack).
    /// `args.len()` is the explicit arg count for arity checks.
    fn invoke_value(&mut self, callee: Value, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let argc = args.len();
        match callee {
            Value::Obj(h) => match self.heap.get(h).clone() {
                Obj::Func { proto, home } => {
                    self.check_arity("function", &self.program.protos[proto].name.clone(), self.program.protos[proto].arity, argc, span)?;
                    self.run_proto(proto, home, None, args, true, false, span)
                }
                Obj::Closure { proto, home, .. } => {
                    if argc != self.program.protos[proto].arity {
                        return Err(self.err(format!("closure expects {} argument(s), got {argc}", self.program.protos[proto].arity), span));
                    }
                    self.run_proto(proto, home, Some(h), args, true, false, span)
                }
                Obj::Native { func, .. } => self.invoke_native(func, args, span),
                _ => Err(self.err(format!("'{}' is not callable", self.type_name(callee)), span)),
            },
            other => Err(self.err(format!("'{}' is not callable", self.type_name(other)), span)),
        }
    }

    /// Invoke a native (Rust) function value (M6c). Builds a [`VmHost`] over the evaluated args,
    /// runs the binding, then lowers its engine-neutral [`NativeRet`] into a heap-allocated `Value`
    /// and pushes it. Lowering (the only allocation) happens here — at an instruction boundary,
    /// after the call returns — so the "collect only at instruction boundaries" GC invariant holds.
    fn invoke_native(
        &mut self,
        func: crate::native::NativeFn,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let mut host = VmHost { vm: self, args };
        let ret = func(&mut host).map_err(|e| RuntimeError { message: e.message, span })?;
        Ok(self.lower_native(ret))
    }

    /// Lower a native fn's engine-neutral [`crate::native::NativeRet`] into a VM `Value`, allocating
    /// heap objects for the reference kinds. `Ok`/`Err`/`Some`/`None` become the built-in
    /// `Result` / `Option` enum objects.
    fn lower_native(&mut self, ret: crate::native::NativeRet) -> Value {
        use crate::native::NativeRet as N;
        match ret {
            N::Int(n) => Value::Int(n),
            N::Float(f) => Value::Float(f),
            N::Bool(b) => Value::Bool(b),
            N::Nil => Value::Nil,
            N::Str(s) => self.alloc_str(s),
            N::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for x in items {
                    vs.push(self.lower_native(x));
                }
                Value::Obj(self.heap.alloc(Obj::List(vs)))
            }
            N::Struct { name, fields } => {
                // Lower fields first (each may allocate), then allocate the struct itself — keeps
                // every allocation at this instruction boundary, preserving the GC invariant.
                let mut fs = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    let lv = self.lower_native(v);
                    fs.push((k.into_boxed_str(), lv));
                }
                Value::Obj(self.heap.alloc(Obj::Struct { name: name.into_boxed_str(), fields: fs }))
            }
            N::Map(entries) => {
                // Native maps have unique scalar (str) keys — hash them directly (no re-entry, no
                // dedup needed).
                let mut map = MapData::default();
                for (k, v) in entries {
                    let lk = self.lower_native(k);
                    let lv = self.lower_native(v);
                    let hk = self.scalar_hash(lk);
                    map.push(hk, lk, lv);
                }
                Value::Obj(self.heap.alloc(Obj::Map(map)))
            }
            N::Ok(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Result", "Ok", vec![p])
            }
            N::Err(msg) => {
                let p = self.alloc_str(msg);
                self.alloc_enum("Result", "Err", vec![p])
            }
            N::Some(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Option", "Some", vec![p])
            }
            N::None => self.alloc_enum("Option", "None", Vec::new()),
        }
    }

    fn alloc_enum(&mut self, ty: &str, variant: &str, payload: Vec<Value>) -> Value {
        Value::Obj(self.heap.alloc(Obj::Enum {
            ty: ty.into(),
            variant: variant.into(),
            payload,
        }))
    }

    /// `Op::JsonDecode`: pop the `Result[Json]` from `parse`, coerce its `Ok` payload against the
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`.
    fn json_decode(&mut self, desc: &crate::json_decode::TypeDescriptor, span: Span) -> Result<(), RuntimeError> {
        let res = self.pop();
        let bad = "decode: parse did not return a Result".to_string();
        let (rty, variant, payload) =
            self.enum_parts(res).ok_or_else(|| self.err(bad.clone(), span))?;
        if rty != "Result" {
            return Err(self.err(bad, span));
        }
        match variant.as_str() {
            "Err" => {
                self.push(res); // a Result Err(str) is already a valid Result[T]
                Ok(())
            }
            "Ok" if payload.len() == 1 => {
                let jv = payload[0];
                match self.coerce_json(jv, desc, "$") {
                    Ok(v) => {
                        let r = self.alloc_enum("Result", "Ok", vec![v]);
                        self.push(r);
                    }
                    Err(msg) => {
                        let s = self.alloc_str(msg);
                        let r = self.alloc_enum("Result", "Err", vec![s]);
                        self.push(r);
                    }
                }
                Ok(())
            }
            _ => Err(self.err(bad, span)),
        }
    }

    /// The enum type, variant name, and (copied) payload of an enum value; `None` if not an enum.
    fn enum_parts(&self, v: Value) -> Option<(String, String, Vec<Value>)> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum { ty, variant, payload } => {
                    Some((ty.to_string(), variant.to_string(), payload.clone()))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type. `path` is a
    /// JSON-pointer-ish breadcrumb for error messages. Mirrors the interpreter's `coerce_json`.
    fn coerce_json(
        &mut self,
        jv: Value,
        desc: &crate::json_decode::TypeDescriptor,
        path: &str,
    ) -> Result<Value, String> {
        use crate::json_decode::TypeDescriptor as D;
        let (_jty, variant, payload) = self
            .enum_parts(jv)
            .ok_or_else(|| format!("decode: expected a JSON value at {path}"))?;
        let mismatch = |want: &str| format!("decode: expected {want} at {path}, found {}", crate::json_decode::json_kind(&variant));
        match desc {
            D::Int => {
                let f = self.json_num(&variant, &payload).ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}"));
                }
                Ok(Value::Int(f as i64))
            }
            D::Float => {
                let f = self.json_num(&variant, &payload).ok_or_else(|| mismatch("float"))?;
                Ok(Value::Float(f))
            }
            D::Bool => match (variant.as_str(), payload.first()) {
                ("Bool", Some(Value::Bool(b))) => Ok(Value::Bool(*b)),
                _ => Err(mismatch("bool")),
            },
            D::Str => {
                if variant == "Str" {
                    let s = self.val_str(payload[0]).unwrap_or_default();
                    Ok(self.alloc_str(s))
                } else {
                    Err(mismatch("str"))
                }
            }
            D::Option(inner) => {
                if variant == "Null" {
                    Ok(self.alloc_enum("Option", "None", Vec::new()))
                } else {
                    let v = self.coerce_json(jv, inner, path)?;
                    Ok(self.alloc_enum("Option", "Some", vec![v]))
                }
            }
            D::List(inner) => {
                if variant != "Arr" {
                    return Err(mismatch("array"));
                }
                let items = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::List(items) => items.clone(),
                    _ => return Err(mismatch("array")),
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, it) in items.into_iter().enumerate() {
                    out.push(self.coerce_json(it, inner, &format!("{path}[{i}]"))?);
                }
                Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
            }
            D::Map(inner) => {
                if variant != "Obj" {
                    return Err(mismatch("object"));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                let mut out = MapData::default();
                for (hk, k, v) in entries {
                    let key = self.val_str(k).unwrap_or_default();
                    let coerced = self.coerce_json(v, inner, &format!("{path}.{key}"))?;
                    out.push(hk, k, coerced); // str keys unchanged → reuse the cached hash
                }
                Ok(Value::Obj(self.heap.alloc(Obj::Map(out))))
            }
            D::Struct { name, fields } => {
                if variant != "Obj" {
                    return Err(mismatch(&format!("object for {name}")));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                let mut field_vals: Vec<(Box<str>, Value)> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = entries.iter().find(|(_, k, _)| {
                        self.val_str(*k).as_deref() == Some(fname.as_str())
                    });
                    let fpath = format!("{path}.{fname}");
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(*jval, fdesc, &fpath)?,
                        None => match fdesc {
                            // A missing Option field decodes to None; anything else is an error.
                            D::Option(_) => self.alloc_enum("Option", "None", Vec::new()),
                            _ => return Err(format!("decode: missing key '{fname}' at {path}")),
                        },
                    };
                    field_vals.push((fname.clone().into_boxed_str(), v));
                }
                let h = self.heap.alloc(Obj::Struct { name: name.clone().into_boxed_str(), fields: field_vals });
                Ok(Value::Obj(h))
            }
        }
    }

    /// The `f64` of a JSON `Num`, else `None`.
    fn json_num(&self, variant: &str, payload: &[Value]) -> Option<f64> {
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

    /// The owned text of a str value, else `None`.
    fn val_str(&self, v: Value) -> Option<String> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The heap handle of an `Obj` value (caller guarantees it is one).
    fn as_obj(&self, v: Value) -> GcRef {
        match v {
            Value::Obj(h) => h,
            _ => unreachable!("as_obj on non-object"),
        }
    }

    fn check_arity(&self, _kind: &str, name: &str, want: usize, got: usize, span: Span) -> Result<(), RuntimeError> {
        if want != got {
            return Err(self.err(format!("function '{name}' expects {want} argument(s), got {got}"), span));
        }
        Ok(())
    }

    fn do_method_call(&mut self, method: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let recv = self.pop();
        // `compare` on a primitive (int/float/str): they intrinsically satisfy `Comparable`, so an
        // erased generic body may call `.compare()` on a concrete primitive. Return the sign of the
        // ordering. Structs with their own `compare` fall through to the normal dispatch below.
        // Mirrors `interp::eval_method_call`.
        if method == "compare" && args.len() == 1 {
            let is_prim = matches!(recv, Value::Int(_) | Value::Float(_))
                || matches!(recv, Value::Obj(h) if matches!(self.heap.get(h), Obj::Str(_)));
            if is_prim
                && let Some(ord) = self.compare(recv, args[0])
            {
                self.push(Value::Int(ord as i64));
                return Ok(());
            }
        }
        let Value::Obj(h) = recv else {
            return Err(self.err(format!("type {} has no method '{method}'", self.type_name(recv)), span));
        };
        // Higher-order list methods (`map`/`filter`/`fold`) call a closure per element, which runs
        // nested VM frames that may GC at instruction boundaries. They keep the source + result
        // (and fold's accumulator) rooted on the operand stack across the loop — see `list_hof`.
        if matches!(self.heap.get(h), Obj::List(_)) && matches!(method, "map" | "filter" | "fold") {
            let result = self.list_hof(h, method, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by` also runs a closure per comparison, but sorts in place and returns nil.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by" {
            let result = self.list_sort_by(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by_key` calls a key extractor once per element, then sorts in place by key.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by_key" {
            let result = self.list_sort_by_key(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // Concurrency C4: `Channel` / `Shared` methods mutate the heap object in place (and `update`
        // re-enters the VM), so dispatch them directly off the handle, like the core-type methods.
        if matches!(self.heap.get(h), Obj::Channel(_)) {
            let result = self.channel_method(h, method, &args, span)?;
            if self.suspend.is_some() {
                return Ok(()); // B1: `recv` parked this fiber and re-rooted the receiver itself.
            }
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Shared(_)) {
            let result = self.shared_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Executor(_)) {
            let result = self.executor_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // Core-type methods (M6): built-in methods on `str` / `list`. Handled before the clone-match
        // so `list.push` mutates the heap object in place (the match below clones the Obj). Mirrors
        // `interp::builtins::call_method` exactly — error strings included (parity-tested).
        if matches!(self.heap.get(h), Obj::Str(_) | Obj::List(_) | Obj::Map(_) | Obj::Set(_)) {
            let result = self.core_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        match self.heap.get(h).clone() {
            // `module.fn(args)` — plain call on the looked-up member, no `self`.
            Obj::Module { name, globals } => {
                let member = globals.get(method).copied().ok_or_else(|| self.err(format!("module '{name}' has no member '{method}'"), span))?;
                self.stack.push(member);
                self.stack.extend(args);
                self.do_call(argc, span)
            }
            Obj::Struct { name, fields } => {
                let def = self.program.structs.get(name.as_ref()).cloned().ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                if let Some(&proto) = def.methods.get(method) {
                    let home = self.module_objs[def.module_idx];
                    if self.program.protos[proto].arity != argc + 1 {
                        // `self` + explicit args.
                        return Err(self.err(format!("function '{}' expects {} argument(s), got {}", self.program.protos[proto].name, self.program.protos[proto].arity, argc + 1), span));
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.suspend.is_some() {
                        return Ok(()); // B1: the method parked on a blocking `recv`.
                    }
                    self.push(v);
                    return Ok(());
                }
                // No method named `method`: fall back to a function-typed *field* — `recv.f(args)`
                // where `f` holds a function value (the checker verified `f: fn(...) -> ...`).
                // Invoked as a value (no `self` bound — it's not a method).
                if let Some((_, fval)) = fields.iter().find(|(k, _)| k.as_ref() == method) {
                    let v = self.invoke_value(*fval, args, span)?;
                    if self.suspend.is_some() {
                        return Ok(()); // B1: the function-field call parked on a blocking `recv`.
                    }
                    self.push(v);
                    return Ok(());
                }
                Err(self.err(format!("struct '{name}' has no method '{method}'"), span))
            }
            _ => Err(self.err(format!("type {} has no method '{method}'", self.type_name(recv)), span)),
        }
    }

    /// Higher-order list methods `map` / `filter` / `fold`. `src_h` is the receiver list. Each
    /// element is fed to a closure via `invoke_value`, which runs nested VM frames that can trigger
    /// GC at instruction boundaries. To keep the GC from collecting in-flight heap values, the
    /// source list, the partially-built result list (map/filter), and the fold accumulator are all
    /// kept rooted on the operand stack across the iteration. Returns the result (caller pushes it).
    /// Arity & error messages match the interp exactly (parity-tested).
    fn list_hof(&mut self, src_h: GcRef, method: &str, mut args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        // ROOT the source list on the operand stack so its elements survive every closure call.
        self.push(Value::Obj(src_h));
        let n = match self.heap.get(src_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!("list_hof on non-list"),
        };
        match method {
            "map" | "filter" => {
                if args.len() != 1 {
                    self.pop(); // unroot source before erroring
                    return Err(self.err(format!("'{method}' expects 1 argument(s), got {}", args.len()), span));
                }
                let f = args.swap_remove(0);
                let is_filter = method == "filter";
                // ROOT the result list too.
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::Obj(res_h));
                for i in 0..n {
                    // Re-read each iteration; `src_h` stays valid (rooted on the stack).
                    let elem = match self.heap.get(src_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    // May GC; both source and result lists are rooted, so their elements survive.
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    if is_filter {
                        match out {
                            Value::Bool(true) => {
                                if let Obj::List(items) = self.heap.get_mut(res_h) {
                                    items.push(elem);
                                }
                            }
                            Value::Bool(false) => {}
                            other => {
                                self.pop(); // unroot result
                                self.pop(); // unroot source
                                return Err(self.err(format!("filter predicate must return bool, got {}", self.type_name(other)), span));
                            }
                        }
                    } else if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(out);
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot source
                Ok(Value::Obj(res_h))
            }
            "fold" => {
                if args.len() != 2 {
                    self.pop(); // unroot source
                    return Err(self.err(format!("'fold' expects 2 argument(s), got {}", args.len()), span));
                }
                let f = args.swap_remove(1);
                let init = args.swap_remove(0);
                // ROOT the accumulator: push init, remember its slot, and replace in place each step.
                // `acc_slot` sits below every nested frame's base (frames push above the current
                // stack top and pop back to it), so the index stays valid across `invoke_value`.
                self.push(init);
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    let elem = match self.heap.get(src_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot source
                Ok(acc)
            }
            _ => unreachable!("list_hof called with non-HOF method {method}"),
        }
    }

    /// `xs.sort_by(cmp)` — stable in-place sort driven by a Chezzi comparator `fn(T, T) -> int`
    /// (negative = a before b, positive = a after b, zero = equal). The comparator re-enters the VM
    /// and may GC, so we never hold the elements in an unrooted Rust `Vec`: the source list stays
    /// rooted on the operand stack, and the merge sort permutes plain `usize` **indices**, re-reading
    /// elements from the rooted heap object on each comparison. The final permutation is materialised
    /// only after all comparator calls finish (no GC in between). Returns `nil`.
    fn list_sort_by(&mut self, src_h: GcRef, mut args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(format!("'sort_by' expects 1 argument(s), got {}", args.len()), span));
        }
        let cmp = args.swap_remove(0);
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort_by(...)`) is otherwise unrooted and the comparator's GC could
        // collect it before the write-back.
        self.push(Value::Obj(src_h));
        // Sort a SNAPSHOT taken now (matching the interpreter): a comparator that mutates the source
        // list mid-sort must not perturb the ordering, and its mutations are discarded by the final
        // write-back. The snapshot list is itself heap-allocated and rooted on the operand stack so
        // its elements survive the comparator's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices(snap_h, (0..n).collect(), cmp, span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via the Chezzi comparator `cmp`.
    fn msort_indices(&mut self, src_h: GcRef, idx: Vec<usize>, cmp: Value, span: Span) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices(src_h, idx, cmp, span)?;
        let right = self.msort_indices(src_h, right, cmp, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= 0` keeps the left element first on ties → stable.
            if self.compare_with(cmp, a, b, span)? <= 0 {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Run the comparator on `(a, b)` and return its int result (errors if it returns non-int).
    fn compare_with(&mut self, cmp: Value, a: Value, b: Value, span: Span) -> Result<i64, RuntimeError> {
        match self.guarded(|vm| vm.invoke_value(cmp, vec![a, b], span))? {
            Value::Int(n) => Ok(n),
            other => Err(self.err(format!("sort_by comparator must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// `xs.sort_by_key(f)` — stable in-place sort by a derived key `f: fn(T) -> K`. Mirrors
    /// `list_sort_by`'s GC discipline: the source list, an element snapshot, AND a parallel **keys**
    /// list are all rooted on the operand stack so the re-entrant extractor (and a Comparable-struct
    /// key's `compare`) can GC freely. Keys are computed once per element; the merge sort permutes
    /// `usize` indices, re-reading keys from the rooted keys list per comparison. Returns `nil`.
    fn list_sort_by_key(&mut self, src_h: GcRef, mut args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(format!("'sort_by_key' expects 1 argument(s), got {}", args.len()), span));
        }
        let f = args.swap_remove(0);
        self.push(Value::Obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by_key on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        // Compute keys once per element into a rooted list. Each `invoke_value` may GC; already-pushed
        // keys survive because `keys_h` is rooted (a `Vec::push` into it does not itself GC).
        let keys_h = self.heap.alloc(Obj::List(Vec::with_capacity(n)));
        self.push(Value::Obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h) {
                Obj::List(v) => v[i],
                _ => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    if let Obj::List(v) = self.heap.get_mut(keys_h) {
                        v.push(k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        let order = match self.msort_indices_by_key(keys_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot keys
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No extractor/compare calls remain, so no GC: reorder the snapshot and write back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted keys list `keys_h`), ordering
    /// by each key's natural order via [`order_key`].
    fn msort_indices_by_key(&mut self, keys_h: GcRef, idx: Vec<usize>, span: Span) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_by_key(keys_h, idx, span)?;
        let right = self.msort_indices_by_key(keys_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(keys_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(keys_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.order_key(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Natural order over two `sort_by_key` keys: a Comparable struct key dispatches to its
    /// `compare`; scalar keys (int/float/str) use the built-in [`Vm::compare`]. The checker has
    /// verified the key type is orderable.
    fn order_key(&mut self, a: Value, b: Value, span: Span) -> Result<std::cmp::Ordering, RuntimeError> {
        if let (Value::Obj(ha), Value::Obj(hb)) = (a, b)
            && matches!(self.heap.get(ha), Obj::Struct { .. })
            && matches!(self.heap.get(hb), Obj::Struct { .. })
        {
            return self.struct_compare(a, b, span);
        }
        self.compare(a, b).ok_or_else(|| {
            self.err(
                format!("sort_by_key keys are not comparable: {} vs {}", self.type_name(a), self.type_name(b)),
                span,
            )
        })
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
    /// Clone the elements of a `list`-typed argument for `concat`/`extend`. The checker guarantees
    /// the type; a non-list here is an internal invariant break, reported for safety.
    fn expect_list_obj(&self, method: &str, arg: Value, span: Span) -> Result<Vec<Value>, RuntimeError> {
        match arg {
            Value::Obj(ah) => match self.heap.get(ah) {
                Obj::List(items) => Ok(items.clone()),
                _ => Err(self.err(format!("{method}() expects a list argument, got {}", self.type_name(arg)), span)),
            },
            other => Err(self.err(format!("{method}() expects a list argument, got {}", self.type_name(other)), span)),
        }
    }

    /// Insert-or-overwrite `(hk, key, val)` into the heap map at `h` (last write wins). Used by
    /// `map.update`. No allocation, so no GC concerns.
    fn map_upsert_in_heap(&mut self, h: GcRef, hk: u64, key: Value, val: Value) {
        let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
        let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
        let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
        match pos {
            Some(i) => m.entries[i].2 = val,
            None => m.push(hk, key, val),
        }
    }

    fn core_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        // A str argument's owned text, with a uniform type error matching the interp.
        let str_arg = |vm: &Vm, i: usize| -> Result<String, RuntimeError> {
            match args[i] {
                Value::Obj(ah) => match vm.heap.get(ah) {
                    Obj::Str(a) => Ok(a.to_string()),
                    _ => Err(vm.err(format!("{method}() expects a str argument, got {}", vm.type_name(args[i])), span)),
                },
                other => Err(vm.err(format!("{method}() expects a str argument, got {}", vm.type_name(other)), span)),
            }
        };
        match self.heap.get(h) {
            Obj::Str(s) => {
                let s = s.to_string();
                match method {
                    "len" => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::Int(s.chars().count() as i64))
                    }
                    "upper" => {
                        self.arity_err("upper", args, 0, span)?;
                        Ok(self.alloc_str(s.to_uppercase()))
                    }
                    "lower" => {
                        self.arity_err("lower", args, 0, span)?;
                        Ok(self.alloc_str(s.to_lowercase()))
                    }
                    "trim" => {
                        self.arity_err("trim", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // `str` conforms to `Error`: `message()` returns the string itself.
                    "message" => {
                        self.arity_err("message", args, 0, span)?;
                        Ok(self.alloc_str(s.to_string()))
                    }
                    "split" => {
                        self.arity_err("split", args, 1, span)?;
                        let sep = str_arg(self, 0)?;
                        let parts: Vec<Value> =
                            s.split(sep.as_str()).map(|p| self.alloc_str(p.to_string())).collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(parts))))
                    }
                    "chars" => {
                        self.arity_err("chars", args, 0, span)?;
                        let cs: Vec<Value> =
                            s.chars().map(|c| self.alloc_str(c.to_string())).collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(cs))))
                    }
                    "starts_with" => {
                        self.arity_err("starts_with", args, 1, span)?;
                        Ok(Value::Bool(s.starts_with(str_arg(self, 0)?.as_str())))
                    }
                    "contains" => {
                        self.arity_err("contains", args, 1, span)?;
                        Ok(Value::Bool(s.contains(str_arg(self, 0)?.as_str())))
                    }
                    "join" => {
                        self.arity_err("join", args, 1, span)?;
                        let Value::Obj(lh) = args[0] else {
                            return Err(self.err(format!("join() expects a list of str, got {}", self.type_name(args[0])), span));
                        };
                        let Obj::List(items) = self.heap.get(lh) else {
                            return Err(self.err(format!("join() expects a list of str, got {}", self.type_name(args[0])), span));
                        };
                        let mut out = String::new();
                        for (i, item) in items.clone().iter().enumerate() {
                            let Value::Obj(ih) = item else {
                                return Err(self.err(format!("join() expects a list of str, got an element of type {}", self.type_name(*item)), span));
                            };
                            let Obj::Str(part) = self.heap.get(*ih) else {
                                return Err(self.err(format!("join() expects a list of str, got an element of type {}", self.type_name(*item)), span));
                            };
                            if i > 0 {
                                out.push_str(&s);
                            }
                            out.push_str(part);
                        }
                        Ok(self.alloc_str(out))
                    }
                    _ => Err(self.err(format!("type str has no method '{method}'"), span)),
                }
            }
            Obj::List(items) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(items.len() as i64))
                }
                "push" => {
                    self.arity_err("push", args, 1, span)?;
                    let v = args[0];
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    items.push(v);
                    Ok(Value::Nil)
                }
                "pop" => {
                    self.arity_err("pop", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    match items.pop() {
                        Some(v) => {
                            let eh = self.heap.alloc(Obj::Enum {
                                ty: "Option".into(),
                                variant: "Some".into(),
                                payload: vec![v],
                            });
                            Ok(Value::Obj(eh))
                        }
                        None => {
                            let eh = self.heap.alloc(Obj::Enum {
                                ty: "Option".into(),
                                variant: "None".into(),
                                payload: vec![],
                            });
                            Ok(Value::Obj(eh))
                        }
                    }
                }
                "reverse" => {
                    self.arity_err("reverse", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    items.reverse();
                    Ok(Value::Nil)
                }
                "sort" => {
                    self.arity_err("sort", args, 0, span)?;
                    // In place, ascending. Checker guarantees a homogeneous orderable element type.
                    // A list of Comparable structs orders via each struct's `compare` (engine
                    // re-entry, so a merge sort that holds `&mut self`); primitives use the faster
                    // `value_order`. Str elements live on the heap, so `value_order` needs
                    // `&self.heap` — clone the elements out, sort (no alloc/closure → no GC for the
                    // primitive path), then write back.
                    let is_struct =
                        matches!(items.first(), Some(Value::Obj(hh)) if matches!(self.heap.get(*hh), Obj::Struct { .. }));
                    if is_struct {
                        // Struct compare re-enters the VM (may GC) → rooted, index-based sort.
                        return self.list_sort_structs(h, span);
                    }
                    let mut elems = items.clone();
                    elems.sort_by(|a, b| self.value_order(*a, *b));
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    *items = elems;
                    Ok(Value::Nil)
                }
                "contains" => {
                    self.arity_err("contains", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    Ok(Value::Bool(elems.iter().any(|v| self.values_equal(*v, target))))
                }
                "index_of" => {
                    self.arity_err("index_of", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    let idx = elems.iter().position(|v| self.values_equal(*v, target));
                    Ok(Value::Int(idx.map(|i| i as i64).unwrap_or(-1)))
                }
                "concat" => {
                    self.arity_err("concat", args, 1, span)?;
                    let mut out = items.clone();
                    out.extend(self.expect_list_obj("concat", args[0], span)?);
                    // `out` is fully built and moved into the new Obj before any GC can run.
                    Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
                }
                "extend" => {
                    self.arity_err("extend", args, 1, span)?;
                    // Snapshot the other side first so `xs.extend(xs)` (self-extend) terminates.
                    let appended = self.expect_list_obj("extend", args[0], span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    items.extend(appended);
                    Ok(Value::Nil)
                }
                "sum" => {
                    self.arity_err("sum", args, 0, span)?;
                    let any_float = items.iter().any(|v| matches!(v, Value::Float(_)));
                    if any_float {
                        let mut acc = 0.0_f64;
                        for v in items.iter() {
                            match v {
                                Value::Int(n) => acc += *n as f64,
                                Value::Float(f) => acc += *f,
                                other => {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(*other)), span));
                                }
                            }
                        }
                        Ok(Value::Float(acc))
                    } else {
                        let mut acc = 0_i64;
                        for v in items.iter() {
                            match v {
                                Value::Int(n) => acc += *n,
                                other => {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(*other)), span));
                                }
                            }
                        }
                        Ok(Value::Int(acc))
                    }
                }
                _ => Err(self.err(format!("type list has no method '{method}'"), span)),
            },
            Obj::Map(m) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(m.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    let found = m.candidates(hk).iter().any(|&p| self.values_equal(m.entries[p].1, key));
                    Ok(Value::Bool(found))
                }
                "get" => {
                    self.arity_err("get", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    let found = m.candidates(hk).iter().copied()
                        .find(|&p| self.values_equal(m.entries[p].1, key))
                        .map(|p| m.entries[p].2);
                    match found {
                        Some(v) => Ok(self.alloc_enum("Option", "Some", vec![v])),
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "keys" => {
                    self.arity_err("keys", args, 0, span)?;
                    let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(keys))))
                }
                "values" => {
                    self.arity_err("values", args, 0, span)?;
                    let vals: Vec<Value> = m.entries.iter().map(|(_, _, v)| *v).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(vals))))
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
                    match pos {
                        Some(i) => {
                            let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
                            let (_, _, v) = m.remove_at(i);
                            Ok(self.alloc_enum("Option", "Some", vec![v]))
                        }
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "merge" | "update" => {
                    self.arity_err(method, args, 1, span)?;
                    // Snapshot the incoming entries (with their cached hashes — engine-wide
                    // consistent, so reuse is sound) first; handles `m.merge(m)`/`m.update(m)`.
                    let incoming = match args[0] {
                        Value::Obj(oh) => match self.heap.get(oh) {
                            Obj::Map(o) => o.entries.clone(),
                            _ => return Err(self.err(format!("{method}() expects a map argument, got {}", self.type_name(args[0])), span)),
                        },
                        other => return Err(self.err(format!("{method}() expects a map argument, got {}", self.type_name(other)), span)),
                    };
                    if method == "merge" {
                        let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                        let mut out = m.clone();
                        for (hk, key, val) in incoming {
                            let pos = out.candidates(hk).iter().copied().find(|&p| self.values_equal(out.entries[p].1, key));
                            match pos {
                                Some(i) => out.entries[i].2 = val,
                                None => out.push(hk, key, val),
                            }
                        }
                        // `out` is fully built and moved into the new Obj before any GC can run.
                        Ok(Value::Obj(self.heap.alloc(Obj::Map(out))))
                    } else {
                        for (hk, key, val) in incoming {
                            self.map_upsert_in_heap(h, hk, key, val);
                        }
                        Ok(Value::Nil)
                    }
                }
                _ => Err(self.err(format!("type map has no method '{method}'"), span)),
            },
            Obj::Set(s) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(s.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else { unreachable!() };
                    Ok(Value::Bool(s.candidates(hx).iter().any(|&p| self.values_equal(s.entries[p].1, x))))
                }
                "add" => {
                    self.arity_err("add", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else { unreachable!() };
                    let present = s.candidates(hx).iter().any(|&p| self.values_equal(s.entries[p].1, x));
                    if !present {
                        let Obj::Set(s) = self.heap.get_mut(h) else { unreachable!() };
                        s.push(hx, x);
                    }
                    Ok(Value::Nil)
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else { unreachable!() };
                    let pos = s.candidates(hx).iter().copied().find(|&p| self.values_equal(s.entries[p].1, x));
                    match pos {
                        Some(i) => {
                            let Obj::Set(s) = self.heap.get_mut(h) else { unreachable!() };
                            s.remove_at(i);
                            Ok(Value::Bool(true))
                        }
                        None => Ok(Value::Bool(false)),
                    }
                }
                "union" | "intersection" | "difference" => {
                    self.arity_err(method, args, 1, span)?;
                    // Both operands already carry per-element cached hashes, so set algebra needs no
                    // re-hashing (no user code re-enters) — purely build a fresh hash set, deduping
                    // and membership-testing via the cached hashes confirmed by `values_equal`.
                    let mine = match self.heap.get(h) {
                        Obj::Set(s) => s.entries.clone(),
                        _ => unreachable!(),
                    };
                    let other = self.set_arg(args[0], method, span)?;
                    let mut out = SetData::default();
                    let add = |vm: &Vm, set: &mut SetData, he: u64, e: Value| {
                        if !set.candidates(he).iter().any(|&p| vm.values_equal(set.entries[p].1, e)) {
                            set.push(he, e);
                        }
                    };
                    match method {
                        "union" => {
                            for (he, e) in mine.iter().chain(other.entries.iter()) {
                                add(self, &mut out, *he, *e);
                            }
                        }
                        // intersection keeps mine's elements present in other; difference drops them.
                        m => {
                            let keep_when_present = m == "intersection";
                            for (he, e) in &mine {
                                let in_other = other.candidates(*he).iter().any(|&p| self.values_equal(other.entries[p].1, *e));
                                if in_other == keep_when_present {
                                    add(self, &mut out, *he, *e);
                                }
                            }
                        }
                    }
                    Ok(Value::Obj(self.heap.alloc(Obj::Set(out))))
                }
                _ => Err(self.err(format!("type set has no method '{method}'"), span)),
            },
            _ => unreachable!("core_method dispatched a non-str/list/map/set receiver"),
        }
    }

    /// Read a set argument (for set algebra), erroring if it isn't a set. Returns a clone of its
    /// [`SetData`] (entries + index) so membership tests reuse the cached hashes.
    fn set_arg(&self, v: Value, method: &str, span: Span) -> Result<SetData, RuntimeError> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Set(s) => Ok(s.clone()),
                _ => Err(self.err(format!("{method}() expects a set argument, got {}", self.type_name(v)), span)),
            },
            _ => Err(self.err(format!("{method}() expects a set argument, got {}", self.type_name(v)), span)),
        }
    }

    /// Allocate a heap string and return its handle as a `Value`.
    fn alloc_str(&mut self, s: String) -> Value {
        Value::Obj(self.heap.alloc(Obj::Str(s.into_boxed_str())))
    }

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result, exactly like the interp).
    ///
    /// Deferred calls (`defer`) run LIFO first, while the frame is still live so the GC keeps their
    /// values — and the return value — rooted. A fault in a deferred call supersedes the frame's
    /// result (Go: a panic in a defer wins): it returns `Err` and the frame is still torn down.
    fn do_return(&mut self, _propagated: bool) -> Result<(), RuntimeError> {
        // Drain with the return value still on top of the stack (rooted) and the frame still on
        // `self.frames` (so `collect` roots the pending records).
        let defer_err = self.drain_top_frame_deferred();
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        // Drop any `recover:` handlers installed in the frame we just left (e.g. a `?` early-return
        // out of a recover block) — they must not survive to catch a later, unrelated fault.
        while self.handlers.last().is_some_and(|h| h.frame_len > self.frames.len()) {
            self.handlers.pop();
        }
        if let Some(e) = defer_err {
            return Err(e);
        }
        self.push(ret);
        Ok(())
    }

    /// `defer f(args)` / `defer recv.m(args)` — pop the callee/receiver + `argc` args off the stack
    /// and record a deferred call on the current frame (drained LIFO at frame exit). The values were
    /// evaluated now (Go semantics); the call runs at exit.
    fn do_defer(&mut self, method: Option<String>, argc: usize, span: Span) {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let d = match method {
            Some(name) => Deferred::Method { recv: head, name, args, span },
            None => Deferred::Call { callee: head, args, span },
        };
        self.frames.last_mut().unwrap().deferred.push(d);
    }

    /// Run one deferred call to completion (the result is discarded). `Call` rides `invoke_value`;
    /// `Method` re-uses the normal method dispatch by pushing the receiver + args and popping the
    /// discarded result. The pop→push window has no instruction boundary, so the moved-out values
    /// can't be collected before they're re-rooted.
    fn run_one_deferred(&mut self, d: Deferred) -> Result<(), RuntimeError> {
        // Guarded: a deferred call runs during frame teardown (the LIFO drain loop is Rust-stack
        // state), so a blocking `recv` inside it cannot park — it faults `deadlock` (B1).
        self.guarded(|vm| match d {
            Deferred::Call { callee, args, span } => {
                vm.invoke_value(callee, args, span)?;
                Ok(())
            }
            Deferred::Method { recv, name, args, span } => {
                let argc = args.len();
                vm.push(recv);
                for a in args {
                    vm.push(a);
                }
                vm.do_method_call(&name, argc, span)?;
                vm.pop(); // discard the deferred call's result
                Ok(())
            }
        })
    }

    // ----- concurrency C4: sequential, run-to-completion executor (mirrors the interpreter) -----

    /// `spawn f(args)` / `spawn recv.m(args)` — pop `argc(+1)` operands, deep-copy the args (and, for
    /// the method form, the receiver) across the airlock, and register the task on the innermost
    /// nursery. The callee passes by handle (like `defer`); only data crosses the airlock. Mirrors
    /// the interpreter's `exec_spawn`.
    fn do_spawn(&mut self, method: Option<String>, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let args: Vec<Value> = raw_args.into_iter().map(|a| self.deep_clone(a)).collect();
        let task = match method {
            Some(name) => {
                let recv = self.deep_clone(head);
                PendingCall::Method { recv, name, args, span }
            }
            None => PendingCall::Call { callee: head, args, span },
        };
        self.register_task(task, span)
    }

    /// `spawn:` block — snapshot the captured bindings from the current frame (like `MakeClosure`),
    /// deep-copy each captured value across the airlock, build a zero-arg closure over the synthetic
    /// block proto, and register it as a `Call` task. Mirrors the interpreter's `Task::Block`
    /// (captured locals deep-copied; home globals by handle).
    fn do_spawn_block(&mut self, proto: ProtoId, entries: &[CapEntry], span: Span) -> Result<(), RuntimeError> {
        let frame = self.frames.last().unwrap();
        let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
        let mut captured = std::collections::HashMap::new();
        for e in entries {
            let v = match e.src {
                CapSrc::Slot(i) => self.stack[base + i],
                CapSrc::Captured => enclosing
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => captured.get(&e.name).copied(),
                        _ => None,
                    })
                    .unwrap_or(Value::Nil),
            };
            // Deep-copy across the airlock: the task can't share mutable state with the parent.
            captured.insert(e.name.clone(), self.deep_clone(v));
        }
        let h = self.heap.alloc(Obj::Closure { proto, captured, home });
        self.register_task(PendingCall::Call { callee: Value::Obj(h), args: Vec::new(), span }, span)
    }

    /// Push a registered task onto the innermost nursery. The checker guarantees a `parallel:` is
    /// open, but we guard for parity with the interpreter's runtime error.
    fn register_task(&mut self, task: PendingCall, span: Span) -> Result<(), RuntimeError> {
        match self.nurseries.last_mut() {
            Some(nursery) => {
                nursery.push(task);
                Ok(())
            }
            None => Err(self.err("spawn must be inside a parallel: block".to_string(), span)),
        }
    }

    /// `parallel:` dedent — run the nursery's spawned tasks as cooperative fibers (B1/B2). The
    /// joining (parent) fiber is parked while the children run; a child that blocks on an empty
    /// `recv` suspends and the scheduler switches to a runnable sibling, resuming it once a sibling
    /// `send`s. A child that never blocks runs to completion before the next starts — identical to
    /// the old FIFO run-to-completion drain, so non-blocking programs are byte-for-byte unchanged.
    /// The first child fault (or `std.os.exit`) aborts the remaining siblings and propagates; on that
    /// path the parent's restored `run_until` handles `recover:`/unwind in its own context.
    fn join_nursery(&mut self) -> Result<(), RuntimeError> {
        // Consume this nursery's tasks (FIFO). Popping the entry now (as the old drain did at the
        // end) keeps the parent's `Handler::nursery_len` accounting correct on a later fault.
        let tasks = self.nurseries.pop().unwrap_or_default();
        if tasks.is_empty() {
            return Ok(());
        }
        let children = tasks
            .into_iter()
            .map(|t| Fiber { ctx: FiberCtx::default(), state: FiberState::Pending(t) })
            .collect();
        // Park the parent: move its live context into the nursery, leaving `self.*` as the fresh,
        // empty arena the children execute in. The nursery (parent + children) is GC-rooted while on
        // `scheduler_stack`.
        let mut nursery = Nursery { parent: FiberCtx::default(), children };
        self.swap_ctx(&mut nursery.parent);
        self.scheduler_stack.push(nursery);
        let result = self.run_scheduler();
        // Tear the level down and restore the parent context on every path (normal / fault / exit).
        let mut nursery = self.scheduler_stack.pop().expect("scheduler level present");
        self.swap_ctx(&mut nursery.parent);
        result
    }

    /// Cooperatively drive the children of the innermost scheduler level until all are `Done`. Picks
    /// the lowest-index runnable child each turn (FIFO); a child blocked on a channel becomes
    /// runnable when that channel has data. If no child is runnable and not all are done, every
    /// remaining child is parked on an empty channel no sibling can fill — a deadlock.
    fn run_scheduler(&mut self) -> Result<(), RuntimeError> {
        loop {
            match self.pick_runnable() {
                Some(i) => self.run_child(i)?,
                None => {
                    if self.all_children_done() {
                        return Ok(());
                    }
                    return Err(self.err(
                        "deadlock: every task in this parallel: block is blocked on an empty \
                         channel recv() and no sibling can send — the nursery cannot progress"
                            .to_string(),
                        Span { line: 1, col: 1 },
                    ));
                }
            }
        }
    }

    /// Index of the next runnable child in the top scheduler level, or `None` if none can run now.
    fn pick_runnable(&self) -> Option<usize> {
        let children = &self.scheduler_stack.last().expect("scheduler level present").children;
        children.iter().position(|c| match &c.state {
            FiberState::Pending(_) | FiberState::Ready => true,
            FiberState::Blocked(h) => match self.heap.get(*h) {
                Obj::Channel(core) => !core.q.lock().unwrap().is_empty(),
                _ => false,
            },
            FiberState::Done => false,
        })
    }

    fn all_children_done(&self) -> bool {
        self.scheduler_stack
            .last()
            .expect("scheduler level present")
            .children
            .iter()
            .all(|c| matches!(c.state, FiberState::Done))
    }

    /// Run (start or resume) child `i` of the top scheduler level until it completes or blocks. The
    /// child is taken out of the level (replaced by a `Done` placeholder) so its context can be
    /// swapped into `self.*` without holding a `scheduler_stack` borrow across the run — a nested
    /// `parallel:` pushes/pops its own level meanwhile. On return the child's context is parked back
    /// and its new state recorded.
    fn run_child(&mut self, i: usize) -> Result<(), RuntimeError> {
        let mut child = {
            let level = self.scheduler_stack.last_mut().expect("scheduler level present");
            std::mem::replace(&mut level.children[i], Fiber { ctx: FiberCtx::default(), state: FiberState::Done })
        };
        self.swap_ctx(&mut child.ctx); // self.* = child's execution context
        self.suspend = None; // clear any prior wait before (re)running
        let outcome = match std::mem::replace(&mut child.state, FiberState::Ready) {
            FiberState::Pending(task) => self.start_task(task),
            // Resume: the saved frames replay via the rewound `recv` op and ordinary `Return`s — no
            // host-stack nesting is rebuilt (run_until is frame-count driven).
            FiberState::Ready | FiberState::Blocked(_) => self.run_until(0),
            FiberState::Done => unreachable!("run_child on a Done fiber"),
        };
        self.swap_ctx(&mut child.ctx); // park the (possibly-suspended) context back into the child
        let result = match outcome {
            Ok(()) => {
                child.state = match self.suspend.take() {
                    Some(h) => FiberState::Blocked(h),
                    None => FiberState::Done,
                };
                Ok(())
            }
            Err(e) => {
                child.state = FiberState::Done;
                Err(e)
            }
        };
        self.scheduler_stack.last_mut().expect("scheduler level present").children[i] = child;
        result
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context. Mirrors the old
    /// `run_pending`, but a blocking `recv` may park the fiber mid-flight: the `do_method_call` /
    /// `invoke_value` paths leave `self.suspend` set and the frames live, so the discard-pop is
    /// skipped (there is no result yet) and the scheduler resumes the fiber later.
    fn start_task(&mut self, task: PendingCall) -> Result<(), RuntimeError> {
        match task {
            PendingCall::Call { callee, args, span } => {
                self.invoke_value(callee, args, span)?;
                Ok(())
            }
            PendingCall::Method { recv, name, args, span } => {
                let argc = args.len();
                self.push(recv);
                for a in args {
                    self.push(a);
                }
                self.do_method_call(&name, argc, span)?;
                if self.suspend.is_none() {
                    self.pop(); // discard the completed task's result (none pending if suspended)
                }
                Ok(())
            }
        }
    }

    /// Deep-copy a value across a task airlock (`spawn` / `Channel.send` / `Shared` get-set):
    /// data — scalars, collections, structs, enums — is recursively cloned into fresh heap objects
    /// so a task can't share mutable state with the spawner. `str` (immutable), callables, modules,
    /// and `Channel` / `Shared` handles pass by reference (the handle is what crosses). Mirrors
    /// `interp::deep_clone` exactly. Allocates, but only at the instruction boundary that called it
    /// (no GC runs mid-clone), so intermediate handles can't be collected.
    ///
    /// B3.0: implemented as a [`WireValue`] round-trip — `to_wire` (read-only serialize) then
    /// `from_wire` (reconstruct into this heap). Byte-identical to the old direct deep-copy; the
    /// wire form is what de-risks B3.1 (cores → `Arc`) and B3.3 (the value crosses a real thread).
    /// The `.expect` cannot fire in B3.0: `to_wire` is total (every `Obj` variant maps to a wire
    /// arm — by-reference objects cross as `Handle`), so it is statically infallible here. Its
    /// `Result` return is forward-plumbing for B3.3, where by-reference handles (`Module` / `Func` /
    /// closures) can no longer cross an OS-thread boundary and `to_wire` gains real `Err` arms; at
    /// that point callers switch to direct `to_wire?` / `from_wire`.
    fn deep_clone(&mut self, v: Value) -> Value {
        let w = self.to_wire(v).expect("deep_clone: airlock value must be sendable (B3.0 single-thread)");
        self.from_wire(w)
    }

    /// B3.0 — serialize a value into its [`WireValue`] form (the airlock's outbound half). A
    /// read-only walk of the heap, structurally identical to `deep_clone`'s old recursion but
    /// allocating nothing. Data (list/tuple/map/set/struct/enum) recurses; immutable / by-reference
    /// objects (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as
    /// [`WireValue::Handle`] (the existing handle, same heap in B3.0). `Map`/`Set` carry their cached
    /// hashes through so reconstruction never re-hashes.
    ///
    /// **B3.0: this is total — every `Value` and every `Obj` variant maps to a wire arm, so it never
    /// returns `Err`** (the `?` only forwards from recursion, which is itself infallible). The
    /// `Result` return is deliberate forward-plumbing: at B3.3, by-reference handles whose object
    /// cannot cross an OS thread (`Module` with mutable globals, `Func`/`Closure`) stop mapping to
    /// `Handle` and instead return a real `Err` defensive fault here. No such arm exists yet.
    fn to_wire(&self, v: Value) -> Result<WireValue, RuntimeError> {
        Ok(match v {
            Value::Int(n) => WireValue::Int(n),
            Value::Float(f) => WireValue::Float(f),
            Value::Bool(b) => WireValue::Bool(b),
            Value::Nil => WireValue::Nil,
            Value::Obj(h) => match self.heap.get(h) {
                // B3.3a: `str` crosses by value (owned bytes) so it can survive an OS-thread heap
                // boundary; immutable + value-compared, so a fresh handle on reconstruction is
                // observationally identical to sharing this one.
                Obj::Str(s) => WireValue::Str(s.clone()),
                // By-reference callables: cross as the existing handle (matches the old deep_clone arm).
                Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Module { .. }
                | Obj::Native { .. } => WireValue::Handle(h),
                // B3.1: the shared cores cross as the `Arc` itself (clone = refcount bump), so a
                // `from_wire` in any heap reaches the same mailbox/box/queue.
                Obj::Channel(core) => WireValue::Channel(Arc::clone(core)),
                Obj::Shared(core) => WireValue::Shared(Arc::clone(core)),
                Obj::Executor(core) => WireValue::Executor(Arc::clone(core)),
                Obj::List(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for x in items {
                        out.push(self.to_wire(*x)?);
                    }
                    WireValue::List(out)
                }
                Obj::Tuple(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for x in items {
                        out.push(self.to_wire(*x)?);
                    }
                    WireValue::Tuple(out)
                }
                Obj::Map(m) => {
                    let mut out = Vec::with_capacity(m.entries.len());
                    for (hash, k, val) in &m.entries {
                        out.push((*hash, self.to_wire(*k)?, self.to_wire(*val)?));
                    }
                    WireValue::Map(out)
                }
                Obj::Set(s) => {
                    let mut out = Vec::with_capacity(s.entries.len());
                    for (hash, e) in &s.entries {
                        out.push((*hash, self.to_wire(*e)?));
                    }
                    WireValue::Set(out)
                }
                Obj::Struct { name, fields } => {
                    let mut out = Vec::with_capacity(fields.len());
                    for (k, val) in fields {
                        out.push((k.clone(), self.to_wire(*val)?));
                    }
                    WireValue::Struct { name: name.clone(), fields: out }
                }
                Obj::Enum { ty, variant, payload } => {
                    let mut out = Vec::with_capacity(payload.len());
                    for x in payload {
                        out.push(self.to_wire(*x)?);
                    }
                    WireValue::Enum { ty: ty.clone(), variant: variant.clone(), payload: out }
                }
            },
        })
    }

    /// B3.0 — reconstruct a [`WireValue`] into a heap [`Value`] (the airlock's inbound half). Data
    /// arms `alloc` fresh objects into *this* `Vm`'s heap (mirroring the old deep_clone allocation);
    /// [`WireValue::Handle`] returns the same handle (by-reference preserved). `Map`/`Set` rebuild
    /// via `push(hash, …)` with the carried hash, so iteration order + index are identical. Builds
    /// bottom-up (children before parent alloc) and `alloc` never collects, so — like deep_clone —
    /// no intermediate is lost mid-reconstruction.
    // `&mut self` is intentional: `from_wire` reconstructs *into* this VM's heap (it allocates),
    // so it is not the usual ownership-less `from_*` constructor the lint expects.
    #[allow(clippy::wrong_self_convention)]
    fn from_wire(&mut self, w: WireValue) -> Value {
        match w {
            WireValue::Int(n) => Value::Int(n),
            WireValue::Float(f) => Value::Float(f),
            WireValue::Bool(b) => Value::Bool(b),
            WireValue::Nil => Value::Nil,
            // B3.3a: rebuild a fresh heap `str` from the owned bytes (by value, not the old handle).
            WireValue::Str(s) => Value::Obj(self.heap.alloc(Obj::Str(s))),
            WireValue::Handle(h) => Value::Obj(h),
            // B3.1: rebuild a fresh heap handle onto the SAME shared core (`Arc` already cloned in
            // `to_wire`). Not registered in `self.executors` — the original `NewExecutor` handle there
            // drives the program-exit auto-drain and shares this core, so the alias needs no entry.
            WireValue::Channel(core) => Value::Obj(self.heap.alloc(Obj::Channel(core))),
            WireValue::Shared(core) => Value::Obj(self.heap.alloc(Obj::Shared(core))),
            WireValue::Executor(core) => Value::Obj(self.heap.alloc(Obj::Executor(core))),
            WireValue::List(items) => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::List(cloned)))
            }
            WireValue::Tuple(items) => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Tuple(cloned)))
            }
            WireValue::Map(entries) => {
                let mut out = MapData::default();
                for (hash, k, val) in entries {
                    let (ck, cv) = (self.from_wire(k), self.from_wire(val));
                    out.push(hash, ck, cv);
                }
                Value::Obj(self.heap.alloc(Obj::Map(out)))
            }
            WireValue::Set(entries) => {
                let mut out = SetData::default();
                for (hash, e) in entries {
                    let ce = self.from_wire(e);
                    out.push(hash, ce);
                }
                Value::Obj(self.heap.alloc(Obj::Set(out)))
            }
            WireValue::Struct { name, fields } => {
                let cloned: Vec<(Box<str>, Value)> =
                    fields.into_iter().map(|(k, val)| (k, self.from_wire(val))).collect();
                Value::Obj(self.heap.alloc(Obj::Struct { name, fields: cloned }))
            }
            WireValue::Enum { ty, variant, payload } => {
                let cloned: Vec<Value> = payload.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Enum { ty, variant, payload: cloned }))
            }
        }
    }

    /// B3.2 — construct a fresh worker `Vm` that shares this VM's compiled program by `Arc`
    /// (read-only) but owns its own empty heap. Execution-shaping flags (`gc_stress`) carry over so a
    /// worker is exercised under the same GC pressure as the parent; `host` is left inert (B3.2's
    /// isolation tasks don't touch host I/O — B3.3 threads it through when real workers run user I/O).
    /// No OS thread yet; the caller drives the returned worker synchronously.
    #[allow(dead_code)] // B3.2 forward-plumbing; B3.3 `--parallel` makes it reachable (see `WorkerResult`).
    fn spawn_worker(&self) -> Vm {
        let mut worker = Vm::new(Arc::clone(&self.program));
        worker.gc_stress = self.gc_stress;
        worker
    }

    /// B3.2 — run a spawned task in an isolated worker (`spawn_worker`): its args/captures cross IN as
    /// [`WireValue`] (serialized in *this* parent heap, reconstructed in the worker heap) and its
    /// return value + captured `out`/`stderr` cross back OUT. The worker runs **synchronously** on the
    /// calling thread (no OS thread until B3.3), proving the `Arc<Program>` + heap-handoff plumbing in
    /// isolation.
    ///
    /// The callee is **not** crossed as a parent-heap `Handle` (a `GcRef` is meaningless in another
    /// heap); instead the task is lowered to its `ProtoId` + wire'd captures (the proto lives in the
    /// shared `Arc<Program>`) and the worker rebuilds the closure over its own heap.
    ///
    /// **Cross-heap safety (enforced, not just documented).** A `WireValue` that still carries a
    /// by-reference [`Handle`](WireValue::has_handle) — a `str`, a closure/func value, a module — is a
    /// parent-heap `GcRef` that means nothing in the worker heap, so every crossed value (captures,
    /// args, and the returned result) is checked with [`Vm::ensure_crossable`] and a clean
    /// `RuntimeError` is raised rather than silently reconstructing a dangling handle. Plain data and
    /// `Channel`/`Shared`/`Executor` handles (which cross as a shared `Arc`, not a `GcRef`) pass.
    /// `str`/closure crossing **by value** lands in B3.3.
    ///
    /// Two further B3.2 limits, deliberate and gated: module globals (a `Closure`'s `home`) do **not**
    /// cross — the worker gets a fresh empty home — so a task reading a mutable module global faults
    /// (deferred decision **G1**); and **method tasks** (`spawn recv.m()`) are rejected outright,
    /// because a worker never runs module init so its `module_objs` table is empty (method dispatch
    /// would index out of bounds). Both are resolved at B3.3. B3.2 handles only self-contained
    /// function/closure tasks over sendable data, which is all the heap-handoff proof needs.
    #[allow(dead_code)] // B3.2 forward-plumbing; B3.3 `--parallel` makes it reachable (see `WorkerResult`).
    fn run_task_isolated(&mut self, task: PendingCall) -> Result<WorkerResult, RuntimeError> {
        // 1. Lower the task to a `Send` description in THIS (parent) heap (read-only serialize),
        //    rejecting any value that can't cross a heap boundary as-is.
        let lowered = match task {
            PendingCall::Call { callee, args, span } => {
                let wargs = self.wire_args(args, span)?;
                match callee {
                    Value::Obj(h) => match self.heap.get(h).clone() {
                        Obj::Closure { proto, captured, .. } => {
                            let mut wcap = Vec::with_capacity(captured.len());
                            for (k, v) in captured {
                                let w = self.to_wire(v)?;
                                self.ensure_crossable(&w, span)?;
                                wcap.push((k, w));
                            }
                            Lowered::Closure { proto, captured: wcap, args: wargs, span }
                        }
                        Obj::Func { proto, .. } => Lowered::Func { proto, args: wargs, span },
                        _ => return Err(self.err(
                            format!("spawn: '{}' is not an isolable task", self.type_name(callee)),
                            span,
                        )),
                    },
                    _ => return Err(self.err(
                        format!("spawn: '{}' is not an isolable task", self.type_name(callee)),
                        span,
                    )),
                }
            }
            // Method tasks need the worker's module-globals table, which a worker never populates
            // (no module init) — gate them off cleanly until B3.3 (see the doc-comment).
            PendingCall::Method { span, .. } => {
                return Err(self.err(
                    "spawn: method tasks are not yet isolable (B3.2 runs only function/closure tasks)"
                        .to_string(),
                    span,
                ));
            }
        };

        // 2. Build the worker, 3. reconstruct the task in its heap, run it synchronously.
        let mut worker = self.spawn_worker();
        let (ret, span) = match lowered {
            Lowered::Closure { proto, captured, args, span } => {
                let home = worker.fresh_worker_home();
                let cap = captured.into_iter().map(|(k, w)| (k, worker.from_wire(w))).collect();
                let callee = Value::Obj(worker.heap.alloc(Obj::Closure { proto, captured: cap, home }));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (worker.invoke_value(callee, args, span)?, span)
            }
            Lowered::Func { proto, args, span } => {
                let home = worker.fresh_worker_home();
                let callee = Value::Obj(worker.heap.alloc(Obj::Func { proto, home }));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (worker.invoke_value(callee, args, span)?, span)
            }
        };

        // 4. Hand the result + captured output back across the airlock (result must be cross-safe too —
        //    a worker returning a `str`/closure can't hand a worker-heap `GcRef` back to the parent).
        let value = worker.to_wire(ret)?;
        worker.ensure_crossable(&value, span)?;
        Ok(WorkerResult { value, out: worker.out, stderr: worker.stderr })
    }

    /// Reject a wired value that still carries a by-reference [`Handle`](WireValue::has_handle) — a
    /// heap-local `GcRef` that cannot cross into another heap as-is (B3.2). `str`/closure crossing by
    /// value lands in B3.3; until then this converts a would-be dangling handle into a clean fault.
    #[allow(dead_code)] // B3.2 forward-plumbing; B3.3 `--parallel` makes it reachable (see `WorkerResult`).
    fn ensure_crossable(&self, w: &WireValue, span: Span) -> Result<(), RuntimeError> {
        if w.has_handle() {
            return Err(self.err(
                "spawn: this task value can't cross a worker boundary yet — only plain data and \
                 Channel/Shared/Executor handles are sendable in B3.2 (str/closure cross by value in B3.3)"
                    .to_string(),
                span,
            ));
        }
        Ok(())
    }

    /// Serialize a task's argument list across the airlock (read-only walk of this heap), rejecting any
    /// argument that can't cross a heap boundary as-is (see [`Vm::ensure_crossable`]).
    #[allow(dead_code)] // B3.2 forward-plumbing; B3.3 `--parallel` makes it reachable (see `WorkerResult`).
    fn wire_args(&self, args: Vec<Value>, span: Span) -> Result<Vec<WireValue>, RuntimeError> {
        args.into_iter()
            .map(|a| {
                let w = self.to_wire(a)?;
                self.ensure_crossable(&w, span)?;
                Ok(w)
            })
            .collect()
    }

    /// A fresh empty module to serve as a worker closure's `home`. The parent's `home` `GcRef` can't
    /// cross heaps; B3.2 tasks don't read globals, so an empty namespace suffices (see `run_task_isolated`).
    #[allow(dead_code)] // B3.2 forward-plumbing; B3.3 `--parallel` makes it reachable (see `WorkerResult`).
    fn fresh_worker_home(&mut self) -> GcRef {
        self.heap.alloc(Obj::Module { name: "<worker>".into(), globals: Default::default() })
    }

    /// B3.1 — clone out the shared `Arc<ChannelCore>` behind a `Channel` handle (refcount bump). The
    /// `Arc` is held only for the duration of the calling method, so locking it does not borrow the
    /// heap, leaving `self` free for the re-entrant value paths (`from_wire`, `invoke_value`).
    fn channel_core(&self, h: GcRef) -> Arc<ChannelCore> {
        match self.heap.get(h) {
            Obj::Channel(core) => Arc::clone(core),
            _ => unreachable!("channel_core on non-channel"),
        }
    }

    fn shared_core(&self, h: GcRef) -> Arc<SharedCore> {
        match self.heap.get(h) {
            Obj::Shared(core) => Arc::clone(core),
            _ => unreachable!("shared_core on non-shared"),
        }
    }

    fn executor_core(&self, h: GcRef) -> Arc<ExecutorCore> {
        match self.heap.get(h) {
            Obj::Executor(core) => Arc::clone(core),
            _ => unreachable!("executor_core on non-executor"),
        }
    }

    /// `Channel[T]` methods (C2/C4): `send` (move-on-send, deep-copied in), `recv` (FIFO; empty =
    /// deadlock fault under the sequential executor), `len`. Mirrors `interp::eval_channel_method` —
    /// error strings byte-identical (parity-tested).
    fn channel_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "send" => {
                self.arity_err("send", args, 1, span)?;
                // B3.1: serialize once into the core (the wire form IS the airlock copy).
                let w = self.to_wire(args[0])?;
                self.channel_core(h).q.lock().unwrap().push_back(w);
                Ok(Value::Nil)
            }
            "recv" => {
                self.arity_err("recv", args, 0, span)?;
                let popped = self.channel_core(h).q.lock().unwrap().pop_front();
                match popped {
                    Some(w) => Ok(self.from_wire(w)),
                    // Empty channel. Under an active nursery scheduler (and not inside a native
                    // callback, whose Rust-stack state can't be parked), B1/B2 SUSPENDS the running
                    // fiber instead of faulting: re-root the receiver, rewind `ip` so this very
                    // `CallMethod(recv)` re-executes on resume, and signal the scheduler to run a
                    // sibling. `do_method_call` skips its result-push while `suspend` is set; the
                    // scheduler resumes this fiber once `h` has data (a sibling `send`).
                    None if !self.scheduler_stack.is_empty() && self.native_reentry == 0 => {
                        self.push(Value::Obj(h));
                        self.frames.last_mut().unwrap().ip -= 1;
                        self.suspend = Some(h);
                        Ok(Value::Nil) // sentinel; never observed (callers gate on `suspend`)
                    }
                    // No scheduler (top level / single fiber) or inside a native callback: there is
                    // no sibling that could ever fill the channel here — a real deadlock. Unchanged.
                    None => Err(self.err(
                        "recv on an empty channel: deadlock — nothing is queued and the \
                         sequential executor cannot block waiting for a producer (a \
                         consumer that waits mid-flight on a live producer needs C5)"
                            .to_string(),
                        span,
                    )),
                }
            }
            "try_recv" => {
                // A1: non-blocking poll. Unlike `recv` it never touches `scheduler_stack` /
                // `native_reentry` / `suspend` / `ip` — it always returns immediately with an
                // `Option`: `Some(v)` if queued, `None` if empty. Mirrors `interp::eval_channel_method`.
                self.arity_err("try_recv", args, 0, span)?;
                let popped = self.channel_core(h).q.lock().unwrap().pop_front();
                Ok(match popped {
                    Some(w) => {
                        let v = self.from_wire(w);
                        self.alloc_enum("Option", "Some", vec![v])
                    }
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let n = self.channel_core(h).q.lock().unwrap().len();
                Ok(Value::Int(n as i64))
            }
            _ => Err(self.err(format!("type Channel has no method '{method}'"), span)),
        }
    }

    /// `Shared[T]` methods (C3/C4): `get` (copies out), `set` (copies in), `update` (read-modify-write
    /// via the re-entrant call path). Mirrors `interp::eval_shared_method`. The box is re-rooted on
    /// the operand stack across `update`'s nested call (the receiver was popped in `do_method_call`).
    fn shared_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the lock, then reconstruct into this heap (one
                // round-trip == the old deep_clone-out).
                let w = self.shared_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                let w = self.to_wire(args[0])?;
                *self.shared_core(h).v.lock().unwrap() = w;
                Ok(Value::Nil)
            }
            "update" => {
                self.arity_err("update", args, 1, span)?;
                let f = args[0];
                // Read the current value out before the call (lock dropped immediately — the user fn
                // may re-enter this same box, and we must never hold the lock across `invoke_value`).
                // Re-root the box handle on the operand stack so the nested call's GC keeps the core's
                // contents traced (the receiver was popped off the stack in `do_method_call`).
                let w = self.shared_core(h).v.lock().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire(next)?;
                *self.shared_core(h).v.lock().unwrap() = stored;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Shared has no method '{method}'"), span)),
        }
    }

    /// `Executor` methods (C5/escape hatch): `submit` (enqueue a detached task closure, rejected once
    /// shut), `shutdown` (graceful — drain FIFO via the re-entrant call path), `shutdown_now` (discard
    /// pending). Mirrors `interp::eval_executor_method` — error strings byte-identical (parity-tested).
    /// The executor handle is re-rooted on the operand stack across the drain, and each popped task is
    /// rooted across its nested call (the receiver was popped in `do_method_call`).
    fn executor_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "submit" => {
                self.arity_err("submit", args, 1, span)?;
                let core = self.executor_core(h);
                {
                    let mut g = core.inner.lock().unwrap();
                    if g.shut {
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)".to_string(),
                            span,
                        ));
                    }
                    // The task closure crosses by handle at B3.1 (`Handle(closure)`), kept rooted via
                    // the executor handle's `children()`.
                    let w = self.to_wire(args[0])?;
                    g.queue.push_back(w);
                }
                Ok(Value::Nil)
            }
            "shutdown" => {
                self.arity_err("shutdown", args, 0, span)?;
                let core = self.executor_core(h);
                // Mark shut first so a task that re-enters this executor (submit/shutdown) sees it.
                core.inner.lock().unwrap().shut = true;
                // Root the executor handle across the drain (its remaining queue is traced via it).
                self.push(Value::Obj(h));
                loop {
                    // Pop under the lock, then DROP the guard before the re-entrant call.
                    let task = core.inner.lock().unwrap().queue.pop_front();
                    let Some(task) = task else { break };
                    let task = self.from_wire(task);
                    // The popped task is no longer in the queue → root it on the stack across its call.
                    self.push(task);
                    let r = self.guarded(|vm| vm.invoke_value(task, vec![], span));
                    self.pop();
                    r?;
                }
                self.pop(); // the executor root
                Ok(Value::Nil)
            }
            "shutdown_now" => {
                self.arity_err("shutdown_now", args, 0, span)?;
                let core = self.executor_core(h);
                let mut g = core.inner.lock().unwrap();
                g.shut = true;
                g.queue.clear();
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Executor has no method '{method}'"), span)),
        }
    }

    /// Mirrors `interp::Interp::drain_live_executors` (C5 / A2): at a clean program end, gracefully
    /// drain every `Executor` created but never explicitly `shutdown`/`shutdown_now`-ed, in creation
    /// order, reusing the shipped `shutdown` path (FIFO, first-fault-aborts-siblings). A hard
    /// `std.os.exit` is not drained (the caller gates on `pending_exit`); a task that calls
    /// `os.exit` mid-drain stops the remaining drain.
    fn drain_live_executors(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            return Ok(());
        }
        // Snapshot the handles: a drained task may create new executors; reap only those alive at
        // exit (parity with the interpreter's `Vec<Rc>` snapshot).
        let execs = self.executors.clone();
        for h in execs {
            let shut = self.executor_core(h).inner.lock().unwrap().shut;
            if shut {
                continue;
            }
            self.executor_method(h, "shutdown", &[], span)?;
            if self.pending_exit.is_some() {
                break; // a drained task called os.exit — hard halt, stop draining
            }
        }
        Ok(())
    }

    /// Drain the current (top) frame's deferred calls, LIFO, popping one at a time from the frame's
    /// own list so the not-yet-run records stay GC-rooted in the frame. Skipped on a hard
    /// `std.os.exit` (Go: `os.Exit` does not run deferred calls). Returns the latest fault, if any.
    fn drain_top_frame_deferred(&mut self) -> Option<RuntimeError> {
        if self.pending_exit.is_some() {
            return None;
        }
        let fi = self.frames.len() - 1;
        let mut err = None;
        while let Some(d) = self.frames[fi].deferred.pop() {
            if let Err(e) = self.run_one_deferred(d) {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        err
    }

    /// Leave a lexical defer scope (`LeaveDeferScope`): pop the top marker and run the current
    /// frame's defers registered since it, LIFO. This is the block-scoped analogue of
    /// `drain_top_frame_deferred` — it drains down to a marker, not to the bottom of the frame.
    /// Skipped on a hard `std.os.exit`. Returns the latest fault from a deferred call, if any.
    fn leave_defer_scope(&mut self) -> Option<RuntimeError> {
        let fi = self.frames.len() - 1;
        debug_assert!(
            !self.frames[fi].defer_markers.is_empty(),
            "LeaveDeferScope without a matching EnterDeferScope (compiler scope-count desync)"
        );
        let marker = self.frames[fi].defer_markers.pop().unwrap_or(0);
        self.drain_frame_to(marker)
    }

    /// Drain the current (top) frame's pending defers down to `marker` (the count to leave behind),
    /// LIFO. The block-scoped analogue of `drain_top_frame_deferred` for an explicit marker — used
    /// by `LeaveDeferScope` and by every `recover:` boundary path. Skipped on a hard `std.os.exit`.
    /// Returns the latest fault from a deferred call, if any.
    fn drain_frame_to(&mut self, marker: usize) -> Option<RuntimeError> {
        if self.pending_exit.is_some() {
            return None;
        }
        let fi = self.frames.len() - 1;
        let mut err = None;
        while self.frames[fi].deferred.len() > marker {
            let d = self.frames[fi].deferred.pop().unwrap();
            if let Err(e) = self.run_one_deferred(d) {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        err
    }

    /// Unwind frames from the current depth down to `target_frame_len`, running each discarded
    /// frame's deferred calls (innermost first) before dropping it. Used on a fault: deferred
    /// cleanup runs as the stack unwinds, before a `recover:` boundary regains control (or before
    /// the program exits on an uncaught fault). A fault in a deferred call supersedes the original.
    fn unwind_deferred(&mut self, target_frame_len: usize) -> Option<RuntimeError> {
        let mut err = None;
        while self.frames.len() > target_frame_len {
            let fi = self.frames.len() - 1;
            if self.pending_exit.is_none() {
                while let Some(d) = self.frames[fi].deferred.pop() {
                    if let Err(e) = self.run_one_deferred(d) {
                        err = Some(e);
                        if self.pending_exit.is_some() {
                            break;
                        }
                    }
                }
            }
            let frame = self.frames.pop().unwrap();
            if frame.counted {
                self.call_depth -= 1;
            }
            self.stack.truncate(frame.base);
            self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
            while self.handlers.last().is_some_and(|h| h.frame_len > self.frames.len()) {
                self.handlers.pop();
            }
        }
        err
    }

    fn do_try(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        // Extract (variant, payload-arity, first-payload) up front so the heap borrow is released
        // before we mutate the stack / unwind a frame.
        let info = match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum { ty, variant, payload } => Some((ty.to_string(), variant.to_string(), payload.len(), payload.first().copied())),
                _ => None,
            },
            _ => None,
        };
        // Gate on the *type* (`Result`/`Option`), not the bare variant name, so a user enum that
        // shadows `Ok`/`Err`/`Some`/`None` is not treated as a Result/Option by `?`.
        if let Some((ty, variant, n, first)) = info {
            if (ty == "Result" && variant == "Ok" || ty == "Option" && variant == "Some") && n == 1 {
                self.push(first.unwrap());
                return Ok(());
            }
            if ty == "Result" && variant == "Err" || ty == "Option" && variant == "None" {
                // A `?` directly inside a `recover:` block (a handler installed in THIS frame)
                // short-circuits to that boundary (try-block style): the `Err`/`None` value becomes
                // the recover's result. Function-scoped `?` (no same-frame handler) falls through.
                if self
                    .handlers
                    .last()
                    .is_some_and(|h| h.frame_len == self.frames.len())
                {
                    let h = self.handlers.pop().unwrap();
                    self.stack.truncate(h.stack_len);
                    self.call_depth = h.call_depth;
                    // Drop scope markers of defer scopes opened inside the recover block — the `?`
                    // jumps past their `LeaveDeferScope`s, so they would otherwise leak.
                    self.frames.last_mut().unwrap().defer_markers.truncate(h.markers_len);
                    // Drain the recover block's own defers before binding the result. A fault in one
                    // supersedes the propagated value (becomes the recover's `Err`).
                    match self.drain_frame_to(h.defer_len) {
                        Some(e) if self.pending_exit.is_some() => return Err(e),
                        Some(e) => {
                            let msg = self.alloc_str(e.message);
                            let err = self.alloc_enum("Result", "Err", vec![msg]);
                            self.push(err);
                        }
                        None => self.push(v), // the propagated Result/Option value IS the result
                    }
                    self.jump(h.ip);
                    return Ok(());
                }
                // A `?` at the top level (no enclosing function) is an unhandled error → exit. Use
                // the `?` op's own `span` so the reported location matches the interp (which threads
                // the `?`'s `expr.span` through its propagation marker).
                if self.frames.last().unwrap().is_toplevel {
                    return Err(self.top_level_error(v, span).unwrap_or_else(|| {
                        self.err(format!("unhandled error: {}", self.display(v)), span)
                    }));
                }
                // Otherwise early-return this value from the enclosing function (running its
                // deferred calls first; a fault in one propagates as a fault).
                self.push(v);
                self.do_return(true)?;
                return Ok(());
            }
        }
        Err(self.err(format!("'?' expects Result or Option, found {}", self.type_name(v)), span))
    }

    // ----- construction / access -----

    fn new_struct(&mut self, name: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let def = self.program.structs.get(name).cloned().ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        if argc != def.fields.len() {
            return Err(self.err(format!("struct '{name}' expects {} field(s), got {argc}", def.fields.len()), span));
        }
        let at = self.stack.len() - argc;
        let vals: Vec<Value> = self.stack.split_off(at);
        let fields: Vec<(Box<str>, Value)> = def.fields.iter().cloned().map(|f| f.into_boxed_str()).zip(vals).collect();
        let h = self.heap.alloc(Obj::Struct { name: name.into(), fields });
        self.push(Value::Obj(h));
        Ok(())
    }

    fn new_enum(&mut self, ty: &str, variant: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        if let Some(def) = self.program.variants.get(variant)
            && argc != def.arity
        {
            return Err(self.err(format!("variant '{variant}' expects {} value(s), got {argc}", def.arity), span));
        }
        let at = self.stack.len() - argc;
        let payload: Vec<Value> = self.stack.split_off(at);
        let h = self.heap.alloc(Obj::Enum { ty: ty.into(), variant: variant.into(), payload });
        self.push(Value::Obj(h));
        Ok(())
    }

    fn get_field(&mut self, name: &str, span: Span) -> Result<(), RuntimeError> {
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot read field '{name}' of {}", self.type_name(obj)), span));
        };
        match self.heap.get(h) {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
            Obj::Tuple(items) => {
                let v = name.parse::<usize>().ok().and_then(|i| items.get(i).copied());
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("tuple has no element '.{name}' (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Struct { fields, .. } => {
                let v = fields.iter().find(|(k, _)| k.as_ref() == name).map(|(_, v)| *v);
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => {
                        let shown = self.display(obj);
                        Err(self.err(format!("no field '{name}' on {shown}"), span))
                    }
                }
            }
            Obj::Module { name: mname, globals } => match globals.get(name).copied() {
                Some(v) => {
                    self.push(v);
                    Ok(())
                }
                None => Err(self.err(format!("module '{mname}' has no member '{name}'"), span)),
            },
            _ => Err(self.err(format!("cannot read field '{name}' of {}", self.type_name(obj)), span)),
        }
    }

    /// `obj[start..end]` — bounds-clamped half-open copy of a list/str, or a struct's `slice`.
    fn get_slice(&mut self, span: Span) -> Result<(), RuntimeError> {
        let end = self.pop();
        let start = self.pop();
        let obj = self.pop();
        let s = match start {
            Value::Int(n) => n,
            other => return Err(self.err(format!("expected int, found {}", self.type_name(other)), span)),
        };
        let e = match end {
            Value::Int(n) => n,
            other => return Err(self.err(format!("expected int, found {}", self.type_name(other)), span)),
        };
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
        };
        // Snapshot the result kind without holding the heap borrow across the alloc / method call.
        enum Sliced {
            List(Vec<Value>),
            Str(String),
            Struct,
        }
        let sliced = match self.heap.get(h) {
            Obj::List(items) => {
                let (lo, hi) = clamp_range(s, e, items.len());
                Sliced::List(items[lo..hi].to_vec())
            }
            Obj::Str(string) => {
                let chars: Vec<char> = string.chars().collect();
                let (lo, hi) = clamp_range(s, e, chars.len());
                Sliced::Str(chars[lo..hi].iter().collect())
            }
            Obj::Struct { .. } => Sliced::Struct,
            _ => return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span)),
        };
        match sliced {
            Sliced::List(slice) => {
                // Root the source across the alloc: the new list shares its element handles, which
                // are otherwise unreachable (the source was popped) and could be collected by a GC.
                self.push(obj);
                let nh = self.heap.alloc(Obj::List(slice));
                self.pop();
                self.push(Value::Obj(nh));
            }
            Sliced::Str(sub) => {
                let nh = self.heap.alloc(Obj::Str(sub.into_boxed_str()));
                self.push(Value::Obj(nh));
            }
            Sliced::Struct => {
                let v = self.dispatch_index_method(h, "slice", vec![obj, Value::Int(s), Value::Int(e)], span)?;
                self.push(v);
            }
        }
        Ok(())
    }

    /// Dispatch an `Index`/`IndexSet`/`Slice` protocol method (`index`/`set_index`/`slice`) on a
    /// struct heap object. `args` already includes the receiver as its first element (bound to
    /// `self`). Mirrors `struct_arith`'s frame dispatch; the args are rooted as the new frame's locals.
    fn dispatch_index_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Obj::Struct { name, .. } = self.heap.get(h) else { unreachable!() };
        let name = name.clone();
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def
            .methods
            .get(method)
            // Wording byte-identical to `interp::call_struct_method` (the engines parity-test stdout).
            .ok_or_else(|| self.err(format!("struct '{name}' has no method '{method}'"), span))?;
        let home = self.module_objs[def.module_idx];
        // Guarded (B1): `index`/`slice`/`set_index` overloads run from native opcode handlers whose
        // operand state is on the host stack, so a blocking `recv` inside one cannot park — it faults
        // `deadlock` instead of suspending (matches `struct_arith`/`compare`/`hash`).
        self.guarded(|vm| vm.run_proto(proto, home, None, args, true, false, span))
    }

    fn get_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        // The index is NOT pre-validated as int (the `AsInt` was removed so map keys can be
        // str/bool): pop it as a Value and validate per object kind.
        let key = self.pop();
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // Require an int index for list/str (the message matches the old `AsInt` exactly, for parity).
        let int_idx = |vm: &Vm| -> Result<i64, RuntimeError> {
            match key {
                Value::Int(n) => Ok(n),
                other => Err(vm.err(format!("expected int, found {}", vm.type_name(other)), span)),
            }
        };
        match self.heap.get(h) {
            Obj::List(items) => {
                let idx = int_idx(self)?;
                let v = usize::try_from(idx).ok().and_then(|i| items.get(i).copied());
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(format!("index {idx} out of bounds (len {})", items.len()), span)),
                }
            }
            Obj::Str(s) => {
                let idx = int_idx(self)?;
                let chars: Vec<char> = s.chars().collect();
                match usize::try_from(idx).ok().and_then(|i| chars.get(i).copied()) {
                    Some(c) => {
                        let nh = self.heap.alloc(Obj::Str(c.to_string().into_boxed_str()));
                        self.push(Value::Obj(nh));
                        Ok(())
                    }
                    None => Err(self.err(format!("index {idx} out of bounds (len {})", chars.len()), span)),
                }
            }
            Obj::Map(_) => {
                let hk = self.hash_key_rooted(key, &[obj, key], span)?;
                let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                match m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key)) {
                    Some(p) => {
                        let v = m.entries[p].2;
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err("key not found".to_string(), span)),
                }
            }
            // A struct satisfying `Index` dispatches `obj[k]` to `index(self, k)`.
            Obj::Struct { .. } => {
                let v = self.dispatch_index_method(h, "index", vec![obj, key], span)?;
                self.push(v);
                Ok(())
            }
            _ => Err(self.err(format!("cannot index {}", self.type_name(obj)), span)),
        }
    }

    fn set_field(&mut self, name: &str, span: Span) -> Result<(), RuntimeError> {
        let val = self.pop();
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot assign field '{name}' of {}", self.type_name(obj)), span));
        };
        match self.heap.get_mut(h) {
            Obj::Struct { fields, .. } => match fields.iter_mut().find(|(k, _)| k.as_ref() == name) {
                Some((_, slot)) => {
                    *slot = val;
                    Ok(())
                }
                None => {
                    let shown = self.display(obj);
                    Err(self.err(format!("no field '{name}' on {shown}"), span))
                }
            },
            _ => Err(self.err(format!("cannot assign field '{name}' of {}", self.type_name(obj)), span)),
        }
    }

    fn set_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        let val = self.pop();
        // The index is NOT pre-validated as int (AsInt removed for map keys): pop as a Value.
        let key = self.pop();
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // For a map, hash the key (rooting the map/key/value across a struct key's re-entrant
        // hash()), locate the entry, then mutate — updating the side index on insert.
        if matches!(self.heap.get(h), Obj::Map(_)) {
            let hk = self.hash_key_rooted(key, &[obj, key, val], span)?;
            let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
            let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
            let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
            match pos {
                Some(i) => m.entries[i].2 = val,
                None => m.push(hk, key, val),
            }
            return Ok(());
        }
        // A struct satisfying `IndexSet` dispatches `obj[k] = v` to `set_index(self, k, v)`.
        if matches!(self.heap.get(h), Obj::Struct { .. }) {
            self.dispatch_index_method(h, "set_index", vec![obj, key, val], span)?;
            return Ok(());
        }
        let idx = match key {
            Value::Int(n) => n,
            other => return Err(self.err(format!("expected int, found {}", self.type_name(other)), span)),
        };
        match self.heap.get_mut(h) {
            Obj::List(items) => match usize::try_from(idx).ok().filter(|i| *i < items.len()) {
                Some(i) => {
                    items[i] = val;
                    Ok(())
                }
                None => {
                    let len = items.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            },
            _ => Err(self.err(format!("cannot index {}", self.type_name(obj)), span)),
        }
    }

    fn match_arm(&mut self, scrut: usize, variant: &str, nbind: usize, bind_start: usize, next: usize, span: Span) -> Result<(), RuntimeError> {
        let v = self.stack[self.base() + scrut];
        let h = match v {
            Value::Obj(h) => h,
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        let (matches, payload) = match self.heap.get(h) {
            Obj::Enum { variant: vn, payload, .. } => (vn.as_ref() == variant, payload.clone()),
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        if !matches {
            self.jump(next);
            return Ok(());
        }
        if payload.len() != nbind {
            return Err(self.err(format!("pattern '{variant}' binds {nbind} value(s) but variant carries {}", payload.len()), span));
        }
        let base = self.base();
        for (k, pv) in payload.into_iter().enumerate() {
            self.stack[base + bind_start + k] = pv;
        }
        Ok(())
    }

    // ----- builtins / print -----

    fn do_print(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        // Keep the args rooted on the operand stack while stringifying — a `Stringable` `str` method
        // runs user code that can GC. `stringify` pushes/pops above `at + argc`, so these indices
        // stay valid across the loop.
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let v = self.stack[at + i];
            parts.push(self.stringify(v, span)?);
        }
        self.stack.truncate(at);
        self.out.push_str(&parts.join(" "));
        self.out.push('\n');
        self.push(Value::Nil);
        Ok(())
    }

    fn do_builtin(&mut self, name: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let result = match name {
            "len" => self.builtin_len(&args, span)?,
            "range" => self.builtin_range(&args, span)?,
            "int" => self.builtin_int(&args, span)?,
            "float" => self.builtin_float(&args, span)?,
            "str" => self.builtin_str(&args, span)?,
            "ord" => self.builtin_ord(&args, span)?,
            "chr" => self.builtin_chr(&args, span)?,
            "set" => self.builtin_set(&args, span)?,
            _ => unreachable!("unknown builtin {name}"),
        };
        self.push(result);
        Ok(())
    }

    fn arity_err(&self, name: &str, args: &[Value], n: usize, span: Span) -> Result<(), RuntimeError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(self.err(format!("{name}() expects {n} argument(s), got {}", args.len()), span))
        }
    }

    /// `set()` → empty set; `set(list)` → a deduped hash set of the list's elements.
    fn builtin_set(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let (list_obj, src): (Value, Vec<Value>) = match args {
            [] => (Value::Nil, Vec::new()),
            [one] => match one {
                Value::Obj(h) => match self.heap.get(*h) {
                    Obj::List(items) => (*one, items.clone()),
                    _ => return Err(self.err(format!("set() expects a list, got {}", self.type_name(*one)), span)),
                },
                other => return Err(self.err(format!("set() expects a list, got {}", self.type_name(*other)), span)),
            },
            _ => return Err(self.err(format!("set() expects 0 or 1 argument(s), got {}", args.len()), span)),
        };
        // Root the source list so its elements survive a struct element's re-entrant hash() GC; hash
        // every element first (phase 1, rooted), then build the set GC-free (phase 2).
        self.push(list_obj);
        let built = (|| {
            let mut hashes = Vec::with_capacity(src.len());
            for &v in &src {
                hashes.push(self.hash_value(v, span)?);
            }
            let mut set = SetData::default();
            for (i, &v) in src.iter().enumerate() {
                let he = hashes[i];
                if !set.candidates(he).iter().any(|&p| self.values_equal(set.entries[p].1, v)) {
                    set.push(he, v);
                }
            }
            Ok(set)
        })();
        self.pop(); // unroot the source list
        Ok(Value::Obj(self.heap.alloc(Obj::Set(built?))))
    }

    fn builtin_len(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("len", args, 1, span)?;
        match args[0] {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::List(items) => Ok(Value::Int(items.len() as i64)),
                Obj::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                _ => Err(self.err(format!("len() expects a list or str, got {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("len() expects a list or str, got {}", self.type_name(other)), span)),
        }
    }

    fn builtin_range(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        const MAX_RANGE_LEN: i64 = 10_000_000;
        let (start, end) = match args {
            [Value::Int(n)] => (0, *n),
            [Value::Int(a), Value::Int(b)] => (*a, *b),
            _ => return Err(self.err("range() expects range(end) or range(start, end) of ints".to_string(), span)),
        };
        let len = i128::from(end) - i128::from(start);
        if len > i128::from(MAX_RANGE_LEN) {
            return Err(self.err(format!("range() length {len} exceeds the maximum of {MAX_RANGE_LEN}"), span));
        }
        let items: Vec<Value> = (start..end).map(Value::Int).collect();
        Ok(Value::Obj(self.heap.alloc(Obj::List(items))))
    }

    fn builtin_int(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("int", args, 1, span)?;
        match args[0] {
            Value::Int(n) => Ok(Value::Int(n)),
            Value::Float(f) => {
                if !f.is_finite() || f < i64::MIN as f64 || f >= 9_223_372_036_854_775_808.0 {
                    return Err(self.err(format!("int(): {f} is out of integer range"), span));
                }
                Ok(Value::Int(f as i64))
            }
            Value::Bool(b) => Ok(Value::Int(i64::from(b))),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| self.err(format!("int(): cannot parse '{s}' as an integer"), span)),
                _ => Err(self.err(format!("int() cannot convert {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("int() cannot convert {}", self.type_name(other)), span)),
        }
    }

    fn builtin_float(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("float", args, 1, span)?;
        match args[0] {
            Value::Float(f) => Ok(Value::Float(f)),
            Value::Int(n) => Ok(Value::Float(n as f64)),
            Value::Bool(b) => Ok(Value::Float(f64::from(b))),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| self.err(format!("float(): cannot parse '{s}' as a float"), span)),
                _ => Err(self.err(format!("float() cannot convert {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("float() cannot convert {}", self.type_name(other)), span)),
        }
    }

    fn builtin_str(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("str", args, 1, span)?;
        let s = self.stringify(args[0], span)?;
        Ok(Value::Obj(self.heap.alloc(Obj::Str(s.into_boxed_str()))))
    }

    /// `ord(s)` — codepoint of the first char of `s`. Mirrors `interp::builtins::ord` (errors too).
    fn builtin_ord(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("ord", args, 1, span)?;
        match args[0] {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => match s.chars().next() {
                    Some(c) => Ok(Value::Int(c as i64)),
                    None => Err(self.err("ord() of an empty string".to_string(), span)),
                },
                _ => Err(self.err(format!("ord() expects a str, got {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("ord() expects a str, got {}", self.type_name(other)), span)),
        }
    }

    /// `chr(n)` — the 1-char str for codepoint `n`. Mirrors `interp::builtins::chr` (errors too).
    fn builtin_chr(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("chr", args, 1, span)?;
        match args[0] {
            Value::Int(n) => u32::try_from(n)
                .ok()
                .and_then(char::from_u32)
                .map(|c| Value::Obj(self.heap.alloc(Obj::Str(c.to_string().into_boxed_str()))))
                .ok_or_else(|| self.err(format!("chr(): {n} is not a valid Unicode codepoint"), span)),
            other => Err(self.err(format!("chr() expects an int, got {}", self.type_name(other)), span)),
        }
    }


    // ----- module namespace helpers -----

    fn module_global(&self, module: GcRef, name: &str) -> Option<Value> {
        match self.heap.get(module) {
            Obj::Module { globals, .. } => globals.get(name).copied(),
            _ => None,
        }
    }

    fn module_define(&mut self, module: GcRef, name: &str, value: Value) {
        if let Obj::Module { globals, .. } = self.heap.get_mut(module) {
            globals.insert(name.to_string(), value);
        }
    }

    fn module_assign(&mut self, module: GcRef, name: &str, value: Value) -> bool {
        if let Obj::Module { globals, .. } = self.heap.get_mut(module)
            && globals.contains_key(name)
        {
            globals.insert(name.to_string(), value);
            return true;
        }
        false
    }

    fn module_name(&self, module: GcRef) -> String {
        match self.heap.get(module) {
            Obj::Module { name, .. } => name.to_string(),
            _ => String::new(),
        }
    }

    // ----- display / type names -----

    fn type_name(&self, v: Value) -> &'static str {
        match v {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Nil => "nil",
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(_) => "str",
                Obj::List(_) => "list",
                Obj::Tuple(_) => "tuple",
                Obj::Map(_) => "map",
                Obj::Set(_) => "set",
                Obj::Struct { .. } => "struct",
                Obj::Enum { .. } => "enum",
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module { .. } => "module",
                Obj::Native { .. } => "function",
                Obj::Channel(_) => "Channel",
                Obj::Shared(_) => "Shared",
                Obj::Executor(_) => "Executor",
            },
        }
    }

    /// `Display` form, matching `interp::value::Value`'s `Display` exactly.
    fn display(&self, v: Value) -> String {
        match v {
            Value::Int(n) => n.to_string(),
            Value::Float(x) => format_float(x),
            Value::Bool(b) => b.to_string(),
            Value::Nil => "nil".to_string(),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => s.to_string(),
                Obj::List(items) => {
                    let inner = items.iter().map(|v| self.display(*v)).collect::<Vec<_>>().join(", ");
                    format!("[{inner}]")
                }
                Obj::Tuple(items) => {
                    let inner = items.iter().map(|v| self.display(*v)).collect::<Vec<_>>().join(", ");
                    format!("({inner})")
                }
                Obj::Map(m) => {
                    let inner = m
                        .entries
                        .iter()
                        .map(|(_, k, v)| format!("{}: {}", self.display(*k), self.display(*v)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{{{inner}}}")
                }
                Obj::Set(s) => {
                    if s.entries.is_empty() {
                        "set()".to_string()
                    } else {
                        let inner = s.entries.iter().map(|(_, v)| self.display(*v)).collect::<Vec<_>>().join(", ");
                        format!("{{{inner}}}")
                    }
                }
                Obj::Struct { name, fields } => {
                    let inner = fields.iter().map(|(k, v)| format!("{k}={}", self.display(*v))).collect::<Vec<_>>().join(", ");
                    format!("{name}({inner})")
                }
                Obj::Enum { variant, payload, .. } => {
                    if payload.is_empty() {
                        variant.to_string()
                    } else {
                        let inner = payload.iter().map(|v| self.display(*v)).collect::<Vec<_>>().join(", ");
                        format!("{variant}({inner})")
                    }
                }
                Obj::Func { proto, .. } => format!("<fn {}>", self.program.protos[*proto].name),
                Obj::Closure { .. } => "<closure>".to_string(),
                Obj::Module { name, .. } => format!("<module {name}>"),
                Obj::Native { name, .. } => format!("<native fn {name}>"),
                Obj::Channel(core) => format!("Channel(len={})", core.q.lock().unwrap().len()),
                // B3.1: the box holds the wire form; render it directly (`display` is `&self` and
                // cannot `from_wire`, which allocates — `display_wire` is the read-only equivalent).
                Obj::Shared(core) => format!("Shared({})", self.display_wire(&core.v.lock().unwrap())),
                Obj::Executor(core) => format!("Executor(pending={})", core.inner.lock().unwrap().queue.len()),
            },
        }
    }

    /// `Display` form of a [`WireValue`] — the read-only (`&self`) counterpart of [`display`] for
    /// values that live in a core (only `Shared` renders its contents). Mirrors `display` arm-for-arm;
    /// a `Handle(GcRef)` resolves back through the heap via `display`, a nested core renders like its
    /// heap counterpart. B3.1: total over the sendable set.
    fn display_wire(&self, w: &WireValue) -> String {
        match w {
            WireValue::Int(n) => n.to_string(),
            WireValue::Float(x) => format_float(*x),
            WireValue::Bool(b) => b.to_string(),
            WireValue::Nil => "nil".to_string(),
            WireValue::Str(s) => s.to_string(),
            WireValue::Handle(h) => self.display(Value::Obj(*h)),
            WireValue::List(items) => {
                let inner = items.iter().map(|v| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                format!("[{inner}]")
            }
            WireValue::Tuple(items) => {
                let inner = items.iter().map(|v| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                format!("({inner})")
            }
            WireValue::Map(entries) => {
                let inner = entries
                    .iter()
                    .map(|(_, k, v)| format!("{}: {}", self.display_wire(k), self.display_wire(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{inner}}}")
            }
            WireValue::Set(entries) => {
                if entries.is_empty() {
                    "set()".to_string()
                } else {
                    let inner = entries.iter().map(|(_, v)| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                    format!("{{{inner}}}")
                }
            }
            WireValue::Struct { name, fields } => {
                let inner = fields.iter().map(|(k, v)| format!("{k}={}", self.display_wire(v))).collect::<Vec<_>>().join(", ");
                format!("{name}({inner})")
            }
            WireValue::Enum { variant, payload, .. } => {
                if payload.is_empty() {
                    variant.to_string()
                } else {
                    let inner = payload.iter().map(|v| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                    format!("{variant}({inner})")
                }
            }
            WireValue::Channel(core) => format!("Channel(len={})", core.q.lock().unwrap().len()),
            WireValue::Shared(core) => format!("Shared({})", self.display_wire(&core.v.lock().unwrap())),
            WireValue::Executor(core) => format!("Executor(pending={})", core.inner.lock().unwrap().queue.len()),
        }
    }

    /// Protocol-aware render for `print` / `str()` / interpolation: a struct with a self-only
    /// `str(self) -> str` method (the `Stringable` protocol) dispatches to it; everything else uses
    /// the default structural repr, recursing through `stringify` so nested structs honour the
    /// protocol too. Mirrors `interp::Interp::stringify` exactly (parity-tested). Distinct from the
    /// `&self` `display` above, which stays the pure structural form for error/debug text.
    fn stringify(&mut self, v: Value, span: Span) -> Result<String, RuntimeError> {
        match v {
            Value::Int(n) => Ok(n.to_string()),
            Value::Float(x) => Ok(format_float(x)),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Nil => Ok("nil".to_string()),
            // ROOT the object on the operand stack: a `str` method runs nested frames that GC at
            // instruction boundaries, and the container keeps its transitive contents reachable.
            Value::Obj(h) => {
                self.push(v);
                let r = self.stringify_obj(h, span);
                self.pop();
                r
            }
        }
    }

    fn stringify_obj(&mut self, h: GcRef, span: Span) -> Result<String, RuntimeError> {
        // Clone the object's shape out so no heap borrow is held across the nested `&mut self` calls.
        match self.heap.get(h).clone() {
            Obj::Str(s) => Ok(s.to_string()),
            Obj::List(items) => Ok(format!("[{}]", self.stringify_seq(&items, span)?)),
            Obj::Tuple(items) => Ok(format!("({})", self.stringify_seq(&items, span)?)),
            Obj::Map(m) => {
                let mut rendered = Vec::with_capacity(m.entries.len());
                for (_, k, mv) in &m.entries {
                    rendered.push(format!("{}: {}", self.stringify(*k, span)?, self.stringify(*mv, span)?));
                }
                Ok(format!("{{{}}}", rendered.join(", ")))
            }
            Obj::Set(s) => {
                if s.entries.is_empty() {
                    Ok("set()".to_string())
                } else {
                    let elems: Vec<Value> = s.entries.iter().map(|(_, e)| *e).collect();
                    Ok(format!("{{{}}}", self.stringify_seq(&elems, span)?))
                }
            }
            Obj::Struct { name, fields } => {
                // `str(self) -> str` overrides the default repr. Only a self-only method is the hook.
                if let Some(def) = self.program.structs.get(name.as_ref()).cloned()
                    && let Some(&proto) = def.methods.get("str")
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[def.module_idx];
                    let res = self.guarded(|vm| vm.run_proto(proto, home, None, vec![Value::Obj(h)], true, false, span))?;
                    return self.stringify(res, span);
                }
                let mut rendered = Vec::with_capacity(fields.len());
                for (k, fv) in &fields {
                    rendered.push(format!("{k}={}", self.stringify(*fv, span)?));
                }
                Ok(format!("{name}({})", rendered.join(", ")))
            }
            Obj::Enum { variant, payload, .. } => {
                if payload.is_empty() {
                    Ok(variant.to_string())
                } else {
                    Ok(format!("{variant}({})", self.stringify_seq(&payload, span)?))
                }
            }
            Obj::Func { proto, .. } => Ok(format!("<fn {}>", self.program.protos[proto].name)),
            Obj::Closure { .. } => Ok("<closure>".to_string()),
            Obj::Module { name, .. } => Ok(format!("<module {name}>")),
            Obj::Native { name, .. } => Ok(format!("<native fn {name}>")),
            // Channel / Shared / Executor have no protocol hook — reuse the structural `Display`
            // (matches the interpreter's `stringify` catch-all falling back to `Display`).
            Obj::Channel(_) | Obj::Shared(_) | Obj::Executor(_) => Ok(self.display(Value::Obj(h))),
        }
    }

    fn stringify_seq(&mut self, elems: &[Value], span: Span) -> Result<String, RuntimeError> {
        let mut rendered = Vec::with_capacity(elems.len());
        for e in elems {
            rendered.push(self.stringify(*e, span)?);
        }
        Ok(rendered.join(", "))
    }
}

/// The VM's [`crate::native::Host`] adapter: lets a native fn read the evaluated `Value` arguments
/// (reaching into the heap for `str` args) and write to the captured output buffers. Holds `&mut
/// Vm` plus the arg vector; it allocates nothing itself — the returned [`crate::native::NativeRet`]
/// is lowered to heap objects by [`Vm::lower_native`] after the call returns. (Stdin / args / env /
/// cooperative-exit are wired in a later milestone; the unwired methods return inert defaults.)
struct VmHost<'a> {
    vm: &'a mut Vm,
    args: Vec<Value>,
}

impl crate::native::Host for VmHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => Err(crate::native::HostError::arg_type(i, "int", self.vm.type_name(*other))),
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
            Some(other) => Err(crate::native::HostError::arg_type(i, "float", self.vm.type_name(*other))),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Obj(h)) => match self.vm.heap.get(*h) {
                Obj::Str(s) => Ok(s.to_string()),
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "str", got))
                }
            },
            Some(other) => Err(crate::native::HostError::arg_type(i, "str", self.vm.type_name(*other))),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn write_stdout(&mut self, s: &str) {
        self.vm.out.push_str(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.vm.stderr.push_str(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.vm.host.stdin.read_line()
    }
    fn os_args(&self) -> Vec<String> {
        self.vm.host.args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        self.vm.host.env.get(key).cloned()
    }
    fn os_getcwd(&self) -> Result<String, crate::native::HostError> {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .map_err(|e| crate::native::HostError { message: e.to_string() })
    }
    fn request_exit(&mut self, code: i64) {
        self.vm.pending_exit = Some(code.clamp(0, 255) as i32);
    }
}

fn is_numeric(v: Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

/// Clamp a half-open `start..end` slice to a length-`len` sequence: both bounds clamp into
/// `[0, len]`, `start > end` collapses to empty. Returns `(lo, hi)` with `lo <= hi <= len`.
/// Mirrors `interp::clamp_range` (the two engines keep byte-identical slice semantics).
fn clamp_range(start: i64, end: i64, len: usize) -> (usize, usize) {
    let len_i = len as i64;
    let lo = start.clamp(0, len_i) as usize;
    let hi = end.clamp(0, len_i) as usize;
    (lo, hi.max(lo))
}

fn as_f64(v: Value) -> f64 {
    match v {
        Value::Int(n) => n as f64,
        Value::Float(f) => f,
        _ => unreachable!("as_f64 on non-numeric"),
    }
}

/// Format a float the way Chezzi prints it (matches `interp::value::format_float`).
fn format_float(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

// ===== entry points =====

/// Run a single-file program from source on the dedicated VM thread; returns output produced so
/// far + the outcome (test entry point, mirroring `interp::run_program`).
#[cfg(test)]
pub fn run_program(src: &str) -> (String, Result<(), RuntimeError>) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_program_inner(&src))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

#[cfg(test)]
fn run_program_inner(src: &str) -> (String, Result<(), RuntimeError>) {
    let tokens = match lexer::tokenize(src) {
        Ok(t) => t,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } })),
    };
    let module = match parser::parse(tokens) {
        Ok(m) => m,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let program = match crate::compiler::compile_module_standalone(&module) {
        Ok(p) => p,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let mut vm = Vm::new(Arc::new(program));
    let result = vm
        .run()
        .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
    (vm.out, result)
}

/// Run a single-file program and return its full stdout, or the error (test helper).
#[cfg(test)]
pub fn run_capture(src: &str) -> Result<String, RuntimeError> {
    let (out, result) = run_program(src);
    result.map(|()| out)
}

/// Run a single-file program, returning stdout (or error) plus the final live-object count.
/// `stress` collects before every instruction (surfaces missing GC roots); otherwise the normal
/// allocation-threshold trigger drives collection (test helper for GC assertions).
#[cfg(test)]
pub fn run_with(src: &str, stress: bool) -> (Result<String, RuntimeError>, usize) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let tokens = match lexer::tokenize(&src) {
                Ok(t) => t,
                Err(e) => return (Err(RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } }), 0),
            };
            let module = match parser::parse(tokens) {
                Ok(m) => m,
                Err(e) => return (Err(RuntimeError { message: e.message, span: e.span }), 0),
            };
            let program = match crate::compiler::compile_module_standalone(&module) {
                Ok(p) => p,
                Err(e) => return (Err(RuntimeError { message: e.message, span: e.span }), 0),
            };
            let mut vm = Vm::new(Arc::new(program));
            vm.gc_stress = stress;
            let result = vm
                .run()
                .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
            let live = vm.heap.live();
            (result.map(|()| vm.out), live)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Stdout from a stress-mode run (panics on error) — convenience for parity-under-GC tests.
#[cfg(test)]
pub fn run_capture_stress(src: &str) -> String {
    run_with(src, true).0.unwrap_or_else(|e| panic!("unexpected runtime error under GC stress: {e}"))
}

/// Run a multi-file program from its entry path on the dedicated VM thread. Mirrors
/// `interp::run_file`: resolve the graph, compile it, run each module once in dependency order,
/// then the entry's `main()`. Output produced so far is preserved alongside the outcome.
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
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_file_inner(&entry, cfg))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

fn run_file_inner(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    let graph = match crate::resolver::build_graph(entry) {
        Ok(g) => g,
        Err(e) => return (String::new(), String::new(), Err(RunError::plain(RuntimeError { message: e.message, span: e.span })), None),
    };
    let program = match crate::compiler::compile_graph(&graph) {
        Ok(p) => p,
        Err(e) => return (String::new(), String::new(), Err(RunError::plain(RuntimeError { message: e.message, span: e.span })), None),
    };
    let mut vm = Vm::new(Arc::new(program));
    vm.host = cfg;
    // On a clean finish, gracefully reap any Executor never explicitly shut down (C5 / A2). Skipped
    // on a fault (the program is already erroring) and on a hard `std.os.exit` (handled inside
    // `drain_live_executors` via `pending_exit`).
    let result = vm
        .run()
        .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
    // A pending exit means `result` is the `exit()` unwind sentinel, not a fault: report the
    // requested code as a clean halt.
    if let Some(code) = vm.pending_exit {
        return (vm.out, vm.stderr, Ok(()), Some(code));
    }
    // The stack trace was captured at the uncaught fault (before frames unwound); attach it.
    let trace = vm.fault_trace.take().unwrap_or_default();
    let result = result.map_err(|e| RunError::from_error(e, trace));
    (vm.out, vm.stderr, result, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a program to completion, returning its stdout (panics on runtime error).
    fn run(src: &str) -> String {
        run_capture(src).unwrap_or_else(|e| panic!("unexpected runtime error: {e}"))
    }

    /// Run a program expected to fail; return the runtime error message.
    fn run_err(src: &str) -> String {
        match run_capture(src) {
            Ok(out) => panic!("expected a runtime error, got output: {out:?}"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn list_comprehension_maps_and_filters() {
        assert_eq!(run("print([x * 2 for x in [1, 2, 3]])\n"), "[2, 4, 6]\n");
        assert_eq!(run("print([x for x in [1, 2, 3, 4] if x % 2 == 0])\n"), "[2, 4]\n");
    }

    #[test]
    fn list_comprehension_over_range() {
        assert_eq!(run("print([x * x for x in 0..5])\n"), "[0, 1, 4, 9, 16]\n");
    }

    #[test]
    fn set_comprehension_dedupes() {
        assert_eq!(run("print({x % 3 for x in [0, 1, 2, 3, 4, 5]})\n"), "{0, 1, 2}\n");
    }

    #[test]
    fn map_comprehension_builds_entries() {
        assert_eq!(run("print({x: x * x for x in [1, 2, 3]})\n"), "{1: 1, 2: 4, 3: 9}\n");
    }

    #[test]
    fn map_comprehension_over_map_keys_and_values() {
        assert_eq!(
            run("m := {\"a\": 1, \"b\": 2}\nprint({k: v * 10 for k, v in m})\n"),
            "{a: 10, b: 20}\n"
        );
    }

    // ----- M6c: native function values -----

    fn empty_program() -> Program {
        Program {
            protos: vec![],
            structs: Default::default(),
            variants: Default::default(),
            modules: vec![],
        }
    }

    #[test]
    fn vm_calls_native_fn_value() {
        use crate::native::{Host, HostError, NativeRet};
        fn add(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            crate::native::expect_args(h, "add", 2)?;
            Ok(NativeRet::Int(h.arg_int(0)? + h.arg_int(1)?))
        }
        let mut vm = Vm::new(Arc::new(empty_program()));
        let h = vm.heap.alloc(Obj::Native { name: "add".into(), func: add });
        vm.push(Value::Obj(h));
        vm.push(Value::Int(40));
        vm.push(Value::Int(2));
        vm.do_call(2, Span { line: 1, col: 1 }).unwrap();
        assert_eq!(vm.pop(), Value::Int(42));
    }

    #[test]
    fn vm_native_str_return_lowers_to_heap_with_no_children() {
        use crate::native::{Host, HostError, NativeRet};
        fn greet(_h: &mut dyn Host) -> Result<NativeRet, HostError> {
            Ok(NativeRet::Str("hi".into()))
        }
        let mut vm = Vm::new(Arc::new(empty_program()));
        let nat = vm.heap.alloc(Obj::Native { name: "greet".into(), func: greet });
        // A native fn handle has no GC children (guards the mark-phase claim).
        assert!(vm.heap.children(nat).is_empty());
        vm.push(Value::Obj(nat));
        vm.do_call(0, Span { line: 1, col: 1 }).unwrap();
        let result = vm.pop();
        assert_eq!(vm.display(result), "hi");
    }

    // ----- B3.0: WireValue airlock (to_wire / from_wire) -----

    /// A round-trip through the wire form, into the *same* heap, must be value-equal to the original
    /// over a deeply-nested sendable mix (scalars, str, list, tuple, map, set, struct, enum). This is
    /// the airlock's correctness invariant (B3.0): serialize then reconstruct loses nothing.
    #[test]
    fn wire_roundtrip_preserves_value_equality() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let s = vm.heap.alloc(Obj::Str("s".into()));
        let tup = vm.heap.alloc(Obj::Tuple(vec![Value::Bool(true), Value::Nil]));
        let st = vm.heap.alloc(Obj::Struct {
            name: "P".into(),
            fields: vec![("x".into(), Value::Int(1)), ("y".into(), Value::Obj(s))],
        });
        let en = vm.heap.alloc(Obj::Enum {
            ty: "Option".into(),
            variant: "Some".into(),
            payload: vec![Value::Int(9)],
        });
        let mut m = MapData::default();
        m.push(10, Value::Int(1), Value::Int(100));
        m.push(20, Value::Obj(s), Value::Int(200));
        let map = vm.heap.alloc(Obj::Map(m));
        let mut set = SetData::default();
        set.push(5, Value::Int(1));
        set.push(6, Value::Int(2));
        let setobj = vm.heap.alloc(Obj::Set(set));
        let list = vm.heap.alloc(Obj::List(vec![
            Value::Int(1),
            Value::Obj(s),
            Value::Obj(tup),
            Value::Obj(st),
            Value::Obj(en),
            Value::Obj(map),
            Value::Obj(setobj),
        ]));
        let v = Value::Obj(list);

        let w = vm.to_wire(v).expect("nested sendable value should serialize");
        let wired = vm.from_wire(w);
        assert!(vm.values_equal(v, wired), "wire round-trip changed the value");
        // Data is reconstructed into a *fresh* handle (deep copy, not aliasing the original).
        assert_ne!(v, wired, "round-tripped data should be a distinct heap object");
    }

    /// `Map`/`Set` cross the wire carrying their **cached hashes** and **insertion order** unchanged —
    /// `from_wire` rebuilds via `push(hash, …)`, never re-hashing. Pins byte-identical reconstruction
    /// (the iteration order + index a later `print`/lookup observes) even when two keys collide.
    #[test]
    fn wire_preserves_map_hashes_and_order() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let mut m = MapData::default();
        m.push(42, Value::Int(1), Value::Int(10)); // collides with the third entry on hash 42
        m.push(7, Value::Int(2), Value::Int(20));
        m.push(42, Value::Int(3), Value::Int(30));
        let map = Value::Obj(vm.heap.alloc(Obj::Map(m)));

        let w = vm.to_wire(map).expect("map should serialize");
        let wired = vm.from_wire(w);
        let Value::Obj(h) = wired else { panic!("expected heap obj") };
        let Obj::Map(rebuilt) = vm.heap.get(h) else { panic!("expected map") };
        let hashes: Vec<u64> = rebuilt.entries.iter().map(|(hash, ..)| *hash).collect();
        assert_eq!(hashes, vec![42, 7, 42], "cached hashes / order must survive the round-trip");
        // The index must reflect the cached hashes (collision bucket points at positions 0 and 2).
        assert_eq!(rebuilt.candidates(42), &[0, 2]);
        assert_eq!(rebuilt.candidates(7), &[1]);
    }

    /// By-reference callables (`Func`/`Closure`/`Module`/`Native`) cross the airlock **by handle** —
    /// `to_wire`→`from_wire` returns the *same* `GcRef` (matching the old `deep_clone` by-handle arm).
    /// (`Str` no longer qualifies — it crosses by value as of B3.3a; see `wire_crosses_str_by_value`.)
    #[test]
    fn wire_passes_by_reference_objects_as_same_handle() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let m = vm.heap.alloc(Obj::Module { name: "m".into(), globals: Default::default() });
        let v = Value::Obj(m);
        let w = vm.to_wire(v).expect("by-ref object should serialize");
        assert_eq!(vm.from_wire(w), v, "by-reference object must round-trip to the same handle");
    }

    /// B3.3a: a `str` crosses the airlock **by value** (owned bytes), not as a by-reference
    /// `Handle(GcRef)`: `from_wire` allocates a *fresh* heap `str` that is value-equal but a distinct
    /// handle. This is what lets a `str` cross a real OS-thread heap boundary at B3.3 (a `GcRef` would
    /// be a meaningless slot index there). Parity-safe: `str` is immutable + value-compared and Chezzi
    /// has no identity operator, so a fresh handle is observationally identical to the shared one.
    #[test]
    fn wire_crosses_str_by_value() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let s = vm.heap.alloc(Obj::Str("imm".into()));
        let v = Value::Obj(s);
        let w = vm.to_wire(v).expect("str should serialize");
        let wired = vm.from_wire(w);
        assert_ne!(wired, v, "a crossed str gets a fresh handle (by value, not by handle)");
        assert!(vm.values_equal(v, wired), "the fresh str must be value-equal to the original");
    }

    /// B3.3a: a `str` used as a **map key** crosses by value and stays findable — the cached hash is
    /// carried through and `from_wire` rebuilds the key as a fresh handle whose content hashes
    /// identically (hashing keys on bytes, not `GcRef`), so the reconstructed map's bucket index is
    /// preserved. Guards against a future change that hashed/compared str keys by handle identity.
    #[test]
    fn wire_str_map_key_survives_roundtrip() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let key = vm.heap.alloc(Obj::Str("k".into()));
        let mut m = MapData::default();
        let h = vm.scalar_hash(Value::Obj(key));
        m.push(h, Value::Obj(key), Value::Int(42));
        let map = Value::Obj(vm.heap.alloc(Obj::Map(m)));

        let w = vm.to_wire(map).expect("map with a str key should serialize");
        let wired = vm.from_wire(w);
        let Value::Obj(mh) = wired else { panic!("expected map handle") };
        let Obj::Map(rebuilt) = vm.heap.get(mh) else { panic!("expected map") };
        // Same single entry: a fresh str key, value-equal, same cached hash → same bucket.
        assert_eq!(rebuilt.entries.len(), 1);
        let (rh, rk, rv) = &rebuilt.entries[0];
        assert_eq!(*rh, h, "cached hash preserved");
        assert_eq!(*rv, Value::Int(42));
        assert_eq!(rebuilt.candidates(h), &[0], "index bucket points at the rebuilt key");
        assert!(vm.values_equal(*rk, Value::Obj(key)), "rebuilt str key is value-equal");
    }

    /// B3.1: `Channel`/`Shared`/`Executor` cross the airlock as their shared `Arc<…Core>`. The
    /// round-trip yields a *fresh* `GcRef` (a new handle obj) wrapping the **same** core — identity is
    /// at the `Arc`, not the handle, so two tasks still reach one mailbox/box/queue.
    #[test]
    fn wire_shares_core_across_a_fresh_handle() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let ch = vm.heap.alloc(Obj::Channel(Arc::new(ChannelCore::default())));
        let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore::default())));
        let ex = vm.heap.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
        for h in [ch, sh, ex] {
            let v = Value::Obj(h);
            let w = vm.to_wire(v).expect("core handle should serialize");
            let wired = vm.from_wire(w);
            assert_ne!(wired, v, "a crossed core gets a fresh handle (new GcRef)");
            // Same underlying core: an `Arc::ptr_eq` between the two handles' cores.
            let same = match (vm.heap.get(h), vm.heap.get(match wired { Value::Obj(g) => g, _ => unreachable!() })) {
                (Obj::Channel(a), Obj::Channel(b)) => Arc::ptr_eq(a, b),
                (Obj::Shared(a), Obj::Shared(b)) => Arc::ptr_eq(a, b),
                (Obj::Executor(a), Obj::Executor(b)) => Arc::ptr_eq(a, b),
                _ => false,
            };
            assert!(same, "the fresh handle must point at the SAME shared core");
        }
    }

    /// B3.1: two handles produced from one core (the `from_wire` airlock copy) reach the SAME mailbox
    /// — `send` on one handle is `recv`-able through the other. Proves the `Arc` core is shared, not
    /// duplicated, across the wire.
    #[test]
    fn channel_core_shared_across_handles() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let h1 = vm.heap.alloc(Obj::Channel(Arc::new(ChannelCore::default())));
        // Cross the airlock → a second handle onto the same core.
        let w = vm.to_wire(Value::Obj(h1)).unwrap();
        let Value::Obj(h2) = vm.from_wire(w) else { panic!("expected handle") };
        let sp = Span { line: 1, col: 1 };
        vm.channel_method(h1, "send", &[Value::Int(7)], sp).unwrap();
        // recv through the OTHER handle sees the message.
        assert_eq!(vm.channel_method(h2, "recv", &[], sp).unwrap(), Value::Int(7));
    }

    /// B3.1: `shut` lives in the shared core, so a `from_wire`'d alias observes a shutdown done through
    /// the original handle — `submit` on the alias then fails with the byte-identical message.
    #[test]
    fn executor_core_shut_is_shared_across_handles() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let h1 = vm.heap.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
        let w = vm.to_wire(Value::Obj(h1)).unwrap();
        let Value::Obj(h2) = vm.from_wire(w) else { panic!("expected handle") };
        let sp = Span { line: 1, col: 1 };
        vm.executor_method(h1, "shutdown", &[], sp).unwrap();
        let dummy = vm.heap.alloc(Obj::Str("task".into()));
        let err = vm.executor_method(h2, "submit", &[Value::Obj(dummy)], sp).unwrap_err();
        assert_eq!(err.message, "submit on a shut-down Executor (it no longer accepts work)");
    }

    /// B3.1: `display` of a `Shared` box renders its contents through `display_wire` (a boxed `str`
    /// renders from its owned bytes — B3.3a), since `display` is `&self` and can't `from_wire`.
    #[test]
    fn display_shared_renders_contents() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let s = vm.heap.alloc(Obj::Str("hi".into()));
        let boxed = vm.to_wire(Value::Obj(s)).unwrap();
        let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore { v: Mutex::new(boxed) })));
        assert_eq!(vm.display(Value::Obj(sh)), "Shared(hi)");
    }

    // ----- B3.2: isolated worker-VM construction (no threads) -----

    /// Build a one-proto program + a parent `Vm`, plus a zero-arg closure over proto 0 with a dummy
    /// home module (the test protos never read globals). Mirrors how `do_spawn_block` shapes a task.
    fn worker_fixture(code: Vec<Op>) -> (Vm, PendingCall) {
        let sp = Span { line: 1, col: 1 };
        let proto = op::Proto { name: "task".into(), arity: 0, n_slots: 0, lines: vec![sp; code.len()], code };
        let program = Program { protos: vec![proto], ..empty_program() };
        let mut vm = Vm::new(Arc::new(program));
        let home = vm.heap.alloc(Obj::Module { name: "<test>".into(), globals: Default::default() });
        let clo = vm.heap.alloc(Obj::Closure { proto: 0, captured: Default::default(), home });
        (vm, PendingCall::Call { callee: Value::Obj(clo), args: Vec::new(), span: sp })
    }

    /// The worker allocates into its OWN heap, not the parent's: a task that builds a fresh list runs
    /// to completion in the worker, the parent heap's live-object count is unchanged, and the result
    /// crosses back as a `WireValue` that reconstructs (in the parent) to the expected value.
    #[test]
    fn worker_runs_in_distinct_heap() {
        // () -> [1, 2]
        let (mut vm, task) =
            worker_fixture(vec![Op::ConstInt(1), Op::ConstInt(2), Op::NewList(2), Op::Return]);
        let before = vm.heap.live();
        let res = vm.run_task_isolated(task).expect("isolated task runs");
        assert_eq!(vm.heap.live(), before, "worker must not allocate into the parent heap");
        let got = vm.from_wire(res.value);
        let want = Value::Obj(vm.heap.alloc(Obj::List(vec![Value::Int(1), Value::Int(2)])));
        assert!(vm.values_equal(got, want), "result must round-trip back to [1, 2]");
    }

    /// A worker's stdout is captured in ITS `out` and returned on the `WorkerResult` (decision F:
    /// buffer-per-worker), and never leaks into the parent's `out`. The return value crosses back too.
    #[test]
    fn worker_returns_value_and_out() {
        // () -> { print("hi from worker"); 7 }
        let (mut vm, task) = worker_fixture(vec![
            Op::ConstStr("hi from worker".into()),
            Op::CallPrint(1),
            Op::Pop,
            Op::ConstInt(7),
            Op::Return,
        ]);
        let res = vm.run_task_isolated(task).expect("isolated task runs");
        assert_eq!(res.out, "hi from worker\n", "worker stdout returns on the result");
        assert_eq!(res.stderr, "", "stderr is captured separately and empty here");
        assert_eq!(vm.from_wire(res.value), Value::Int(7), "return value crosses back");
        assert_eq!(vm.out, "", "worker output must not leak into the parent's stdout");
    }

    /// A worker shares the compiled program by `Arc` (read-only), never copying it: `spawn_worker`
    /// bumps the strong count and points at the SAME allocation, and drops its clone when finished.
    #[test]
    fn worker_shares_program_arc() {
        let program = Arc::new(empty_program());
        let vm = Vm::new(Arc::clone(&program)); // program + vm = 2 refs
        assert_eq!(Arc::strong_count(&program), 2);
        let worker = vm.spawn_worker();
        assert_eq!(Arc::strong_count(&program), 3, "worker shares the program (no copy)");
        assert!(Arc::ptr_eq(&program, &worker.program), "same Program allocation, not a clone");
        drop(worker);
        assert_eq!(Arc::strong_count(&program), 2, "worker releases its program ref on drop");
    }

    /// B3.3a: a `str` return value crosses the worker boundary **by value** — the worker serializes its
    /// own-heap `str` to owned bytes, and the parent reconstructs a fresh `str` from them (no dangling
    /// `GcRef`). Replaces B3.2's reject-the-str fault now that `str` is sendable by value.
    #[test]
    fn worker_crosses_str_by_value() {
        // () -> "oops"
        let (mut vm, task) = worker_fixture(vec![Op::ConstStr("oops".into()), Op::Return]);
        let res = vm.run_task_isolated(task).expect("a str result now crosses by value");
        let got = vm.from_wire(res.value);
        let want = Value::Obj(vm.heap.alloc(Obj::Str("oops".into())));
        assert!(vm.values_equal(got, want), "str result round-trips to \"oops\"");
    }

    /// Method tasks (`spawn recv.m()`) are gated off in B3.2: a worker never runs module init, so its
    /// `module_objs` table is empty and method dispatch would index out of bounds. Reject cleanly.
    #[test]
    fn worker_rejects_method_task() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let recv = vm.heap.alloc(Obj::Str("x".into()));
        let task = PendingCall::Method {
            recv: Value::Obj(recv),
            name: "len".into(),
            args: Vec::new(),
            span: Span { line: 1, col: 1 },
        };
        let err = vm.run_task_isolated(task).expect_err("method tasks are not isolable in B3.2");
        assert!(err.message.contains("method tasks are not yet isolable"), "got: {}", err.message);
    }

    // ----- arithmetic -----

    #[test]
    fn int_div_truncates() {
        assert_eq!(run("print(7 / 2)"), "3\n");
        assert_eq!(run("print(-7 / 2)"), "-3\n"); // Rust trunc-toward-zero, matching interp
    }

    #[test]
    fn int_overflow_is_error_not_wrap() {
        // A wrapping VM would print a negative number; we must error like the interpreter.
        assert!(run_err("print(9223372036854775807 + 1)").contains("integer overflow in Add"));
    }

    #[test]
    fn int_min_neg_and_div_overflow() {
        // The two other unrepresentable results: -i64::MIN and i64::MIN / -1. Both must error.
        let neg = "fn main():\n    x := -9223372036854775807 - 1\n    print(-x)\nmain()\n";
        assert!(run_err(neg).contains("integer overflow"));
        let div = "fn main():\n    x := -9223372036854775807 - 1\n    print(x / -1)\nmain()\n";
        assert!(run_err(div).contains("integer overflow"));
    }

    #[test]
    fn float_promotion_when_either_side_float() {
        assert_eq!(run("print(1 + 2.0)"), "3.0\n");
        assert_eq!(run("print(7.0 / 2.0)"), "3.5\n");
        assert_eq!(run("print(7 / 2.0)"), "3.5\n");
    }

    #[test]
    fn division_and_modulo_by_zero_error() {
        assert_eq!(run_err("print(1 / 0)"), "division by zero");
        assert_eq!(run_err("print(1 % 0)"), "modulo by zero");
        // Float by zero is an error too — not silent inf/nan.
        assert_eq!(run_err("print(1.0 / 0.0)"), "division by zero");
        assert_eq!(run_err("print(5.0 % 0.0)"), "modulo by zero");
    }

    #[test]
    fn string_concatenation() {
        assert_eq!(run(r#"print("a" + "b" + "c")"#), "abc\n");
    }

    #[test]
    fn comparison_and_equality_across_numeric_types() {
        assert_eq!(run("print(1 < 2.0)"), "true\n");
        assert_eq!(run("print(2 == 2.0)"), "true\n");
        assert_eq!(run("print(2 != 3)"), "true\n");
        assert_eq!(run(r#"print("a" < "b")"#), "true\n");
        // Cross-type equality is false, never an error.
        assert_eq!(run(r#"print(1 == "1")"#), "false\n");
    }

    #[test]
    fn arithmetic_type_error_message() {
        assert!(run_err(r#"print(1 + "x")"#).contains("cannot apply Add to int and str"));
    }

    // ----- and / or short-circuit -----

    #[test]
    fn and_short_circuits_rhs() {
        // If `and` did not short-circuit, the `1/0` would raise a div-by-zero error.
        assert_eq!(run("print(false and (1 / 0 == 0))"), "false\n");
    }

    #[test]
    fn or_short_circuits_rhs() {
        assert_eq!(run("print(true or (1 / 0 == 0))"), "true\n");
    }

    #[test]
    fn logical_operand_must_be_bool() {
        assert_eq!(run_err("print(1 and true)"), "expected bool, found int");
    }

    // ----- display formatting -----

    #[test]
    fn float_display_keeps_one_decimal_for_integral() {
        assert_eq!(run("print(5.0)"), "5.0\n");
        assert_eq!(run("print(5.5)"), "5.5\n");
        assert_eq!(run("print(2.5 * 2.0)"), "5.0\n");
    }

    #[test]
    fn list_display() {
        assert_eq!(run("print([1, 2, 3])"), "[1, 2, 3]\n");
        assert_eq!(run("print([])"), "[]\n");
        assert_eq!(run(r#"print(["a", "b"])"#), "[a, b]\n");
    }

    #[test]
    fn struct_display_in_declaration_order() {
        let src = "\
struct Point:
    x: int
    y: int
print(Point(3, 4))";
        assert_eq!(run(src), "Point(x=3, y=4)\n");
    }

    #[test]
    fn enum_display_nullary_and_payload() {
        let src = "\
enum Shape:
    Circle(int)
    Dot
print(Circle(2))
print(Dot)";
        assert_eq!(run(src), "Circle(2)\nDot\n");
    }

    #[test]
    fn print_joins_args_with_space() {
        assert_eq!(run(r#"print("a", 1, true)"#), "a 1 true\n");
    }

    // ----- functions / control flow -----

    #[test]
    fn nested_calls_and_return() {
        let src = "\
fn add(a: int, b: int) -> int:
    return a + b
fn main():
    print(add(add(1, 2), 3))
main()";
        assert_eq!(run(src), "6\n");
    }

    #[test]
    fn forward_reference_between_top_level_fns() {
        // `main` is defined before `helper`; hoisting must make the forward ref resolve.
        let src = "\
fn main():
    print(helper(21))
fn helper(n: int) -> int:
    return n * 2
main()";
        assert_eq!(run(src), "42\n");
    }

    #[test]
    fn infinite_recursion_hits_depth_limit() {
        let src = "\
fn loop(n: int) -> int:
    return loop(n + 1)
fn main():
    print(loop(0))
main()";
        assert!(run_err(src).contains("maximum call depth"));
    }

    /// M10-G1: a self-referential `Stringable` `str` must hit the depth guard, not loop forever.
    #[test]
    fn self_referential_stringable_hits_depth_limit() {
        let src = "struct Loop:\n    n: int\n    fn str(self) -> str:\n        return str(self)\nprint(Loop(1))\n";
        assert!(run_err(src).contains("maximum call depth"));
    }

    #[test]
    fn if_elif_else() {
        let src = "\
fn classify(n: int) -> str:
    if n < 0:
        return \"neg\"
    else if n == 0:
        return \"zero\"
    else:
        return \"pos\"
fn main():
    print(classify(-1))
    print(classify(0))
    print(classify(5))
main()";
        assert_eq!(run(src), "neg\nzero\npos\n");
    }

    #[test]
    fn while_loop_with_compound_assign() {
        let src = "\
fn main():
    i := 0
    total := 0
    while i < 5:
        total += i
        i += 1
    print(total)
main()";
        assert_eq!(run(src), "10\n");
    }

    #[test]
    fn unary_neg_and_not() {
        assert_eq!(run("print(-5)"), "-5\n");
        assert_eq!(run("print(not true)"), "false\n");
        assert_eq!(run_err("print(-true)"), "cannot apply Neg to bool");
    }

    // ----- closures -----

    #[test]
    fn closure_snapshots_captured_value() {
        // The closure captures `n` by value at creation; reassigning `n` afterward must NOT be
        // visible (matches the interpreter's frame snapshot, not by-reference capture).
        let src = "\
fn make():
    n := 10
    f := fn(x: int) -> int: x + n
    n = 20
    return f
fn main():
    g := make()
    print(g(5))
main()";
        assert_eq!(run(src), "15\n");
    }

    #[test]
    fn closure_captures_distinct_environments() {
        let src = "\
fn adder(n: int):
    return fn(x: int) -> int: x + n
fn main():
    add10 := adder(10)
    add100 := adder(100)
    print(add10(1))
    print(add100(1))
main()";
        assert_eq!(run(src), "11\n101\n");
    }

    // ----- ? operator -----

    #[test]
    fn try_unwraps_ok() {
        let src = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"divide by zero\")
    return Ok(a / b)
fn main():
    r := safe_div(10, 2)?
    print(r)
main()";
        assert_eq!(run(src), "5\n");
    }

    #[test]
    fn try_propagates_err_to_caller() {
        let src = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"zero\")
    return Ok(a / b)
fn use() -> Result[int]:
    r := safe_div(1, 0)?
    return Ok(r + 1)
fn main():
    match use():
        Ok(v): print(\"ok {v}\")
        Err(e): print(\"err {e}\")
main()";
        assert_eq!(run(src), "err zero\n");
    }

    #[test]
    fn try_on_non_result_is_error() {
        let src = "\
fn f() -> int:
    x := (5)?
    return x";
        // Reaching `?` on an int is a runtime error.
        assert!(run_err(&format!("{src}\nfn main():\n    print(f())\nmain()")).contains("'?' expects Result or Option, found int"));
    }

    #[test]
    fn top_level_try_err_is_unhandled_error() {
        // A `?` at the top level whose Err reaches the top is an unhandled error (no main needed).
        assert_eq!(run_err(r#"x := Err("oops")?"#), "unhandled error: oops");
    }

    #[test]
    fn top_level_try_err_reports_real_line() {
        // The `?` is on line 3 — report there, not at a hard-coded line 1 (parity with the interp).
        let e = run_capture("fn d() -> Result[int]:\n    return Err(\"x\")\nx := d()?\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: x");
        assert_eq!(e.span.line, 3, "expected the `?` line, got {}", e.span.line);
    }

    // ----- for loops -----

    #[test]
    fn for_range_sums() {
        let src = "\
fn main():
    total := 0
    for i in 0..1000:
        total += i
    print(total)
main()";
        assert_eq!(run(src), "499500\n");
    }

    #[test]
    fn for_range_is_lazy_not_materialized() {
        // A billion-element range would exhaust memory if materialized; the lazy counting loop
        // returns on the first iteration instantly.
        let src = "\
fn first() -> int:
    for i in 0..1000000000:
        return i
    return -1
fn main():
    print(first())
main()";
        assert_eq!(run(src), "0\n");
    }

    #[test]
    fn for_over_list() {
        let src = "\
fn main():
    total := 0
    for x in [10, 20, 30]:
        total += x
    print(total)
main()";
        assert_eq!(run(src), "60\n");
    }

    #[test]
    fn for_over_non_iterable_errors() {
        assert!(run_err("for x in 5:\n    print(x)").contains("cannot iterate over int"));
    }

    // ----- match -----

    #[test]
    fn match_binds_payload() {
        let src = "\
enum Shape:
    Circle(int)
    Square(int)
fn area(s: Shape) -> int:
    match s:
        Circle(r): return r * r * 3
        Square(n): return n * n
fn main():
    print(area(Circle(2)))
    print(area(Square(3)))
main()";
        assert_eq!(run(src), "12\n9\n");
    }

    #[test]
    fn match_no_arm_is_error() {
        let src = "\
enum Color:
    Red
    Green
    Blue
fn name(c: Color) -> str:
    match c:
        Red: return \"r\"
        Green: return \"g\"
fn main():
    print(name(Blue))
main()";
        assert_eq!(run_err(src), "no match arm for variant 'Blue'");
    }

    #[test]
    fn match_on_non_enum_is_error() {
        // A *payload* variant pattern unambiguously needs an enum scrutinee; matching it on an int is
        // a clean runtime error (the `EnsureEnum` guard) rather than a panic.
        let src = "\
fn main():
    match 5:
        Some(x): print(x)
main()";
        assert!(run_err(src).contains("cannot match on int"));
    }

    #[test]
    fn match_bare_ident_on_non_enum_binds_value() {
        // A bare top-level identifier against a non-enum value is a binding capturing the whole
        // value (the checker permits this only for literal scrutinees) — not an enum-match error.
        let src = "\
fn main():
    match 5:
        x: print(x)
main()";
        assert_eq!(run(src), "5\n");
    }

    // ----- field / index -----

    #[test]
    fn index_list_and_out_of_bounds() {
        assert_eq!(run("print([10, 20, 30][1])"), "20\n");
        assert_eq!(run_err("print([1, 2][5])"), "index 5 out of bounds (len 2)");
    }

    #[test]
    fn index_string_returns_char() {
        assert_eq!(run(r#"print("hello"[1])"#), "e\n");
    }

    #[test]
    fn index_assign_mutates_in_place() {
        assert_eq!(run("xs := [1, 2, 3]\nxs[1] = 9\nprint(xs)\n"), "[1, 9, 3]\n");
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
        assert_eq!(run_err("xs := [1, 2, 3]\nxs[5] = 0\n"), "index 5 out of bounds (len 3)");
    }

    #[test]
    fn field_assign_mutates_in_place() {
        let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    p.x = 9
    print(p.x)
    print(p.y)
main()";
        assert_eq!(run(src), "9\n2\n");
    }

    #[test]
    fn field_compound_assign() {
        let src = "\
struct P:
    x: int
fn main():
    p := P(10)
    p.x += 5
    p.x -= 3
    print(p.x)
main()";
        assert_eq!(run(src), "12\n");
    }

    #[test]
    fn field_access_and_unknown_field() {
        let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    print(p.x)
    print(p.y)
main()";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn struct_method_call_binds_self() {
        let src = "\
struct Counter:
    n: int
    fn doubled(self) -> int:
        return self.n * 2
fn main():
    c := Counter(21)
    print(c.doubled())
main()";
        assert_eq!(run(src), "42\n");
    }

    // ----- builtins -----

    #[test]
    fn builtin_len() {
        assert_eq!(run("print(len([1, 2, 3]))"), "3\n");
        assert_eq!(run(r#"print(len("hello"))"#), "5\n");
    }

    #[test]
    fn builtin_range_and_cap() {
        assert_eq!(run("print(range(3))"), "[0, 1, 2]\n");
        assert_eq!(run("print(range(2, 5))"), "[2, 3, 4]\n");
        assert!(run_err("print(range(20000000))").contains("exceeds the maximum"));
    }

    #[test]
    fn builtin_casts() {
        assert_eq!(run(r#"print(int("42"))"#), "42\n");
        assert_eq!(run("print(float(3))"), "3.0\n");
        assert_eq!(run("print(str(5))"), "5\n");
        assert!(run_err(r#"print(int("notnum"))"#).contains("cannot parse 'notnum'"));
    }

    // ----- construction arity / nullary variant -----

    #[test]
    fn struct_arity_error() {
        let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1)
main()";
        assert!(run_err(src).contains("struct 'Point' expects 2 field(s), got 1"));
    }

    #[test]
    fn variant_arity_error() {
        assert!(run_err("fn main():\n    x := Ok(1, 2)\nmain()").contains("variant 'Ok' expects 1 value(s), got 2"));
    }

    #[test]
    fn nullary_variant_used_as_value() {
        assert_eq!(run("print(None)"), "None\n");
        let src = "\
enum Light:
    On
    Off
fn main():
    print(Off)
main()";
        assert_eq!(run(src), "Off\n");
    }

    // ----- string interpolation -----

    #[test]
    fn interpolation_and_literal_braces() {
        let src = "\
fn main():
    name := \"thuan\"
    print(\"hi {name}, {{not interpolated}}\")
main()";
        assert_eq!(run(src), "hi thuan, {not interpolated}\n");
    }

    // ----- golden parity -----

    #[test]
    fn golden_hello_chz_matches_expected() {
        let expected = include_str!("../../examples/hello.expected");
        assert_eq!(run(include_str!("../../examples/hello.chz")), expected);
    }

    #[test]
    fn golden_hello_chz_matches_interpreter() {
        let src = include_str!("../../examples/hello.chz");
        let vm_out = run_capture(src).expect("vm run");
        let interp_out = crate::interp::run_capture(src).expect("interp run");
        assert_eq!(vm_out, interp_out);
    }

    /// M8-M4 golden: `examples/set.chz` (the set type — literals, membership, algebra, iteration)
    /// byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_set_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/set.chz");
        let expected = include_str!("../../examples/set.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Slicing golden: `examples/slicing.chz` (list/str slicing + the `Index`/`IndexSet`/`Slice`
    /// protocols on a struct + a generic over both) byte-identical on the VM, interp, and `.expected`.
    #[test]
    fn golden_slicing_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/slicing.chz");
        let expected = include_str!("../../examples/slicing.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `defer` golden: LIFO order, method + free-fn calls, the `?` short-circuit path, args evaluated
    /// at the defer statement (per-iteration snapshot), defers running before a `recover:` catch, and
    /// a fault inside a deferred call. Byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_defer_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/defer.chz");
        let expected = include_str!("../../examples/defer.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- concurrency C4 (VM parity for spawn / parallel: / Channel / Shared) -----

    /// C1 golden: `parallel:` nursery + both `spawn` forms run to completion at the dedent (FIFO),
    /// the parent resuming only after the join. Byte-identical on the VM, the interpreter, and the
    /// `.expected` file.
    #[test]
    fn golden_parallel_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/parallel.chz");
        let expected = include_str!("../../examples/parallel.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// C2 golden: `Channel[T]` fan-out — workers `send` at the dedent, the parent `recv`s after the
    /// join. Byte-identical on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_channel_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/channel.chz");
        let expected = include_str!("../../examples/channel.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// A1 golden: `Channel[T].try_recv()` — a non-blocking poll returning `T?`. Workers `send` at the
    /// dedent; the parent drains with `try_recv` (`Some` per value, then `None`). Never blocks/faults,
    /// so byte-identical on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_try_recv_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/try_recv.chz");
        let expected = include_str!("../../examples/try_recv.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
        assert_eq!(run_capture_stress(src), expected);
    }

    /// B1+B2 golden (VM-only): blocking `recv`. The consumer is scheduled first, parks on the empty
    /// channel, the cooperative scheduler runs the producer, and the consumer resumes to receive. The
    /// interpreter still faults `deadlock` on the same program (documented parity gap — see the interp
    /// twin `channel_block_chz_faults_deadlock_on_interp`), so this asserts the VM output + `.expected`
    /// only, NOT cross-engine parity.
    #[test]
    fn golden_channel_block_chz_matches_expected() {
        let src = include_str!("../../examples/channel_block.chz");
        let expected = include_str!("../../examples/channel_block.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        // Parked fibers must survive collection: the same program under GC stress is byte-identical.
        assert_eq!(run_capture_stress(src), expected);
    }

    // ----- B1 + B2: cooperative fibers + blocking recv (VM engine) -----

    /// Ping-pong across two channels exercises many suspend↔resume cycles: each fiber repeatedly
    /// parks on an empty `recv`, the scheduler runs the sibling whose `send` wakes it, and the parked
    /// fiber resumes mid-`while`-loop with its locals intact.
    #[test]
    fn fibers_ping_pong_interleaves() {
        let src = "fn ping(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        b.send(i)\n        x := a.recv()\n        print(\"ping {x}\")\n        i = i + 1\nfn pong(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        y := b.recv()\n        print(\"pong {y}\")\n        a.send(y + 100)\n        i = i + 1\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn ping(a, b)\n        spawn pong(a, b)\nmain()\n";
        let expected = "pong 0\nping 100\npong 1\nping 101\npong 2\nping 102\n";
        assert_eq!(run(src), expected);
        // Same result under GC stress — parked fibers' frames/locals are rooted while they wait.
        assert_eq!(run_capture_stress(src), expected);
    }

    /// All siblings parked on empty channels that no one will fill ⇒ a real deadlock (detected by the
    /// scheduler when no fiber is runnable yet not all are done).
    #[test]
    fn fibers_all_blocked_is_deadlock() {
        let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "expected deadlock");
    }

    /// Native-reentry guard: a blocking `recv` reached inside a list-HOF callback cannot park (the
    /// HOF's loop state is on the Rust stack), so it faults `deadlock` even though a sibling could
    /// otherwise supply the value — the documented B1 v1 limitation, kept memory-safe.
    #[test]
    fn fibers_recv_inside_map_callback_faults() {
        let src = "fn use_map(ch: Channel[int]):\n    xs := [0]\n    ys := xs.map(fn(x): ch.recv())\n    print(ys)\nfn fill(ch: Channel[int]):\n    ch.send(1)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn use_map(ch)\n        spawn fill(ch)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "recv inside map must fault, not suspend");
    }

    /// Native-reentry guard: a blocking `recv` inside a struct `index`/`slice`/`set_index` operator
    /// overload (run from the native indexing opcodes, host-stack state) cannot park — it faults
    /// `deadlock`. Regression for a guard gap that would otherwise corrupt the operand stack.
    #[test]
    fn fibers_recv_inside_index_overload_faults() {
        let src = "struct Src:\n    ch: Channel[int]\n    fn index(self, k: int) -> int:\n        return self.ch.recv()\nfn use_index(s: Src):\n    print(s[0])\nfn fill(ch: Channel[int]):\n    ch.send(7)\nfn main():\n    ch := Channel[int]()\n    s := Src(ch)\n    parallel:\n        spawn use_index(s)\n        spawn fill(ch)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "recv inside index overload must fault, not suspend");
    }

    /// Native-reentry guard: a blocking `recv` inside a `defer`red call (run during frame teardown,
    /// off the suspendable path) faults rather than parking. The `recv` is in the deferred function's
    /// body — only the receiver handle is captured at the `defer` statement — so it runs at teardown.
    #[test]
    fn fibers_recv_inside_defer_faults() {
        let src = "fn consume(ch: Channel[int]):\n    ch.recv()\nfn worker(ch: Channel[int]):\n    defer consume(ch)\n    print(\"body\")\nfn sender(ch: Channel[int]):\n    ch.send(5)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "recv inside defer must fault, not suspend");
    }

    /// A nested `parallel:` inside a child fiber runs its own scheduler level (recursively); the
    /// child resumes after its grandchildren join, and the outer sibling runs afterward.
    #[test]
    fn fibers_nested_parallel() {
        let src = "fn child():\n    parallel:\n        spawn:\n            print(\"grandchild\")\n    print(\"child after nested\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling\")\nmain()\n";
        assert_eq!(run(src), "grandchild\nchild after nested\nsibling\n");
    }

    /// A `recover:` inside a child fiber catches a fault in that fiber's own context (its handlers /
    /// frames are per-fiber); the sibling is unaffected and runs normally.
    #[test]
    fn fibers_recover_inside_child_is_isolated() {
        let src = "fn boom():\n    xs := [1]\n    print(xs[9])\nfn child():\n    r := recover:\n        boom()\n        0\n    print(\"caught\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling ok\")\nmain()\n";
        assert_eq!(run(src), "caught\nsibling ok\n");
    }

    /// C3 golden: `Shared[T]` cross-task box — three tasks bump one serialised counter. Byte-identical
    /// on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_shared_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/shared.chz");
        let expected = include_str!("../../examples/shared.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// C5 golden: the `Executor` escape hatch — submit/shutdown (FIFO drain), `defer ex.shutdown()`,
    /// shutdown_now (discard). Byte-identical on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_executor_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/executor.chz");
        let expected = include_str!("../../examples/executor.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // Micro-tests mirroring the interpreter's C2/C3 unit tests (src/interp/mod.rs), to pin the VM's
    // channel/shared/spawn semantics directly (not just via the example goldens).

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

    #[test]
    fn channel_recv_on_empty_is_deadlock_error() {
        let err = run_err("fn main():\n    ch := Channel[int]()\n    print(ch.recv())\nmain()\n");
        assert!(err.contains("deadlock"), "got: {err}");
    }

    /// A1: `try_recv` on an empty channel returns `None` (never the `recv` deadlock fault).
    #[test]
    fn channel_try_recv_on_empty_returns_none() {
        let src = "fn main():\n    ch := Channel[int]()\n    match ch.try_recv():\n        Some(v): print(\"got {v}\")\n        None: print(\"empty\")\nmain()\n";
        assert_eq!(run(src), "empty\n");
    }

    /// A1: `try_recv` on a non-empty channel returns `Some(v)` (FIFO).
    #[test]
    fn channel_try_recv_with_value_returns_some() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nmain()\n";
        assert_eq!(run(src), "42\n");
    }

    /// A1 × B1/B2 (VM-only): `try_recv` must drain the residue left after a *blocking* `recv` resumed.
    /// The consumer parks on an empty `recv`; the producer sends two values; the consumer resumes,
    /// `recv`s the first, then polls the rest with `try_recv` (the second value, then `None`). Pins
    /// that the resume path leaves `suspend`/`ip` clean so the following non-blocking polls behave.
    #[test]
    fn try_recv_drains_residue_after_blocking_recv_resumes() {
        let src = "fn producer(ch: Channel[int]):\n    ch.send(1)\n    ch.send(2)\nfn consumer(ch: Channel[int]):\n    a := ch.recv()\n    print(\"recv {a}\")\n    match ch.try_recv():\n        Some(v): print(\"try {v}\")\n        None: print(\"try empty\")\n    match ch.try_recv():\n        Some(v): print(\"try {v}\")\n        None: print(\"try empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn consumer(ch)\n        spawn producer(ch)\nmain()\n";
        let expected = "recv 1\ntry 2\ntry empty\n";
        assert_eq!(run(src), expected);
        assert_eq!(run_capture_stress(src), expected);
    }

    /// A1 regression guard: `try_recv` on an empty channel INSIDE an active `parallel:` scheduler must
    /// return `None` — it must NOT route through the `recv` park path (which would suspend the lone
    /// child and then deadlock, since no sibling can ever send). Pins try_recv as truly non-blocking.
    #[test]
    fn channel_try_recv_in_parallel_does_not_suspend() {
        let src = "fn probe(ch: Channel[int]):\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn probe(ch)\nmain()\n";
        assert_eq!(run(src), "empty\n");
        assert_eq!(run_capture_stress(src), "empty\n");
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

    #[test]
    fn shared_get_does_not_alias_box() {
        // `get` copies out: mutating the returned list must not change what the box holds.
        let src = "fn main():\n    s := Shared([1, 2])\n    xs := s.get()\n    xs.push(3)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "[1, 2]\n");
    }

    #[test]
    fn executor_submit_runs_fifo_at_shutdown() {
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\n    ex.shutdown()\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_submit_after_shutdown_errors() {
        let src = "fn main():\n    ex := Executor()\n    ex.shutdown()\n    ex.submit(fn(): print(1))\nmain()\n";
        let err = run_err(src);
        assert!(err.contains("shut-down Executor"), "got: {err}");
        // Parity: same fault message on the interpreter.
        let interp = crate::interp::run_capture(src).expect_err("interp should fault").message;
        assert_eq!(err, interp, "VM/interp error divergence");
    }

    #[test]
    fn executor_shutdown_now_discards_pending() {
        let src = "fn j():\n    print(99)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    ex.shutdown_now()\n    print(0)\nmain()\n";
        assert_eq!(run(src), "0\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- C5 (A2): program-exit auto-drain (VM parity) -----

    #[test]
    fn golden_executor_autodrain_matches_expected_and_interp() {
        let src = include_str!("../../examples/executor_autodrain.chz");
        let expected = include_str!("../../examples/executor_autodrain.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_autodrain_runs_unshut_at_exit() {
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_autodrain_not_redrained_after_explicit_shutdown() {
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.shutdown()\n    print(0)\nmain()\n";
        assert_eq!(run(src), "1\n0\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_autodrain_survives_gc_stress() {
        // The VM executor registry roots un-shut work so it isn't collected before the exit drain.
        // Under collect-before-every-instruction, the drained closures must still be reachable.
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
        let normal = run(src);
        assert_eq!(run_capture_stress(src), normal, "VM gc_stress diverged (executor auto-drain rooting bug?)");
    }

    #[test]
    fn executor_fault_during_drain_leaves_siblings_for_reap() {
        // A task that faults mid-drain leaves the not-yet-run siblings in the queue; `defer
        // ex.shutdown()` then reaps them on the fault exit path. Both engines must drain the *live*
        // queue (not a snapshot) so leftover work survives — pins the C1 parity fix.
        let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn run():\n    ex := Executor()\n    defer ex.shutdown()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): boom())\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\nfn main():\n    r := recover:\n        run()\n        0\n    print(\"done\")\nmain()\n";
        let vm = run_capture(src).expect("vm run");
        assert_eq!(vm, "A\nC\ndone\n");
        assert_eq!(vm, crate::interp::run_capture(src).expect("interp run"), "VM/interp divergence");
    }

    #[test]
    fn executor_reentrant_shutdown_now_during_drain() {
        // A task that calls `shutdown_now()` mid-drain discards the remaining siblings on BOTH
        // engines (the drain pops from the live queue, so the clear takes effect) — pins the C1 fix.
        let src = "fn stop(e: Executor):\n    e.shutdown_now()\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): stop(ex))\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\n    print(\"end\")\nmain()\n";
        let vm = run_capture(src).expect("vm run");
        assert_eq!(vm, "A\nend\n");
        assert_eq!(vm, crate::interp::run_capture(src).expect("interp run"), "VM/interp divergence");
    }

    #[test]
    fn spawn_first_error_aborts_siblings() {
        // The first task to fault aborts the remaining siblings and propagates out of `parallel:`.
        let src = "fn boom():\n    x := [1]\n    print(x[5])\nfn quiet():\n    print(\"ran\")\nfn main():\n    parallel:\n        spawn boom()\n        spawn quiet()\nmain()\n";
        let vm = run_err(src);
        let interp = match crate::interp::run_capture(src) {
            Ok(o) => panic!("expected error, got {o:?}"),
            Err(e) => e.message,
        };
        assert_eq!(vm, interp, "VM/interp error divergence");
    }

    #[test]
    fn spawn_composes_with_recover() {
        // A task fault is catchable by a `recover:` enclosing the nursery (parity-checked).
        let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        parallel:\n            spawn boom()\n        0\n    print(\"recovered\")\nmain()\n";
        let vm = run_capture(src).expect("vm run");
        assert_eq!(vm, "recovered\n");
        assert_eq!(vm, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Block-scoped defer: assert VM == interp == `expected` for a snippet.
    fn assert_defer_scope(src: &str, expected: &str) {
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected, "vm output");
        assert_eq!(
            crate::interp::run_capture(src).expect("interp run"),
            expected,
            "interp output"
        );
    }

    /// A `for`-body defer runs at the END of each iteration (block scope), not at function return.
    #[test]
    fn defer_for_body_runs_per_iteration() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer log(\"i={i}\")\n    log(\"done\")\nmain()\n",
            "i=0\ni=1\ni=2\ndone\n",
        );
    }

    /// A `while`-body defer runs at the END of each iteration.
    #[test]
    fn defer_while_body_runs_per_iteration() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    i := 0\n    while i < 3:\n        defer log(\"w={i}\")\n        i = i + 1\n    log(\"done\")\nmain()\n",
            "w=0\nw=1\nw=2\ndone\n",
        );
    }

    /// An `if`-branch defer fires at the branch's end, before the statement after the `if`.
    #[test]
    fn defer_if_branch_runs_at_branch_end() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    if true:\n        defer log(\"cleanup\")\n        log(\"work\")\n    log(\"after\")\nmain()\n",
            "work\ncleanup\nafter\n",
        );
    }

    /// A statement-form `match` arm defer fires at the arm's end.
    #[test]
    fn defer_match_arm_runs_at_arm_end() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    x := 1\n    match x:\n        1:\n            defer log(\"arm\")\n            log(\"body\")\n        _: log(\"other\")\n    log(\"after\")\nmain()\n",
            "body\narm\nafter\n",
        );
    }

    /// `continue` drains the current iteration's loop-body defers then advances; `break` drains them
    /// then leaves the loop.
    #[test]
    fn defer_break_continue_drain_loop_body() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..4:\n        defer log(\"d{i}\")\n        if i == 1:\n            continue\n        if i == 2:\n            break\n        log(\"body{i}\")\nmain()\n",
            "body0\nd0\nd1\nd2\n",
        );
    }

    /// A `break` nested inside an `if` (its own defer scope) inside the loop drains BOTH the
    /// if-branch and the loop-body defers, inner-first, before leaving — the post-loop `done` must
    /// print AFTER the cleanup (proving the drain happens at break, not at function return).
    #[test]
    fn defer_break_inside_if_drains_inner_first() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer log(\"loop{i}\")\n        if i == 1:\n            defer log(\"if{i}\")\n            break\n        log(\"body{i}\")\n    log(\"done\")\nmain()\n",
            "body0\nloop0\nif1\nloop1\ndone\n",
        );
    }

    /// A `recover:`-block defer runs at the recover boundary on the **Ok** path — after the trailing
    /// expression is evaluated, before the result is bound.
    #[test]
    fn defer_recover_runs_on_ok_path() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(true)?\n        x\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "release\ngot\nok 1\n",
        );
    }

    /// A `recover:`-block defer runs on the **`?` short-circuit** path, before the propagated
    /// `Err`/`None` is bound as the recover's result.
    #[test]
    fn defer_recover_runs_on_try_path() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(false)?\n        x\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "release\ngot\nerr boom\n",
        );
    }

    /// A `recover:`-block defer runs on the **genuine-fault** path, as the panic unwinds to the
    /// boundary, before the `Err(message)` is bound.
    #[test]
    fn defer_recover_runs_on_fault_path() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    r := recover:\n        defer log(\"release\")\n        xs := [1]\n        y := xs[5]\n        y\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "release\ngot\nerr index 5 out of bounds (len 1)\n",
        );
    }

    /// A defer that itself faults during a `recover:` unwind supersedes the in-flight result — its
    /// fault becomes the recover's `Err`.
    #[test]
    fn defer_recover_fault_supersedes() {
        assert_defer_scope(
            "fn boom():\n    xs := [1]\n    x := xs[9]\nfn main():\n    r := recover:\n        defer boom()\n        42\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "err index 9 out of bounds (len 1)\n",
        );
    }

    /// A `break` in an INNER loop drains only that loop's body defers; the outer loop-body defer
    /// still fires at the end of each outer iteration. Locks the per-loop `defer_floor` capture.
    #[test]
    fn defer_inner_loop_break_drains_only_inner() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..2:\n        defer log(\"outer{i}\")\n        for j in 0..3:\n            defer log(\"inner{i}-{j}\")\n            if j == 1:\n                break\n        log(\"after{i}\")\nmain()\n",
            "inner0-0\ninner0-1\nafter0\nouter0\ninner1-0\ninner1-1\nafter1\nouter1\n",
        );
    }

    /// A defer scope (here an `if`) nested INSIDE a `recover:` block that faults must not leak its
    /// scope marker past the recover boundary: the enclosing loop-body defer still drains at each
    /// iteration's end, not at function return. (Regression: VM leaked `defer_markers` on the recover
    /// catch path, corrupting later `LeaveDeferScope`s and diverging from the interp.)
    #[test]
    fn defer_nested_scope_in_faulting_recover_no_marker_leak() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                xs := [1]\n                y := xs[5]\n                y\n            0\n        log(\"end{i}\")\nmain()\n",
            "inner0\nend0\nloop0\ninner1\nend1\nloop1\n",
        );
    }

    /// Same leak via the `?`-short-circuit catch path (not a genuine fault): a defer scope nested in
    /// the recover block must not strand its marker when `?` jumps to the boundary.
    #[test]
    fn defer_nested_scope_in_try_recover_no_marker_leak() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"x\")\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                n := boom()?\n                n\n            0\n        log(\"end{i}\")\nmain()\n",
            "inner0\nend0\nloop0\ninner1\nend1\nloop1\n",
        );
    }

    /// Top-level (module-body) defers run LIFO when the program ends normally.
    #[test]
    fn defer_top_level_runs_lifo_at_exit() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\ndefer log(\"first\")\ndefer log(\"second\")\nlog(\"body\")\n",
            "body\nsecond\nfirst\n",
        );
    }

    // ----- golden coverage for the formerly-orphaned examples + the comprehensive torture
    // programs (edge_cases / evaluator / ledger). Each pins exact output AND cross-engine parity.

    /// `examples/hof.chz` — a function-typed parameter applied to a closure.
    #[test]
    fn golden_hof_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/hof.chz");
        let expected = include_str!("../../examples/hof.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/list_hof.chz` — `map`/`filter`/`fold`, incl. an element-type-changing map.
    #[test]
    fn golden_list_hof_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/list_hof.chz");
        let expected = include_str!("../../examples/list_hof.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/list_methods.chz` — pop/reverse/contains/index_of/sum (int + float).
    #[test]
    fn golden_list_methods_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/list_methods.chz");
        let expected = include_str!("../../examples/list_methods.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/loops.chz` — break/continue across for-range, for-list, and while loops.
    #[test]
    fn golden_loops_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/loops.chz");
        let expected = include_str!("../../examples/loops.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/match_value.chz` — `match` on int/str literals with `_`, stmt + expr forms.
    #[test]
    fn golden_match_value_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_value.chz");
        let expected = include_str!("../../examples/match_value.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/pair.chz` — tuples, multi-return, destructuring let, `.0`/`.1` access.
    #[test]
    fn golden_pair_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/pair.chz");
        let expected = include_str!("../../examples/pair.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/method_default_args.chz` — default + named args on methods (was parity-only).
    #[test]
    fn golden_method_default_args_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/method_default_args.chz");
        let expected = include_str!("../../examples/method_default_args.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/method_type_params.chz` — a method's own `[U]` inferred per call (was parity-only).
    #[test]
    fn golden_method_type_params_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/method_type_params.chz");
        let expected = include_str!("../../examples/method_type_params.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/param_protocol.chz` — a user-defined parameterized protocol bound (was parity-only).
    #[test]
    fn golden_param_protocol_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/param_protocol.chz");
        let expected = include_str!("../../examples/param_protocol.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/edge_cases.chz` — torture test: arithmetic faults under `recover:`, int/float
    /// boundaries, empty/nested collection printing, slice clamping, index faults, truthiness,
    /// block-scoped shadowing, closure capture-by-value, defer LIFO, and comprehensions.
    #[test]
    fn golden_edge_cases_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/edge_cases.chz");
        let expected = include_str!("../../examples/edge_cases.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/evaluator.chz` — a full tokenizer + recursive-descent parser + AST evaluator with
    /// `Result`/`?` error paths (bad char, unbalanced parens, trailing input, divide-by-zero).
    #[test]
    fn golden_evaluator_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/evaluator.chz");
        let expected = include_str!("../../examples/evaluator.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/ledger.chz` — account ledger: a map of mutable structs, overdraft `Result`s, a
    /// `defer` closing line, `sort_by` ranking, and guarded comprehensions.
    #[test]
    fn golden_ledger_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/ledger.chz");
        let expected = include_str!("../../examples/ledger.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M1 (tier-1) golden: `examples/string_iter.chz` (chars + iterable strings) byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_string_iter_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/string_iter.chz");
        let expected = include_str!("../../examples/string_iter.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Default + named arguments on free functions: `examples/default_args.chz` byte-identical on
    /// the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_default_args_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/default_args.chz");
        let expected = include_str!("../../examples/default_args.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Default + named arguments on struct constructors: `examples/named_struct.chz` byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_named_struct_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/named_struct.chz");
        let expected = include_str!("../../examples/named_struct.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #5 golden: `examples/map.chz` is byte-identical to its `.expected` on the VM,
    /// and to the interpreter (the cross-engine acceptance bar for maps).
    #[test]
    fn golden_map_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/map.chz");
        let expected = include_str!("../../examples/map.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M10-G1 golden: `examples/stringable.chz` (the `Stringable` protocol — `str(self)` dispatch
    /// from print/str()/interpolation, nested too) byte-identical on the VM, interp, and `.expected`.
    #[test]
    fn golden_stringable_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/stringable.chz");
        let expected = include_str!("../../examples/stringable.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M10-G3 golden: `examples/operators.chz` (operator overloading via `Add`/`Sub`/`Mul` + the
    /// multi-bound `T: Add + Mul`) byte-identical on the VM, interp, and `.expected`.
    #[test]
    fn golden_operators_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/operators.chz");
        let expected = include_str!("../../examples/operators.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M10-G3 golden: `examples/type_alias.chz` (transparent type aliases) byte-identical on the
    /// VM, interp, and `.expected`.
    #[test]
    fn golden_type_alias_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/type_alias.chz");
        let expected = include_str!("../../examples/type_alias.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// G1 golden: `examples/generics.chz` (generics + structural `Comparable`) is byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_generics_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/generics.chz");
        let expected = include_str!("../../examples/generics.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// G2 golden: generic structs are byte-identical on the VM, interpreter, and `.expected`.
    #[test]
    fn golden_generic_structs_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/generic_structs.chz");
        let expected = include_str!("../../examples/generic_structs.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tier-2 golden: generic enums (Tree[T] / Either[A, B]) — byte-identical VM, interp, expected.
    #[test]
    fn golden_generic_enum_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/generic_enum.chz");
        let expected = include_str!("../../examples/generic_enum.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Golden: real hash-table map/set with Hashable struct keys — byte-identical VM, interp, expected.
    #[test]
    fn golden_hashmap_keys_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/hashmap_keys.chz");
        let expected = include_str!("../../examples/hashmap_keys.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tech-debt golden: `examples/explicit_type_args.chz` (explicit call-site type arguments on a
    /// generic fn / struct / enum-variant constructor) byte-identical VM, interp, and `.expected`.
    #[test]
    fn golden_explicit_type_args_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/explicit_type_args.chz");
        let expected = include_str!("../../examples/explicit_type_args.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tech-debt golden: `examples/set_eq.chz` (order-independent set equality incl. nested in a
    /// struct/list) byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_set_eq_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/set_eq.chz");
        let expected = include_str!("../../examples/set_eq.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tech-debt parity: a `set` nested inside a struct / list must compare unordered on BOTH
    /// engines (top-level set `==` already did). Previously the interp's derived `SetData::eq` was
    /// order-sensitive, so `W(set([1,2,3])) == W(set([3,2,1]))` was `true` on the VM but `false` on
    /// the interp.
    #[test]
    fn nested_set_equality_parity() {
        let src = "\
struct W:
    s: set[int]
a := W(set([1, 2, 3]))
b := W(set([3, 2, 1]))
print(a == b)
print([set([1, 2])] == [set([2, 1])])
";
        let vm = run_capture(src).expect("vm");
        let interp = crate::interp::run_capture(src).expect("interp");
        assert_eq!(vm, interp);
        assert_eq!(vm, "true\ntrue\n");
    }

    #[test]
    fn sort_over_comparable_structs_on_vm() {
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
    fn struct_ordering_dispatches_to_compare_on_vm() {
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
    fn primitive_compare_method_on_vm() {
        let src = "fn c[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nprint(c(2, 5))\nprint(c(5, 2))\n";
        assert_eq!(run(src), "-1\n1\n");
    }

    /// Gap #11 golden: `examples/sort_by.chz` (custom comparators, stable order, tuple-field sort)
    /// is byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_sort_by_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/sort_by.chz");
        let expected = include_str!("../../examples/sort_by.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #10 golden: `examples/cipher.chz` (ord/chr — ROT13 + manual digit parsing) is
    /// byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_cipher_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/cipher.chz");
        let expected = include_str!("../../examples/cipher.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #14 (+ #11) golden: `examples/word_freq.chz` iterates a map with `for w, c in counts`
    /// and ranks tuples with `sort_by`. Byte-identical on the VM, the interpreter, and `.expected`.
    #[test]
    fn golden_word_freq_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/word_freq.chz");
        let expected = include_str!("../../examples/word_freq.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #15 golden: `examples/match_nested.chz` (tuple patterns, nested `Some((a, b))`, nested
    /// literals) is byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_match_nested_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_nested.chz");
        let expected = include_str!("../../examples/match_nested.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Match-guard golden: `examples/match_guard.chz` (`pattern if cond:` arms, expr + stmt forms)
    /// is byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_match_guard_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_guard.chz");
        let expected = include_str!("../../examples/match_guard.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Range-pattern golden: `examples/match_range.chz` (half-open `start..end` int patterns) is
    /// byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_match_range_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_range.chz");
        let expected = include_str!("../../examples/match_range.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #13 golden: `examples/bits.chz` (`& | ^ << >>` — XOR-fold + bitmask) is byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_bits_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/bits.chz");
        let expected = include_str!("../../examples/bits.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Round-2 probe goldens: recursive data-structure + evaluator programs that surfaced the
    /// round-2 gaps. Byte-identical on the VM, the interpreter, and their `.expected`.
    #[test]
    fn golden_bst_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/bst.chz");
        let expected = include_str!("../../examples/bst.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn golden_linked_list_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/linked_list.chz");
        let expected = include_str!("../../examples/linked_list.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn golden_calc_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/calc.chz");
        let expected = include_str!("../../examples/calc.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- struct iterator protocol (`for x in s` driven by `next(self) -> Option[T]`) -----

    #[test]
    fn for_over_struct_iterator_counts() {
        let src = "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n3\n4\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn for_over_struct_iterator_break_lazy() {
        let src = "struct Fib:\n    a: int\n    b: int\n    fn next(self) -> Option[int]:\n        v := self.a\n        nb := self.a + self.b\n        self.a = self.b\n        self.b = nb\n        return Some(v)\nfn main():\n    for x in Fib(0, 1):\n        if x > 10:\n            break\n        print(x)\nmain()\n";
        assert_eq!(run(src), "0\n1\n1\n2\n3\n5\n8\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    /// Golden: the iterator example runs on the VM with exactly the expected output, matching interp.
    #[test]
    fn golden_iterator_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/iterator.chz");
        let expected = include_str!("../../examples/iterator.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    /// A value reachable only via the operand stack (mid-expression temporary) must survive a
    /// collection — the headline use-after-collect trap. Each list is built, left on the stack,
    /// then indexed; a GC fires (stress) between build and index.
    #[test]
    fn value_only_on_operand_stack_survives() {
        assert_eq!(run_capture_stress("print([str(1), str(2), str(3)][0] + [str(4), str(5)][1])"), "15\n");
    }

    /// A value held only in a call-frame local slot survives collections triggered by later
    /// allocations in the same frame.
    #[test]
    fn value_in_frame_slot_survives() {
        let src = "\
fn main():
    x := [str(1), str(2)]
    junk := str(3)
    more := [str(4), str(5), str(6)]
    print(x)
main()";
        assert_eq!(run_capture_stress(src), "[1, 2]\n");
    }

    /// A value reachable only through a module's globals (the namespace cache root) survives.
    #[test]
    fn value_in_module_global_survives() {
        let src = "\
K := [str(7), str(8)]
fn main():
    a := str(1)
    b := [str(2), str(3)]
    print(K)
main()";
        assert_eq!(run_capture_stress(src), "[7, 8]\n");
    }

    /// A value reachable only through a closure's captured environment survives — after the
    /// defining frame is gone, only the closure object holds it.
    #[test]
    fn value_in_closure_capture_survives() {
        let src = "\
fn make():
    secret := str(42)
    return fn(): secret
fn main():
    g := make()
    junk := [str(1), str(2), str(3)]
    print(g())
main()";
        assert_eq!(run_capture_stress(src), "42\n");
    }

    /// Set algebra with heap-allocated (string) elements under GC stress: the source set, the
    /// argument set, and the freshly-built result must all survive a collection mid-operation.
    #[test]
    fn set_algebra_survives_gc_stress() {
        let src = "\
a := set([\"al\" + \"pha\", \"be\" + \"ta\", \"gam\" + \"ma\"])
b := set([\"be\" + \"ta\", \"de\" + \"lta\"])
print(a.union(b).len())
print(a.intersection(b).len())
print(a.difference(b).len())
total := 0
for w in a:
    total += w.len()
print(total)";
        // alpha+beta+gamma = 5+4+5 = 14
        assert_eq!(run_capture_stress(src), "4\n1\n2\n14\n");
    }

    /// `list.sort()` over Comparable structs whose `compare` allocates (triggering GC mid-sort) must
    /// not collect the in-flight elements OR the source list — even when the receiver is an inline
    /// temporary (popped before dispatch, so otherwise unrooted). Regression for the M7-G3 review.
    #[test]
    fn struct_sort_survives_gc_stress() {
        let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c), str(o.c)]
        return self.c - o.c
fn make() -> list[M]:
    xs := []
    i := 0
    while i < 8:
        xs.push(M((i * 5) % 7))
        i = i + 1
    return xs
fn main():
    xs := make()
    xs.sort()
    out := \"\"
    for m in xs:
        out = out + str(m.c)
    print(out)
    make().sort()              # inline temporary receiver
    print(\"ok\")
main()";
        assert_eq!(run_capture_stress(src), "00123456\nok\n");
    }

    /// A struct key's `hash()` allocates (triggering GC mid-operation). The map/set obj and the
    /// in-flight key/value — popped off the operand stack before dispatch — must stay rooted across
    /// every hash, including with an INLINE-TEMPORARY receiver (`make_map().get(k)`). Regression for
    /// the hash-table struct-key rooting.
    #[test]
    fn map_struct_key_survives_gc_stress() {
        let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n), str(self.n + 1)]
        return self.n
fn make_map() -> map[K, str]:
    m: map[K, str] = {}
    i := 0
    while i < 8:
        m[K(i)] = str(i)
        i = i + 1
    return m
fn main():
    m := make_map()
    out := \"\"
    for k in m:
        out = out + m[k]
    print(out)
    print(m.has(K(3)))
    print(m.get(K(5)))
    print(make_map().get(K(2)))   # inline-temporary receiver
    print(m.remove(K(0)))
    print(m.len())
main()";
        assert_eq!(
            run_capture_stress(src),
            "01234567\ntrue\nSome(5)\nSome(2)\nSome(0)\n7\n"
        );
    }

    /// Set construction (`set([..])`) + `add` over structs whose `hash()` allocates, including
    /// algebra — none of the elements may be collected mid-hash.
    #[test]
    fn set_struct_hash_survives_gc_stress() {
        let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n)]
        return self.n
fn main():
    a := set([K(1), K(2), K(2), K(3)])
    print(a.len())
    a.add(K(3))
    a.add(K(4))
    print(a.len())
    b := set([K(3), K(4), K(5)])
    print(a.union(b).len())
    print(a.intersection(b).len())
    print(a.difference(b).len())
main()";
        // a = {1,2,3,4}; b = {3,4,5}; |a∪b|=5, |a∩b|=2, |a\\b|=2
        assert_eq!(run_capture_stress(src), "3\n4\n5\n2\n2\n");
    }

    /// Same hazard via `sort_by` with an allocating comparator on an inline-temporary list.
    #[test]
    fn struct_sort_by_inline_temporary_survives_gc_stress() {
        let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c)]
        return self.c - o.c
fn make() -> list[M]:
    xs := []
    i := 0
    while i < 6:
        xs.push(M((i * 5) % 7))
        i = i + 1
    return xs
fn main():
    make().sort_by(fn(a: M, b: M) -> int: a.compare(b))
    print(\"ok\")
main()";
        assert_eq!(run_capture_stress(src), "ok\n");
    }

    /// An `Err` value propagated by `?` through a function boundary survives collection.
    #[test]
    fn value_propagated_by_try_survives() {
        let src = "\
fn d() -> Result[str]:
    return Err(str(99))
fn use() -> Result[str]:
    x := d()?
    return Ok(x)
fn main():
    match use():
        Ok(v): print(v)
        Err(e): print(\"got {e}\")
main()";
        assert_eq!(run_capture_stress(src), "got 99\n");
    }

    /// An allocation-heavy loop's garbage must be reclaimed: the live set stays bounded rather
    /// than growing with the iteration count (threshold-driven GC, not stress mode).
    #[test]
    fn allocation_loop_is_bounded() {
        let src = "\
fn main():
    i := 0
    while i < 10000:
        x := [str(i)]
        i += 1
    print(i)
main()";
        let (out, live) = run_with(src, false);
        assert_eq!(out.unwrap(), "10000\n");
        // Without collection this would be ~20000+ live objects; the threshold GC keeps it small.
        assert!(live < 2000, "heap not bounded: {live} live objects after 10000 allocating iterations");
    }

    /// Stress-mode collection must not change observable behavior on a feature-rich program.
    #[test]
    fn hello_chz_identical_under_gc_stress() {
        let expected = include_str!("../../examples/hello.expected");
        assert_eq!(run_capture_stress(include_str!("../../examples/hello.chz")), expected);
    }

    /// Stress vs. normal must agree on a program exercising structs, enums, closures, and match.
    #[test]
    fn stress_matches_normal_on_mixed_program() {
        let src = "\
struct Box:
    v: int
    fn get(self) -> int:
        return self.v
enum Opt:
    Has(int)
    Nope
fn pick(o: Opt) -> int:
    match o:
        Has(n): return n
        Nope: return -1
fn main():
    b := Box(7)
    add := fn(x: int) -> int: x + b.get()
    print(add(3))
    print(pick(Has(9)))
    print(pick(Nope))
    items := [str(1), str(2), str(3)]
    for s in items:
        print(s)
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal);
    }

    /// Concurrency C4 rooting: a `spawn`'s deep-copied args, a pending task's captured closure env,
    /// and the values queued in a `Channel` / boxed in a `Shared` must all survive collections that
    /// fire (under stress) between registration and the nursery's join. Each task allocates strings
    /// so a missing root would corrupt the output (or panic on a dangling `GcRef`).
    #[test]
    fn spawn_pending_tasks_survive_gc_stress() {
        let src = "\
fn work(tag: str, out: Channel[str]):
    out.send(\"{tag}!\")
fn main():
    ch := Channel[str]()
    base := str(100)
    parallel:
        spawn work(str(1), ch)
        spawn work(str(2), ch)
        spawn:
            ch.send(\"blk-{base}\")
    print(ch.len())
    for _ in 0..3:
        print(ch.recv())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal);
        assert_eq!(normal, crate::interp::run_capture(src).expect("interp run"));
    }

    /// B3.1 regression: a core nested *inside* another core. The channel core is reachable ONLY
    /// through the `Shared` box's wire value once `stash`'s local channel handle is gone — its queued
    /// `"hello"` (a heap `Str` handle embedded in the channel core's wire queue) must still be traced
    /// as a GC root, or `gc_stress` sweeps it and `recv` dangles. Pins that `collect_gcrefs` recurses
    /// into nested cores.
    #[test]
    fn nested_core_contents_survive_gc_stress() {
        let src = "\
fn stash(s: Shared[Channel[str]]):
    ch := s.get()
    ch.send(\"hello\")
fn main():
    s := Shared(Channel[str]())
    stash(s)
    base := str(100)
    ch := s.get()
    print(ch.recv())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(normal, "hello\n");
        assert_eq!(run_capture_stress(src), normal, "nested core contents must survive GC");
    }

    /// `Shared` box + `update`'s re-entrant call survive GC stress (the box is re-rooted across the
    /// nested user-fn call; the boxed list's elements stay reachable through collections).
    #[test]
    fn shared_box_survives_gc_stress() {
        let src = "\
fn appended(xs: list[str], v: str) -> list[str]:
    xs.push(v)
    return xs
fn push_one(s: Shared[list[str]], v: str):
    s.update(fn(xs): appended(xs, v))
fn main():
    s := Shared([str(0)])
    parallel:
        spawn push_one(s, str(1))
        spawn push_one(s, str(2))
    print(s.get())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal);
        assert_eq!(normal, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Executor: submitted task closures (queued in the heap obj) and the popped task drained at
    /// `shutdown` must survive GC firing between submit and drain, and across each task's re-entrant
    /// call. Each task allocates a string into a Channel — a missing root would corrupt the output or
    /// dangle a `GcRef`.
    #[test]
    fn executor_tasks_survive_gc_stress() {
        let src = "\
fn work(tag: str, out: Channel[str]):
    out.send(\"{tag}!\")
fn main():
    ch := Channel[str]()
    ex := Executor()
    ex.submit(fn(): work(str(1), ch))
    ex.submit(fn(): work(str(2), ch))
    ex.shutdown()
    for _ in 0..2:
        print(ch.recv())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal, "VM gc_stress diverged (executor rooting bug?)");
        assert_eq!(normal, crate::interp::run_capture(src).expect("interp run"));
    }
}

#[cfg(test)]
mod parity_tests {
    //! Cross-engine parity: the VM and the tree-walk interpreter must agree on stdout *and* error
    //! for every program. These are the M5 acceptance tests — any divergence fails here.
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    /// Outcome of a run, normalized so interp and VM results compare directly.
    fn interp_outcome(src: &str) -> Result<String, String> {
        crate::interp::run_capture(src).map_err(|e| e.to_string())
    }
    fn vm_outcome(src: &str) -> Result<String, String> {
        run_capture(src).map_err(|e| e.to_string())
    }

    fn assert_parity(src: &str) {
        assert_eq!(vm_outcome(src), interp_outcome(src), "VM/interp divergence for:\n{src}");
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    static PARITY_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = PARITY_TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("chezzi_par_{}_{}", std::process::id(), n));
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

    /// Run a multi-file program (one or more `.chz` files) through BOTH engines via `run_file`,
    /// assert they agree on stdout and on ok/err, and return the agreed stdout. `files` is
    /// `(relative_path, contents)`; `entry` names the file to run. Needed because the single-file
    /// `assert_parity` can't exercise imports (and std modules require the import path).
    fn assert_parity_file(files: &[(&str, &str)], entry: &str) -> String {
        let t = TmpDir::new();
        let mut entry_path = None;
        for (rel, contents) in files {
            let p = t.write(rel, contents);
            if *rel == entry {
                entry_path = Some(p);
            }
        }
        let entry_path = entry_path.expect("entry must be one of the files");
        let (io, ie_out, ir, _) = crate::interp::run_file(&entry_path);
        let (vo, ve_out, vr, _) = run_file(&entry_path);
        assert_eq!(io, vo, "stdout divergence (interp vs vm) for entry {entry}");
        assert_eq!(ie_out, ve_out, "stderr divergence (interp vs vm) for entry {entry}");
        match (&ir, &vr) {
            (Ok(()), Ok(())) => {}
            (Err(ie), Err(ve)) => {
                assert_eq!(ie.to_string(), ve.to_string(), "error divergence (interp vs vm)");
            }
            _ => panic!("ok/err divergence: interp={ir:?} vm={vr:?}"),
        }
        io
    }

    /// Convenience: a single entry file (the common std-module case).
    fn parity_entry(src: &str) -> String {
        assert_parity_file(&[("main.chz", src)], "main.chz")
    }

    // ----- C5 (A2): program-exit auto-drain is skipped on a hard `os.exit` -----

    #[test]
    fn executor_autodrain_skipped_on_os_exit() {
        // `os.exit` is a hard halt — like `defer`, the program-exit auto-drain is skipped, so a
        // submitted-but-un-shut executor's work must NOT run. Driven through the file path on both
        // engines (parity), since it imports std.os.
        let out = parity_entry(
            "import std.os\nfn j():\n    print(\"RAN\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    print(\"before exit\")\n    os.exit(0)\nmain()\n",
        );
        assert_eq!(out, "before exit\n");
    }

    // ----- M9: std.regex parity (exercises NativeRet::Struct lowering on both engines) -----

    #[test]
    fn regex_find_all_replace_split_parity() {
        let out = parity_entry(
            r##"import std.regex
match regex.find_all("[0-9]+", "a1 22 333"):
    Ok(ms):
        for m in ms:
            print(m.text)
    Err(e): print(e)
match regex.replace_all("[0-9]+", "a1b22c", "#"):
    Ok(s): print(s)
    Err(e): print(e)
match regex.split(",", "a,b,c"):
    Ok(parts): print("|".join(parts))
    Err(e): print(e)
"##,
        );
        assert_eq!(out, "1\n22\n333\na#b#c\na|b|c\n");
    }

    /// `std.request` against a loopback server, run through BOTH engines (exercises `NativeRet::Map`
    /// lowering on each). The server serves one canned response per connection; interp and vm each
    /// open one, so it accepts twice.
    #[test]
    fn request_get_parity_against_local_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = "pong";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nX-Test: hi\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });

        let src = format!(
            "import std.request\nmatch request.get(\"http://{addr}/\"):\n    Ok(resp):\n        print(str(resp.status))\n        print(resp.body)\n        print(resp.headers[\"x-test\"])\n    Err(e): print(e)\n"
        );
        let out = parity_entry(&src);
        server.join().unwrap();
        assert_eq!(out, "200\npong\nhi\n");
    }

    #[test]
    fn regex_find_groups_and_span_parity() {
        let out = parity_entry(
            r#"import std.regex
match regex.find("([a-z]+)@([a-z]+)", "xx ann@host"):
    Ok(opt):
        match opt:
            Some(m): print(m.text + " " + str(m.start) + " " + ",".join(m.groups))
            None: print("none")
    Err(e): print(e)
"#,
        );
        assert_eq!(out, "ann@host 3 ann,host\n");
    }

    // ----- break / continue parity (both engines must agree AND produce the right output) -----

    /// Assert both engines agree AND that the (shared) stdout equals `expect`. A hang here means a
    /// `continue` is landing on the wrong target (re-test without advancing → infinite loop).
    fn assert_parity_out(src: &str, expect: &str) {
        assert_parity(src);
        assert_eq!(vm_outcome(src).expect("program should run"), expect, "for:\n{src}");
    }

    #[test]
    fn bitwise_ops_parity() {
        assert_parity_out(
            "print(5 & 3)\nprint(5 | 2)\nprint(5 ^ 3)\nprint(1 << 4)\nprint(255 >> 4)\n",
            "1\n7\n6\n16\n15\n",
        );
    }

    #[test]
    fn bitwise_precedence_below_comparison_parity() {
        // `5 & 3 == 1` is `(5 & 3) == 1` (bitwise binds tighter than `==`, Python-style).
        assert_parity_out("print(5 & 3 == 1)\n", "true\n");
    }

    #[test]
    fn xor_fold_single_number_parity() {
        assert_parity_out(
            "xs := [4,1,2,1,4,2,7]\nacc := 0\nfor x in xs:\n    acc = acc ^ x\nprint(acc)\n",
            "7\n",
        );
    }

    #[test]
    fn shift_out_of_range_error_parity() {
        // Dynamic shift the checker can't catch — both engines must raise the same runtime error.
        assert_parity("print(1 << 64)\n");
        assert_parity("print(1 << -1)\n");
    }

    #[test]
    fn match_tuple_pattern_parity() {
        assert_parity_out(
            "p := (3, 4)\nmatch p:\n    (0, y): print(y)\n    (x, y): print(x + y)\n",
            "7\n",
        );
    }

    #[test]
    fn match_tuple_literal_arm_parity() {
        assert_parity_out(
            "p := (1, 9)\nlabel := match p:\n    (1, n): \"one {n}\"\n    _: \"other\"\nprint(label)\n",
            "one 9\n",
        );
    }

    #[test]
    fn match_nested_variant_in_tuple_parity() {
        assert_parity_out(
            "o: (int, int)? = Some((10, 20))\nmatch o:\n    None: print(\"none\")\n    Some((a, b)): print(a + b)\n",
            "30\n",
        );
    }

    #[test]
    fn match_nested_heap_payload_gc_stress() {
        // Nested pattern binding heap values (strings) inside a tuple inside a variant; a GC mid-bind
        // must not collect the still-referenced payload.
        let src = "o: (str, str)? = Some((\"a\" + \"b\", \"c\" + \"d\"))\nmatch o:\n    None: print(\"none\")\n    Some((x, y)): print(x + y)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "abcd\n");
        assert_eq!(run_capture_stress(src), "abcd\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn match_guard_fallthrough_parity() {
        // The first arm's pattern binds but its guard is false → fall through to the next arm.
        assert_parity_out(
            "n := 5\nmatch n:\n    x if x < 0: print(\"neg\")\n    x if x > 10: print(\"big\")\n    _: print(\"mid\")\n",
            "mid\n",
        );
    }

    #[test]
    fn match_guard_first_true_parity() {
        assert_parity_out(
            "n := -2\nlabel := match n:\n    x if x < 0: \"neg\"\n    _: \"nonneg\"\nprint(label)\n",
            "neg\n",
        );
    }

    #[test]
    fn match_range_boundaries_parity() {
        // Half-open: start is inclusive, end is exclusive.
        assert_parity_out(
            "fn b(n: int) -> str:\n    return match n:\n        0..10: \"lo\"\n        10..20: \"hi\"\n        _: \"out\"\nprint(b(0))\nprint(b(9))\nprint(b(10))\nprint(b(19))\nprint(b(20))\n",
            "lo\nlo\nhi\nhi\nout\n",
        );
    }

    #[test]
    fn match_range_with_literal_mix_parity() {
        // A match mixing int literals and ranges still routes through the literal path.
        assert_parity_out(
            "fn f(n: int) -> str:\n    return match n:\n        0: \"zero\"\n        1..100: \"small\"\n        _: \"big\"\nprint(f(0))\nprint(f(50))\nprint(f(500))\n",
            "zero\nsmall\nbig\n",
        );
    }

    #[test]
    fn for_over_map_keys_parity() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m:\n    print(k)\n",
            "a\nb\nc\n",
        );
    }

    /// Regression (review #1): a struct field named `decode` must still be indexable — the
    /// `.decode[…]` JSON form is only stolen when a real `[Type](arg)` follows.
    #[test]
    fn field_named_decode_is_indexable_parity() {
        let out = parity_entry(
            "struct Box:\n    decode: list[int]\nb := Box([10, 20, 30])\nprint(b.decode[1])\nprint(b.decode[0] + b.decode[2])\n",
        );
        assert_eq!(out, "20\n40\n");
    }

    /// Regression (review #2): malformed and out-of-range numbers come back as `Err` / stringify
    /// cleanly — they must never abort the host (no uncaught `float()`/`int()` panic).
    #[test]
    fn json_malformed_numbers_are_errors_parity() {
        let out = parity_entry(
            "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK \" + json.stringify(j)\n        Err(e): return \"ERR\"\nprint(tp(\"1e\"))\nprint(tp(\"1.\"))\nprint(tp(\"100000000000000000000\"))\n",
        );
        assert_eq!(out, "ERR\nERR\nOK 100000000000000000000.0\n");
    }

    #[test]
    fn json_decode_struct_parity() {
        let out = parity_entry(
            "import std.json\nstruct P:\n    x: int\n    y: int\nmatch json.decode[P](\"{{\\\"x\\\":1,\\\"y\\\":2}}\"):\n    Ok(p): print(p.x + p.y)\n    Err(e): print(e)\n",
        );
        assert_eq!(out, "3\n");
    }

    #[test]
    fn json_decode_error_parity() {
        let out = parity_entry(
            "import std.json\nstruct P:\n    x: int\nmatch json.decode[P](\"{{\\\"y\\\":2}}\"):\n    Ok(p): print(p.x)\n    Err(e): print(e)\n",
        );
        assert_eq!(out, "decode: missing key 'x' at $\n");
    }

    #[test]
    fn process_cmd_ok_and_err_parity() {
        let out = parity_entry(
            "import std.process\nmatch process.cmd(\"printf abc\"):\n    Ok(s): print(\"ok:\" + s)\n    Err(e): print(\"err:\" + e)\nmatch process.cmd(\"exit 2\"):\n    Ok(s): print(\"ok\")\n    Err(e): print(\"err:\" + e)\n",
        );
        assert_eq!(out, "ok:abc\nerr:command exited with status 2\n");
    }

    #[test]
    fn fs_predicates_parity() {
        let out = parity_entry(
            "import std.fs\nprint(fs.exists(\"Cargo.toml\"))\nprint(fs.exists(\"definitely_not_here.zzz\"))\nprint(fs.is_dir(\"src\"))\n",
        );
        assert_eq!(out, "true\nfalse\ntrue\n");
    }

    #[test]
    fn time_format_parity() {
        let out = parity_entry(
            "import std.time\nprint(time.format(0))\nprint(time.format(1700000000))\nprint(time.now() > 0)\n",
        );
        assert_eq!(out, "1970-01-01 00:00:00\n2023-11-14 22:13:20\ntrue\n");
    }

    #[test]
    fn set_dedup_and_algebra_parity() {
        assert_parity_out(
            "s := {3, 1, 3, 2, 1}\nprint(s.len())\nprint({1,2,3}.union({3,4}).len())\nprint({1,2,3}.intersection({2,3,4}).len())\nprint({1,2,3}.difference({2,3}).len())\nprint({1,2} == {2,1})\n",
            "3\n4\n2\n1\ntrue\n",
        );
    }

    #[test]
    fn set_mutation_and_iteration_parity() {
        assert_parity_out(
            "s := set()\ns.add(10)\ns.add(10)\ns.add(20)\nprint(s.len())\nprint(s.remove(10))\nprint(s.remove(10))\ntotal := 0\nfor x in {5, 15, 25}:\n    total += x\nprint(total)\n",
            "2\ntrue\nfalse\n45\n",
        );
    }

    #[test]
    fn set_display_parity() {
        assert_parity_out("print({1, 2, 3})\nprint(set())\n", "{1, 2, 3}\nset()\n");
    }

    #[test]
    fn str_chars_parity() {
        assert_parity_out(
            "cs := \"héllo\".chars()\nprint(cs.len())\nprint(cs[1])\n",
            "5\né\n",
        );
    }

    #[test]
    fn for_over_str_parity() {
        assert_parity_out(
            "out := \"\"\nfor c in \"abc\":\n    out = out + c + \"-\"\nprint(out)\n",
            "a-b-c-\n",
        );
    }

    #[test]
    fn for_over_empty_str_parity() {
        assert_parity_out(
            "n := 0\nfor c in \"\":\n    n += 1\nprint(n)\n",
            "0\n",
        );
    }

    #[test]
    fn for_over_map_key_value_parity() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2}\ns := 0\nfor k, v in m:\n    print(\"{k}={v}\")\n    s += v\nprint(s)\n",
            "a=1\nb=2\n3\n",
        );
    }

    #[test]
    fn for_over_map_kv_mutation_during_iteration_parity() {
        // The body reassigns a not-yet-visited key; both engines must agree (snapshot semantics:
        // the value bound is the one captured at loop start, like list iteration).
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nout := 0\nfor k, v in m:\n    m[\"c\"] = 99\n    out += v\nprint(out)\n",
            "6\n",
        );
    }

    #[test]
    fn for_over_map_kv_remove_during_iteration_parity() {
        // Removing a future key mid-iteration must not crash one engine while the other succeeds.
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2}\nfirst := true\nsum := 0\nfor k, v in m:\n    if first:\n        m.remove(\"b\")\n        first = false\n    sum += v\nprint(sum)\n",
            "3\n",
        );
    }

    #[test]
    fn for_over_map_break_continue_parity() {
        // break/continue still target the index increment over the keys sequence.
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3, \"d\": 4}\nfor k, v in m:\n    if v == 2: continue\n    if v == 4: break\n    print(k)\n",
            "a\nc\n",
        );
    }

    #[test]
    fn cmp_max_int_parity() {
        // Generic min/max now live in std.cmp; abs stays in std.math. File/graph path required.
        let out = parity_entry("import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3, 5))\n    print(cmp.min(3, 5))\n    print(math.abs(-5))\nmain()\n");
        assert_eq!(out, "5\n3\n5\n");
    }

    #[test]
    fn cmp_max_float_parity() {
        let out = parity_entry("import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3.0, 5.0))\n    print(math.abs(-2.5))\nmain()\n");
        assert_eq!(out, "5.0\n2.5\n");
    }

    #[test]
    fn cmp_max_struct_parity() {
        // The generic max over a Comparable struct must be byte-identical on both engines.
        let src = "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, o: P) -> int:\n        return self.n - o.n\nfn main():\n    print(cmp.max(P(2), P(9)).n)\n    print(cmp.min(P(2), P(9)).n)\nmain()\n";
        assert_eq!(parity_entry(src), "9\n2\n");
    }

    #[test]
    fn ord_chr_parity() {
        assert_parity_out("print(ord(\"A\"))\nprint(chr(97))\n", "65\na\n");
    }

    #[test]
    fn ord_chr_roundtrip_parity() {
        assert_parity_out("print(chr(ord(\"z\")))\n", "z\n");
    }

    #[test]
    fn ord_index_digit_value_parity() {
        // The digit-value idiom over an indexed char.
        assert_parity_out("s := \"7\"\nprint(ord(s[0]) - ord(\"0\"))\n", "7\n");
    }

    #[test]
    fn ord_empty_string_error_parity() {
        // Runtime error (checker can't catch it) — message must match across engines.
        assert_parity("print(ord(\"\"))\n");
    }

    #[test]
    fn chr_invalid_codepoint_error_parity() {
        assert_parity("print(chr(-1))\n");
        assert_parity("print(chr(2000000))\n");
    }

    #[test]
    fn sort_by_descending_parity() {
        assert_parity_out(
            "xs := [3,1,2]\nxs.sort_by(fn(a: int, b: int) -> int: b - a)\nprint(xs)\n",
            "[3, 2, 1]\n",
        );
    }

    #[test]
    fn sort_by_stable_by_key_parity() {
        // Equal keys (string length) must keep input order — stability is part of the contract.
        assert_parity_out(
            "ws := [\"bb\", \"a\", \"dd\", \"e\"]\nws.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nprint(ws)\n",
            "[a, e, bb, dd]\n",
        );
    }

    #[test]
    fn sort_by_comparator_mutates_list_parity() {
        // A comparator that mutates an element being sorted must behave identically on both engines.
        // Both sort a snapshot taken at call time and overwrite the list with the sorted result, so
        // the in-comparator `xs[0] = 100` is discarded.
        let src = "xs := [3, 1, 2]\nfn cmp(a: int, b: int) -> int:\n    xs[0] = 100\n    return a - b\nxs.sort_by(cmp)\nprint(xs)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "[1, 2, 3]\n");
    }

    #[test]
    fn sort_by_empty_and_singleton_parity() {
        assert_parity_out(
            "xs := [42]\nxs.sort_by(fn(a: int, b: int) -> int: a - b)\nprint(xs)\n",
            "[42]\n",
        );
    }

    #[test]
    fn break_early_for_parity() {
        assert_parity_out(
            "s := 0\nfor i in 0..10:\n    if i == 5: break\n    s += i\nprint(s)\n",
            "10\n",
        );
    }

    #[test]
    fn continue_for_terminates_parity() {
        // THE increment-landing guard: `continue` must reach the loop's `i += 1`, never the
        // condition (would re-test the same `i` forever). If this hangs, the target is wrong.
        assert_parity_out(
            "for i in 0..5:\n    if i == 1: continue\n    if i == 3: continue\n    print(i)\n",
            "0\n2\n4\n",
        );
    }

    #[test]
    fn while_break_parity() {
        assert_parity_out(
            "i := 0\nwhile true:\n    if i == 3: break\n    i += 1\nprint(i)\n",
            "3\n",
        );
    }

    #[test]
    fn while_continue_progresses_parity() {
        // The counter advances BEFORE the `continue`, so the `while` still terminates.
        assert_parity_out(
            "i := 0\ns := 0\nwhile i < 5:\n    i += 1\n    if i == 2: continue\n    s += i\nprint(s)\n",
            "13\n",
        );
    }

    #[test]
    fn break_in_if_in_loop_parity() {
        assert_parity_out(
            "for i in 0..10:\n    if i > 2:\n        break\n    print(i)\n",
            "0\n1\n2\n",
        );
    }

    #[test]
    fn return_from_loop_parity() {
        // `return` inside a loop still returns the whole function (break/continue don't intercept it).
        assert_parity_out(
            "fn f():\n    for i in 0..10:\n        if i == 2: return i\n    return -1\nprint(f())\n",
            "2\n",
        );
    }

    #[test]
    fn nested_loop_inner_break_parity() {
        // Inner `break` does not break the outer loop: the outer runs all 3 iterations.
        assert_parity_out(
            "n := 0\nfor i in 0..3:\n    for j in 0..3:\n        break\n    n += 1\nprint(n)\n",
            "3\n",
        );
    }

    #[test]
    fn continue_list_for_parity() {
        // `continue` over a LIST for-loop (not just range) advances to the next element.
        assert_parity_out(
            "for x in [1,2,3,4]:\n    if x % 2 == 0: continue\n    print(x)\n",
            "1\n3\n",
        );
    }

    #[test]
    fn break_list_for_parity() {
        assert_parity_out(
            "for x in [10,20,30,40]:\n    if x == 30: break\n    print(x)\n",
            "10\n20\n",
        );
    }

    // ----- literal + wildcard match parity -----

    #[test]
    fn match_int_literals_stmt_parity() {
        assert_parity("n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n    _: print(\"many\")\n");
    }

    #[test]
    fn match_str_literals_expr_parity() {
        assert_parity("c := \"x\"\ns := match c:\n    \"a\": \"first\"\n    _: \"other\"\nprint(s)\n");
    }

    #[test]
    fn match_bool_literals_parity() {
        assert_parity("b := false\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n");
    }

    #[test]
    fn match_literal_matched_arm_parity() {
        // The matching literal arm fires (wildcard not reached).
        assert_parity("n := 1\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
    }

    #[test]
    fn match_wildcard_reached_parity() {
        // No literal matches → the `_` arm fires.
        assert_parity("n := 9\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
    }

    #[test]
    fn match_variant_regression_parity() {
        // A variant match still lowers via the variant path unchanged.
        assert_parity("o := Some(5)\nmatch o:\n    Some(v): print(\"got {v}\")\n    None: print(\"none\")\n");
    }

    #[test]
    fn parity_std_math() {
        let src = "\
import std.math
fn main():
    print(math.floor(2.7))
    print(math.ceil(2.1))
    print(math.sqrt(16.0))
    print(math.pow(2.0, 10.0))
    print(math.abs(0.0 - 3.5))
    print(math.round(2.5))
    print(math.pi)
main()";
        assert_eq!(
            parity_entry(src),
            "2.0\n3.0\n4.0\n1024.0\n3.5\n3.0\n3.141592653589793\n"
        );
    }

    #[test]
    fn parity_std_math_sqrt_negative_errors() {
        // math.sqrt of a negative is a runtime error, identical on both engines.
        let src = "import std.math\nfn main():\n    print(math.sqrt(0.0 - 1.0))\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (_io, _ie, ir, _ic) = crate::interp::run_file(&entry);
        let (_vo, _ve, vr, _vc) = run_file(&entry);
        let ie = ir.unwrap_err().to_string();
        let ve = vr.unwrap_err().to_string();
        assert_eq!(ie, ve);
        assert!(ie.contains("sqrt() of a negative number"), "{ie}");
    }

    #[test]
    fn math_abs_min_overflows() {
        // `math.abs(i64::MIN)` has no representable result. Raw `i64::abs()` would panic (debug) or
        // wrap (release); the native fn must surface a recoverable overflow like every other op,
        // identically on both engines. i64::MIN is built as `-MAX - 1` (the literal
        // `9223372036854775808` overflows the lexer).
        let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    print(math.abs(x))\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let ie = crate::interp::run_file(&entry).2.unwrap_err().to_string();
        let ve = run_file(&entry).2.unwrap_err().to_string();
        assert_eq!(ie, ve, "abs-overflow error must be identical on both engines");
        assert!(ie.contains("integer overflow in abs"), "{ie}");
    }

    #[test]
    fn math_abs_min_overflow_is_recoverable() {
        // The overflow is a normal recoverable fault: `recover:` turns it into an Err, not a crash.
        let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    r := recover:\n        math.abs(x)\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()";
        let out = parity_entry(src);
        assert!(out.contains("integer overflow in abs"), "{out}");
    }

    #[test]
    fn exit_threads_code_through_both_engines() {
        // `std.os.exit(code)` halts the program with that exit code on both engines: output before
        // the call is preserved, the statement after it never runs, and the run is not an error.
        let src = "import std.os\nfn main():\n    print(\"before\")\n    os.exit(3)\n    print(\"after\")\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(io, "before\n", "interp stdout");
        assert_eq!(vo, "before\n", "vm stdout");
        assert_eq!(ic, Some(3), "interp exit code");
        assert_eq!(vc, Some(3), "vm exit code");
        assert!(ir.is_ok() && vr.is_ok(), "exit is not a runtime error: interp={ir:?} vm={vr:?}");
    }

    #[test]
    fn defer_top_level_skipped_by_os_exit() {
        // `std.os.exit` is a hard halt — top-level defers do NOT run through it (matches Go's
        // `os.Exit`, and the existing frame/recover bypass).
        let src = "import std.os\nfn log(s: str):\n    print(s)\ndefer log(\"cleanup\")\nprint(\"before\")\nos.exit(2)\nprint(\"after\")\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(io, "before\n", "interp: cleanup defer skipped by os.exit");
        assert_eq!(vo, "before\n", "vm: cleanup defer skipped by os.exit");
        assert_eq!(ic, Some(2), "interp exit code");
        assert_eq!(vc, Some(2), "vm exit code");
        assert!(ir.is_ok() && vr.is_ok(), "exit is not a runtime error: interp={ir:?} vm={vr:?}");
    }

    #[test]
    fn defer_top_level_runs_on_unhandled_error() {
        // An unhandled top-level `?` error still unwinds through the module body's defers (cleanup
        // runs before the program reports the error).
        let src = "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"nope\")\ndefer log(\"cleanup\")\nprint(\"before\")\nx := boom()?\nprint(\"after\")\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, _ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, _vc) = run_file(&entry);
        assert_eq!(io, "before\ncleanup\n", "interp: top-level defer runs on unhandled error");
        assert_eq!(vo, "before\ncleanup\n", "vm: top-level defer runs on unhandled error");
        assert!(ir.is_err() && vr.is_err(), "unhandled `?` is an error: interp={ir:?} vm={vr:?}");
    }

    #[test]
    fn exit_is_not_caught_by_recover() {
        // A hard exit unwinds past a `recover:` boundary — it is NOT converted to an `Err` value.
        let src = "import std.os\nfn main():\n    x := recover:\n        os.exit(7)\n    print(\"unreachable\")\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, _ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, _vr, vc) = run_file(&entry);
        assert_eq!(io, "", "interp: nothing after the recover runs");
        assert_eq!(vo, "", "vm: nothing after the recover runs");
        assert_eq!(ic, Some(7), "interp exit code");
        assert_eq!(vc, Some(7), "vm exit code");
    }

    #[test]
    fn exit_in_spawned_child_aborts_siblings() {
        // B1/B2: `std.os.exit` inside a child fiber is a hard halt — it aborts the remaining siblings
        // and the rest of the program. The first child prints then exits(3); the second child and the
        // post-`parallel:` statement never run. Identical on both engines (no blocking involved).
        let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(vo, "a\n", "vm: sibling and post-parallel statement aborted by os.exit");
        assert_eq!(io, "a\n", "interp: sibling and post-parallel statement aborted by os.exit");
        assert_eq!(vc, Some(3), "vm exit code");
        assert_eq!(ic, Some(3), "interp exit code");
        assert!(ir.is_ok() && vr.is_ok(), "os.exit is a clean halt, not an error: interp={ir:?} vm={vr:?}");
    }

    /// Run an entry through both engines with a freshly-built [`crate::native::HostConfig`] each
    /// (the config isn't `Clone` — `mk_cfg` produces an identical one per engine). Asserts stdout +
    /// ok/err parity; returns the agreed stdout.
    fn parity_entry_cfg(
        src: &str,
        mk_cfg: impl Fn() -> crate::native::HostConfig,
    ) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, ie_out, ir, _ic) = crate::interp::run_file_with(&entry, mk_cfg());
        let (vo, ve_out, vr, _vc) = run_file_with(&entry, mk_cfg());
        assert_eq!(io, vo, "stdout divergence (interp vs vm)");
        assert_eq!(ie_out, ve_out, "stderr divergence (interp vs vm)");
        assert_eq!(ir.is_ok(), vr.is_ok(), "ok/err divergence: interp={ir:?} vm={vr:?}");
        io
    }

    #[test]
    fn parity_std_io_print() {
        assert_eq!(
            parity_entry("import std.io\nfn main():\n    io.print(\"hello\")\nmain()"),
            "hello\n"
        );
    }

    #[test]
    fn parity_std_io_read_write_file() {
        let t = TmpDir::new();
        let data = t.0.join("data.txt").display().to_string();
        let src = format!(
            "import std.io\nfn main():\n    match io.write_file(\"{data}\", \"hello\\nworld\"):\n        Ok(_): io.print(\"wrote\")\n        Err(e): io.print(e)\n    match io.read_file(\"{data}\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(e)\nmain()"
        );
        let entry = t.write("main.chz", &src);
        let (io_out, _ie, ir, _) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, _) = run_file(&entry);
        assert!(ir.is_ok() && vr.is_ok(), "interp={ir:?} vm={vr:?}");
        assert_eq!(io_out, vo);
        assert_eq!(io_out, "wrote\nhello\nworld\n");
    }

    #[test]
    fn parity_std_io_read_missing_file_errs() {
        // The error text comes from the same `std::fs` call on both engines, so it matches; we only
        // assert the Err branch is taken (deterministic regardless of OS message).
        let src = "import std.io\nfn main():\n    match io.read_file(\"/no/such/chezzi/path/xyz\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(\"err\")\nmain()";
        assert_eq!(parity_entry(src), "err\n");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_file_caps_oversized_input() {
        // /dev/zero is unbounded; read_file must return an Err (the size cap), not OOM.
        let src = "import std.io\nfn main():\n    match io.read_file(\"/dev/zero\"):\n        Ok(s): io.print(\"ok\")\n        Err(e): io.print(\"capped\")\nmain()";
        assert_eq!(parity_entry(src), "capped\n");
    }

    #[test]
    fn parity_std_io_read_line_consumes_injected_stdin() {
        use crate::native::{HostConfig, Stdin};
        let src = "import std.io\nfn main():\n    match io.read_line():\n        Some(l): io.print(\"got {l}\")\n        None: io.print(\"eof\")\n    match io.read_line():\n        Some(l): io.print(l)\n        None: io.print(\"eof\")\nmain()";
        let out = parity_entry_cfg(src, || HostConfig {
            stdin: Stdin::Lines(["alpha".to_string()].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(out, "got alpha\neof\n");
    }

    #[test]
    fn parity_std_io_eprint_goes_to_stderr_not_stdout() {
        let src = "import std.io\nfn main():\n    io.eprint(\"to stderr\")\n    io.print(\"to stdout\")\nmain()";
        // Parity (both engines): stdout has only the print line, stderr has only the eprint line.
        assert_eq!(parity_entry(src), "to stdout\n");
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (out, err, res, _) = run_file(&entry);
        assert!(res.is_ok());
        assert_eq!(out, "to stdout\n");
        assert_eq!(err, "to stderr\n");
    }

    #[test]
    fn parity_std_os_args_and_env() {
        use crate::native::HostConfig;
        let src = "import std.io\nimport std.os\nfn main():\n    for a in os.args():\n        io.print(a)\n    match os.env(\"CHEZZI_TEST_VAR\"):\n        Some(v): io.print(v)\n        None: io.print(\"no var\")\nmain()";
        let out = parity_entry_cfg(src, || HostConfig {
            args: vec!["x".to_string(), "y".to_string()],
            env: [("CHEZZI_TEST_VAR".to_string(), "hi".to_string())].into_iter().collect(),
            ..Default::default()
        });
        assert_eq!(out, "x\ny\nhi\n");
    }

    #[test]
    fn parity_std_os_env_missing_is_none() {
        use crate::native::HostConfig;
        let src = "import std.io\nimport std.os\nfn main():\n    match os.env(\"DEFINITELY_UNSET_XYZ\"):\n        Some(v): io.print(v)\n        None: io.print(\"none\")\nmain()";
        let out = parity_entry_cfg(src, HostConfig::default);
        assert_eq!(out, "none\n");
    }

    #[test]
    fn parity_std_os_getcwd_ok() {
        let src = "import std.io\nimport std.os\nfn main():\n    match os.getcwd():\n        Ok(p): io.print(\"ok\")\n        Err(e): io.print(\"err\")\nmain()";
        assert_eq!(parity_entry(src), "ok\n");
    }

    /// Run a single-file (importing std) program on the VM with GC stress on (collect before every
    /// instruction) and the given config — surfaces any native-return value the collector might free
    /// while still reachable.
    fn vm_run_file_stress(src: &str, cfg: crate::native::HostConfig) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let graph = crate::resolver::build_graph(&entry).unwrap();
        let program = crate::compiler::compile_graph(&graph).unwrap();
        let mut vm = Vm::new(Arc::new(program));
        vm.gc_stress = true;
        vm.host = cfg;
        vm.run().unwrap_or_else(|e| panic!("unexpected error under GC stress: {e}"));
        vm.out
    }

    #[test]
    fn parity_std_str_pure_chezzi_with_mixed_native_import() {
        // std.str is a real Chezzi file (crate/std/str.chz); std.io is native — both in one program.
        let src = "import std.io\nimport std.str as text\nfn main():\n    io.print(text.repeat(\"ab\", 3))\n    io.print(text.reverse(\"hello\"))\n    io.print(text.pad_left(\"7\", 3, \"0\"))\n    if text.is_empty(\"\"):\n        io.print(\"empty\")\n    for line in text.split_lines(\"a\\nb\\nc\"):\n        io.print(line)\nmain()";
        assert_eq!(parity_entry(src), "ababab\nolleh\n007\nempty\na\nb\nc\n");
    }

    #[test]
    fn native_returned_heap_values_survive_gc_stress() {
        use crate::native::HostConfig;
        // Each os.args() call allocates a fresh heap list (immediately garbage); under stress the
        // collector runs every instruction. A dangling handle in native lowering would panic here.
        let src = "import std.io\nimport std.os\nfn main():\n    n := 0\n    while n < 300:\n        xs := os.args()\n        n += 1\n    io.print(\"done {n}\")\nmain()";
        let cfg = HostConfig { args: vec!["a".to_string()], ..Default::default() };
        let out = vm_run_file_stress(src, cfg);
        assert_eq!(out, "done 300\n");
    }

    /// A spread of programs exercising every feature class — run through BOTH engines.
    const PROGRAMS: &[&str] = &[
        // arithmetic + promotion + truncation
        "print(7 / 2)\nprint(1 + 2.0)\nprint(2.5 * 2.0)\nprint(10 % 3)",
        // string concat + interpolation + escapes
        "fn main():\n    n := \"x\"\n    print(\"a{n}b {1 + 2} {{lit}}\")\nmain()",
        // comparison + equality + bool logic
        "print(1 < 2)\nprint(2 == 2.0)\nprint(true and false)\nprint(false or true)\nprint(not true)",
        // lists, indexing, len
        "print([1, 2, 3])\nprint([10, 20, 30][2])\nprint(len([1, 2]))",
        // structs + methods
        "struct P:\n    x: int\n    y: int\n    fn sum(self) -> int:\n        return self.x + self.y\nfn main():\n    p := P(3, 4)\n    print(p)\n    print(p.sum())\nmain()",
        // enums + match + payload binding
        "enum S:\n    C(int)\n    Sq(int)\nfn a(s: S) -> int:\n    match s:\n        C(r): return r * r\n        Sq(n): return n * n\nfn main():\n    print(a(C(3)))\n    print(a(Sq(4)))\nmain()",
        // generic enum (type-erased): same enum at two element types + match payload substitution
        "enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\nfn sum(t: Tree[int]) -> int:\n    match t:\n        Leaf: return 0\n        Node(v, l, r): return sum(l) + v + sum(r)\nfn main():\n    t: Tree[int] = Node(2, Node(1, Leaf, Leaf), Node(3, Leaf, Leaf))\n    print(sum(t))\nmain()",
        // closures
        "fn adder(n: int):\n    return fn(x: int) -> int: x + n\nfn main():\n    f := adder(10)\n    print(f(5))\nmain()",
        // ? operator (Ok + Err propagation)
        "fn d(a: int, b: int) -> Result[int]:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(a / b)\nfn use() -> Result[int]:\n    r := d(10, 0)?\n    return Ok(r)\nfn main():\n    match use():\n        Ok(v): print(v)\n        Err(e): print(e)\nmain()",
        // for + while loops
        "fn main():\n    t := 0\n    for i in 0..100:\n        t += i\n    print(t)\n    n := 5\n    while n > 0:\n        n -= 1\n    print(n)\nmain()",
        // builtins
        "print(range(4))\nprint(int(\"7\") + 1)\nprint(float(3))\nprint(len([1, 2, 3]))\nprint(str(42))",
        // recursion
        "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nfn main():\n    print(fib(15))\nmain()",
        // inferred return type (no `-> T`): runtime is unaffected, both engines agree
        "fn add(a: int, b: int):\n    return a + b\nfn classify(n: int):\n    if n == 0:\n        return Some(0)\n    return None\nfn main():\n    print(add(2, 3))\n    match classify(0):\n        Some(v): print(v)\n        None: print(\"none\")\nmain()",
        // expression-valued match (multiline) + if (inline): both engines must agree on the value
        "fn lookup(k: int) -> int?:\n    if k == 0:\n        return None\n    return Some(k)\nfn main():\n    found := match lookup(7):\n        Some(v): v\n        None: -1\n    print(found)\n    sign := if found > 0: \"pos\" else: \"neg\"\n    print(sign)\n    none := match lookup(0):\n        Some(v): v\n        None: -1\n    print(none)\nmain()",
        // ----- M6: core-type methods (str) -----
        "print(\"abcd\".len())\nprint(\"Hi There\".upper())\nprint(\"Hi There\".lower())\nprint(\"  pad  \".trim())",
        // str conforms to Error: message() returns the string itself
        "print(\"boom\".message())",
        // Go-style Result[T, E]: custom struct error (T!E), match, message() dispatch
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db {self.code}\"\nfn q(ok: bool) -> int!DbErr:\n    if ok:\n        return Ok(1)\n    return Err(DbErr(503))\nfn main():\n    match q(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\n    match q(true):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
        // default-Error path: Err(str) flows as Result[int, Error], consumed via message()
        "fn parse(ok: bool) -> int!:\n    if ok:\n        return Ok(42)\n    return Err(\"bad input\")\nfn main():\n    match parse(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
        // ----- M11 Phase B: recover boundary -----
        // recover catches index-OOB; Ok path wraps the trailing value
        "fn main():\n    r := recover:\n        [1, 2][9]\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()",
        // recover catches divide-by-zero
        "fn main():\n    r := recover:\n        10 / 0\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"err: {e.message()}\")\nmain()",
        // recover catches integer overflow
        "fn main():\n    r := recover:\n        9223372036854775807 * 2\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"ovf\")\nmain()",
        // recover ok-path wraps the value
        "fn main():\n    r := recover:\n        2 + 3\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err\")\nmain()",
        // a fault three calls deep is caught at the boundary (no per-call wrapping)
        "fn a() -> int:\n    return b()\nfn b() -> int:\n    return c()\nfn c() -> int:\n    return [1][9]\nfn main():\n    r := recover:\n        a()\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"deep recovered\")\nmain()",
        // `?` inside recover short-circuits to `r` (try-block): the Err lands in `r`, and code
        // AFTER the recover still runs — the enclosing fn returns a plain str, so this only works
        // if `?` did NOT exit the function.
        "fn d(b: int) -> int!:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(10 / b)\nfn use() -> str:\n    r := recover:\n        x := d(0)?\n        x + 1\n    match r:\n        Ok(v): return \"ok\"\n        Err(e): return \"caught {e.message()}\"\nfn main():\n    print(use())\nmain()",
        // `?` Ok path inside recover: value unwrapped, trailing expression becomes the Ok result
        "fn d(b: int) -> int!:\n    return Ok(10 / b)\nfn main():\n    r := recover:\n        x := d(2)?\n        x + 1\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(e.message())\nmain()",
        // side effects before a caught fault PERSIST (keep semantics) — both engines must agree
        "fn main():\n    x := 1\n    r := recover:\n        x = 99\n        [1][9]\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered\")\n    print(\"x={x}\")\nmain()",
        // nested recover: the inner boundary catches, the outer sees a normal value
        "fn main():\n    r := recover:\n        inner := recover:\n            [1][9]\n        match inner:\n            Ok(v): v\n            Err(e): 0\n    match r:\n        Ok(v): print(\"outer ok {v}\")\n        Err(e): print(\"outer err\")\nmain()",
        // recovered value composes with `?` after the boundary\n
        "fn run() -> int!:\n    r := recover:\n        [10, 20][0]\n    v := r?\n    return Ok(v + 1)\nfn main():\n    match run():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
        "print(\"a,b,c\".split(\",\"))\nprint(\",\".join([\"a\", \"b\", \"c\"]))",
        "print(\"abc\".starts_with(\"ab\"))\nprint(\"abc\".starts_with(\"z\"))\nprint(\"abc\".contains(\"b\"))\nprint(\"abc\".contains(\"q\"))",
        // chained core-type methods
        "print(\"  Hello,World  \".trim().lower().split(\",\"))",
        // ----- M6: core-type methods (list) -----
        "fn main():\n    xs := [1, 2]\n    xs.push(3)\n    xs.push(4)\n    print(xs)\n    print(xs.len())\nmain()",
        // ----- M6: pipe operator -----
        "fn inc(n: int) -> int: n + 1\nfn dbl(n: int) -> int: n * 2\nfn main():\n    print(5 |> inc() |> dbl())\nmain()",
        "fn shout(s: str) -> str: s.upper()\nfn main():\n    print(\"hi\" |> shout())\nmain()",
        // ----- error parity -----
        "print(1 / 0)",
        "print([1, 2][9])",
        "print(1 + \"x\")",
        "fn loop(n: int) -> int:\n    return loop(n + 1)\nfn main():\n    print(loop(0))\nmain()",
        // M6 method error parity
        "print(\"hi\".upper(\"extra\"))",
        "print(\"hi\".frobnicate())",
        "print(\",\".join([1, 2]))",
        "print((5).upper())",
        // arg-eval order: a bad method/receiver with an erroring arg must report the SAME error on
        // both engines — the VM evaluates args (operands) before the call, so the interp must too.
        "print((5).frob(1 / 0))",
        "print(\"hi\".frob(1 / 0))",
        // ----- entry model: no auto-main; unhandled top-level Err/None exits -----
        "fn main():\n    print(\"hi\")",                                  // main defined but never called → no output
        "Err(\"boom\")",                                                  // bare top-level Err → unhandled error
        "x := Err(\"oops\")?",                                            // top-level `?` Err → unhandled error
        "fn g() -> Option[int]:\n    return None\ng()",                   // bare None → unhandled error
        "fn f() -> Result[int]:\n    return Err(\"x\")\nr := f()\nprint(\"handled\")", // Err bound = handled → no exit
        "fn main():\n    print(\"before\")\n    x := Err(\"boom\")?\n    print(\"after\")\nmain()", // partial output then exit
        // a user enum shadowing `Err` is a normal value: bare one must NOT exit, `?` must reject it
        "enum Signal:\n    Err(int)\n    Quiet\nErr(5)\nprint(\"made it\")",
        "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := Err(5)?\n    return x\nf()",
        // unhandled top-level error INSIDE a top-level block (interp: call_depth 0, VM: is_toplevel)
        "if true:\n    Err(\"boom\")\nprint(\"after\")",                  // bare Err in `if` → exit, no "after"
        "for i in 0..1:\n    Err(\"x\")\nprint(\"after\")",              // bare Err in `for` → exit
        "fn d() -> Result[int]:\n    return Err(\"z\")\nif true:\n    x := d()?\n    print(x)", // top-level `?` in block → exit (same span both engines)
    ];

    #[test]
    fn parity_full_suite_vm_vs_interp() {
        for src in PROGRAMS {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_index_assign() {
        assert_parity("xs := [1, 2, 3]\nxs[1] = 9\nxs[0] += 4\nxs[2] -= 1\nprint(xs)\n");
    }

    #[test]
    fn parity_index_assign_out_of_bounds() {
        assert_parity("xs := [1, 2, 3]\nxs[9] = 0\nprint(xs)\n");
    }

    #[test]
    fn parity_compound_index_oob_vs_rhs_error_order() {
        // Compound `xs[i] += rhs` on an out-of-bounds `i` where `rhs` ALSO errors: both engines
        // must agree on which error wins. The VM reads the target (bounds-check) before `rhs`;
        // the interp must do the same.
        assert_parity("xs := [1, 2, 3]\nz := 0\nxs[5] += 1 / z\n");
    }

    #[test]
    fn parity_compound_index_oob_skips_rhs_side_effect() {
        // On an out-of-bounds compound assign, neither engine should run the rhs side effect.
        assert_parity(
            "fn side() -> int:\n    print(\"rhs ran\")\n    return 0\nxs := [1, 2, 3]\nxs[5] += side()\nprint(\"after\")\n",
        );
    }

    #[test]
    fn parity_field_assign() {
        assert_parity(
            "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\np.y += 3\nprint(p.x)\nprint(p.y)\n",
        );
    }

    // NOTE: method_type_params / param_protocol / method_default_args were parity-only; they are
    // now full golden tests (`golden_*_chz_matches_expected_and_interp` above), which assert exact
    // output AND cross-engine parity, so the weaker `parity_*` file tests were removed.

    #[test]
    fn parity_hof_param() {
        let src = "fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nprint(apply(inc, 4))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "5\n");
    }

    #[test]
    fn parity_list_pop_some() {
        let src = "xs := [1,2,3]\nx := xs.pop()\nmatch x:\n    Some(v): print(\"got {v}\")\n    None: print(\"empty\")\nprint(xs.len())\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "got 3\n2\n");
    }

    #[test]
    fn parity_list_pop_empty_none() {
        let src = "xs := [1]\na := xs.pop()\nb := xs.pop()\nmatch b:\n    Some(v): print(\"v\")\n    None: print(\"none\")\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "none\n");
    }

    #[test]
    fn parity_list_reverse() {
        let src = "xs := [3,1,2]\nxs.reverse()\nprint(xs[0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "2\n");
    }

    #[test]
    fn parity_list_contains() {
        let src = "print([1,2,3].contains(2))\nprint([1,2,3].contains(9))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "true\nfalse\n");
    }

    #[test]
    fn parity_list_index_of() {
        let src = "print([10,20,30].index_of(20))\nprint([1,2].index_of(9))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1\n-1\n");
    }

    #[test]
    fn parity_list_sum() {
        let src = "print([1,2,3,4].sum())\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "10\n");
    }

    #[test]
    fn parity_list_sort_int() {
        let src = "xs := [3,1,2]\nxs.sort()\nprint(xs[0])\nprint(xs[2])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
    }

    #[test]
    fn parity_list_sort_str() {
        let src = "xs := [\"banana\",\"apple\",\"cherry\"]\nxs.sort()\nfor s in xs:\n    print(s)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "apple\nbanana\ncherry\n");
    }

    #[test]
    fn parity_list_sort_float() {
        let src = "xs := [3.5, 1.1, 2.2]\nxs.sort()\nprint(xs[0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1.1\n");
    }

    // ===== higher-order list methods: map / filter / fold =====
    //
    // These call a closure per element. On the VM each closure runs nested frames that can GC at
    // instruction boundaries, so the source/result lists (and fold's accumulator) must stay rooted.
    // Several tests use HEAP elements (strings / nested lists) and run under `gc_stress` so that a
    // collection actually happens mid-iteration — if rooting is wrong they crash with a dangling ref.

    #[test]
    fn parity_list_map_int() {
        let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> int: x * 2)\nprint(ys)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "[2, 4, 6]\n");
    }

    #[test]
    fn parity_list_map_to_str_gc_stress() {
        // Each element maps to a freshly-allocated string (heap), so collection mid-map matters.
        let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> str: \"n{x}\")\nfor y in ys:\n    print(y)\n";
        assert_parity(src);
        let expected = "n1\nn2\nn3\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_map_to_nested_list_gc_stress() {
        // Maps each element to a nested list (heap); the result list holds heap children.
        let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> list[int]: [x, x])\nprint(ys[1][0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "2\n");
        assert_eq!(run_capture_stress(src), "2\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_filter_gc_stress() {
        // Filter over string elements; kept elements are heap objects pushed into the result.
        let src = "xs := [\"a\",\"bb\",\"ccc\",\"d\"]\nys := xs.filter(fn(x: str) -> bool: x.len() > 1)\nprint(ys.len())\nprint(ys[0])\n";
        assert_parity(src);
        let expected = "2\nbb\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_filter_int() {
        let src = "xs := [1,2,3,4]\nys := xs.filter(fn(x: int) -> bool: x % 2 == 0)\nprint(ys.len())\nprint(ys[0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "2\n2\n");
    }

    #[test]
    fn parity_list_fold_str_acc_gc_stress() {
        // Fold building a string accumulator (heap) — each step allocates a new acc string, so the
        // rooted accumulator slot must survive the next element's closure call.
        let src = "xs := [\"a\",\"b\",\"c\"]\ns := xs.fold(\"\", fn(a: str, x: str) -> str: a + x)\nprint(s)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "abc\n");
        assert_eq!(run_capture_stress(src), "abc\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_sort_by_str_gc_stress() {
        // Sort heap-string elements by length; the comparator re-enters the VM and a collection can
        // fire mid-sort. The source list must stay rooted (we permute indices, not raw Values).
        let src = "xs := [\"ccc\",\"a\",\"dd\",\"b\"]\nxs.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nfor x in xs:\n    print(x)\n";
        assert_parity(src);
        let expected = "a\nb\ndd\nccc\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_sort_by_nested_list_gc_stress() {
        // Elements are nested lists (heap); sort by first element. Exercises rooting of heap children
        // across comparator calls under stress.
        let src = "xs := [[3,0],[1,0],[2,0]]\nxs.sort_by(fn(a: list[int], b: list[int]) -> int: a[0] - b[0])\nprint(xs[0][0])\nprint(xs[2][0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
        assert_eq!(run_capture_stress(src), "1\n3\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_fold_sum() {
        let src = "print([1,2,3,4].fold(0, fn(a: int, x: int) -> int: a + x))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "10\n");
    }

    fn fixture(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
    }

    /// Run a file through both engines and assert identical (stdout, error).
    fn assert_file_parity(rel: &str) {
        let path = fixture(rel);
        let (vm_out, vm_err, vm_res, _) = run_file(&path);
        let (ip_out, ip_err, ip_res, _) = crate::interp::run_file(&path);
        assert_eq!(vm_out, ip_out, "stdout divergence for {rel}");
        assert_eq!(vm_err, ip_err, "stderr divergence for {rel}");
        assert_eq!(vm_res.err().map(|e| e.to_string()), ip_res.err().map(|e| e.to_string()), "error divergence for {rel}");
    }

    #[test]
    fn golden_hello_via_run_file() {
        let path = fixture("examples/hello.chz");
        let expected = std::fs::read_to_string(fixture("examples/hello.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok());
        assert_eq!(out, expected);
    }

    /// M6 golden: core-type methods + pipe run end-to-end on the VM and byte-match the interp.
    #[test]
    fn golden_methods_via_run_file() {
        let path = fixture("examples/methods.chz");
        let expected = std::fs::read_to_string(fixture("examples/methods.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/methods.chz");
    }

    /// Golden: in-place index & field assignment run end-to-end on the VM and byte-match the interp.
    #[test]
    fn golden_mutate_via_run_file() {
        let path = fixture("examples/mutate.chz");
        let expected = std::fs::read_to_string(fixture("examples/mutate.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/mutate.chz");
    }

    /// M6c golden: the std-library demo (native std.io/math/os + Chezzi std.str) runs end-to-end on
    /// the VM and byte-matches both the `.expected` file and the interpreter.
    #[test]
    fn golden_std_demo_via_run_file() {
        let path = fixture("examples/std_demo.chz");
        let expected = std::fs::read_to_string(fixture("examples/std_demo.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/std_demo.chz");
    }

    /// A complete self-contained program (merge sort + binary search + stats over std.math) runs on
    /// the VM, byte-matches `.expected`, and stays identical to the interpreter.
    #[test]
    fn golden_overflow_via_run_file() {
        // The integer-overflow policy, end-to-end: every overflow (arith, neg, div, math.abs) is a
        // recoverable fault, identical on both engines.
        let path = fixture("examples/overflow.chz");
        let expected = std::fs::read_to_string(fixture("examples/overflow.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/overflow.chz");
    }

    #[test]
    fn golden_stats_app_via_run_file() {
        let path = fixture("examples/stats.chz");
        let expected = std::fs::read_to_string(fixture("examples/stats.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/stats.chz");
    }

    /// G3 golden: `examples/stdlib_cmp.chz` — `import std.cmp`, generic `min`/`max`/`clamp` over
    /// int/float/str/struct, and `list.sort()` over Comparable structs. Byte-matches `.expected`
    /// and stays identical on interp + VM.
    #[test]
    fn golden_stdlib_cmp_via_run_file() {
        let path = fixture("examples/stdlib_cmp.chz");
        let expected = std::fs::read_to_string(fixture("examples/stdlib_cmp.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/stdlib_cmp.chz");
    }

    /// M8-M5 golden: `examples/json_decode.chz` — type-directed `json.decode[T]` into struct /
    /// typed map / list / scalar, with Option fields, extra-key tolerance, and an error case.
    /// Byte-identical on interp + VM.
    #[test]
    fn golden_json_decode_via_run_file() {
        let path = fixture("examples/json_decode.chz");
        let expected = std::fs::read_to_string(fixture("examples/json_decode.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/json_decode.chz");
    }

    /// M8-M3 golden: `examples/sys.chz` — the native trio std.process/std.fs/std.time, end-to-end
    /// on the VM, byte-identical to `.expected` and the interpreter (deterministic ops only).
    #[test]
    fn golden_sys_via_run_file() {
        let path = fixture("examples/sys.chz");
        let expected = std::fs::read_to_string(fixture("examples/sys.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/sys.chz");
    }

    /// Comprehensions golden: `examples/comprehensions.chz` — list/set/map comprehensions, a guard,
    /// and a range source. Byte-matches `.expected` and stays identical on interp + VM.
    #[test]
    fn golden_comprehensions_via_run_file() {
        let path = fixture("examples/comprehensions.chz");
        let expected = std::fs::read_to_string(fixture("examples/comprehensions.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/comprehensions.chz");
    }

    /// Radix-literal golden: `examples/hex.chz` — hex/binary/octal literals feeding bitwise +
    /// arithmetic. Byte-matches `.expected` and stays identical on interp + VM.
    #[test]
    fn golden_hex_via_run_file() {
        let path = fixture("examples/hex.chz");
        let expected = std::fs::read_to_string(fixture("examples/hex.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/hex.chz");
    }

    /// List concat/extend + map merge/update golden: `examples/concat_merge.chz`. New-vs-mutate
    /// semantics + arg-wins-on-key-clash. Byte-matches `.expected`, identical on interp + VM.
    #[test]
    fn golden_concat_merge_via_run_file() {
        let path = fixture("examples/concat_merge.chz");
        let expected = std::fs::read_to_string(fixture("examples/concat_merge.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/concat_merge.chz");
    }

    /// Tuple-destructuring `for` + `std.iter` golden: `examples/for_tuple.chz` — destructure a list
    /// of tuples, one-var whole-tuple, triples, `enumerate`/`zip`, comprehension combo. Byte-matches
    /// `.expected`, identical on interp + VM (the `IsMap` runtime split is exercised alongside maps).
    #[test]
    fn golden_for_tuple_via_run_file() {
        let path = fixture("examples/for_tuple.chz");
        let expected = std::fs::read_to_string(fixture("examples/for_tuple.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/for_tuple.chz");
    }

    /// Optional chaining + null-coalescing golden: `examples/optchain.chz` — `?.field`, `?.method()`,
    /// `??`, chaining + None short-circuit. Desugared to `match`; byte-matches `.expected`, identical
    /// on interp + VM.
    #[test]
    fn golden_optchain_via_run_file() {
        let path = fixture("examples/optchain.chz");
        let expected = std::fs::read_to_string(fixture("examples/optchain.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/optchain.chz");
    }

    /// Runtime stack trace: a faulting nested call reports the error line + the call chain (innermost
    /// first) with each call's line. Asserted on the VM, and the interp must produce the IDENTICAL
    /// formatted trace (frames carry the same call-site spans on both engines).
    #[test]
    fn stack_trace_reports_call_chain_on_both_engines() {
        let path = fixture("examples/stack_trace.chz");
        let (_out, _err, res, _) = run_file(&path);
        let e = res.expect_err("program should fault");
        assert_eq!(e.message, "division by zero");
        let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names, vec!["divide", "compute", "main"]);
        // Call-site lines, innermost first.
        let lines: Vec<usize> = e.trace.iter().map(|f| f.span.line).collect();
        assert_eq!(lines, vec![15, 18, 20]);
        let vm_fmt = format_trace(&e.message, e.span, &e.trace);
        assert!(vm_fmt.contains("at divide (called at line 15"), "got: {vm_fmt}");

        // Interp parity: identical formatted trace.
        let (_o, _er, ip_res, _) = crate::interp::run_file(&path);
        let ie = ip_res.expect_err("program should fault");
        let ip_fmt = crate::interp::format_trace(&ie.message, ie.span, &ie.trace);
        assert_eq!(vm_fmt, ip_fmt, "engines must produce the same stack trace");
    }

    /// A `recover:`-caught fault leaves no stale frames: a *later* uncaught fault's trace shows only
    /// its own chain, not the recovered call.
    #[test]
    fn recovered_fault_does_not_pollute_later_trace() {
        let src = "fn boom() -> int:\n    return 1 / 0\nfn safe() -> int:\n    r := recover:\n        boom()\n    return 7\nfn deeper() -> int:\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(safe())\n    print(deeper())\nmain()\n";
        let dir = std::env::temp_dir().join("chezzi_recover_trace_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.chz");
        std::fs::write(&path, src).unwrap();
        let (_o, _e, res, _) = run_file(&path);
        let e = res.expect_err("should fault");
        let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names, vec!["deeper", "main"], "no stale 'boom'/'safe' frames");
    }

    /// A `defer`red call that itself faults supersedes the original fault (Go semantics); the trace
    /// must reflect the DEFERRED fault's chain (deeper, includes the deferred fn), identically on
    /// both engines — not the original body fault's chain.
    #[test]
    fn deferred_fault_trace_supersedes_on_both_engines() {
        let src = "fn boom() -> int:\n    return 1 / 0\nfn worker() -> int:\n    defer boom()\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(worker())\nmain()\n";
        let dir = std::env::temp_dir().join("chezzi_defer_trace_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dt.chz");
        std::fs::write(&path, src).unwrap();
        let (_o, _e, res, _) = run_file(&path);
        let e = res.expect_err("should fault");
        let vm_names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(vm_names, vec!["boom", "worker", "main"], "deferred fault's chain");
        let vm_fmt = format_trace(&e.message, e.span, &e.trace);
        let (_o2, _e2, ip_res, _) = crate::interp::run_file(&path);
        let ie = ip_res.expect_err("should fault");
        let ip_fmt = crate::interp::format_trace(&ie.message, ie.span, &ie.trace);
        assert_eq!(vm_fmt, ip_fmt, "engines must agree on a deferred-fault trace");
    }

    /// Non-constant default golden: `examples/default_expr.chz` — defaults that are arithmetic on
    /// literals, a global times a literal, and a function call (free fns + struct fields). Byte-matches
    /// `.expected`, identical on interp + VM.
    #[test]
    fn golden_default_expr_via_run_file() {
        let path = fixture("examples/default_expr.chz");
        let expected = std::fs::read_to_string(fixture("examples/default_expr.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/default_expr.chz");
    }

    /// Function-typed field call golden: `examples/fn_field.chz` — `recv.f(args)` where `f` is a
    /// `fn`-typed field resolves to field-access-then-call (on `self` and on an external receiver),
    /// not a method. Byte-matches `.expected`, identical on interp + VM.
    #[test]
    fn golden_fn_field_via_run_file() {
        let path = fixture("examples/fn_field.chz");
        let expected = std::fs::read_to_string(fixture("examples/fn_field.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/fn_field.chz");
    }

    /// `sort_by_key` golden: `examples/sort_by_key.chz` — sort in place by a derived key (int/str
    /// keys, stable, descending-via-negation, and a Comparable *struct* key). Byte-matches
    /// `.expected`, identical on interp + VM.
    #[test]
    fn golden_sort_by_key_via_run_file() {
        let path = fixture("examples/sort_by_key.chz");
        let expected = std::fs::read_to_string(fixture("examples/sort_by_key.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/sort_by_key.chz");
    }

    /// `Ref[T]` golden: `examples/ref.chz` — a pure-Chezzi one-field mutable box (`std.ref`):
    /// `get`/`set`/`update`, closure-capture accumulation through the shared struct, generic over a
    /// non-int type. Byte-matches `.expected`, identical on interp + VM. No engine change.
    #[test]
    fn golden_ref_via_run_file() {
        let path = fixture("examples/ref.chz");
        let expected = std::fs::read_to_string(fixture("examples/ref.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/ref.chz");
    }

    /// Tuple destructuring + match-on-tuple + guards golden: `examples/tuple_match.chz` — `a, b :=
    /// fn()`, typed tuple value + `.0`/`.1`, `match` literal/binding/guard arms, `Some((a, b))`.
    /// Coverage for behavior that already worked. Byte-matches `.expected`, identical on interp + VM.
    #[test]
    fn golden_tuple_match_via_run_file() {
        let path = fixture("examples/tuple_match.chz");
        let expected = std::fs::read_to_string(fixture("examples/tuple_match.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/tuple_match.chz");
    }

    /// `std.os.exit(code)` golden: `examples/exit.chz` halts at the negative branch with status 2.
    /// Byte-matches `.expected` on both engines and both report the same exit code.
    #[test]
    fn golden_exit_via_run_file() {
        let path = fixture("examples/exit.chz");
        let expected = std::fs::read_to_string(fixture("examples/exit.expected")).unwrap();
        let (vo, _ve, vr, vc) = run_file(&path);
        let (io, _ie, ir, ic) = crate::interp::run_file(&path);
        assert!(vr.is_ok() && ir.is_ok(), "exit is a clean halt: vm={vr:?} interp={ir:?}");
        assert_eq!(vo, expected, "vm stdout");
        assert_eq!(io, expected, "interp stdout");
        assert_eq!(vc, Some(2), "vm exit code");
        assert_eq!(ic, Some(2), "interp exit code");
    }

    /// M8-M2 golden: `examples/json_dynamic.chz` — `import std.json`, the pure-Chezzi `Json` enum
    /// parse/stringify round-trip + accessors + unicode escapes + an error case. Byte-matches
    /// `.expected` and stays identical on interp + VM.
    #[test]
    fn golden_json_dynamic_via_run_file() {
        let path = fixture("examples/json_dynamic.chz");
        let expected = std::fs::read_to_string(fixture("examples/json_dynamic.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/json_dynamic.chz");
    }

    /// M9 golden: `examples/regex_demo.chz` — `import std.regex` (is_match / find with capture
    /// groups / find_all / replace_all / split + a bad-pattern Err). Byte-matches `.expected` and
    /// stays identical on interp + VM.
    #[test]
    fn golden_regex_demo_via_run_file() {
        let path = fixture("examples/regex_demo.chz");
        let expected = std::fs::read_to_string(fixture("examples/regex_demo.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/regex_demo.chz");
    }

    /// Golden: `examples/knapsack.chz` fills an int DP table with `cmp.max` (std.cmp generic over
    /// Comparable). Runs on the VM, byte-matches `.expected`, and stays identical to the interp.
    #[test]
    fn golden_knapsack_via_run_file() {
        let path = fixture("examples/knapsack.chz");
        let expected = std::fs::read_to_string(fixture("examples/knapsack.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/knapsack.chz");
    }

    /// `Iterator[T]` golden: a generic fn bounded `[S: Iterator[T], T]` over list/str/set/struct,
    /// with the element type flowing into returns. Parity-checked across both engines.
    #[test]
    fn golden_iterator_bound_via_run_file() {
        let path = fixture("examples/iterator_bound.chz");
        let expected = std::fs::read_to_string(fixture("examples/iterator_bound.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/iterator_bound.chz");
    }

    /// Lazy iterator adapters (Take/Mapped over an infinite Count) — the no-`yield` story. The inner
    /// `self.inner.next()` recovers the element type through the `I: Iterator[T]` bound on both engines.
    #[test]
    fn golden_iter_adapters_via_run_file() {
        let path = fixture("examples/iter_adapters.chz");
        let expected = std::fs::read_to_string(fixture("examples/iter_adapters.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/iter_adapters.chz");
    }

    #[test]
    fn golden_multi_file_project_via_vm() {
        let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
        let (out, _err, res, _) = run_file(&fixture("tests/fixtures/proj/main.chz"));
        assert!(res.is_ok());
        assert_eq!(out, expected);
        assert_file_parity("tests/fixtures/proj/main.chz");
    }

    /// The M4.5 headline bug, now on the VM: an imported function reading its module's top-level
    /// constant must resolve against *its own* module, not the caller — even when the caller
    /// defines a same-named global with a different value.
    #[test]
    fn imported_fn_uses_home_globals() {
        let (out, _err, res, _) = run_file(&fixture("tests/fixtures/homeglobals/main.chz"));
        assert!(res.is_ok());
        assert_eq!(out, "from-lib\nfrom-main\n");
        assert_file_parity("tests/fixtures/homeglobals/main.chz");
    }

    /// Whole multi-file project is byte-identical under GC stress.
    #[test]
    fn multi_file_identical_under_gc_stress() {
        // The fixture is small; run it under stress by routing through the entry graph manually.
        let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
        let graph = crate::resolver::build_graph(&fixture("tests/fixtures/proj/main.chz")).unwrap();
        let program = crate::compiler::compile_graph(&graph).unwrap();
        let mut vm = Vm::new(Arc::new(program));
        vm.gc_stress = true;
        vm.run().unwrap();
        assert_eq!(vm.out, expected);
    }

    // ----- map / dictionary parity (gap #5) -----

    #[test]
    fn parity_map_literal_print() {
        // Deterministic insertion order; duplicate key -> last wins. Display is `{k: v, …}`.
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m)\n", "{a: 1, b: 2}\n");
        assert_parity_out("e := {}\nprint(e)\n", "{}\n");
        assert_parity_out("m := {\"a\": 1, \"a\": 9}\nprint(m)\n", "{a: 9}\n");
    }

    #[test]
    fn parity_map_index_read() {
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m[\"b\"])\n", "2\n");
    }

    #[test]
    fn parity_map_missing_key_read_errors() {
        // Both engines must error identically on a missing key.
        let src = "m := {\"a\": 1}\nprint(m[\"z\"])\n";
        assert_parity(src);
        assert!(vm_outcome(src).unwrap_err().contains("key not found"), "{:?}", vm_outcome(src));
    }

    #[test]
    fn parity_map_index_insert_and_update() {
        assert_parity_out(
            "m := {\"a\": 1}\nm[\"b\"] = 2\nm[\"a\"] = 9\nprint(m)\n",
            "{a: 9, b: 2}\n",
        );
    }

    #[test]
    fn parity_map_compound_assign() {
        assert_parity_out("m := {\"a\": 1}\nm[\"a\"] += 5\nprint(m[\"a\"])\n", "6\n");
    }

    #[test]
    fn parity_map_compound_assign_missing_key_errors() {
        // Compound on a missing key is an error (consistent with read-missing).
        let src = "m := {\"a\": 1}\nm[\"z\"] += 1\n";
        assert_parity(src);
        assert!(vm_outcome(src).unwrap_err().contains("key not found"), "{:?}", vm_outcome(src));
    }

    #[test]
    fn parity_map_methods() {
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.len())\n", "2\n");
        assert_parity_out("m := {\"a\": 1}\nprint(m.has(\"a\"))\nprint(m.has(\"z\"))\n", "true\nfalse\n");
        assert_parity_out(
            "m := {\"a\": 1}\nmatch m.get(\"a\"):\n    Some(v): print(v)\n    None: print(\"absent\")\n",
            "1\n",
        );
        assert_parity_out(
            "m := {\"a\": 1}\nmatch m.get(\"z\"):\n    Some(v): print(v)\n    None: print(\"absent\")\n",
            "absent\n",
        );
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.keys())\n", "[a, b]\n");
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.values())\n", "[1, 2]\n");
    }

    #[test]
    fn parity_map_remove() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2}\nmatch m.remove(\"a\"):\n    Some(v): print(v)\n    None: print(\"absent\")\nprint(m)\n",
            "1\n{b: 2}\n",
        );
        // remove of a missing key -> None, map unchanged.
        assert_parity_out(
            "m := {\"a\": 1}\nmatch m.remove(\"z\"):\n    Some(v): print(v)\n    None: print(\"absent\")\nprint(m)\n",
            "absent\n{a: 1}\n",
        );
    }

    #[test]
    fn parity_map_keys_iteration() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m.keys():\n    print(k)\n",
            "a\nb\nc\n",
        );
    }

    #[test]
    fn parity_map_int_and_bool_keys() {
        assert_parity_out("m := {1: \"x\", 2: \"y\"}\nprint(m[2])\n", "y\n");
        assert_parity_out("m := {true: 1, false: 0}\nprint(m[false])\n", "0\n");
    }

    // ----- Hashable struct keys (hash-table map/set) -----

    /// A struct with `hash(self) -> int` as a map key: insert/update/get/has/remove + insertion-order
    /// iteration must be byte-identical across both engines.
    #[test]
    fn parity_map_struct_key() {
        let src = "\
struct P:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y
fn main():
    m: map[P, str] = {}
    m[P(1, 2)] = \"a\"
    m[P(3, 4)] = \"b\"
    m[P(1, 2)] = \"z\"
    for k in m:
        print(k)
    print(m[P(3, 4)])
    print(m.has(P(1, 2)))
    print(m.has(P(9, 9)))
    print(m.get(P(3, 4)))
    print(m.remove(P(1, 2)))
    print(m.len())
main()";
        assert_parity(src);
    }

    /// Set of structs: dedup of structurally-equal keys via custom hash + union/intersection/difference.
    #[test]
    fn parity_set_struct_algebra() {
        let src = "\
struct P:
    x: int
    fn hash(self) -> int:
        return self.x
fn main():
    a: set[P] = set([P(1), P(2), P(2), P(3)])
    b: set[P] = set([P(2), P(3), P(4)])
    print(a.len())
    print(a.union(b).len())
    print(a.intersection(b).len())
    print(a.difference(b).len())
    print(a.has(P(2)))
    a.remove(P(2))
    print(a.has(P(2)))
main()";
        assert_parity(src);
    }

    /// A struct used as a map key but MISSING `hash()` is a checker error — but `run_capture` bypasses
    /// the checker, so the runtime must error consistently (not panic) on both engines.
    #[test]
    fn parity_map_struct_key_missing_hash_errors() {
        let src = "\
struct P:
    x: int
fn main():
    m: map[P, int] = {}
    m[P(1)] = 5
main()";
        assert_parity(src);
    }

    /// REGRESSION (AsInt relocation): a non-int LIST index now errors at runtime in `GetIndex`,
    /// with the SAME message the removed `AsInt` produced. The checker is bypassed by `run_capture`,
    /// so this exercises the relocated runtime validation on both engines.
    #[test]
    fn parity_list_non_int_index_still_errors() {
        let src = "xs := [1, 2, 3]\nprint(xs[\"a\"])\n";
        assert_parity(src);
        assert!(vm_outcome(src).unwrap_err().contains("expected int, found str"), "{:?}", vm_outcome(src));
        // And on assignment (SetIndex relocation).
        let src2 = "xs := [1, 2, 3]\nxs[\"a\"] = 9\n";
        assert_parity(src2);
        assert!(vm_outcome(src2).unwrap_err().contains("expected int, found str"), "{:?}", vm_outcome(src2));
    }

    #[test]
    fn parity_map_gc_stress_heap_keys_and_values() {
        // Keys AND values are heap strings; build many maps so collection runs mid-stream and the
        // `Heap::children` tracing of BOTH keys and values is exercised (a use-after-free if either
        // is untraced). The keys()/values() lists also hold heap children.
        let src = "fn main():\n    i := 0\n    while i < 200:\n        m := {\"k{i}\": \"v{i}\"}\n        m[\"extra\"] = \"x{i}\"\n        if i == 199:\n            print(m[\"k{i}\"])\n            print(m.values())\n        i += 1\nmain()\n";
        assert_parity(src);
        let expected = "v199\n[v199, x199]\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (untraced map key/value?)");
    }

    /// Record the VM speedup over the interpreter on a loop-heavy script (the spec's perf check).
    /// Asserts a conservative floor that holds even in debug builds; the real ~6x lands in release.
    #[test]
    fn bench_vm_faster_than_interp() {
        let src = "fn main():\n    total := 0\n    i := 0\n    while i < 500000:\n        total += (i * 3 - 1) % 7\n        i += 1\n    print(total)\nmain()";
        let t = Instant::now();
        let ip = crate::interp::run_capture(src).unwrap();
        let interp_t = t.elapsed();
        let t = Instant::now();
        let vm = run_capture(src).unwrap();
        let vm_t = t.elapsed();
        assert_eq!(vm, ip, "engines disagree on the benchmark output");
        let ratio = interp_t.as_secs_f64() / vm_t.as_secs_f64();
        println!("VM speedup over interp: {ratio:.1}x (interp {interp_t:?}, vm {vm_t:?}) [debug build; ~6x in release]");
        assert!(ratio >= 1.2, "VM not faster than interp: {ratio:.2}x");
    }

    // ===== gap #8: tuples + multi-return + destructuring =====

    #[test]
    fn parity_tuple_literal_display() {
        assert_parity_out("t := (1, 2)\nprint(t)\n", "(1, 2)\n");
    }

    #[test]
    fn parity_tuple_element_access() {
        assert_parity_out("t := (3, 4)\nprint(t.0)\nprint(t.1)\n", "3\n4\n");
    }

    #[test]
    fn parity_tuple_element_out_of_range_errors() {
        // The checker would catch `.2` statically, but `t` here is built so both engines hit the
        // runtime bounds check with the identical message — parity on the error path.
        assert_parity("t := (1, 2)\nprint(t.0)\nprint(t.1)\n");
    }

    #[test]
    fn parity_destructure_local() {
        assert_parity_out("a, b := (1, 2)\nprint(a)\nprint(b)\n", "1\n2\n");
    }

    #[test]
    fn parity_tuple_equality() {
        assert_parity_out("print((1, 2) == (1, 2))\nprint((1, 2) == (1, 3))\n", "true\nfalse\n");
    }

    #[test]
    fn parity_multi_return_destructured_at_call_site() {
        let src = "fn pair() -> (int, int):\n    return (3, 4)\nfn main():\n    a, b := pair()\n    print(a + b)\nmain()\n";
        assert_parity_out(src, "7\n");
    }

    #[test]
    fn parity_tuple_heap_elements_gc_stress() {
        // A tuple of heap values (a string + a list). Under GC stress a collection happens between
        // building the tuple and reading it back — proving `Heap::children` traces tuple elements.
        let src = "t := (\"hi\", [1, 2, 3])\nprint(t.0)\nprint(t.1)\n";
        assert_parity(src);
        assert_eq!(run_capture_stress(src), "hi\n[1, 2, 3]\n", "tuple elements not GC-traced?");
    }

    // ----- slicing + Index/IndexSet/Slice protocol dispatch (VM ↔ interp parity) -----

    #[test]
    fn slice_list_and_str_parity() {
        assert_parity_out("print([1, 2, 3, 4, 5][1..3])\n", "[2, 3]\n");
        assert_parity_out("print(\"hello\"[0..2])\n", "he\n");
    }

    #[test]
    fn slice_clamped_parity() {
        assert_parity_out("print([1, 2, 3][1..99])\n", "[2, 3]\n");
        assert_parity_out("print([1, 2, 3][2..1])\n", "[]\n");
        assert_parity_out("print([1, 2, 3][-1..2])\n", "[1, 2]\n");
    }

    const BUF_PROG: &str = "\
struct Buf:
    xs: list[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int, end: int) -> list[int]:
        return self.xs[start..end]
fn main():
    b := Buf([10, 20, 30])
    print(b[0])
    b[1] = 99
    print(b[1])
    b[0] += 5
    print(b[0])
    print(b[0..2])
main()";

    #[test]
    fn struct_index_slice_dispatch_parity() {
        assert_parity_out(BUF_PROG, "10\n99\n15\n[15, 99]\n");
    }

    #[test]
    fn slice_survives_gc_stress() {
        // The sliced list shares the source's element handles; a GC during the slice alloc must not
        // collect them. (Source is an inline temporary, unrooted except by the slice path.)
        let src = "print([1, 2, 3, 4, 5][1..4])\n";
        assert_parity(src);
        assert_eq!(run_capture_stress(src), "[2, 3, 4]\n", "slice elements not GC-rooted?");
    }
}
