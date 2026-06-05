//! Runtime values produced by the tree-walk interpreter.

use crate::ast::{Expr, FnDecl, Param};
use crate::native::NativeFn;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A native (Rust) function exposed as a Chezzi value — a member of a native std module (M6c).
/// Wraps a bare [`NativeFn`] pointer plus its name for display. `PartialEq`/`Debug` are hand-written
/// because a `fn` pointer's derived `Debug` prints an address and its `PartialEq` compares by
/// address (native fns are never compared in real programs — this just satisfies `Value`'s derive).
#[derive(Clone)]
pub struct NativeFnEntry {
    pub name: Rc<str>,
    pub func: NativeFn,
}

impl PartialEq for NativeFnEntry {
    fn eq(&self, other: &Self) -> bool {
        self.func as usize == other.func as usize
    }
}

impl std::fmt::Debug for NativeFnEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<native fn {}>", self.name)
    }
}

// `HashMap` is still used by `Closure::captured`.

/// A module's top-level bindings (its globals), shared by reference. Every callable carries the
/// `ModEnv` of the module that *defined* it, so a function imported into another module still
/// resolves its module-level names against its own home — not the caller's (see `Interp::call`).
///
/// Equality is by pointer identity and `Debug` is opaque: the table is self-referential (it holds
/// the very `Func`s whose home it is), so a structural compare or print would recurse forever.
#[derive(Clone)]
pub struct ModEnv(pub Rc<RefCell<HashMap<String, Value>>>);

impl ModEnv {
    pub fn new() -> Self {
        ModEnv(Rc::new(RefCell::new(HashMap::new())))
    }
}

impl Default for ModEnv {
    fn default() -> Self {
        ModEnv::new()
    }
}

impl PartialEq for ModEnv {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for ModEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<module globals>")
    }
}

/// A module value: a named namespace whose members are its module's top-level bindings.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleNamespace {
    pub name: Rc<str>,
    pub members: ModEnv,
}

/// An anonymous function plus the lexical environment it closed over. The captured local frames
/// let `fn(x): x + n` keep referring to `n` after the enclosing function has returned; `home` is
/// the module globals it resolves top-level names against.
#[derive(Debug, Clone, PartialEq)]
pub struct Closure {
    pub params: Vec<Param>,
    pub body: Expr,
    pub captured: Vec<HashMap<String, Value>>,
    pub home: ModEnv,
}

/// A runtime value. Reference types (lists, structs) share via `Rc` so assignment is by-reference,
/// matching the spec's growable `list` / mutable struct semantics.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<str>),
    /// `[a, b, c]` — growable, shared by reference.
    List(Rc<RefCell<Vec<Value>>>),
    /// A named function (top-level `fn` or struct method) plus the module globals it resolves
    /// top-level names against (its "home" — see [`ModEnv`]).
    Func(Rc<FnDecl>, ModEnv),
    /// An anonymous function with its captured environment.
    Closure(Rc<Closure>),
    /// A native (Rust) function — a member of a native std module (`std.math` etc., M6c).
    Native(NativeFnEntry),
    /// An imported module — a namespace of its top-level bindings. `io.read()` is a field access
    /// on one of these.
    Module(Rc<ModuleNamespace>),
    /// A struct instance: type name + mutable, by-reference fields kept in declaration order
    /// (so `Display` and iteration are deterministic — a `HashMap` would print fields randomly).
    Struct {
        name: Rc<str>,
        fields: Rc<RefCell<Vec<(String, Value)>>>,
    },
    /// An enum value: type name, variant name, and its payload. `Ok`/`Err`/`Some`/`None` are
    /// just enums of type `Result` / `Option`.
    Enum {
        ty: Rc<str>,
        variant: Rc<str>,
        payload: Vec<Value>,
    },
    /// The result of a statement-like expression (e.g. `print(...)`) or a function with no
    /// `return`. Not directly constructible in source.
    Nil,
}

impl Value {
    /// Human-readable type name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Str(_) => "str",
            Value::List(_) => "list",
            Value::Func(_, _) => "function",
            Value::Closure(_) => "function",
            Value::Native(_) => "function",
            Value::Module(_) => "module",
            Value::Struct { .. } => "struct",
            Value::Enum { .. } => "enum",
            Value::Nil => "nil",
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{}", format_float(*x)),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Str(s) => write!(f, "{s}"),
            Value::List(items) => {
                let inner = items
                    .borrow()
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "[{inner}]")
            }
            Value::Func(decl, _) => write!(f, "<fn {}>", decl.name),
            Value::Closure(_) => write!(f, "<closure>"),
            Value::Native(e) => write!(f, "<native fn {}>", e.name),
            Value::Module(ns) => write!(f, "<module {}>", ns.name),
            Value::Struct { name, fields } => {
                let inner = fields
                    .borrow()
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "{name}({inner})")
            }
            Value::Enum {
                variant, payload, ..
            } => {
                if payload.is_empty() {
                    write!(f, "{variant}")
                } else {
                    let inner = payload
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(f, "{variant}({inner})")
                }
            }
            Value::Nil => write!(f, "nil"),
        }
    }
}

/// Format a float the way Chezzi prints it: integral values keep one decimal place (`5.0`),
/// everything else uses Rust's shortest round-trip form.
fn format_float(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}
