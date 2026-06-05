//! Tree-walk interpreter (M3): executes an AST `Module` directly — the reference semantics for
//! Chezzi before the bytecode VM (M5). Single-file programs run here.

use crate::ast::{
    AssignOp, BinaryOp, Expr, ExprKind, FnDecl, MatchArm, Pattern, Span, Stmt, StmtKind, UnaryOp,
};
use crate::{lexer, parser};

mod builtins;
mod env;
mod value;

pub use value::Value;

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

/// Control-flow signal threaded out of statement execution so `return` (and `?` propagation)
/// can unwind cleanly through nested blocks.
enum Flow {
    /// Fell off the end of the block normally.
    Normal,
    /// `return value` — unwind to the enclosing function call.
    Return(Value),
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

/// The interpreter: environment, output buffer, and the declared struct / enum types.
struct Interp {
    env: env::Env,
    out: String,
    structs: std::collections::HashMap<String, std::rc::Rc<StructDef>>,
    variants: std::collections::HashMap<String, VariantDef>,
    /// Evaluated module namespaces, keyed by module id (run-once cache for a multi-file program).
    namespaces: std::collections::HashMap<crate::resolver::ModuleId, std::rc::Rc<value::ModuleNamespace>>,
    /// Set by the `?` operator when it hits `Err`/`None`: the value to early-return from the
    /// enclosing function. While set, an `Err(RuntimeError)` carries the unwind up to the nearest
    /// call boundary (`call`/`call_closure`), which converts it into that function's return value.
    /// No evaluation runs between `?` raising and the boundary catching, so the channel can't be
    /// clobbered.
    propagating: Option<Value>,
    /// Current user-function call depth. Bounds native-stack recursion so an infinite/very deep
    /// Chezzi recursion returns a `RuntimeError` instead of overflowing the host stack (SIGABRT).
    call_depth: usize,
}

/// Maximum user-function call depth. Bounds recursion well within the dedicated interpreter
/// thread's [`INTERP_STACK_BYTES`] stack, so infinite recursion returns a `RuntimeError` instead
/// of overflowing the host stack — while still allowing deep, legitimate recursion.
const MAX_CALL_DEPTH: usize = 10_000;

impl Interp {
    fn new() -> Self {
        let mut interp = Interp {
            env: env::Env::new(),
            out: String::new(),
            structs: std::collections::HashMap::new(),
            variants: std::collections::HashMap::new(),
            namespaces: std::collections::HashMap::new(),
            propagating: None,
            call_depth: 0,
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
            variant.to_string(),
            VariantDef {
                enum_name: enum_name.into(),
                arity,
            },
        );
    }

    /// Evaluate an expression to a value.
    fn eval(&mut self, expr: &Expr) -> Result<Value, RuntimeError> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Float(f) => Ok(Value::Float(*f)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(s) => Ok(Value::Str(self.interpolate(s, expr.span)?.into())),
            ExprKind::Ident(name) => {
                // A nullary enum variant used as a value (e.g. `None`, `Red`).
                if let Some(def) = self.variants.get(name).filter(|d| d.arity == 0) {
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
                eval_binary(*op, l, r, expr.span)
            }
            ExprKind::List(items) => {
                let vals = items
                    .iter()
                    .map(|e| self.eval(e))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(vals))))
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
                let target = self.eval(obj)?;
                match &target {
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
                        ns.members.0.borrow().get(name).cloned().ok_or_else(|| RuntimeError {
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
                let target = self.eval(obj)?;
                let idx = self.eval_int(index)?;
                let bounds_err = |len: usize| RuntimeError {
                    message: format!("index {idx} out of bounds (len {len})"),
                    span: expr.span,
                };
                match &target {
                    Value::List(items) => {
                        let items = items.borrow();
                        usize::try_from(idx)
                            .ok()
                            .and_then(|i| items.get(i).cloned())
                            .ok_or_else(|| bounds_err(items.len()))
                    }
                    Value::Str(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        usize::try_from(idx)
                            .ok()
                            .and_then(|i| chars.get(i).copied())
                            .map(|c| Value::Str(c.to_string().into()))
                            .ok_or_else(|| bounds_err(chars.len()))
                    }
                    other => Err(RuntimeError {
                        message: format!("cannot index {}", other.type_name()),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Call { callee, args } => self.eval_call(callee, args, expr.span),
            ExprKind::Try(inner) => {
                let v = self.eval(inner)?;
                match &v {
                    // Unwrap the success case. Gate on the *type* (`Result`/`Option`), not the bare
                    // variant name, so a user enum that shadows `Ok`/`Some` isn't unwrapped by `?`.
                    Value::Enum { ty, variant, payload }
                        if (ty.as_ref() == "Result" && variant.as_ref() == "Ok"
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
                        message: format!("'?' expects Result or Option, found {}", other.type_name()),
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
                    let expr = parse_expr_str(&inner)?;
                    let value = self.eval(&expr)?;
                    out.push_str(&value.to_string());
                }
                '}' => {
                    return Err(RuntimeError {
                        message: "unmatched '}' in string (use '}}' for a literal brace)".to_string(),
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
        // A method call `obj.name(args)` — bind `obj` as `self`.
        if let ExprKind::Field { obj, name } = &callee.kind {
            return self.eval_method_call(obj, name, args, span);
        }

        let arg_vals = args
            .iter()
            .map(|a| self.eval(a))
            .collect::<Result<Vec<_>, _>>()?;

        // Builtins and struct constructors are resolved by name.
        if let ExprKind::Ident(name) = &callee.kind {
            if name == "print" {
                let line = arg_vals
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                self.out.push_str(&line);
                self.out.push('\n');
                return Ok(Value::Nil);
            }
            if builtins::is_builtin(name) {
                return builtins::call(name, arg_vals, span);
            }
            if let Some(def) = self.structs.get(name).cloned() {
                return self.construct_struct(name, &def, arg_vals, span);
            }
            if let Some(def) = self.variants.get(name).cloned() {
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
    fn call_value(&mut self, callee: Value, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        match callee {
            Value::Func(decl, home) => self.call(&decl, &home, args, span),
            Value::Closure(clo) => self.call_closure(&clo, args, span),
            other => Err(RuntimeError {
                message: format!("'{}' is not callable", other.type_name()),
                span,
            }),
        }
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
            frame.insert(param.name.clone(), arg);
        }
        let mut new_locals = clo.captured.clone();
        new_locals.push(frame);
        self.enter_call(span)?;
        let saved_globals = self.env.swap_globals(clo.home.clone());
        let saved = self.env.swap_locals(new_locals);
        let result = self.eval(&clo.body);
        self.env.swap_locals(saved);
        self.env.swap_globals(saved_globals);
        self.call_depth -= 1;
        if let Some(v) = self.propagating.take() {
            return Ok(v);
        }
        result
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
        let Value::Enum {
            variant, payload, ..
        } = &value
        else {
            return Err(RuntimeError {
                message: format!("cannot match on {}", value.type_name()),
                span: scrutinee.span,
            });
        };
        for arm in arms {
            let Pattern::Variant { name, bindings } = &arm.pattern;
            if name != variant.as_ref() {
                continue;
            }
            if bindings.len() != payload.len() {
                return Err(RuntimeError {
                    message: format!(
                        "pattern '{}' binds {} value(s) but variant carries {}",
                        name,
                        bindings.len(),
                        payload.len()
                    ),
                    span: scrutinee.span,
                });
            }
            self.env.push();
            for (b, v) in bindings.iter().zip(payload.iter()) {
                self.env.define(b, v.clone());
            }
            let flow = self.exec_block(&arm.body);
            self.env.pop();
            return flow;
        }
        Err(RuntimeError {
            message: format!("no match arm for variant '{variant}'"),
            span: scrutinee.span,
        })
    }

    /// Construct a struct instance, binding the positional args to fields in declaration order.
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
                    name,
                    def.fields.len(),
                    args.len()
                ),
                span,
            });
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
        let arg_vals = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
        // Core-type methods (M6): built-in methods on `str` and `list` dispatch on the value.
        if matches!(receiver, Value::Str(_) | Value::List(_)) {
            return builtins::call_method(&receiver, method, arg_vals, span);
        }
        // `module.fn(args)` is a plain call on the looked-up member — no `self` is bound.
        if let Value::Module(ns) = &receiver {
            let member = ns.members.0.borrow().get(method).cloned().ok_or_else(|| RuntimeError {
                message: format!("module '{}' has no member '{method}'", ns.name),
                span,
            })?;
            return self.call_value(member, arg_vals, span);
        }
        let Value::Struct { name, .. } = &receiver else {
            return Err(RuntimeError {
                message: format!("type {} has no method '{method}'", receiver.type_name()),
                span,
            });
        };
        let def = self.structs.get(name.as_ref()).cloned().ok_or_else(|| RuntimeError {
            message: format!("unknown struct type '{name}'"),
            span,
        })?;
        let decl = def.methods.get(method).cloned().ok_or_else(|| RuntimeError {
            message: format!("struct '{name}' has no method '{method}'"),
            span,
        })?;
        let mut call_args = Vec::with_capacity(arg_vals.len() + 1);
        call_args.push(receiver.clone());
        call_args.extend(arg_vals);
        self.call(&decl, &def.home, call_args, span)
    }

    /// Call a user function: bind params in a fresh local frame (lexical scoping — the callee
    /// sees only globals + its params), run the body, and surface a `return` value (or `Nil`).
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
        let mut frame = std::collections::HashMap::new();
        for (param, arg) in decl.params.iter().zip(args) {
            frame.insert(param.name.clone(), arg);
        }
        self.enter_call(span)?;
        // Resolve the callee's top-level names against *its* module, not the caller's.
        let saved_globals = self.env.swap_globals(home.clone());
        let saved = self.env.swap_locals(vec![frame]);
        let result = self.exec_block(&decl.body);
        self.env.swap_locals(saved);
        self.env.swap_globals(saved_globals);
        self.call_depth -= 1;
        // A `?` inside the body early-returns its Err/None value as this function's result.
        if let Some(v) = self.propagating.take() {
            return Ok(v);
        }
        match result? {
            Flow::Return(v) => Ok(v),
            Flow::Normal => Ok(Value::Nil),
        }
    }

    /// Drive a single-file module (the `run_program` test path): the whole program shares one set
    /// of globals and runs top-to-bottom (no auto-`main`). Errors surface with output preserved.
    #[cfg(test)]
    fn execute(&mut self, stmts: &[Stmt]) -> Result<(), RuntimeError> {
        self.eval_top_level(stmts)
    }

    /// Hoist declarations and run top-level statements. There is **no** automatic entry point —
    /// `main` is an ordinary function the program calls itself (scripting-language model). An
    /// `Err`/`None` propagated to the top by a top-level `?` is an unhandled error and exits.
    fn eval_top_level(&mut self, stmts: &[Stmt]) -> Result<(), RuntimeError> {
        self.hoist_declarations(stmts)?;
        let result = self.exec_block(stmts);
        // A `?` that propagated to the top (no enclosing function) is an unhandled error.
        if let Some(value) = self.propagating.take() {
            return Err(top_level_error(&value, Span { line: 1, col: 1 }).unwrap_or_else(|| {
                RuntimeError { message: format!("unhandled error: {value}"), span: Span { line: 1, col: 1 } }
            }));
        }
        result.map(|_| ())
    }

    /// Evaluate one module of a multi-file program into its own fresh globals, then snapshot those
    /// globals as the module's namespace (cached for importers). Run-once: each module is evaluated
    /// exactly once, in dependency order. No module auto-runs `main` (it's a normal function).
    fn eval_module(&mut self, lm: &crate::resolver::LoadedModule) -> Result<(), RuntimeError> {
        let mod_globals = value::ModEnv::new();
        let saved = self.env.swap_globals(mod_globals.clone());
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
        let ns = self.namespaces.get(&imp.target).cloned().ok_or_else(|| RuntimeError {
            message: "internal: imported module not evaluated before importer".to_string(),
            span: imp.span,
        })?;
        match &imp.import {
            Import::Module { path, alias } => {
                let name = alias.clone().unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.env.define(&name, Value::Module(ns));
            }
            Import::From { path: _, names } => {
                for (member, alias) in names {
                    let value = ns.members.0.borrow().get(member).cloned().ok_or_else(|| {
                        RuntimeError {
                            message: format!("module '{}' has no member '{member}'", ns.name),
                            span: imp.span,
                        }
                    })?;
                    self.env.define(alias.as_ref().unwrap_or(member), value);
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
                } => {
                    // Type names are program-global in M4.5 (per-module type namespacing is
                    // deferred): a name reused across modules is a collision, not a shadow.
                    if self.structs.contains_key(name) {
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
                    self.structs.insert(name.clone(), std::rc::Rc::new(def));
                }
                StmtKind::Enum { name, variants } => {
                    for v in variants {
                        self.register_variant(&v.name, name, v.payload.len());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Execute a sequence of statements in the current scope, stopping early on `return`.
    fn exec_block(&mut self, stmts: &[Stmt]) -> Result<Flow, RuntimeError> {
        for stmt in stmts {
            match self.exec_stmt(stmt)? {
                Flow::Normal => {}
                flow @ Flow::Return(_) => return Ok(flow),
            }
        }
        Ok(Flow::Normal)
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
            StmtKind::Let { name, value, .. } => {
                let v = self.eval(value)?;
                self.env.define(name, v);
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
            StmtKind::Fn(decl) => {
                let home = self.env.globals_rc();
                self.env
                    .define(&decl.name, Value::Func(std::rc::Rc::new(decl.clone()), home));
                Ok(Flow::Normal)
            }
            // Type declarations are registered up-front by `hoist_declarations`; imports are bound
            // before the body runs (see `eval_module`). Nothing to do when execution reaches them.
            StmtKind::Struct { .. } | StmtKind::Enum { .. } | StmtKind::Import(_) => Ok(Flow::Normal),
            StmtKind::Match { scrutinee, arms } => self.exec_match(scrutinee, arms),
            StmtKind::Return(value) => {
                let v = match value {
                    Some(e) => self.eval(e)?,
                    None => Value::Nil,
                };
                Ok(Flow::Return(v))
            }
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
            StmtKind::For { var, iter, body } => self.exec_for(var, iter, body),
            StmtKind::While { cond, body } => {
                while as_bool(self.eval(cond)?, cond.span)? {
                    if let Flow::Return(v) = self.exec_scoped_block(body)? {
                        return Ok(Flow::Return(v));
                    }
                }
                Ok(Flow::Normal)
            }
        }
    }

    /// Execute a `for var in iter:` loop. `iter` is either a `start..end` range or any
    /// expression evaluating to a list. Each iteration runs the body in a fresh scope with
    /// `var` bound, so the loop variable doesn't leak. Ranges are iterated **lazily** (never
    /// materialized) so `for i in 0..huge:` can't exhaust memory.
    fn exec_for(&mut self, var: &str, iter: &Expr, body: &[Stmt]) -> Result<Flow, RuntimeError> {
        if let ExprKind::Range { start, end } = &iter.kind {
            let lo = self.eval_int(start)?;
            let hi = self.eval_int(end)?;
            let mut i = lo;
            while i < hi {
                if let Flow::Return(v) = self.run_for_body(var, Value::Int(i), body)? {
                    return Ok(Flow::Return(v));
                }
                i += 1;
            }
            return Ok(Flow::Normal);
        }
        let items = match self.eval(iter)? {
            Value::List(items) => items.borrow().clone(),
            other => {
                return Err(RuntimeError {
                    message: format!("cannot iterate over {}", other.type_name()),
                    span: iter.span,
                });
            }
        };
        for item in items {
            if let Flow::Return(v) = self.run_for_body(var, item, body)? {
                return Ok(Flow::Return(v));
            }
        }
        Ok(Flow::Normal)
    }

    /// Run one `for` iteration: bind `var` in a fresh scope, execute the body, pop the scope.
    fn run_for_body(
        &mut self,
        var: &str,
        item: Value,
        body: &[Stmt],
    ) -> Result<Flow, RuntimeError> {
        self.env.push();
        self.env.define(var, item);
        let flow = self.exec_block(body);
        self.env.pop();
        flow
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
        let ExprKind::Ident(name) = &target.kind else {
            return Err(RuntimeError {
                message: "invalid assignment target".to_string(),
                span,
            });
        };
        let rhs = self.eval(value)?;
        let new_val = match op {
            AssignOp::Eq => rhs,
            AssignOp::PlusEq => {
                let cur = self.env.get(name).ok_or_else(|| RuntimeError {
                    message: format!("undefined name '{name}'"),
                    span,
                })?;
                eval_binary(BinaryOp::Add, cur, rhs, span)?
            }
            AssignOp::MinusEq => {
                let cur = self.env.get(name).ok_or_else(|| RuntimeError {
                    message: format!("undefined name '{name}'"),
                    span,
                })?;
                eval_binary(BinaryOp::Sub, cur, rhs, span)?
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
}

/// Stack size for the interpreter thread. The tree-walk interpreter recurses on the host stack
/// (several frames per Chezzi call), so it runs on a dedicated large-stack thread; this decouples
/// the recursion limit from the caller's (possibly small, e.g. 2 MB test) thread stack.
const INTERP_STACK_BYTES: usize = 256 * 1024 * 1024;

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
    let module = match parsed {
        Ok(m) => m,
        Err(e) => return (String::new(), Err(e)),
    };

    let mut interp = Interp::new();
    let result = interp.execute(&module.stmts);
    (interp.out, result)
}

/// Run a multi-file program from its entry path: resolve the dependency graph, evaluate each
/// module once in dependency order, then run the entry's `main()`. Output produced so far is
/// preserved alongside the outcome (so the CLI can print partial output before an error).
pub fn run_file(entry: &std::path::Path) -> (String, Result<(), RuntimeError>) {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(INTERP_STACK_BYTES)
        .spawn(move || run_file_inner(&entry))
        .expect("failed to spawn interpreter thread")
        .join()
        .expect("interpreter thread panicked")
}

fn run_file_inner(entry: &std::path::Path) -> (String, Result<(), RuntimeError>) {
    let graph = match crate::resolver::build_graph(entry) {
        Ok(g) => g,
        Err(e) => {
            return (String::new(), Err(RuntimeError { message: e.message, span: e.span }));
        }
    };
    let mut interp = Interp::new();
    // Modules are in load order: dependencies first, entry last.
    for lm in &graph.modules {
        if let Err(e) = interp.eval_module(lm) {
            return (interp.out, Err(e));
        }
    }
    (interp.out, Ok(()))
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
    parser::parse_expr(tokens).map_err(|e| RuntimeError {
        message: e.message,
        span: e.span,
    })
}

/// Apply a binary operator to two already-evaluated operands.
fn eval_binary(op: BinaryOp, l: Value, r: Value, span: Span) -> Result<Value, RuntimeError> {
    use BinaryOp::*;
    use Value::{Float, Int, Str};

    let type_err = |op: BinaryOp, l: &Value, r: &Value| RuntimeError {
        message: format!("cannot apply {op:?} to {} and {}", l.type_name(), r.type_name()),
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
        Eq => Ok(Value::Bool(values_equal(&l, &r))),
        NotEq => Ok(Value::Bool(!values_equal(&l, &r))),
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

/// If `v` is an unhandled error (`Err(..)` or `None`) reaching the top level, build the runtime
/// error that exits the program. Mirrors the VM's `top_level_error` — keep the message identical.
fn top_level_error(v: &Value, span: Span) -> Option<RuntimeError> {
    let Value::Enum { ty, variant, payload } = v else { return None };
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
    Some(RuntimeError { message: format!("unhandled error: {detail}"), span })
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
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            as_f64(l).partial_cmp(&as_f64(r))
        }
        _ => None,
    }
}

/// Structural equality for `==` / `!=`. Cross-type compares are simply unequal.
fn values_equal(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            as_f64(l) == as_f64(r)
        }
        _ => l == r,
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
        let src = "total := 0\nfor i in 0..10:\n    if i % 2 == 0:\n        total += i\nprint(total)\n";
        assert_eq!(run(src), "20\n");
    }

    #[test]
    fn for_iterates_a_list() {
        assert_eq!(run("for x in [10, 20, 30]:\n    print(x)\n"), "10\n20\n30\n");
    }

    #[test]
    fn for_loop_var_does_not_leak() {
        assert!(run_capture("for i in 0..3:\n    print(i)\nprint(i)\n").is_err());
    }

    #[test]
    fn while_loops_until_false() {
        assert_eq!(run("n := 3\nwhile n > 0:\n    print(n)\n    n -= 1\n"), "3\n2\n1\n");
    }

    #[test]
    fn call_function_with_params() {
        assert_eq!(run("fn add(a: int, b: int) -> int:\n    return a + b\nprint(add(2, 3))\n"), "5\n");
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
        assert_eq!(run("double := fn(x: int) -> int: x * 2\nprint(double(5))\n"), "10\n");
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
        assert_eq!(run(&format!("{POINT}p := Point(3, 4)\nprint(p.x)\nprint(p.y)\n")), "3\n4\n");
    }

    #[test]
    fn struct_method_binds_self() {
        assert_eq!(run(&format!("{POINT}p := Point(3, 4)\nprint(p.sum())\n")), "7\n");
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
        Circle(r): return r * r
        Square(n): return n * n
";

    #[test]
    fn enum_variant_with_payload_and_match() {
        let src = format!("{SHAPE}print(area(Circle(3)))\nprint(area(Square(4)))\n");
        assert_eq!(run(&src), "9\n16\n");
    }

    #[test]
    fn enum_bare_variant_and_match() {
        let src = "enum Color:\n    Red\n    Green\nfn name(c: Color) -> str:\n    match c:\n        Red: return \"r\"\n        Green: return \"g\"\nprint(name(Red))\nprint(name(Green))\n";
        assert_eq!(run(src), "r\ng\n");
    }

    #[test]
    fn match_without_matching_arm_errors() {
        let src = format!("{SHAPE}fn f(s: Shape):\n    match s:\n        Circle(r): print(r)\nf(Square(2))\n");
        assert!(run_capture(&src).is_err());
    }

    #[test]
    fn enum_variant_arity_mismatch_errors() {
        assert!(run_capture(&format!("{SHAPE}print(area(Circle(1, 2)))\n")).is_err());
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
        assert_eq!(run("name := \"thuan\"\nprint(\"hi {name}\")\n"), "hi thuan\n");
    }

    #[test]
    fn interpolation_evaluates_expression() {
        assert_eq!(run("a := 2\nb := 3\nprint(\"sum: {a + b}\")\n"), "sum: 5\n");
    }

    #[test]
    fn interpolation_escapes_double_braces() {
        assert_eq!(run("print(\"brace: {{not}} {1 + 1}\")\n"), "brace: {not} 2\n");
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
        assert_eq!(run("xs := [1, 2]\nxs.push(3)\nprint(xs)\nprint(xs.len())\n"), "[1, 2, 3]\n3\n");
    }

    #[test]
    fn str_method_wrong_arity_errors() {
        let e = run_capture("print(\"hi\".upper(\"extra\"))\n").unwrap_err();
        assert!(e.message.contains("upper() expects 0 argument(s), got 1"), "{}", e.message);
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
    fn builtin_sqrt() {
        assert_eq!(run("print(sqrt(25.0))\n"), "5.0\n");
        assert_eq!(run("print(sqrt(9))\n"), "3.0\n");
    }

    #[test]
    fn builtin_wrong_arity_errors() {
        assert!(run_capture("print(len())\n").is_err());
        assert!(run_capture("print(sqrt(1, 2))\n").is_err());
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

    #[test]
    fn list_indexing() {
        assert_eq!(eval("[10, 20, 30][1]"), Value::Int(20));
        assert_eq!(run("xs := [1, 2, 3]\nprint(xs[2])\n"), "3\n");
    }

    #[test]
    fn list_index_out_of_bounds_errors() {
        assert!(run_capture("print([1, 2, 3][5])\n").is_err());
        assert!(run_capture("print([1, 2, 3][-1])\n").is_err());
    }

    #[test]
    fn string_indexing() {
        assert_eq!(run("s := \"abc\"\nprint(s[1])\n"), "b\n");
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
        assert_eq!(run(&format!("{POINT}print(Point(3, 4))\n")), "Point(x=3, y=4)\n");
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

    /// M6 golden: core-type methods + pipe produce exactly the expected output on the interp.
    #[test]
    fn golden_methods_chz() {
        let source = include_str!("../../examples/methods.chz");
        let expected = include_str!("../../examples/methods.expected");
        assert_eq!(run_capture(source).expect("methods.chz should run"), expected);
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
    fn bare_top_level_none_exits() {
        let e = run_capture("fn g() -> Option[int]:\n    return None\ng()\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: None");
    }

    #[test]
    fn user_enum_named_err_is_not_a_top_level_error() {
        // A user enum that shadows `Err` is a normal value — a bare one must NOT exit (only the
        // builtin Result's Err does). Gating is on the type, not the bare variant name.
        assert_eq!(run("enum Signal:\n    Err(int)\n    Quiet\nErr(5)\nprint(\"made it\")\n"), "made it\n");
    }

    #[test]
    fn try_on_user_enum_named_err_is_type_error_not_propagation() {
        // `?` must reject a user `Err` (not of type Result/Option), not silently propagate it.
        let e = run_capture(
            "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := Err(5)?\n    return x\nf()\n",
        )
        .unwrap_err();
        assert!(e.message.contains("expects Result or Option"), "{}", e.message);
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
            let dir = std::env::temp_dir().join(format!("chezzi_interp_{}_{}", std::process::id(), n));
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
        let (out, res) = run_file(entry);
        res.expect("run_file should succeed");
        out
    }

    /// Run an entry file, asserting it fails, returning the error message.
    fn run_err(entry: &std::path::Path) -> String {
        let (_out, res) = run_file(entry);
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
        let entry = t.write("main.chz", "import helper from b\nfn main(): print(helper())\nmain()\n");
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
        let entry = t.write("main.chz", "import f, g from lib\nfn main():\n    f()\n    g()\nmain()\n");
        assert_eq!(run_ok(&entry), "f\ng\n");
    }

    // 11. `import f as h from a`: the alias is bound, the original name is not.
    #[test]
    fn from_named_import_alias() {
        let t = TmpDir::new();
        t.write("lib.chz", "fn f(): print(\"f\")\n");
        let entry = t.write("main.chz", "import f as h from lib\nfn main(): h()\nmain()\n");
        assert_eq!(run_ok(&entry), "f\n");

        let bad = t.write("bad.chz", "import f as h from lib\nfn main(): f()\nmain()\n");
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
        assert_eq!(out.matches("init C").count(), 1, "C init ran more than once: {out:?}");
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
        t.write("lib.chz", "fn main(): print(\"lib main\")\nfn f(): print(\"f\")\n");
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
}
