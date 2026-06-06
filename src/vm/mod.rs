//! Bytecode stack VM (M5) — the Phase-2 execution path. Runs the [`Program`] produced by the
//! compiler, reproducing the tree-walk interpreter's semantics byte-for-byte (golden/parity tests
//! cross-check the two engines). M5a: handle-addressed values, no collector yet (the mark-sweep
//! GC lands in M5b).

pub mod heap;
pub mod op;
pub mod value;

use heap::{Heap, Obj};
use op::{CapSrc, Op, Program, ProtoId};
use std::rc::Rc;
use value::{GcRef, Value};

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
}

struct Vm {
    program: Rc<Program>,
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
    /// Test mode: collect before *every* instruction, to surface any missing GC root.
    gc_stress: bool,
}

impl Vm {
    fn new(program: Rc<Program>) -> Self {
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
            gc_stress: false,
        }
    }

    fn err(&self, message: String, span: Span) -> RuntimeError {
        RuntimeError { message, span }
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
        self.frames.push(CallFrame { proto, ip: 0, base, home, closure, counted, is_toplevel });
        self.cur_base = base;
        Ok(())
    }

    // ----- the dispatch loop -----

    fn run_until(&mut self, base_level: usize) -> Result<(), RuntimeError> {
        let program = Rc::clone(&self.program);
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
            // `Rc` clone is a single refcount bump per loop entry; `op` then borrows program data
            // that is disjoint from the `&mut self` fields `step` touches.
            let op = &program.protos[pid].code[ip];
            let span = program.protos[pid].lines[ip];
            self.step(op, span)?;
        }
        Ok(())
    }

    /// Mark-sweep collection. Roots: the whole operand stack (which contains every frame's local
    /// slots *and* any in-flight expression temporaries), each frame's home module + backing
    /// closure, and the module namespace cache. Everything else is garbage.
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
        }
        work.extend(self.module_objs.iter().copied());

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
            Op::CallPrint(argc) => self.do_print(*argc),
            Op::Return => self.do_return(false),
            Op::Try => self.do_try(span)?,
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
                // Pop 2n values `[k0,v0,…]`; build insertion-ordered entries with last-key-wins upsert.
                let at = self.stack.len() - 2 * *n;
                let flat: Vec<Value> = self.stack.split_off(at);
                let mut entries: Vec<(Value, Value)> = Vec::with_capacity(*n);
                let mut it = flat.into_iter();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    match entries.iter().position(|(ek, _)| self.values_equal(*ek, k)) {
                        Some(i) => entries[i].1 = v,
                        None => entries.push((k, v)),
                    }
                }
                let h = self.heap.alloc(Obj::Map(entries));
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
                let v = self.pop();
                let s = self.display(v);
                let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::BuildStr(n) => {
                let at = self.stack.len() - *n;
                let parts: Vec<Value> = self.stack.split_off(at);
                let mut s = String::new();
                for p in parts {
                    s.push_str(&self.display(p));
                }
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
                        Obj::Map(entries) => {
                            let keys: Vec<Value> = entries.iter().map(|(k, _)| *k).collect();
                            let nh = self.heap.alloc(Obj::List(keys));
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
                    (Obj::Map(a), Obj::Map(b)) => {
                        a.len() == b.len()
                            && a.iter().zip(b).all(|((ka, va), (kb, vb))| self.values_equal(*ka, *kb) && self.values_equal(*va, *vb))
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
        // Core-type methods (M6): built-in methods on `str` / `list`. Handled before the clone-match
        // so `list.push` mutates the heap object in place (the match below clones the Obj). Mirrors
        // `interp::builtins::call_method` exactly — error strings included (parity-tested).
        if matches!(self.heap.get(h), Obj::Str(_) | Obj::List(_) | Obj::Map(_)) {
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
            Obj::Struct { name, .. } => {
                let def = self.program.structs.get(name.as_ref()).cloned().ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let proto = *def.methods.get(method).ok_or_else(|| self.err(format!("struct '{name}' has no method '{method}'"), span))?;
                let home = self.module_objs[def.module_idx];
                if self.program.protos[proto].arity != argc + 1 {
                    // `self` + explicit args.
                    return Err(self.err(format!("function '{}' expects {} argument(s), got {}", self.program.protos[proto].name, self.program.protos[proto].arity, argc + 1), span));
                }
                let mut call_args = Vec::with_capacity(argc + 1);
                call_args.push(recv);
                call_args.extend(args);
                let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                self.push(v);
                Ok(())
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
                    let out = self.invoke_value(f, vec![elem], span)?;
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
                    let new = self.invoke_value(f, vec![acc, elem], span)?;
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
        match self.invoke_value(cmp, vec![a, b], span)? {
            Value::Int(n) => Ok(n),
            other => Err(self.err(format!("sort_by comparator must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
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
                    "split" => {
                        self.arity_err("split", args, 1, span)?;
                        let sep = str_arg(self, 0)?;
                        let parts: Vec<Value> =
                            s.split(sep.as_str()).map(|p| self.alloc_str(p.to_string())).collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(parts))))
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
                    // Str elements live on the heap, so the comparator needs `&self.heap` — which
                    // would conflict with `get_mut`. Clone the elements out, sort (no alloc/closure
                    // → no GC), then write back. `value_order` reads strings via `self.heap`.
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
            Obj::Map(entries) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let key = args[0];
                    let entries = entries.clone();
                    Ok(Value::Bool(entries.iter().any(|(k, _)| self.values_equal(*k, key))))
                }
                "get" => {
                    self.arity_err("get", args, 1, span)?;
                    let key = args[0];
                    let found = entries.iter().find(|(k, _)| self.values_equal(*k, key)).map(|(_, v)| *v);
                    match found {
                        Some(v) => Ok(self.alloc_enum("Option", "Some", vec![v])),
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "keys" => {
                    self.arity_err("keys", args, 0, span)?;
                    let keys: Vec<Value> = entries.iter().map(|(k, _)| *k).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(keys))))
                }
                "values" => {
                    self.arity_err("values", args, 0, span)?;
                    let vals: Vec<Value> = entries.iter().map(|(_, v)| *v).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(vals))))
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let key = args[0];
                    let pos = entries.iter().position(|(k, _)| self.values_equal(*k, key));
                    match pos {
                        Some(i) => {
                            let Obj::Map(entries) = self.heap.get_mut(h) else { unreachable!() };
                            let (_, v) = entries.remove(i);
                            Ok(self.alloc_enum("Option", "Some", vec![v]))
                        }
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                _ => Err(self.err(format!("type map has no method '{method}'"), span)),
            },
            _ => unreachable!("core_method dispatched a non-str/list/map receiver"),
        }
    }

    /// Allocate a heap string and return its handle as a `Value`.
    fn alloc_str(&mut self, s: String) -> Value {
        Value::Obj(self.heap.alloc(Obj::Str(s.into_boxed_str())))
    }

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result, exactly like the interp).
    fn do_return(&mut self, _propagated: bool) {
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        self.push(ret);
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
                // A `?` at the top level (no enclosing function) is an unhandled error → exit. Use
                // the `?` op's own `span` so the reported location matches the interp (which threads
                // the `?`'s `expr.span` through its propagation marker).
                if self.frames.last().unwrap().is_toplevel {
                    return Err(self.top_level_error(v, span).unwrap_or_else(|| {
                        self.err(format!("unhandled error: {}", self.display(v)), span)
                    }));
                }
                // Otherwise early-return this value from the enclosing function.
                self.push(v);
                self.do_return(true);
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
            Obj::Map(entries) => {
                match entries.iter().find(|(k, _)| self.values_equal(*k, key)) {
                    Some((_, v)) => {
                        let v = *v;
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err("key not found".to_string(), span)),
                }
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
        // For a map, locate the entry first (needs `&self.heap` for value-equality), then mutate.
        if let Obj::Map(entries) = self.heap.get(h) {
            let pos = entries.iter().position(|(k, _)| self.values_equal(*k, key));
            let Obj::Map(entries) = self.heap.get_mut(h) else { unreachable!() };
            match pos {
                Some(i) => entries[i].1 = val,
                None => entries.push((key, val)),
            }
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

    fn do_print(&mut self, argc: usize) {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let line = args.iter().map(|v| self.display(*v)).collect::<Vec<_>>().join(" ");
        self.out.push_str(&line);
        self.out.push('\n');
        self.push(Value::Nil);
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
        let s = self.display(args[0]);
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
                Obj::Struct { .. } => "struct",
                Obj::Enum { .. } => "enum",
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module { .. } => "module",
                Obj::Native { .. } => "function",
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
                Obj::Map(entries) => {
                    let inner = entries
                        .iter()
                        .map(|(k, v)| format!("{}: {}", self.display(*k), self.display(*v)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{{{inner}}}")
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
            },
        }
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
}

fn is_numeric(v: Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
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
    let mut vm = Vm::new(Rc::new(program));
    let result = vm.run();
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
            let mut vm = Vm::new(Rc::new(program));
            vm.gc_stress = stress;
            let result = vm.run();
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

/// A finished run: captured `(stdout, stderr, outcome)`. Stderr holds `std.io.eprint` output.
pub type RunOutput = (String, String, Result<(), RuntimeError>);

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
        Err(e) => return (String::new(), String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let program = match crate::compiler::compile_graph(&graph) {
        Ok(p) => p,
        Err(e) => return (String::new(), String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let mut vm = Vm::new(Rc::new(program));
    vm.host = cfg;
    let result = vm.run();
    (vm.out, vm.stderr, result)
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
        let mut vm = Vm::new(Rc::new(empty_program()));
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
        let mut vm = Vm::new(Rc::new(empty_program()));
        let nat = vm.heap.alloc(Obj::Native { name: "greet".into(), func: greet });
        // A native fn handle has no GC children (guards the mark-phase claim).
        assert!(vm.heap.children(nat).is_empty());
        vm.push(Value::Obj(nat));
        vm.do_call(0, Span { line: 1, col: 1 }).unwrap();
        let result = vm.pop();
        assert_eq!(vm.display(result), "hi");
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
        let src = "\
fn main():
    match 5:
        Red: print(\"x\")
main()";
        assert!(run_err(src).contains("cannot match on int"));
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
        let (io, ie_out, ir) = crate::interp::run_file(&entry_path);
        let (vo, ve_out, vr) = run_file(&entry_path);
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
    fn for_over_map_keys_parity() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m:\n    print(k)\n",
            "a\nb\nc\n",
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
    fn math_max_int_parity() {
        // imports std.math, so it must go through the file/graph path (`parity_entry`).
        let out = parity_entry("import std.math\nfn main():\n    print(math.max(3, 5))\n    print(math.min(3, 5))\n    print(math.abs(-5))\nmain()\n");
        assert_eq!(out, "5\n3\n5\n");
    }

    #[test]
    fn math_max_float_parity() {
        let out = parity_entry("import std.math\nfn main():\n    print(math.max(3.0, 5.0))\n    print(math.abs(-2.5))\nmain()\n");
        assert_eq!(out, "5.0\n2.5\n");
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
    print(math.min(2.0, 5.0))
    print(math.max(2.0, 5.0))
    print(math.round(2.5))
    print(math.pi)
main()";
        assert_eq!(
            parity_entry(src),
            "2.0\n3.0\n4.0\n1024.0\n3.5\n2.0\n5.0\n3.0\n3.141592653589793\n"
        );
    }

    #[test]
    fn parity_std_math_sqrt_negative_errors() {
        // math.sqrt of a negative is a runtime error, identical on both engines.
        let src = "import std.math\nfn main():\n    print(math.sqrt(0.0 - 1.0))\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (_io, _ie, ir) = crate::interp::run_file(&entry);
        let (_vo, _ve, vr) = run_file(&entry);
        let ie = ir.unwrap_err().to_string();
        let ve = vr.unwrap_err().to_string();
        assert_eq!(ie, ve);
        assert!(ie.contains("sqrt() of a negative number"), "{ie}");
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
        let (io, ie_out, ir) = crate::interp::run_file_with(&entry, mk_cfg());
        let (vo, ve_out, vr) = run_file_with(&entry, mk_cfg());
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
        let (io_out, _ie, ir) = crate::interp::run_file(&entry);
        let (vo, _ve, vr) = run_file(&entry);
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
        let (out, err, res) = run_file(&entry);
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
        let mut vm = Vm::new(Rc::new(program));
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
        let (vm_out, vm_err, vm_res) = run_file(&path);
        let (ip_out, ip_err, ip_res) = crate::interp::run_file(&path);
        assert_eq!(vm_out, ip_out, "stdout divergence for {rel}");
        assert_eq!(vm_err, ip_err, "stderr divergence for {rel}");
        assert_eq!(vm_res.err().map(|e| e.to_string()), ip_res.err().map(|e| e.to_string()), "error divergence for {rel}");
    }

    #[test]
    fn golden_hello_via_run_file() {
        let path = fixture("examples/hello.chz");
        let expected = std::fs::read_to_string(fixture("examples/hello.expected")).unwrap();
        let (out, _err, res) = run_file(&path);
        assert!(res.is_ok());
        assert_eq!(out, expected);
    }

    /// M6 golden: core-type methods + pipe run end-to-end on the VM and byte-match the interp.
    #[test]
    fn golden_methods_via_run_file() {
        let path = fixture("examples/methods.chz");
        let expected = std::fs::read_to_string(fixture("examples/methods.expected")).unwrap();
        let (out, _err, res) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/methods.chz");
    }

    /// Golden: in-place index & field assignment run end-to-end on the VM and byte-match the interp.
    #[test]
    fn golden_mutate_via_run_file() {
        let path = fixture("examples/mutate.chz");
        let expected = std::fs::read_to_string(fixture("examples/mutate.expected")).unwrap();
        let (out, _err, res) = run_file(&path);
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
        let (out, _err, res) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/std_demo.chz");
    }

    /// A complete self-contained program (merge sort + binary search + stats over std.math) runs on
    /// the VM, byte-matches `.expected`, and stays identical to the interpreter.
    #[test]
    fn golden_stats_app_via_run_file() {
        let path = fixture("examples/stats.chz");
        let expected = std::fs::read_to_string(fixture("examples/stats.expected")).unwrap();
        let (out, _err, res) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/stats.chz");
    }

    /// Gap #12 golden: `examples/knapsack.chz` fills an int DP table with `math.max` (now int+float
    /// polymorphic). Runs on the VM, byte-matches `.expected`, and stays identical to the interp.
    #[test]
    fn golden_knapsack_via_run_file() {
        let path = fixture("examples/knapsack.chz");
        let expected = std::fs::read_to_string(fixture("examples/knapsack.expected")).unwrap();
        let (out, _err, res) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/knapsack.chz");
    }

    #[test]
    fn golden_multi_file_project_via_vm() {
        let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
        let (out, _err, res) = run_file(&fixture("tests/fixtures/proj/main.chz"));
        assert!(res.is_ok());
        assert_eq!(out, expected);
        assert_file_parity("tests/fixtures/proj/main.chz");
    }

    /// The M4.5 headline bug, now on the VM: an imported function reading its module's top-level
    /// constant must resolve against *its own* module, not the caller — even when the caller
    /// defines a same-named global with a different value.
    #[test]
    fn imported_fn_uses_home_globals() {
        let (out, _err, res) = run_file(&fixture("tests/fixtures/homeglobals/main.chz"));
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
        let mut vm = Vm::new(Rc::new(program));
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
}
