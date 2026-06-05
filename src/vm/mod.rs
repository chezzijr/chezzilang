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
    /// Module toplevel frame — a `?` that unwinds here is "used outside a function".
    is_toplevel: bool,
}

struct Vm {
    program: Rc<Program>,
    heap: Heap,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    out: String,
    call_depth: usize,
    /// Each module's namespace object, indexed by module index (run-once cache + import targets).
    module_objs: Vec<GcRef>,
}

impl Vm {
    fn new(program: Rc<Program>) -> Self {
        Vm {
            program,
            heap: Heap::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            out: String::new(),
            call_depth: 0,
            module_objs: Vec::new(),
        }
    }

    fn err(&self, message: String, span: Span) -> RuntimeError {
        RuntimeError { message, span }
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

        // Bind imports (dependencies already ran, so their namespaces are populated).
        for imp in &m.imports {
            self.bind_import(mod_obj, imp)?;
        }

        // Run the module body once.
        self.run_proto(m.toplevel, mod_obj, None, Vec::new(), false, true, Span { line: 1, col: 1 })?;

        // Entry module: auto-run a nullary `main()`.
        if m.is_entry {
            let main = match self.module_global(mod_obj, "main") {
                Some(Value::Obj(h)) => match self.heap.get(h) {
                    Obj::Func { proto, home } => Some((*proto, *home)),
                    _ => None,
                },
                _ => None,
            };
            if let Some((proto, home)) = main {
                if self.program.protos[proto].arity != 0 {
                    return Err(self.err("main() must take no arguments".to_string(), Span { line: 1, col: 1 }));
                }
                self.run_proto(proto, home, None, Vec::new(), true, false, Span { line: 1, col: 1 })?;
            }
        }
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
        Ok(())
    }

    // ----- the dispatch loop -----

    fn run_until(&mut self, base_level: usize) -> Result<(), RuntimeError> {
        while self.frames.len() > base_level {
            let fi = self.frames.len() - 1;
            let pid = self.frames[fi].proto;
            let ip = self.frames[fi].ip;
            let op = self.program.protos[pid].code[ip].clone();
            let span = self.program.protos[pid].lines[ip];
            self.frames[fi].ip = ip + 1;
            self.step(op, span)?;
        }
        Ok(())
    }

    fn base(&self) -> usize {
        self.frames.last().unwrap().base
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

    fn step(&mut self, op: Op, span: Span) -> Result<(), RuntimeError> {
        match op {
            Op::ConstInt(n) => self.push(Value::Int(n)),
            Op::ConstFloat(x) => self.push(Value::Float(x)),
            Op::ConstStr(s) => {
                let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::True => self.push(Value::Bool(true)),
            Op::False => self.push(Value::Bool(false)),
            Op::Nil => self.push(Value::Nil),
            Op::Pop => {
                self.pop();
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
                let v = self.module_global(home, &name).ok_or_else(|| self.err(format!("undefined name '{name}'"), span))?;
                self.push(v);
            }
            Op::DefineGlobal(name) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.module_define(home, &name, v);
            }
            Op::SetGlobal(name) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                if !self.module_assign(home, &name, v) {
                    return Err(self.err(format!("cannot assign to undefined name '{name}'"), span));
                }
            }
            Op::GetCaptured(name) => {
                let clo = self.frames.last().unwrap().closure;
                let v = clo
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => captured.get(&name).copied(),
                        _ => None,
                    })
                    .or_else(|| {
                        let home = self.frames.last().unwrap().home;
                        self.module_global(home, &name)
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
            Op::Jump(t) => self.jump(t),
            Op::JumpIfFalse(t) => {
                if let Value::Bool(false) = self.pop() {
                    self.jump(t);
                }
            }
            Op::JumpIfFalseKeep(t) => {
                if let Value::Bool(false) = *self.stack.last().unwrap() {
                    self.jump(t);
                }
            }
            Op::JumpIfTrueKeep(t) => {
                if let Value::Bool(true) = *self.stack.last().unwrap() {
                    self.jump(t);
                }
            }
            Op::Call(argc) => self.do_call(argc, span)?,
            Op::CallMethod(name, argc) => self.do_method_call(&name, argc, span)?,
            Op::CallBuiltin(name, argc) => self.do_builtin(&name, argc, span)?,
            Op::CallPrint(argc) => self.do_print(argc),
            Op::Return => self.do_return(false),
            Op::Try => self.do_try(span)?,
            Op::NewList(n) => {
                let at = self.stack.len() - n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::List(items));
                self.push(Value::Obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(&name, argc, span)?,
            Op::NewEnum(ty, variant, argc) => self.new_enum(&ty, &variant, argc, span)?,
            Op::MakeFunc(proto) => {
                let home = self.frames.last().unwrap().home;
                let h = self.heap.alloc(Obj::Func { proto, home });
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
                    captured.insert(e.name, v);
                }
                let h = self.heap.alloc(Obj::Closure { proto, captured, home });
                self.push(Value::Obj(h));
            }
            Op::GetField(name) => self.get_field(&name, span)?,
            Op::GetIndex => self.get_index(span)?,
            Op::ToStr => {
                let v = self.pop();
                let s = self.display(v);
                let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::BuildStr(n) => {
                let at = self.stack.len() - n;
                let parts: Vec<Value> = self.stack.split_off(at);
                let mut s = String::new();
                for p in parts {
                    s.push_str(&self.display(p));
                }
                let h = self.heap.alloc(Obj::Str(s.into_boxed_str()));
                self.push(Value::Obj(h));
            }
            Op::ListClone => {
                let v = self.pop();
                match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => {
                            let cloned = items.clone();
                            let nh = self.heap.alloc(Obj::List(cloned));
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
                let v = self.stack[self.base() + slot];
                if !matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Enum { .. })) {
                    return Err(self.err(format!("cannot match on {}", self.type_name(v)), span));
                }
            }
            Op::MatchArm { scrut, variant, nbind, bind_start, next } => self.match_arm(scrut, &variant, nbind, bind_start, next, span)?,
            Op::MatchNoArm(slot) => {
                let v = self.stack[self.base() + slot];
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

    fn arith(&mut self, op: Op, span: Span) -> Result<(), RuntimeError> {
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

    fn compare_op(&mut self, op: Op, span: Span) -> Result<(), RuntimeError> {
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

    // ----- calls -----

    fn do_call(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let callee = self.pop();
        match callee {
            Value::Obj(h) => match self.heap.get(h).clone() {
                Obj::Func { proto, home } => {
                    self.check_arity("function", &self.program.protos[proto].name.clone(), self.program.protos[proto].arity, argc, span)?;
                    let v = self.run_proto(proto, home, None, args, true, false, span)?;
                    self.push(v);
                    Ok(())
                }
                Obj::Closure { proto, home, .. } => {
                    if argc != self.program.protos[proto].arity {
                        return Err(self.err(format!("closure expects {} argument(s), got {argc}", self.program.protos[proto].arity), span));
                    }
                    let v = self.run_proto(proto, home, Some(h), args, true, false, span)?;
                    self.push(v);
                    Ok(())
                }
                _ => Err(self.err(format!("'{}' is not callable", self.type_name(callee)), span)),
            },
            other => Err(self.err(format!("'{}' is not callable", self.type_name(other)), span)),
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
        let Value::Obj(h) = recv else {
            return Err(self.err(format!("type {} has no method '{method}'", self.type_name(recv)), span));
        };
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

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result, exactly like the interp).
    fn do_return(&mut self, _propagated: bool) {
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.push(ret);
    }

    fn do_try(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        // Extract (variant, payload-arity, first-payload) up front so the heap borrow is released
        // before we mutate the stack / unwind a frame.
        let info = match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum { variant, payload, .. } => Some((variant.to_string(), payload.len(), payload.first().copied())),
                _ => None,
            },
            _ => None,
        };
        if let Some((variant, n, first)) = info {
            if (variant == "Ok" || variant == "Some") && n == 1 {
                self.push(first.unwrap());
                return Ok(());
            }
            if variant == "Err" || variant == "None" {
                // Early-return this value from the enclosing function.
                if self.frames.last().unwrap().is_toplevel {
                    let shown = self.display(v);
                    return Err(self.err(format!("'?' used outside a function (propagated {shown})"), Span { line: 1, col: 1 }));
                }
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
        let idx = match self.pop() {
            Value::Int(n) => n,
            _ => unreachable!("index pre-checked by AsInt"),
        };
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        match self.heap.get(h) {
            Obj::List(items) => {
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
            "sqrt" => self.builtin_sqrt(&args, span)?,
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

    fn builtin_sqrt(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("sqrt", args, 1, span)?;
        let x = match args[0] {
            Value::Int(n) => n as f64,
            Value::Float(f) => f,
            other => return Err(self.err(format!("sqrt() expects a number, got {}", self.type_name(other)), span)),
        };
        if x < 0.0 {
            return Err(self.err(format!("sqrt() of a negative number ({x})"), span));
        }
        Ok(Value::Float(x.sqrt()))
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
                Obj::Struct { .. } => "struct",
                Obj::Enum { .. } => "enum",
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module { .. } => "module",
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
            },
        }
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

/// Run a multi-file program from its entry path on the dedicated VM thread. Mirrors
/// `interp::run_file`: resolve the graph, compile it, run each module once in dependency order,
/// then the entry's `main()`. Output produced so far is preserved alongside the outcome.
pub fn run_file(entry: &std::path::Path) -> (String, Result<(), RuntimeError>) {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_file_inner(&entry))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

fn run_file_inner(entry: &std::path::Path) -> (String, Result<(), RuntimeError>) {
    let graph = match crate::resolver::build_graph(entry) {
        Ok(g) => g,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let program = match crate::compiler::compile_graph(&graph) {
        Ok(p) => p,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let mut vm = Vm::new(Rc::new(program));
    let result = vm.run();
    (vm.out, result)
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
    print(add(add(1, 2), 3))";
        assert_eq!(run(src), "6\n");
    }

    #[test]
    fn forward_reference_between_top_level_fns() {
        // `main` is defined before `helper`; hoisting must make the forward ref resolve.
        let src = "\
fn main():
    print(helper(21))
fn helper(n: int) -> int:
    return n * 2";
        assert_eq!(run(src), "42\n");
    }

    #[test]
    fn infinite_recursion_hits_depth_limit() {
        let src = "\
fn loop(n: int) -> int:
    return loop(n + 1)
fn main():
    print(loop(0))";
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
    print(classify(5))";
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
    print(total)";
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
    print(g(5))";
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
    print(add100(1))";
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
    print(r)";
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
        Err(e): print(\"err {e}\")";
        assert_eq!(run(src), "err zero\n");
    }

    #[test]
    fn try_on_non_result_is_error() {
        let src = "\
fn f() -> int:
    x := (5)?
    return x";
        // Reaching `?` on an int is a runtime error.
        assert!(run_err(&format!("{src}\nfn main():\n    print(f())")).contains("'?' expects Result or Option, found int"));
    }

    #[test]
    fn try_at_top_level_is_error() {
        assert!(run_err(r#"x := Err("oops")?"#).contains("'?' used outside a function"));
    }

    // ----- for loops -----

    #[test]
    fn for_range_sums() {
        let src = "\
fn main():
    total := 0
    for i in 0..1000:
        total += i
    print(total)";
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
    print(first())";
        assert_eq!(run(src), "0\n");
    }

    #[test]
    fn for_over_list() {
        let src = "\
fn main():
    total := 0
    for x in [10, 20, 30]:
        total += x
    print(total)";
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
    print(area(Square(3)))";
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
    print(name(Blue))";
        assert_eq!(run_err(src), "no match arm for variant 'Blue'");
    }

    #[test]
    fn match_on_non_enum_is_error() {
        let src = "\
fn main():
    match 5:
        Red: print(\"x\")";
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
    fn field_access_and_unknown_field() {
        let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    print(p.x)
    print(p.y)";
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
    print(c.doubled())";
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

    #[test]
    fn builtin_sqrt() {
        assert_eq!(run("print(sqrt(9.0))"), "3.0\n");
        assert!(run_err("print(sqrt(-1.0))").contains("sqrt() of a negative number"));
    }

    // ----- construction arity / nullary variant -----

    #[test]
    fn struct_arity_error() {
        let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1)";
        assert!(run_err(src).contains("struct 'Point' expects 2 field(s), got 1"));
    }

    #[test]
    fn variant_arity_error() {
        assert!(run_err("fn main():\n    x := Ok(1, 2)").contains("variant 'Ok' expects 1 value(s), got 2"));
    }

    #[test]
    fn nullary_variant_used_as_value() {
        assert_eq!(run("print(None)"), "None\n");
        let src = "\
enum Light:
    On
    Off
fn main():
    print(Off)";
        assert_eq!(run(src), "Off\n");
    }

    // ----- string interpolation -----

    #[test]
    fn interpolation_and_literal_braces() {
        let src = "\
fn main():
    name := \"thuan\"
    print(\"hi {name}, {{not interpolated}}\")";
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
}
