//! Tree-walk interpreter (M3): executes an AST `Module` directly — the reference semantics for
//! Chezzi before the bytecode VM (M5). Single-file programs run here.

use crate::ast::{
    AssignOp, BinaryOp, Expr, ExprKind, FnDecl, LitPattern, MatchArm, MatchExprArm, Pattern,
    Span, Stmt, StmtKind, UnaryOp,
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
    /// Captured stderr (written by `std.io.eprint`). Separate from `out` so streams don't mix;
    /// the CLI flushes it to the real stderr.
    stderr: String,
    /// Runtime configuration the native std modules read (args/env/stdin). Default = inert.
    host: crate::native::HostConfig,
    structs: std::collections::HashMap<String, std::rc::Rc<StructDef>>,
    /// Struct name → declared fields (with types), for building `json.decode` descriptors.
    struct_fields: std::collections::HashMap<String, Vec<crate::ast::Field>>,
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
            stderr: String::new(),
            host: crate::native::HostConfig::default(),
            structs: std::collections::HashMap::new(),
            struct_fields: std::collections::HashMap::new(),
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
                // Operator overloading: ordering (`< <= > >=`) on two structs dispatches to the
                // type's `compare(self, other) -> int` method (the `Comparable` protocol). The
                // checker has already verified conformance. Equality stays structural (handled by
                // `eval_binary`); only ordering is overloaded.
                if matches!(op, BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq)
                    && matches!((&l, &r), (Value::Struct { .. }, Value::Struct { .. }))
                {
                    return self.struct_ordering(*op, l, r, expr.span);
                }
                // Arithmetic overloading: `+`/`-`/`*` on two structs dispatch to `add`/`sub`/`mul`
                // (the `Add`/`Sub`/`Mul` protocols). The checker has verified conformance.
                if matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul)
                    && matches!((&l, &r), (Value::Struct { .. }, Value::Struct { .. }))
                {
                    return self.struct_arith(*op, l, r, expr.span);
                }
                eval_binary(*op, l, r, expr.span)
            }
            ExprKind::List(items) => {
                let vals = items
                    .iter()
                    .map(|e| self.eval(e))
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
                for (k_expr, v_expr) in entries {
                    let k = self.eval(k_expr)?;
                    let v = self.eval(v_expr)?;
                    let hk = self.hash_value(&k, expr.span)?;
                    match map.candidates(hk).iter().copied().find(|&p| values_equal(&map.entries[p].1, &k)) {
                        Some(i) => map.entries[i].2 = v,
                        None => map.push(hk, k, v),
                    }
                }
                Ok(Value::Map(std::rc::Rc::new(std::cell::RefCell::new(map))))
            }
            ExprKind::Set(elems) => {
                let mut set = SetData::default();
                for e in elems {
                    let v = self.eval(e)?;
                    let hv = self.hash_value(&v, expr.span)?;
                    if !set.candidates(hv).iter().copied().any(|p| values_equal(&set.entries[p].1, &v)) {
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
                let target = self.eval(obj)?;
                match &target {
                    // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
                    Value::Tuple(items) => match name.parse::<usize>().ok().and_then(|i| items.get(i)) {
                        Some(v) => Ok(v.clone()),
                        None => Err(RuntimeError {
                            message: format!("tuple has no element '.{name}' (len {})", items.len()),
                            span: expr.span,
                        }),
                    },
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
                        usize::try_from(idx)
                            .ok()
                            .and_then(|i| items.get(i).cloned())
                            .ok_or_else(|| RuntimeError {
                                message: format!("index {idx} out of bounds (len {})", items.len()),
                                span: expr.span,
                            })
                    }
                    Value::Str(s) => {
                        let idx = want_int(self.eval(index)?)?;
                        let chars: Vec<char> = s.chars().collect();
                        usize::try_from(idx)
                            .ok()
                            .and_then(|i| chars.get(i).copied())
                            .map(|c| Value::Str(c.to_string().into()))
                            .ok_or_else(|| RuntimeError {
                                message: format!("index {idx} out of bounds (len {})", chars.len()),
                                span: expr.span,
                            })
                    }
                    other => Err(RuntimeError {
                        message: format!("cannot index {}", other.type_name()),
                        span: expr.span,
                    }),
                }
            }
            ExprKind::Call { callee, args } => self.eval_call(callee, args, expr.span),
            ExprKind::Match { scrutinee, arms } => self.eval_match_expr(scrutinee, arms),
            ExprKind::IfElse { cond, then, els } => {
                if as_bool(self.eval(cond)?, cond.span)? {
                    self.eval(then)
                } else {
                    self.eval(els)
                }
            }
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
            ExprKind::DecodeCall { obj, ty, arg } => {
                // Reuse the json module's `parse` (obj.parse(arg) → Result[Json]), then coerce.
                let parse_call = Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            kind: ExprKind::Field { obj: obj.clone(), name: "parse".to_string() },
                            span: expr.span,
                        }),
                        args: vec![(**arg).clone()],
                    },
                    span: expr.span,
                };
                let res = self.eval(&parse_call)?;
                let desc = crate::json_decode::from_type(ty, &self.struct_fields, &mut Vec::new())
                    .map_err(|message| RuntimeError { message, span: expr.span })?;
                match &res {
                    Value::Enum { ty: rty, variant, payload }
                        if rty.as_ref() == "Result" && variant.as_ref() == "Ok" && payload.len() == 1 =>
                    {
                        let jv = payload[0].clone();
                        match self.coerce_json(&jv, &desc, "$") {
                            Ok(v) => Ok(enum_val("Result", "Ok", vec![v])),
                            Err(msg) => Ok(enum_val("Result", "Err", vec![Value::Str(msg.into())])),
                        }
                    }
                    // Parse error: the Result Err(str) is already a valid Result[T].
                    Value::Enum { ty: rty, variant, .. }
                        if rty.as_ref() == "Result" && variant.as_ref() == "Err" =>
                    {
                        Ok(res)
                    }
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
        let Value::Enum { ty, variant, payload } = jv else {
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
            D::Float => Ok(Value::Float(json_num(variant, payload).ok_or_else(|| mismatch("float"))?)),
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
                    Ok(enum_val("Option", "Some", vec![self.coerce_json(jv, inner, path)?]))
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
                    out.push(*hk, k.clone(), self.coerce_json(v, inner, &format!("{path}.{key}"))?);
                }
                Ok(Value::Map(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            D::Struct { name, fields } => {
                if variant.as_ref() != "Obj" {
                    return Err(mismatch(&format!("object for {name}")));
                }
                let Value::Map(entries) = payload.first().cloned().unwrap_or(Value::Nil) else {
                    return Err(mismatch("object"));
                };
                let src = entries.borrow().clone();
                let mut field_vals: Vec<(String, Value)> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = src.entries.iter().find(|(_, k, _)| matches!(k, Value::Str(s) if s.as_ref() == fname));
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
                Ok(Value::Struct {
                    name: name.clone().into(),
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
                    let expr = parse_expr_str(&inner)?;
                    let value = self.eval(&expr)?;
                    out.push_str(&self.stringify(&value, span)?);
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
                let mut parts = Vec::with_capacity(arg_vals.len());
                for v in &arg_vals {
                    parts.push(self.stringify(v, span)?);
                }
                self.out.push_str(&parts.join(" "));
                self.out.push('\n');
                return Ok(Value::Nil);
            }
            // `str(x)` dispatches to a `Stringable` struct's `str` method (else default repr).
            // Arity ≠ 1 falls through to the builtin so its arity error is preserved.
            if name == "str" && arg_vals.len() == 1 {
                let s = self.stringify(&arg_vals[0], span)?;
                return Ok(Value::Str(s.into()));
            }
            // `set(list)` can take a list of structs whose `hash()` re-enters the engine, so it can't
            // live in the pure `builtins` table — route it here. Other builtins stay pure.
            if name == "set" {
                return self.builtin_set(arg_vals, span);
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
            Value::Native(e) => self.call_native(e.func, args, span),
            other => Err(RuntimeError {
                message: format!("'{}' is not callable", other.type_name()),
                span,
            }),
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
        };
        let ret = func(&mut host).map_err(|e| RuntimeError { message: e.message, span })?;
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
            if let Some(binds) = try_bind(&arm.pattern, &value) {
                self.env.push();
                for (name, v) in binds {
                    self.env.define(&name, v);
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
            if let Some(binds) = try_bind(&arm.pattern, &value) {
                self.env.push();
                for (name, v) in binds {
                    self.env.define(&name, v);
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
        // Higher-order list methods call a Chezzi function value per element, so they need the
        // interpreter handle (`self.call_value`) and can't live in the pure `builtins` table.
        if let Value::List(items) = &receiver
            && matches!(method, "map" | "filter" | "fold" | "sort_by")
        {
            // Clone the elements out so we don't hold the `RefCell` borrow across `call_value`
            // (the closure body could re-borrow this same list).
            let elems: Vec<Value> = items.borrow().clone();
            if method == "sort_by" {
                // `sort_by` sorts in place; keep the `Rc` so we can write the result back.
                let list = std::rc::Rc::clone(items);
                return self.eval_list_sort_by(list, elems, arg_vals, span);
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

    /// Render a value the way `print` / `str()` / `{…}` interpolation should: a struct that defines
    /// `str(self) -> str` (the `Stringable` protocol) dispatches to that method; everything else
    /// uses the default structural repr, recursing through `stringify` so a struct nested in a list
    /// / tuple / map / set / enum payload still honours the protocol. Mirrors `Value`'s `Display`
    /// for the non-dispatch cases (kept in lock-step with the VM's `stringify`, parity-tested).
    fn stringify(&mut self, v: &Value, span: Span) -> Result<String, RuntimeError> {
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
                    return self.stringify(&res?, span);
                }
                let parts = fields.borrow().clone();
                let mut rendered = Vec::with_capacity(parts.len());
                for (k, fv) in &parts {
                    rendered.push(format!("{k}={}", self.stringify(fv, span)?));
                }
                Ok(format!("{name}({})", rendered.join(", ")))
            }
            Value::List(items) => {
                let elems = items.borrow().clone();
                Ok(format!("[{}]", self.stringify_seq(&elems, span)?))
            }
            Value::Tuple(items) => {
                let elems = (**items).clone();
                Ok(format!("({})", self.stringify_seq(&elems, span)?))
            }
            Value::Map(m) => {
                let entries = m.borrow().entries.clone();
                let mut rendered = Vec::with_capacity(entries.len());
                for (_, k, mv) in &entries {
                    rendered.push(format!("{}: {}", self.stringify(k, span)?, self.stringify(mv, span)?));
                }
                Ok(format!("{{{}}}", rendered.join(", ")))
            }
            Value::Set(s) => {
                let entries = s.borrow().entries.clone();
                if entries.is_empty() {
                    Ok("set()".to_string())
                } else {
                    let elems: Vec<Value> = entries.into_iter().map(|(_, e)| e).collect();
                    Ok(format!("{{{}}}", self.stringify_seq(&elems, span)?))
                }
            }
            Value::Enum { variant, payload, .. } => {
                if payload.is_empty() {
                    Ok(variant.to_string())
                } else {
                    Ok(format!("{variant}({})", self.stringify_seq(payload, span)?))
                }
            }
            // Scalars, functions, modules — no protocol dispatch; reuse `Display`.
            other => Ok(other.to_string()),
        }
    }

    /// `stringify` each element and join with `, ` (shared by list/tuple/set/enum-payload).
    fn stringify_seq(&mut self, elems: &[Value], span: Span) -> Result<String, RuntimeError> {
        let mut rendered = Vec::with_capacity(elems.len());
        for e in elems {
            rendered.push(self.stringify(e, span)?);
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
    fn struct_arith(&mut self, op: BinaryOp, l: Value, r: Value, span: Span) -> Result<Value, RuntimeError> {
        let method = match op {
            BinaryOp::Add => "add",
            BinaryOp::Sub => "sub",
            BinaryOp::Mul => "mul",
            _ => unreachable!("struct_arith only handles + - *"),
        };
        let Value::Struct { name, .. } = &l else { unreachable!() };
        let def = self.structs.get(name.as_ref()).cloned().ok_or_else(|| RuntimeError {
            message: format!("unknown struct type '{name}'"),
            span,
        })?;
        let decl = def.methods.get(method).cloned().ok_or_else(|| RuntimeError {
            message: format!("struct '{name}' has no '{method}' method"),
            span,
        })?;
        self.call(&decl, &def.home, vec![l, r], span)
    }

    /// Call a struct's `compare(self, other) -> int` method and return the resulting `Ordering`.
    /// Shared by ordering operators (`struct_ordering`) and `list.sort()` over Comparable structs.
    fn struct_compare(
        &mut self,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        let Value::Struct { name, .. } = &l else { unreachable!() };
        let def = self.structs.get(name.as_ref()).cloned().ok_or_else(|| RuntimeError {
            message: format!("unknown struct type '{name}'"),
            span,
        })?;
        let decl = def.methods.get("compare").cloned().ok_or_else(|| RuntimeError {
            message: format!("struct '{name}' has no 'compare' method (needed to order its values)"),
            span,
        })?;
        match self.call(&decl, &def.home, vec![l, r], span)? {
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
            Value::Str(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Nil => {
                Ok(scalar_hash(v))
            }
            other => Err(RuntimeError {
                message: format!("{} is not hashable (cannot be a map/set key)", other.type_name()),
                span,
            }),
        }
    }

    /// Dispatch a struct key's user `hash(self) -> int`, returning its `i64` as a `u64`. Mirrors
    /// [`struct_compare`].
    fn struct_hash(&mut self, v: &Value, span: Span) -> Result<u64, RuntimeError> {
        let Value::Struct { name, .. } = v else { unreachable!() };
        let def = self.structs.get(name.as_ref()).cloned().ok_or_else(|| RuntimeError {
            message: format!("unknown struct type '{name}'"),
            span,
        })?;
        let decl = def.methods.get("hash").cloned().ok_or_else(|| RuntimeError {
            message: format!("struct '{name}' has no 'hash' method (needed to use it as a map/set key)"),
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

    /// `set()` → empty set; `set(list)` → a deduped hash set of the list's elements. On `Interp`
    /// (not `builtins`) because a struct element's `hash()` re-enters the engine. Mirrors
    /// `vm::Vm::builtin_set`.
    fn builtin_set(&mut self, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let src: Vec<Value> = match args.as_slice() {
            [] => Vec::new(),
            [Value::List(items)] => items.borrow().clone(),
            [other] => {
                return Err(RuntimeError {
                    message: format!("set() expects a list, got {}", other.type_name()),
                    span,
                });
            }
            _ => {
                return Err(RuntimeError {
                    message: format!("set() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                });
            }
        };
        let mut set = SetData::default();
        for v in src {
            let hv = self.hash_value(&v, span)?;
            if !set.candidates(hv).iter().copied().any(|p| values_equal(&set.entries[p].1, &v)) {
                set.push(hv, v);
            }
        }
        Ok(Value::Set(std::rc::Rc::new(std::cell::RefCell::new(set))))
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
        let some = |v: Value| Value::Enum { ty: "Option".into(), variant: "Some".into(), payload: vec![v] };
        let none = || Value::Enum { ty: "Option".into(), variant: "None".into(), payload: vec![] };
        match method {
            "len" => {
                builtins::arity("len", &args, 0, span)?;
                Ok(Value::Int(m.borrow().entries.len() as i64))
            }
            "has" => {
                builtins::arity("has", &args, 1, span)?;
                let hk = self.hash_value(&args[0], span)?; // hash before borrowing (re-entrant)
                let mm = m.borrow();
                Ok(Value::Bool(mm.candidates(hk).iter().any(|&p| values_equal(&mm.entries[p].1, &args[0]))))
            }
            "get" => {
                builtins::arity("get", &args, 1, span)?;
                let hk = self.hash_value(&args[0], span)?;
                let found = {
                    let mm = m.borrow();
                    mm.candidates(hk).iter().copied()
                        .find(|&p| values_equal(&mm.entries[p].1, &args[0]))
                        .map(|p| mm.entries[p].2.clone())
                };
                Ok(found.map(some).unwrap_or_else(none))
            }
            "keys" => {
                builtins::arity("keys", &args, 0, span)?;
                let keys: Vec<Value> = m.borrow().entries.iter().map(|(_, k, _)| k.clone()).collect();
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(keys))))
            }
            "values" => {
                builtins::arity("values", &args, 0, span)?;
                let vals: Vec<Value> = m.borrow().entries.iter().map(|(_, _, v)| v.clone()).collect();
                Ok(Value::List(std::rc::Rc::new(std::cell::RefCell::new(vals))))
            }
            "remove" => {
                builtins::arity("remove", &args, 1, span)?;
                let hk = self.hash_value(&args[0], span)?;
                let pos = {
                    let mm = m.borrow();
                    mm.candidates(hk).iter().copied().find(|&p| values_equal(&mm.entries[p].1, &args[0]))
                };
                match pos {
                    Some(i) => {
                        let (_, _, v) = m.borrow_mut().remove_at(i);
                        Ok(some(v))
                    }
                    None => Ok(none()),
                }
            }
            _ => Err(RuntimeError { message: format!("type map has no method '{method}'"), span }),
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
                Ok(Value::Bool(ss.candidates(hx).iter().any(|&p| values_equal(&ss.entries[p].1, &args[0]))))
            }
            "add" => {
                builtins::arity("add", &args, 1, span)?;
                let hx = self.hash_value(&args[0], span)?;
                let present = {
                    let ss = s.borrow();
                    ss.candidates(hx).iter().any(|&p| values_equal(&ss.entries[p].1, &args[0]))
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
                    ss.candidates(hx).iter().copied().find(|&p| values_equal(&ss.entries[p].1, &args[0]))
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
                        message: format!("{method}() expects a set argument, got {}", args[0].type_name()),
                        span,
                    });
                };
                // Both operands carry per-element cached hashes — no re-hashing, no re-entry.
                let mine = s.borrow().entries.clone();
                let other = other.borrow().clone();
                let mut out = SetData::default();
                let add = |out: &mut SetData, he: u64, e: &Value| {
                    if !out.candidates(he).iter().any(|&p| values_equal(&out.entries[p].1, e)) {
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
                            let in_other = other.candidates(*he).iter().any(|&p| values_equal(&other.entries[p].1, e));
                            if in_other == keep_when_present {
                                add(&mut out, *he, e);
                            }
                        }
                    }
                }
                Ok(Value::Set(std::rc::Rc::new(std::cell::RefCell::new(out))))
            }
            _ => Err(RuntimeError { message: format!("type set has no method '{method}'"), span }),
        }
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
                message: format!("sort_by comparator must return int, got {}", other.type_name()),
                span,
            }),
        }
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
            // A function body falling off the end (or a stray break/continue the checker would
            // have rejected) yields nil.
            Flow::Normal | Flow::Break | Flow::Continue => Ok(Value::Nil),
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
        // A `?` that propagated to the top (no enclosing function) is an unhandled error. The
        // propagation marker still carries the `?`'s `expr.span`, so report at the real location
        // (matching the bare-expr path and the VM) rather than a hard-coded line 1.
        if let Some(value) = self.propagating.take() {
            let span = result.as_ref().err().map(|e| e.span).unwrap_or(Span { line: 1, col: 1 });
            return Err(top_level_error(&value, span).unwrap_or_else(|| RuntimeError {
                message: format!("unhandled error: {value}"),
                span,
            }));
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
                        Value::Native(value::NativeFnEntry { name: (*mname).into(), func: *func }),
                    );
                }
                for (cname, cval) in crate::native::native_consts(name) {
                    m.insert(cname.to_string(), Value::Float(*cval));
                }
            }
            self.namespaces.insert(
                lm.id.clone(),
                std::rc::Rc::new(value::ModuleNamespace { name: name.into(), members }),
            );
            return Ok(());
        }

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
                    ..
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
                    self.struct_fields.insert(name.clone(), fields.clone());
                }
                // Type-erased: type parameters are checker-only (identical runtime per instantiation).
                StmtKind::Enum { name, variants, .. } => {
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
                // Any non-normal flow (return/break/continue) short-circuits the block and
                // propagates up to the enclosing loop (or function, for `return`).
                other => return Ok(other),
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
            StmtKind::Let { names, value, .. } => {
                let v = self.eval(value)?;
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
            | StmtKind::Protocol { .. }
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
    fn exec_for(&mut self, vars: &[String], iter: &Expr, body: &[Stmt]) -> Result<Flow, RuntimeError> {
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
        // Materialize the per-iteration value tuples up front (clone out) so a body that mutates the
        // collection doesn't disturb iteration, and no borrow is held across the body.
        let rows: Vec<Vec<Value>> = match self.eval(iter)? {
            Value::List(items) => items.borrow().iter().map(|v| vec![v.clone()]).collect(),
            Value::Map(m) => m
                .borrow()
                .entries
                .iter()
                .map(|(_, k, v)| {
                    if vars.len() == 2 {
                        vec![k.clone(), v.clone()]
                    } else {
                        vec![k.clone()]
                    }
                })
                .collect(),
            Value::Set(s) => s.borrow().entries.iter().map(|(_, e)| vec![e.clone()]).collect(),
            // Strings iterate as 1-char strings (Python-style; the checker binds a single str var).
            Value::Str(s) => s.chars().map(|c| vec![Value::Str(c.to_string().into())]).collect(),
            other => {
                return Err(RuntimeError {
                    message: format!("cannot iterate over {}", other.type_name()),
                    span: iter.span,
                });
            }
        };
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
                let new_val = match op {
                    AssignOp::Eq => rhs,
                    AssignOp::PlusEq | AssignOp::MinusEq => {
                        let cur = self.env.get(name).ok_or_else(|| RuntimeError {
                            message: format!("undefined name '{name}'"),
                            span,
                        })?;
                        let bin = if op == AssignOp::PlusEq { BinaryOp::Add } else { BinaryOp::Sub };
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
                    let new_val = match op {
                        AssignOp::Eq => self.eval(value)?,
                        AssignOp::PlusEq | AssignOp::MinusEq => {
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
                            let bin = if op == AssignOp::PlusEq { BinaryOp::Add } else { BinaryOp::Sub };
                            eval_binary(bin, cur, rhs, span)?
                        }
                    };
                    let pos = {
                        let m = entries.borrow();
                        m.candidates(hk).iter().copied().find(|&p| values_equal(&m.entries[p].1, &key))
                    };
                    match pos {
                        Some(i) => entries.borrow_mut().entries[i].2 = new_val,
                        None => entries.borrow_mut().push(hk, key, new_val),
                    }
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
                    usize::try_from(idx).ok().filter(|i| *i < len).ok_or_else(|| RuntimeError {
                        message: format!("index {idx} out of bounds (len {len})"),
                        span,
                    })
                };
                let new_val = match op {
                    AssignOp::Eq => self.eval(value)?,
                    AssignOp::PlusEq | AssignOp::MinusEq => {
                        let i = bounds_check(idx, items.borrow().len())?;
                        let cur = items.borrow()[i].clone();
                        let rhs = self.eval(value)?;
                        let bin = if op == AssignOp::PlusEq { BinaryOp::Add } else { BinaryOp::Sub };
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
                        message: format!("cannot assign field '{name}' of {}", target_val.type_name()),
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
                let new_val = match op {
                    AssignOp::Eq => rhs,
                    AssignOp::PlusEq => eval_binary(BinaryOp::Add, fields.borrow()[pos].1.clone(), rhs, span)?,
                    AssignOp::MinusEq => eval_binary(BinaryOp::Sub, fields.borrow()[pos].1.clone(), rhs, span)?,
                };
                fields.borrow_mut()[pos].1 = new_val;
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
            return (String::new(), String::new(), Err(RuntimeError { message: e.message, span: e.span }));
        }
    };
    let mut interp = Interp::new();
    interp.host = cfg;
    // Modules are in load order: dependencies first, entry last.
    for lm in &graph.modules {
        if let Err(e) = interp.eval_module(lm) {
            return (interp.out, interp.stderr, Err(e));
        }
    }
    (interp.out, interp.stderr, Ok(()))
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

/// The interpreter's [`crate::native::Host`] adapter: lets a native fn read the evaluated `Value`
/// arguments and write to the captured output buffers. Borrows only the fields it needs so it can
/// be built inside an `&mut self` method. (Stdin / args / env / cooperative-exit are wired in a
/// later milestone; the unwired methods return inert defaults — empty stdin/env, real cwd.)
struct InterpHost<'a> {
    args: Vec<Value>,
    out: &'a mut String,
    stderr: &'a mut String,
    cfg: &'a mut crate::native::HostConfig,
}

impl crate::native::Host for InterpHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => Err(crate::native::HostError::arg_type(i, "int", other.type_name())),
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
            Some(other) => Err(crate::native::HostError::arg_type(i, "float", other.type_name())),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Str(s)) => Ok(s.to_string()),
            Some(other) => Err(crate::native::HostError::arg_type(i, "str", other.type_name())),
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
            .map_err(|e| crate::native::HostError { message: e.to_string() })
    }
}

/// Lower a native fn's engine-neutral [`crate::native::NativeRet`] into an interpreter `Value`.
/// `Ok`/`Err`/`Some`/`None` become the built-in `Result` / `Option` enum values.
fn lower_native(ret: crate::native::NativeRet) -> Value {
    use crate::native::NativeRet as N;
    match ret {
        N::Int(n) => Value::Int(n),
        N::Float(f) => Value::Float(f),
        N::Bool(b) => Value::Bool(b),
        N::Str(s) => Value::Str(s.into()),
        N::List(items) => {
            let vs = items.into_iter().map(lower_native).collect();
            Value::List(std::rc::Rc::new(std::cell::RefCell::new(vs)))
        }
        N::Struct { name, fields } => {
            let fs = fields.into_iter().map(|(k, v)| (k, lower_native(v))).collect();
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
    Value::Enum { ty: ty.into(), variant: variant.into(), payload }
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
                        if op == Shl { a << (*b as u32) } else { a >> (*b as u32) }
                    }
                    _ => unreachable!(),
                };
                Ok(Int(v))
            }
            _ => Err(type_err(op, &l, &r)),
        },
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
        _ => 0,
    }
}

pub(super) fn values_equal(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            as_f64(l) == as_f64(r)
        }
        // Sets are unordered: equal iff same size and every element of one is in the other.
        (Value::Set(a), Value::Set(b)) => {
            let (a, b) = (a.borrow(), b.borrow());
            a.entries.len() == b.entries.len()
                && a.entries.iter().all(|(_, x)| b.entries.iter().any(|(_, y)| values_equal(x, y)))
        }
        _ => l == r,
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
/// clean "cannot match on …" error rather than silently falling through). A nullary `Variant`
/// (`None`, `Red`) and a variant-with-payload both do; literals/wildcards/tuples don't.
fn pattern_needs_enum(pattern: &Pattern) -> bool {
    matches!(pattern, Pattern::Variant { .. })
}

/// Try to match `value` against `pattern`, returning the name→value bindings to install on success,
/// or `None` on a mismatch. Recurses through nested tuple/variant patterns (gap #15). The program is
/// type-checked, so a shape mismatch here is a genuine value mismatch (a different variant / a
/// non-matching literal / a different tuple shape), not a type error.
fn try_bind(pattern: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
    match pattern {
        Pattern::Wildcard => Some(Vec::new()),
        Pattern::Ident(name) => Some(vec![(name.clone(), value.clone())]),
        Pattern::Literal(lit) => literal_matches(lit, value).then(Vec::new),
        Pattern::Tuple(subs) => {
            let Value::Tuple(elems) = value else { return None };
            if elems.len() != subs.len() {
                return None;
            }
            let mut out = Vec::new();
            for (sub, v) in subs.iter().zip(elems.iter()) {
                out.extend(try_bind(sub, v)?);
            }
            Some(out)
        }
        Pattern::Variant { name, bindings } => {
            let Value::Enum { variant, payload, .. } = value else { return None };
            if name != variant.as_ref() || bindings.len() != payload.len() {
                return None;
            }
            let mut out = Vec::new();
            for (sub, v) in bindings.iter().zip(payload.iter()) {
                out.extend(try_bind(sub, v)?);
            }
            Some(out)
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
            Value::Native(value::NativeFnEntry { name: "add".into(), func: add }),
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
            Value::Native(value::NativeFnEntry { name: "boom".into(), func: boom }),
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
    fn str_conforms_to_error_message_returns_self() {
        // `str` is an `Error` (Go-style): `.message()` yields the string itself.
        assert_eq!(run("print(\"boom\".message())\n"), "boom\n");
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
    fn list_map_doubles() {
        assert_eq!(
            run("xs := [1,2,3]\nys := xs.map(fn(x: int) -> int: x * 2)\nprint(ys)\n"),
            "[2, 4, 6]\n"
        );
    }

    #[test]
    fn list_map_changes_type_to_str() {
        assert_eq!(
            run("xs := [1,2,3]\nys := xs.map(fn(x: int) -> str: \"n{x}\")\nfor y in ys:\n    print(y)\n"),
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
            run("xs := [\"a\",\"b\",\"c\"]\ns := xs.fold(\"\", fn(a: str, x: str) -> str: a + x)\nprint(s)\n"),
            "abc\n"
        );
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
        assert!(run_capture("xs := [1, 2, 3]\nxs[5] = 0\n").is_err());
    }

    #[test]
    fn field_assign_mutates_in_place() {
        assert_eq!(
            run("struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\nprint(p.x)\nprint(p.y)\n"),
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

    /// M8-M4 golden: the set type (literals, membership, algebra, iteration).
    #[test]
    fn golden_set_chz() {
        let source = include_str!("../../examples/set.chz");
        let expected = include_str!("../../examples/set.expected");
        assert_eq!(run_capture(source).expect("set.chz should run"), expected);
    }

    /// M1 (tier-1) golden: Python-style char handling — `s.chars()` + iterable strings.
    #[test]
    fn golden_string_iter_chz() {
        let source = include_str!("../../examples/string_iter.chz");
        let expected = include_str!("../../examples/string_iter.expected");
        assert_eq!(run_capture(source).expect("string_iter.chz should run"), expected);
    }

    /// M6 golden: core-type methods + pipe produce exactly the expected output on the interp.
    #[test]
    fn golden_methods_chz() {
        let source = include_str!("../../examples/methods.chz");
        let expected = include_str!("../../examples/methods.expected");
        assert_eq!(run_capture(source).expect("methods.chz should run"), expected);
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
        assert_eq!(run_capture(source).expect("generics.chz should run"), expected);
    }

    /// G2 golden: generic structs (Pair / Stack) on the interp.
    #[test]
    fn golden_generic_structs_chz() {
        let source = include_str!("../../examples/generic_structs.chz");
        let expected = include_str!("../../examples/generic_structs.expected");
        assert_eq!(run_capture(source).expect("generic_structs.chz should run"), expected);
    }

    /// Tier-2 golden: generic enums (Tree[T] / Either[A, B]) on the interp.
    #[test]
    fn golden_generic_enum_chz() {
        let source = include_str!("../../examples/generic_enum.chz");
        let expected = include_str!("../../examples/generic_enum.expected");
        assert_eq!(run_capture(source).expect("generic_enum.chz should run"), expected);
    }

    /// Golden: real hash-table map/set with Hashable struct keys, on the interp.
    #[test]
    fn golden_hashmap_keys_chz() {
        let source = include_str!("../../examples/hashmap_keys.chz");
        let expected = include_str!("../../examples/hashmap_keys.expected");
        assert_eq!(run_capture(source).expect("hashmap_keys.chz should run"), expected);
    }

    /// M10-G1 golden: the `Stringable` protocol — `str(self)` overrides print/str()/interpolation.
    #[test]
    fn golden_stringable_chz() {
        let source = include_str!("../../examples/stringable.chz");
        let expected = include_str!("../../examples/stringable.expected");
        assert_eq!(run_capture(source).expect("stringable.chz should run"), expected);
    }

    /// M10-G3 golden: operator overloading (`Add`/`Sub`/`Mul`) + multi-bound `T: Add + Mul`.
    #[test]
    fn golden_operators_chz() {
        let source = include_str!("../../examples/operators.chz");
        let expected = include_str!("../../examples/operators.expected");
        assert_eq!(run_capture(source).expect("operators.chz should run"), expected);
    }

    /// M10-G3 golden: transparent type aliases (`type UserId = int`).
    #[test]
    fn golden_type_alias_chz() {
        let source = include_str!("../../examples/type_alias.chz");
        let expected = include_str!("../../examples/type_alias.expected");
        assert_eq!(run_capture(source).expect("type_alias.chz should run"), expected);
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
        let e = run_capture("fn d() -> Result[int]:\n    return Err(\"x\")\nx := d()?\n").unwrap_err();
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

    // ----- gap #8: tuples + multi-return + destructuring -----

    #[test]
    fn tuple_literal_and_element_access() {
        assert_eq!(run("t := (1, 2)\nprint(t.0)\nprint(t.1)\nprint(t)\n"), "1\n2\n(1, 2)\n");
    }

    #[test]
    fn destructuring_binds_elements() {
        assert_eq!(run("a, b := (10, 20)\nprint(a + b)\n"), "30\n");
    }

    #[test]
    fn tuple_element_out_of_range_is_runtime_error() {
        let err = run_capture("t := (1, 2)\nprint(t.2)\n").expect_err("out-of-range should error");
        assert!(err.message.contains("has no element '.2'"), "{}", err.message);
    }

    /// Robustness for `--interp` (the checker normally prevents this): destructuring a non-tuple is
    /// a clean runtime error, not a panic.
    #[test]
    fn destructuring_non_tuple_is_runtime_error() {
        let err = run_capture("a, b := 5\n").expect_err("non-tuple destructure should error");
        assert!(err.message.contains("cannot destructure"), "{}", err.message);
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
        let (out, _err, res) = run_file(entry);
        res.expect("run_file should succeed");
        out
    }

    /// Run an entry file, asserting it fails, returning the error message.
    fn run_err(entry: &std::path::Path) -> String {
        let (_out, _err, res) = run_file(entry);
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
