//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching the interpreter's hoist); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Expr, ExprKind, FnDecl, Module, Param, Pattern, Span, Stmt,
    StmtKind, Type, UnaryOp,
};
use std::collections::HashMap;
use std::fmt;

pub use ty::Ty;
use ty::compatible;

/// A type error, with the source span it occurred at. Mirrors `ParseError` / `RuntimeError`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "type error ({}): {}", self.span, self.message)
    }
}

/// A function (or method) signature: parameter types and return type.
#[derive(Clone)]
struct FnSig {
    params: Vec<Ty>,
    ret: Ty,
}

/// A struct's shape: ordered `(field, type)` pairs and its methods by name.
struct StructInfo {
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
}

/// A user enum variant: which enum it belongs to and its payload field types.
#[derive(Clone)]
struct VariantInfo {
    enum_name: String,
    payload: Vec<Ty>,
}

/// Entry point: type-check a parsed module. Returns every error found, or `Ok(())` if clean.
pub fn check(module: &Module) -> Result<(), Vec<CheckError>> {
    let mut c = Checker::new();
    c.collect_names(&module.stmts);
    c.hoist(&module.stmts);
    c.push_scope();
    for stmt in &module.stmts {
        c.check_stmt(stmt);
    }
    c.pop_scope();
    if c.errors.is_empty() {
        Ok(())
    } else {
        Err(c.errors)
    }
}

struct Checker {
    errors: Vec<CheckError>,
    scopes: Vec<HashMap<String, Ty>>,
    functions: HashMap<String, FnSig>,
    structs: HashMap<String, StructInfo>,
    /// enum name → its variant names, in declaration order (for exhaustiveness).
    enums: HashMap<String, Vec<String>>,
    variants: HashMap<String, VariantInfo>,
    struct_names: std::collections::HashSet<String>,
    enum_names: std::collections::HashSet<String>,
    /// Declared return type of the function body currently being checked (`Nil` at top level).
    current_ret: Ty,
}

impl Checker {
    fn new() -> Self {
        Checker {
            errors: Vec::new(),
            scopes: Vec::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variants: HashMap::new(),
            struct_names: std::collections::HashSet::new(),
            enum_names: std::collections::HashSet::new(),
            current_ret: Ty::Nil,
        }
    }

    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.errors.push(CheckError { message: message.into(), span });
    }

    // ===== scopes =====

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.scopes.pop();
    }
    fn declare(&mut self, name: &str, ty: Ty) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
    }
    fn lookup(&self, name: &str) -> Option<Ty> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }

    // ===== pass 1: hoist declarations =====

    /// First sub-pass: learn every struct/enum *name* so `resolve_type` can recognize them even
    /// when used before their definition (or inside each other).
    fn collect_names(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Struct { name, .. } => {
                    self.struct_names.insert(name.clone());
                }
                StmtKind::Enum { name, .. } => {
                    self.enum_names.insert(name.clone());
                }
                _ => {}
            }
        }
    }

    /// Second sub-pass: resolve and register signatures, fields, and variants. Redeclarations
    /// (a name defined twice) are reported here — otherwise "last write wins" would silently
    /// mis-type or, for struct methods, panic in pass 2 on a key that no longer exists.
    fn hoist(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Fn(decl) => {
                    if self.functions.contains_key(&decl.name) {
                        self.error(s.span, format!("function '{}' is already defined", decl.name));
                    }
                    let sig = self.fn_sig(decl, s.span);
                    self.functions.insert(decl.name.clone(), sig);
                }
                StmtKind::Struct { name, fields, methods } => {
                    if self.structs.contains_key(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    let fields: Vec<(String, Ty)> = fields
                        .iter()
                        .map(|f| (f.name.clone(), self.resolve_type(&f.ty, s.span)))
                        .collect();
                    let methods = methods
                        .iter()
                        .map(|m| (m.name.clone(), self.fn_sig(m, s.span)))
                        .collect();
                    self.structs.insert(name.clone(), StructInfo { fields, methods });
                }
                StmtKind::Enum { name, variants } => {
                    if self.enums.contains_key(name) {
                        self.error(s.span, format!("type '{name}' is already defined"));
                    }
                    let mut names = Vec::new();
                    for v in variants {
                        // Variants are keyed globally by bare name; a name shared across two enums
                        // would otherwise collapse and mis-type. Reject the collision outright.
                        if self.variants.contains_key(&v.name) {
                            self.error(s.span, format!("variant '{}' is already defined", v.name));
                        }
                        names.push(v.name.clone());
                        let payload =
                            v.payload.iter().map(|t| self.resolve_type(t, s.span)).collect();
                        self.variants.insert(
                            v.name.clone(),
                            VariantInfo { enum_name: name.clone(), payload },
                        );
                    }
                    self.enums.insert(name.clone(), names);
                }
                _ => {}
            }
        }
    }

    /// Build a function's signature, resolving param/return annotations. `self` (an un-annotated
    /// first param of a method) is left for `check_fn_body` to bind to the struct type.
    fn fn_sig(&mut self, decl: &FnDecl, span: Span) -> FnSig {
        let params = decl
            .params
            .iter()
            .map(|p| match &p.ty {
                Some(t) => self.resolve_type(t, span),
                None if p.name == "self" => Ty::Unknown, // bound in check_fn_body
                None => {
                    self.error(span, format!("parameter '{}' needs a type annotation", p.name));
                    Ty::Unknown
                }
            })
            .collect();
        let ret = decl.ret.as_ref().map(|t| self.resolve_type(t, span)).unwrap_or(Ty::Nil);
        FnSig { params, ret }
    }

    /// Resolve an AST `Type` annotation into a checker `Ty`, reporting unknown type names.
    fn resolve_type(&mut self, t: &Type, span: Span) -> Ty {
        match t {
            Type::Named(n) => match n.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "nil" => Ty::Nil,
                _ if self.struct_names.contains(n) => Ty::Struct(n.clone()),
                _ if self.enum_names.contains(n) => Ty::Enum(n.clone()),
                _ => {
                    self.error(span, format!("unknown type '{n}'"));
                    Ty::Unknown
                }
            },
            Type::Generic(n, args) => match (n.as_str(), args.as_slice()) {
                ("list", [inner]) => Ty::list(self.resolve_type(inner, span)),
                ("Result", [inner]) => Ty::result(self.resolve_type(inner, span)),
                ("Option", [inner]) => Ty::option(self.resolve_type(inner, span)),
                ("map", [_, _]) => Ty::Unknown, // map typing deferred (no map literals yet)
                _ => {
                    self.error(span, format!("unknown generic type '{n}'"));
                    Ty::Unknown
                }
            },
        }
    }

    // ===== pass 2: check statements =====

    fn check_block(&mut self, block: &Block) {
        self.push_scope();
        for stmt in block {
            self.check_stmt(stmt);
        }
        self.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        let span = stmt.span;
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                let val_ty = self.infer(value);
                let declared = match ty {
                    Some(t) => {
                        let expected = self.resolve_type(t, span);
                        if !compatible(&expected, &val_ty) {
                            self.error(
                                value.span,
                                format!("cannot assign {val_ty} to variable of type {expected}"),
                            );
                        }
                        expected
                    }
                    None => val_ty,
                };
                self.declare(name, declared);
            }
            StmtKind::Assign { target, op, value } => {
                let val_ty = self.infer(value);
                self.check_assign(target, *op, val_ty, span);
            }
            StmtKind::Fn(decl) => {
                // `.get` (not index) is panic-safe even when a redeclaration left a different sig.
                if let Some(sig) = self.functions.get(&decl.name).cloned() {
                    self.check_fn_body(decl, None, sig);
                }
            }
            StmtKind::Struct { name, methods, .. } => {
                let self_ty = Ty::Struct(name.clone());
                for m in methods {
                    // Panic-safe: a redeclared struct name means `structs[name]` is a *different*
                    // struct whose method table may not contain `m.name`.
                    if let Some(sig) =
                        self.structs.get(name).and_then(|s| s.methods.get(&m.name)).cloned()
                    {
                        self.check_fn_body(m, Some(self_ty.clone()), sig);
                    }
                }
            }
            StmtKind::Enum { .. } | StmtKind::Import(_) => {} // nothing to check in pass 2
            StmtKind::If { branches, else_block } => {
                for (cond, body) in branches {
                    self.expect_bool(cond, "if condition");
                    self.check_block(body);
                }
                if let Some(body) = else_block {
                    self.check_block(body);
                }
            }
            StmtKind::For { var, iter, body } => {
                let elem = self.iter_elem(iter);
                self.push_scope();
                self.declare(var, elem);
                for stmt in body {
                    self.check_stmt(stmt);
                }
                self.pop_scope();
            }
            StmtKind::While { cond, body } => {
                self.expect_bool(cond, "while condition");
                self.check_block(body);
            }
            StmtKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms),
            StmtKind::Return(value) => self.check_return(value.as_ref(), span),
            StmtKind::Expr(e) => {
                self.infer(e);
            }
        }
    }

    fn check_assign(&mut self, target: &Expr, op: AssignOp, val_ty: Ty, span: Span) {
        match &target.kind {
            ExprKind::Ident(name) => {
                let Some(var_ty) = self.lookup(name) else {
                    self.error(span, format!("cannot assign to undeclared variable '{name}'"));
                    return;
                };
                self.check_assign_value(&var_ty, op, &val_ty, target.span);
            }
            // The interpreter only supports assigning to a bare variable (interp `exec_assign`);
            // field/index assignment is not implemented yet, so reject it here rather than let
            // the checker green-light a program that errors at runtime.
            _ => self.error(target.span, "invalid assignment target (only variables can be assigned)"),
        }
    }

    fn check_assign_value(&mut self, target_ty: &Ty, op: AssignOp, val_ty: &Ty, span: Span) {
        match op {
            AssignOp::Eq => {
                if !compatible(target_ty, val_ty) {
                    self.error(span, format!("cannot assign {val_ty} to {target_ty}"));
                }
            }
            AssignOp::PlusEq | AssignOp::MinusEq => {
                // `+=` mirrors `+` (numeric, or str+str for `+=`); `-=` is numeric only.
                let str_ok = op == AssignOp::PlusEq && *target_ty == Ty::Str && *val_ty == Ty::Str;
                let num_ok = target_ty.is_numeric() && val_ty.is_numeric();
                let known = !target_ty.is_unknown() && !val_ty.is_unknown();
                if known && !str_ok && !num_ok {
                    let sym = if op == AssignOp::PlusEq { "+=" } else { "-=" };
                    self.error(span, format!("cannot apply {sym} to {target_ty} and {val_ty}"));
                }
            }
        }
    }

    fn check_return(&mut self, value: Option<&Expr>, span: Span) {
        let ret = self.current_ret.clone();
        match value {
            Some(e) => {
                let ty = self.infer(e);
                if ret == Ty::Nil {
                    self.error(e.span, "function returns nothing, cannot return a value");
                } else if !compatible(&ret, &ty) {
                    self.error(e.span, format!("expected return type {ret}, found {ty}"));
                }
            }
            None => {
                if ret != Ty::Nil {
                    self.error(span, format!("expected a return value of type {ret}"));
                }
            }
        }
    }

    fn check_fn_body(&mut self, decl: &FnDecl, self_ty: Option<Ty>, sig: FnSig) {
        let saved_ret = std::mem::replace(&mut self.current_ret, sig.ret.clone());
        self.push_scope();
        for (i, param) in decl.params.iter().enumerate() {
            let ty = if param.name == "self" {
                self_ty.clone().unwrap_or(Ty::Unknown)
            } else {
                sig.params.get(i).cloned().unwrap_or(Ty::Unknown)
            };
            self.declare(&param.name, ty);
        }
        for stmt in &decl.body {
            self.check_stmt(stmt);
        }
        self.pop_scope();
        self.current_ret = saved_ret;
    }

    /// The element type produced by iterating `iter` in a `for` loop.
    fn iter_elem(&mut self, iter: &Expr) -> Ty {
        if let ExprKind::Range { start, end } = &iter.kind {
            self.expect_int(start, "range bound");
            self.expect_int(end, "range bound");
            return Ty::Int;
        }
        match self.infer(iter) {
            Ty::List(inner) => *inner,
            Ty::Str => Ty::Str,
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(iter.span, format!("cannot iterate over {other}"));
                Ty::Unknown
            }
        }
    }

    fn check_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchArm]) {
        let sty = self.infer(scrutinee);
        // (variant name -> payload types) the scrutinee admits, plus a label for messages.
        let (label, variants): (String, HashMap<String, Vec<Ty>>) = match &sty {
            Ty::Enum(name) => {
                let map = self
                    .enums
                    .get(name)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| (v.clone(), self.variants[&v].payload.clone()))
                    .collect();
                (name.clone(), map)
            }
            Ty::Result(inner) => (
                "Result".into(),
                HashMap::from([
                    ("Ok".into(), vec![(**inner).clone()]),
                    ("Err".into(), vec![Ty::Unknown]),
                ]),
            ),
            Ty::Option(inner) => (
                "Option".into(),
                HashMap::from([("Some".into(), vec![(**inner).clone()]), ("None".into(), vec![])]),
            ),
            Ty::Unknown => (String::new(), HashMap::new()), // un-inferable: skip exhaustiveness
            other => {
                self.error(scrutinee.span, format!("cannot match on non-enum type {other}"));
                (String::new(), HashMap::new())
            }
        };

        let skip_exhaustive = sty.is_unknown();
        let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
        for arm in arms {
            let Pattern::Variant { name, bindings } = &arm.pattern;
            let payload = if variants.is_empty() && skip_exhaustive {
                None // scrutinee unknown — accept any binding count
            } else {
                match variants.get(name) {
                    Some(p) => Some(p.clone()),
                    None => {
                        if !label.is_empty() {
                            self.error(
                                scrutinee.span,
                                format!("'{name}' is not a variant of {label}"),
                            );
                        }
                        None
                    }
                }
            };
            if !covered.insert(name.clone()) {
                self.error(scrutinee.span, format!("duplicate match arm '{name}'"));
            }
            self.push_scope();
            if let Some(payload) = &payload {
                if payload.len() != bindings.len() {
                    self.error(
                        scrutinee.span,
                        format!(
                            "variant '{name}' binds {} value(s), but {} given",
                            payload.len(),
                            bindings.len()
                        ),
                    );
                }
                for (b, t) in bindings.iter().zip(payload.iter()) {
                    self.declare(b, t.clone());
                }
            } else {
                for b in bindings {
                    self.declare(b, Ty::Unknown);
                }
            }
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }

        if !skip_exhaustive {
            let missing: Vec<String> =
                variants.keys().filter(|v| !covered.contains(*v)).cloned().collect();
            if !missing.is_empty() {
                let mut missing = missing;
                missing.sort();
                self.error(
                    scrutinee.span,
                    format!("non-exhaustive match on {label}: missing {}", missing.join(", ")),
                );
            }
        }
    }

    // ===== expression inference =====

    fn infer(&mut self, expr: &Expr) -> Ty {
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(_) => Ty::Str, // opaque; interpolation contents are not checked (M2 defer)
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Ident(name) => self.infer_ident(name, expr.span),
            ExprKind::List(items) => self.infer_list(items),
            ExprKind::Unary { op, expr: inner } => self.infer_unary(*op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs),
            ExprKind::Range { start, end } => {
                self.expect_int(start, "range bound");
                self.expect_int(end, "range bound");
                Ty::list(Ty::Int)
            }
            ExprKind::Call { callee, args } => self.infer_call(callee, args, expr.span),
            ExprKind::Field { obj, name } => self.infer_field(obj, name),
            ExprKind::Index { obj, index } => self.infer_index(obj, index),
            ExprKind::Try(inner) => self.infer_try(inner, expr.span),
            ExprKind::Closure { params, ret, body } => self.infer_closure(params, ret.as_ref(), body),
        }
    }

    fn infer_ident(&mut self, name: &str, span: Span) -> Ty {
        if let Some(ty) = self.lookup(name) {
            return ty;
        }
        if let Some(sig) = self.functions.get(name) {
            return Ty::Func { params: sig.params.clone(), ret: Box::new(sig.ret.clone()) };
        }
        if name == "None" {
            return Ty::option(Ty::Unknown);
        }
        // A nullary user variant used as a value (e.g. `Red`).
        if let Some(v) = self.variants.get(name)
            && v.payload.is_empty()
        {
            return Ty::Enum(v.enum_name.clone());
        }
        self.error(span, format!("unknown name '{name}'"));
        Ty::Unknown
    }

    fn infer_list(&mut self, items: &[Expr]) -> Ty {
        let mut elem = Ty::Unknown;
        for item in items {
            let t = self.infer(item);
            if elem.is_unknown() {
                elem = t;
            } else if !t.is_unknown() && !compatible(&elem, &t) {
                self.error(item.span, format!("list elements differ: {elem} vs {t}"));
            }
        }
        Ty::list(elem)
    }

    fn infer_unary(&mut self, op: UnaryOp, inner: &Expr) -> Ty {
        let t = self.infer(inner);
        match op {
            UnaryOp::Neg => {
                if !t.is_numeric() && !t.is_unknown() {
                    self.error(inner.span, format!("cannot negate {t}"));
                    return Ty::Unknown;
                }
                t
            }
            UnaryOp::Not => {
                if t != Ty::Bool && !t.is_unknown() {
                    self.error(inner.span, format!("'not' expects bool, found {t}"));
                }
                Ty::Bool
            }
        }
    }

    fn infer_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Ty {
        use BinaryOp::*;
        let l = self.infer(lhs);
        let r = self.infer(rhs);
        let either_unknown = l.is_unknown() || r.is_unknown();
        match op {
            And | Or => {
                if l != Ty::Bool && !l.is_unknown() {
                    self.error(lhs.span, format!("logical operator expects bool, found {l}"));
                }
                if r != Ty::Bool && !r.is_unknown() {
                    self.error(rhs.span, format!("logical operator expects bool, found {r}"));
                }
                Ty::Bool
            }
            Add => {
                if l == Ty::Str && r == Ty::Str {
                    Ty::Str
                } else if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply + to {l} and {r}"));
                    Ty::Unknown
                }
            }
            Sub | Mul | Div | Mod => {
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply {} to {l} and {r}", op_sym(op)));
                    Ty::Unknown
                }
            }
            Lt | LtEq | Gt | GtEq => {
                let ok = (l.is_numeric() && r.is_numeric()) || (l == Ty::Str && r == Ty::Str);
                if !ok && !either_unknown {
                    self.error(lhs.span, format!("cannot compare {l} and {r}"));
                }
                Ty::Bool
            }
            Eq | NotEq => Ty::Bool, // equality is permissive (matches the interpreter)
        }
    }

    fn infer_field(&mut self, obj: &Expr, name: &str) -> Ty {
        let obj_ty = self.infer(obj);
        match &obj_ty {
            Ty::Struct(sname) => {
                if let Some(info) = self.structs.get(sname) {
                    if let Some((_, ty)) = info.fields.iter().find(|(f, _)| f == name) {
                        return ty.clone();
                    }
                    if info.methods.contains_key(name) {
                        let sig = &info.methods[name];
                        return Ty::Func {
                            params: sig.params.clone(),
                            ret: Box::new(sig.ret.clone()),
                        };
                    }
                }
                self.error(obj.span, format!("type {obj_ty} has no field '{name}'"));
                Ty::Unknown
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(obj.span, format!("type {other} has no field '{name}'"));
                Ty::Unknown
            }
        }
    }

    fn infer_index(&mut self, obj: &Expr, index: &Expr) -> Ty {
        self.expect_int(index, "index");
        match self.infer(obj) {
            Ty::List(inner) => *inner,
            Ty::Str => Ty::Str,
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(obj.span, format!("cannot index into {other}"));
                Ty::Unknown
            }
        }
    }

    fn infer_try(&mut self, inner: &Expr, span: Span) -> Ty {
        let t = self.infer(inner);
        // The enclosing function must be able to early-return the Err/None. We allow Result/Option
        // (propagate) and Nil (top-level / `fn main()` — the interpreter unwinds it at the boundary).
        match &self.current_ret {
            Ty::Result(_) | Ty::Option(_) | Ty::Nil => {}
            other => self.error(
                span,
                format!("'?' used in a function that returns {other}, not Result or Option"),
            ),
        }
        match t {
            Ty::Result(inner) | Ty::Option(inner) => *inner,
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(span, format!("'?' expects Result or Option, found {other}"));
                Ty::Unknown
            }
        }
    }

    fn infer_closure(&mut self, params: &[Param], ret: Option<&Type>, body: &Expr) -> Ty {
        self.push_scope();
        let param_tys: Vec<Ty> = params
            .iter()
            .map(|p| {
                let ty = p.ty.as_ref().map(|t| self.resolve_type(t, body.span)).unwrap_or(Ty::Unknown);
                self.declare(&p.name, ty.clone());
                ty
            })
            .collect();
        let body_ty = self.infer(body);
        self.pop_scope();
        let ret_ty = match ret {
            Some(t) => {
                let declared = self.resolve_type(t, body.span);
                if !compatible(&declared, &body_ty) {
                    self.error(
                        body.span,
                        format!("closure body has type {body_ty}, but its return type is {declared}"),
                    );
                }
                declared
            }
            None => body_ty,
        };
        Ty::Func { params: param_tys, ret: Box::new(ret_ty) }
    }

    // ===== calls =====

    fn infer_call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Ty {
        // Method call: `obj.method(args)`.
        if let ExprKind::Field { obj, name } = &callee.kind {
            return self.infer_method_call(obj, name, args, span);
        }
        if let ExprKind::Ident(name) = &callee.kind {
            // Shadowing local (e.g. a closure bound to a variable) wins over a global of the same name.
            if self.lookup(name).is_none()
                && let Some(ty) = self.infer_named_call(name, args, span)
            {
                return ty;
            }
        }
        // Fall back: the callee is an arbitrary expression; it must evaluate to a function.
        let callee_ty = self.infer(callee);
        match callee_ty {
            Ty::Func { params, ret } => {
                self.check_args("closure", &params, args, span);
                *ret
            }
            Ty::Unknown => {
                for a in args {
                    self.infer(a);
                }
                Ty::Unknown
            }
            other => {
                for a in args {
                    self.infer(a);
                }
                self.error(span, format!("{other} is not callable"));
                Ty::Unknown
            }
        }
    }

    /// Resolve a by-name call (builtin / constructor / variant / global fn). Returns `None` if
    /// `name` is none of those, so the caller can treat it as a value-call.
    fn infer_named_call(&mut self, name: &str, args: &[Expr], span: Span) -> Option<Ty> {
        match name {
            "print" => {
                for a in args {
                    self.infer(a);
                }
                Some(Ty::Nil)
            }
            "len" => {
                self.check_arity("len", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer(a) {
                        Ty::List(_) | Ty::Str | Ty::Unknown => {}
                        other => self.error(a.span, format!("len() expects a list or str, got {other}")),
                    }
                }
                Some(Ty::Int)
            }
            "range" => {
                for a in args {
                    self.expect_int_val(a);
                }
                if args.is_empty() || args.len() > 2 {
                    self.error(span, "range() expects range(end) or range(start, end)");
                }
                Some(Ty::list(Ty::Int))
            }
            "int" => {
                self.check_arity("int", 1, args, span);
                self.infer_all(args);
                Some(Ty::Int)
            }
            "float" => {
                self.check_arity("float", 1, args, span);
                self.infer_all(args);
                Some(Ty::Float)
            }
            "str" => {
                self.check_arity("str", 1, args, span);
                self.infer_all(args);
                Some(Ty::Str)
            }
            "sqrt" => {
                self.check_arity("sqrt", 1, args, span);
                if let Some(a) = args.first() {
                    let t = self.infer(a);
                    if !t.is_numeric() && !t.is_unknown() {
                        self.error(a.span, format!("sqrt() expects a number, got {t}"));
                    }
                }
                Some(Ty::Float)
            }
            // Generic built-in constructors for Result / Option.
            "Ok" => Some(Ty::result(self.one_arg(name, args, span))),
            "Some" => Some(Ty::option(self.one_arg(name, args, span))),
            "Err" => {
                let _ = self.one_arg(name, args, span);
                Some(Ty::result(Ty::Unknown))
            }
            _ => {
                // Struct constructor?
                if let Some(info) = self.structs.get(name) {
                    let params: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
                    self.check_args(name, &params, args, span);
                    return Some(Ty::Struct(name.to_string()));
                }
                // User enum variant constructor?
                if let Some(v) = self.variants.get(name).cloned() {
                    self.check_args(name, &v.payload, args, span);
                    return Some(Ty::Enum(v.enum_name));
                }
                // Global function?
                if let Some(sig) = self.functions.get(name).cloned() {
                    self.check_args(name, &sig.params, args, span);
                    return Some(sig.ret);
                }
                None
            }
        }
    }

    fn infer_method_call(&mut self, obj: &Expr, method: &str, args: &[Expr], span: Span) -> Ty {
        let obj_ty = self.infer(obj);
        match &obj_ty {
            Ty::Struct(sname) => {
                let sig = self.structs.get(sname).and_then(|i| i.methods.get(method).cloned());
                if let Some(sig) = sig {
                    self.check_args(method, &sig.params, args, span);
                    return sig.ret;
                }
                self.infer_all(args);
                self.error(span, format!("type {obj_ty} has no method '{method}'"));
                Ty::Unknown
            }
            Ty::Unknown => {
                self.infer_all(args);
                Ty::Unknown
            }
            other => {
                self.infer_all(args);
                self.error(span, format!("type {other} has no method '{method}'"));
                Ty::Unknown
            }
        }
    }

    // ===== small helpers =====

    fn one_arg(&mut self, name: &str, args: &[Expr], span: Span) -> Ty {
        self.check_arity(name, 1, args, span);
        args.first().map(|a| self.infer(a)).unwrap_or(Ty::Unknown)
    }

    fn infer_all(&mut self, args: &[Expr]) {
        for a in args {
            self.infer(a);
        }
    }

    /// Check argument count and each argument's type against a known parameter list.
    fn check_args(&mut self, name: &str, params: &[Ty], args: &[Expr], span: Span) {
        if params.len() != args.len() {
            self.error(
                span,
                format!("'{name}' expects {} argument(s), got {}", params.len(), args.len()),
            );
        }
        for (i, arg) in args.iter().enumerate() {
            let at = self.infer(arg);
            if let Some(pt) = params.get(i)
                && !compatible(pt, &at)
            {
                self.error(
                    arg.span,
                    format!("argument {} of '{name}': expected {pt}, found {at}", i + 1),
                );
            }
        }
    }

    fn check_arity(&mut self, name: &str, n: usize, args: &[Expr], span: Span) {
        if args.len() != n {
            self.error(span, format!("{name}() expects {n} argument(s), got {}", args.len()));
        }
    }

    fn expect_bool(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer(e);
        if t != Ty::Bool && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be bool, found {t}"));
        }
    }

    fn expect_int(&mut self, e: &Expr, ctx: &str) {
        let t = self.infer(e);
        if t != Ty::Int && !t.is_unknown() {
            self.error(e.span, format!("{ctx} must be int, found {t}"));
        }
    }

    fn expect_int_val(&mut self, e: &Expr) {
        self.expect_int(e, "argument");
    }
}

/// Result type of a numeric binary op: float if either side is float, else int.
fn numeric_result(l: &Ty, r: &Ty) -> Ty {
    if *l == Ty::Float || *r == Ty::Float {
        Ty::Float
    } else {
        Ty::Int
    }
}

fn op_sym(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        _ => "?",
    }
}

#[cfg(test)]
mod tests;
