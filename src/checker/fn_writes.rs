//! Param-escape summaries of statically named functions (TICKET-190). Function values remain
//! opaque.

use super::{Checker, FnSig, Ty};
use crate::ast::{BinaryOp, Expr, ExprKind, FnDecl, Stmt, StmtKind, UnaryOp};
use std::collections::{HashMap, HashSet};

/// TICKET-189 — where one declaration slot of a bound call gets its value, read from the checker's
/// call plan (`Checker::bound_slots`).
#[derive(Clone, Debug)]
pub(super) enum SlotSrc<'a> {
    /// A caller-written expression, positional or keyword.
    Arg(&'a Expr),
    /// The variadic slot: a list built at the call site from these expressions.
    Pack(Vec<&'a Expr>),
    /// An omitted slot filled from the declaration's default: `inline` for the declaration's own
    /// literal node, else a provider call.
    Default { inline: bool },
}

/// A statically named function's summary: per declared param, whether the argument can escape the
/// call (TICKET-190), from one least fixed point.
#[derive(Clone, Debug, Default)]
pub(super) struct FnSummary {
    /// `escapes[i]`: argument `i` may be stored, returned or aliased past the call. An index at or
    /// past the end escapes (native and extern sigs carry an empty vector).
    pub escapes: Vec<bool>,
    /// A generator call stores its arguments in the new frame without running the body, so every
    /// argument escapes.
    pub is_generator: bool,
}

/// TICKET-190 — a use of a bare name that keeps its root in the frame when the name's type allows
/// it ([`Checker::root_escapes`] owns the type rules).
#[derive(Clone, Debug, PartialEq)]
pub(super) enum RootUse {
    /// The receiver of method `m`.
    Recv(String),
    /// `n.f`, read or written.
    Field(String),
    /// `n[i]` or a slice of `n`, read or written.
    Index,
    /// An operand of an operator [`aliases_operand`] proves fresh, or a compound assignment target.
    Op,
    /// `for v in n`.
    Iter,
    /// Positional argument `j` of a statically named callee.
    Arg(ArgCallee, usize),
}

/// TICKET-247 — what the result of an operator node is to its operands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum OperandAlias {
    /// A new object that holds each operand (`?x`, `!x`).
    Holds,
    /// A value the operator computes. It is no operand when the operand's type has no user
    /// operator ([`Checker::use_keeps_root`] owns that type rule).
    Fresh,
    /// One of the operands itself (`a and b`, `a or b`, `a ?? b`).
    Child,
}

/// TICKET-247 — the one decider of "can the result of this operator node be, or hold, an
/// operand?". [`UseWalk`] and `Checker::fresh_shape` both read it. Each operator enum is matched
/// with no wildcard, so a new operator does not compile until it is classified here. A kind that
/// is no operator holds its operands.
pub(super) fn aliases_operand(kind: &ExprKind) -> OperandAlias {
    match kind {
        ExprKind::Unary { op, .. } => match op {
            UnaryOp::Neg | UnaryOp::Not => OperandAlias::Fresh,
            UnaryOp::Wrap | UnaryOp::ErrVal => OperandAlias::Holds,
        },
        ExprKind::Binary { op, .. } => match op {
            BinaryOp::And | BinaryOp::Or => OperandAlias::Child,
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Mod
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::Shl
            | BinaryOp::Shr
            | BinaryOp::In => OperandAlias::Fresh,
        },
        ExprKind::Compare { .. } => OperandAlias::Fresh,
        ExprKind::NullCoalesce { .. } => OperandAlias::Child,
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_)
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Field { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => OperandAlias::Holds,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ArgCallee {
    /// A bare callee `f(..)` that no binding of the function shadows.
    Fn(String),
    /// `m.f(..)` where `m` is no binding of the function.
    Module(String, String),
}

/// TICKET-240 — a use of a VIEW of a frame slot, keyed by the slot's root name in [`Uses::deep`].
/// A view of name `n` is `n`, an alias of `n` (a `match` binder or loop name over a view), or
/// `V.f` / `V[i]` with `V` a view. `Checker::deep_private` owns the rules.
#[derive(Clone, Debug)]
pub(super) enum DeepUse {
    /// `V.m(args)`: the receiver and call nodes, and the call's arguments.
    Recv {
        recv: crate::ast::NodeId,
        call: crate::ast::NodeId,
        args: Vec<Expr>,
    },
    /// An assignment through `V.f` (`index` is `None`) or `V[i]`.
    Store {
        value: Box<Expr>,
        index: Option<Box<Expr>>,
    },
    /// Any other occurrence of a view: its value may leave the graph.
    Read { view: crate::ast::NodeId },
}

/// Every use of every bare name in one function body (TICKET-190). A name in `escaped` was used as
/// a plain value somewhere; `kept` holds the uses whose verdict depends on the name's type. The
/// last three are filled only by [`frame_uses`] (TICKET-240): `deep` holds every use of a view,
/// `matched` the roots a `match` statement took apart (their binders are aliases, so the root is
/// not `escaped`), and `lost` the roots with an alias the walk cannot follow.
#[derive(Clone, Debug, Default)]
pub(super) struct Uses {
    pub escaped: HashSet<String>,
    pub kept: Vec<(String, RootUse)>,
    pub deep: Vec<(String, DeepUse)>,
    pub matched: HashSet<String>,
    pub lost: HashSet<String>,
}

/// Can a native method's declared return type hold its receiver? Only a value with no heap
/// reference cannot: a scalar, `bool`, `str`/`bytes` by value, `nil`, or an `Option`/`Result` of
/// those. A type parameter, a container or the receiver's own type (`own`) may hold it.
pub(super) fn ret_may_hold_receiver(ret: &Ty, own: &Ty) -> bool {
    if ret == own {
        return true;
    }
    match ret.scalar() {
        Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Nil => false,
        Ty::Option(t) => ret_may_hold_receiver(t, own),
        Ty::Result(t, e) => ret_may_hold_receiver(t, own) || ret_may_hold_receiver(e, own),
        Ty::ByteArray
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => true,
    }
}

/// The native method table that owns a receiver type's methods, and the receiver type itself.
pub(super) fn native_receiver(t: &Ty) -> Option<(&'static str, Ty)> {
    match t {
        Ty::List(_) => Some(("List", t.clone())),
        Ty::Map(..) => Some(("Map", t.clone())),
        Ty::Set(_) => Some(("Set", t.clone())),
        Ty::Str => Some(("str", Ty::Str)),
        Ty::Bytes => Some(("bytes", Ty::Bytes)),
        Ty::ByteArray => Some(("bytearray", Ty::ByteArray)),
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Nil
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Result(..)
        | Ty::Option(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => None,
    }
}

/// TICKET-254 — `t[a..b]` on `t` builds a NEW value of the same type, so a mutating call on it
/// changes a copy: the built-in sequence types. A type with a `slice` hook is no row: the hook may
/// return its own field (measured, `a_user_slice_hook_result_is_no_fresh_temporary`).
pub(super) fn builtin_slice(t: &Ty) -> bool {
    match t {
        Ty::List(_) | Ty::Str | Ty::Bytes | Ty::ByteArray => true,
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Nil
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Result(..)
        | Ty::Option(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => false,
    }
}

/// TICKET-254 — `t` is one of the four mutable native containers (`List`, `Map`, `Set`,
/// `bytearray`). A native method that returns one of them returns a new object.
pub(super) fn native_container(t: &Ty) -> bool {
    match t {
        Ty::List(_) | Ty::Map(..) | Ty::Set(_) | Ty::ByteArray => true,
        Ty::Str
        | Ty::Bytes
        | Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Nil
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Result(..)
        | Ty::Option(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => false,
    }
}

/// Collects [`Uses`] for one body. `bound` holds every name the function binds (params, lets, loop
/// and pattern variables, nested fns), so a callee outside it names a top-level fn or a module.
struct UseWalk<'a> {
    bound: &'a HashSet<String>,
    uses: Uses,
    /// TICKET-240: this walk feeds a generator frame verdict, so it follows views.
    deep: bool,
    /// Alias name to the root it views.
    aliases: HashMap<String, String>,
    /// Every bare name that is an assignment target.
    assigned: HashSet<String>,
}

impl UseWalk<'_> {
    /// The root a view expression reads through, when `e` is a view.
    fn view_root(&self, e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Ident(n) => Some(self.aliases.get(n).unwrap_or(n).clone()),
            ExprKind::Field { obj, .. } | ExprKind::Index { obj, .. } => self.view_root(obj),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        }
    }

    /// Record `u()` as a use of the view `view`. A walk that feeds no frame verdict records none.
    fn deep_use(&mut self, view: &Expr, u: impl FnOnce() -> DeepUse) {
        if self.deep
            && let Some(root) = self.view_root(view)
        {
            self.uses.deep.push((root, u()));
        }
    }

    /// `name` now views `root`. A name that already was an alias loses both roots.
    fn alias(&mut self, name: &str, root: &str) {
        if let Some(prev) = self.aliases.insert(name.to_string(), root.to_string()) {
            self.uses.lost.insert(prev);
            self.uses.lost.insert(root.to_string());
        }
    }

    /// Walk the steps of a `V.f` / `V[i]` chain whose own use the caller recorded: the bare root
    /// keeps its `Field`/`Index` use, an index expression is a value, an alias keeps nothing.
    fn path(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Field { obj, name, .. } => match obj.kind.as_ident() {
                Some(n) => self.keep(n, RootUse::Field(name.clone())),
                None => self.path(obj),
            },
            ExprKind::Index { obj, index, .. } => {
                match obj.kind.as_ident() {
                    Some(n) => self.keep(n, RootUse::Index),
                    None => self.path(obj),
                }
                if let Some(index) = index {
                    self.value(index);
                }
            }
            ExprKind::Ident(n) if self.aliases.contains_key(n) => {}
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => self.value(e),
        }
    }

    fn escape_free(&mut self, e: &Expr) {
        self.uses
            .escaped
            .extend(crate::compiler::free_names_of_expr(e, &HashSet::new()));
    }

    fn keep(&mut self, n: &str, u: RootUse) {
        self.uses.kept.push((n.to_string(), u));
    }

    fn block(&mut self, body: &[Stmt]) {
        for s in body {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.value(value),
            StmtKind::Assign { target, op, value } => {
                match &target.kind {
                    ExprKind::Ident(n) => {
                        self.assigned.insert(n.clone());
                        if *op != crate::ast::AssignOp::Eq {
                            self.keep(n, RootUse::Op);
                        }
                    }
                    ExprKind::Field { .. } | ExprKind::Index { .. } => {
                        self.deep_use(target, || DeepUse::Store {
                            value: Box::new(value.clone()),
                            index: match &target.kind {
                                ExprKind::Index { index, .. } => index.clone(),
                                ExprKind::Int(_)
                                | ExprKind::Float(_)
                                | ExprKind::Str(_)
                                | ExprKind::Interp(_)
                                | ExprKind::Bytes(_)
                                | ExprKind::RawStr(_)
                                | ExprKind::Bool(_)
                                | ExprKind::NoneLit
                                | ExprKind::Pass
                                | ExprKind::Ident(_)
                                | ExprKind::List(..)
                                | ExprKind::Tuple(_)
                                | ExprKind::Map(_)
                                | ExprKind::Set(_)
                                | ExprKind::Comprehension { .. }
                                | ExprKind::Unary { .. }
                                | ExprKind::Binary { .. }
                                | ExprKind::Compare { .. }
                                | ExprKind::Range { .. }
                                | ExprKind::Call { .. }
                                | ExprKind::Field { .. }
                                | ExprKind::Try(_)
                                | ExprKind::OptChain { .. }
                                | ExprKind::NullCoalesce { .. }
                                | ExprKind::Closure { .. }
                                | ExprKind::Match { .. }
                                | ExprKind::IfElse { .. }
                                | ExprKind::Recover(..)
                                | ExprKind::ElseGuard { .. } => None,
                            },
                        });
                        self.path(target);
                    }
                    ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Str(_)
                    | ExprKind::Interp(_)
                    | ExprKind::Bytes(_)
                    | ExprKind::RawStr(_)
                    | ExprKind::Bool(_)
                    | ExprKind::NoneLit
                    | ExprKind::Pass
                    | ExprKind::List(..)
                    | ExprKind::Tuple(_)
                    | ExprKind::Map(_)
                    | ExprKind::Set(_)
                    | ExprKind::Comprehension { .. }
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
                    | ExprKind::Compare { .. }
                    | ExprKind::Range { .. }
                    | ExprKind::Call { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => self.escape_free(target),
                }
                self.value(value);
            }
            StmtKind::Expr(e) | StmtKind::Yield(e) | StmtKind::Return(Some(e)) => self.value(e),
            StmtKind::Assert { cond, msg } => {
                self.value(cond);
                if let Some(m) = msg {
                    self.value(m);
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    self.value(cond);
                    self.block(body);
                }
                if let Some(body) = else_block {
                    self.block(body);
                }
            }
            StmtKind::While { cond, body } => {
                self.value(cond);
                self.block(body);
            }
            StmtKind::For { target, iter, body } => {
                // TICKET-240: a loop over a view binds views of the same root.
                if self.deep
                    && let Some(root) = self.view_root(iter)
                {
                    for v in &target.binder_names() {
                        self.alias(v, &root);
                    }
                }
                match &iter.kind {
                    ExprKind::Ident(n) => self.keep(n, RootUse::Iter),
                    ExprKind::Field { .. } | ExprKind::Index { .. } => self.path(iter),
                    ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Str(_)
                    | ExprKind::Interp(_)
                    | ExprKind::Bytes(_)
                    | ExprKind::RawStr(_)
                    | ExprKind::Bool(_)
                    | ExprKind::NoneLit
                    | ExprKind::Pass
                    | ExprKind::List(..)
                    | ExprKind::Tuple(_)
                    | ExprKind::Map(_)
                    | ExprKind::Set(_)
                    | ExprKind::Comprehension { .. }
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
                    | ExprKind::Compare { .. }
                    | ExprKind::Range { .. }
                    | ExprKind::Call { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => self.value(iter),
                }
                self.block(body);
            }
            StmtKind::Match { scrutinee, arms } => {
                // TICKET-240: a `match` over a view binds views of the same root, so the
                // scrutinee itself is neither an escape nor a read.
                let root = if self.deep {
                    self.view_root(scrutinee)
                } else {
                    None
                };
                match (&root, &scrutinee.kind) {
                    (Some(_), ExprKind::Ident(n)) => {
                        if !self.aliases.contains_key(n) {
                            self.uses.matched.insert(n.clone());
                        }
                    }
                    (Some(_), _) => self.path(scrutinee),
                    (None, _) => self.value(scrutinee),
                }
                for arm in arms {
                    if let Some(root) = &root {
                        let mut binders = HashSet::new();
                        binders.extend(arm.pattern.binder_names());
                        for b in binders {
                            self.alias(&b, root);
                        }
                    }
                    if let Some(g) = &arm.guard {
                        self.value(g);
                    }
                    self.block(&arm.body);
                }
            }
            StmtKind::Parallel { body } => self.block(body),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Pass => {}
            // A nested fn, `spawn`, `defer`, `wait` or anything else: every name it reads escapes.
            StmtKind::Fn(_)
            | StmtKind::Struct { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Enum { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Defer(_)
            | StmtKind::Spawn(_)
            | StmtKind::Wait { .. }
            | StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. } => {
                self.uses
                    .escaped
                    .extend(crate::compiler::free_names_of_block(
                        std::slice::from_ref(stmt),
                        &HashSet::new(),
                    ))
            }
        }
    }

    /// Walk `e` in value position: a bare name here is a plain value, so it escapes.
    fn value(&mut self, e: &Expr) {
        match &e.kind {
            // TICKET-240: a bare alias is a view of its root, not an escape of its own name.
            ExprKind::Ident(n) if self.aliases.contains_key(n) => {
                self.deep_use(e, || DeepUse::Read { view: e.id });
            }
            ExprKind::Ident(n) => {
                self.uses.escaped.insert(n.clone());
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass => {}
            ExprKind::List(items, _) | ExprKind::Tuple(items) | ExprKind::Set(items) => {
                for item in items {
                    self.value(item);
                }
            }
            ExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    self.value(k);
                    self.value(v);
                }
            }
            ExprKind::Unary { expr, .. } => self.operator(e, &[expr]),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
                self.operator(e, &[lhs, rhs])
            }
            ExprKind::Compare { operands, .. } => {
                self.operator(e, &operands.iter().collect::<Vec<_>>())
            }
            ExprKind::Range { start, end, .. } => {
                if let Some(start) = start {
                    self.value(start);
                }
                if let Some(end) = end {
                    self.value(end);
                }
            }
            ExprKind::Call {
                callee,
                args,
                named,
                bracket,
                ..
            } => {
                if let Some(b) = bracket {
                    self.value(b);
                }
                let target = match &callee.kind {
                    ExprKind::Ident(f) if !self.bound.contains(f) => Some(ArgCallee::Fn(f.clone())),
                    ExprKind::Field { obj, name, .. } => match &obj.kind {
                        ExprKind::Ident(m) if !self.bound.contains(m) => {
                            Some(ArgCallee::Module(m.clone(), name.clone()))
                        }
                        ExprKind::Ident(n) => {
                            self.deep_use(obj, || recv_use(obj, e, args, named));
                            self.keep(n, RootUse::Recv(name.clone()));
                            None
                        }
                        ExprKind::Field { .. } | ExprKind::Index { .. } => {
                            self.deep_use(obj, || recv_use(obj, e, args, named));
                            self.path(obj);
                            None
                        }
                        ExprKind::Int(_)
                        | ExprKind::Float(_)
                        | ExprKind::Str(_)
                        | ExprKind::Interp(_)
                        | ExprKind::Bytes(_)
                        | ExprKind::RawStr(_)
                        | ExprKind::Bool(_)
                        | ExprKind::NoneLit
                        | ExprKind::Pass
                        | ExprKind::List(..)
                        | ExprKind::Tuple(_)
                        | ExprKind::Map(_)
                        | ExprKind::Set(_)
                        | ExprKind::Comprehension { .. }
                        | ExprKind::Unary { .. }
                        | ExprKind::Binary { .. }
                        | ExprKind::Compare { .. }
                        | ExprKind::Range { .. }
                        | ExprKind::Call { .. }
                        | ExprKind::Try(_)
                        | ExprKind::OptChain { .. }
                        | ExprKind::NullCoalesce { .. }
                        | ExprKind::Closure { .. }
                        | ExprKind::Match { .. }
                        | ExprKind::IfElse { .. }
                        | ExprKind::Recover(..)
                        | ExprKind::ElseGuard { .. } => {
                            self.value(obj);
                            None
                        }
                    },
                    ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Str(_)
                    | ExprKind::Interp(_)
                    | ExprKind::Bytes(_)
                    | ExprKind::RawStr(_)
                    | ExprKind::Bool(_)
                    | ExprKind::NoneLit
                    | ExprKind::Pass
                    | ExprKind::Ident(_)
                    | ExprKind::List(..)
                    | ExprKind::Tuple(_)
                    | ExprKind::Map(_)
                    | ExprKind::Set(_)
                    | ExprKind::Comprehension { .. }
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
                    | ExprKind::Compare { .. }
                    | ExprKind::Range { .. }
                    | ExprKind::Call { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => {
                        self.value(callee);
                        None
                    }
                };
                for (j, a) in args.iter().enumerate() {
                    match (&target, &a.kind) {
                        (Some(c), ExprKind::Ident(n)) => {
                            if self.aliases.contains_key(n) {
                                self.deep_use(a, || DeepUse::Read { view: a.id });
                            }
                            self.keep(n, RootUse::Arg(c.clone(), j))
                        }
                        _ => self.value(a),
                    }
                }
                for (_, a) in named {
                    self.value(a);
                }
            }
            ExprKind::Field { .. } | ExprKind::Index { .. } => {
                self.deep_use(e, || DeepUse::Read { view: e.id });
                self.path(e);
            }
            ExprKind::IfElse { cond, then, els } => {
                self.value(cond);
                self.value(then);
                self.value(els);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.value(scrutinee);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.value(g);
                    }
                    self.value(&arm.body);
                }
            }
            ExprKind::Recover(body, _) => self.block(body),
            ExprKind::ElseGuard { value, body, .. } => {
                self.value(value);
                self.block(body);
            }
            // A closure, comprehension, interpolation, `?`, `?.` or anything else.
            ExprKind::Interp(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::Closure { .. } => self.escape_free(e),
        }
    }

    /// Walk the operands of operator node `node`: a bare name keeps its root only under an
    /// operator [`aliases_operand`] proves fresh; under any other it is a plain value.
    fn operator(&mut self, node: &Expr, operands: &[&Expr]) {
        let fresh = aliases_operand(&node.kind) == OperandAlias::Fresh;
        for o in operands {
            if fresh {
                self.operand(o);
            } else {
                self.value(o);
            }
        }
    }

    fn operand(&mut self, e: &Expr) {
        match e.kind.as_ident() {
            Some(n) => {
                if self.aliases.contains_key(n) {
                    self.deep_use(e, || DeepUse::Read { view: e.id });
                }
                self.keep(n, RootUse::Op)
            }
            None => self.value(e),
        }
    }
}

/// The [`DeepUse::Recv`] of call `call` on receiver `recv`.
fn recv_use(recv: &Expr, call: &Expr, args: &[Expr], named: &[(String, Expr)]) -> DeepUse {
    DeepUse::Recv {
        recv: recv.id,
        call: call.id,
        args: args
            .iter()
            .chain(named.iter().map(|(_, a)| a))
            .cloned()
            .collect(),
    }
}

/// [`Uses`] of one function body.
fn uses_of(decl: &FnDecl) -> Uses {
    walk_uses(decl, false)
}

/// TICKET-240 — [`Uses`] of a generator body for its frame verdict: `deep`, `matched` and `lost`
/// are filled too. A root is `lost` when one of its aliases has more than one binding site in
/// the frame, is an assignment target, or is itself in `escaped` (a closure, a comprehension or
/// another construct the walk does not enter reads it).
pub(super) fn frame_uses(decl: &FnDecl) -> Uses {
    walk_uses(decl, true)
}

fn walk_uses(decl: &FnDecl, deep: bool) -> Uses {
    let mut bound: HashSet<String> = decl.params.iter().map(|p| p.name.clone()).collect();
    crate::compiler::collect_frame_binds(&decl.body, &mut bound);
    let mut w = UseWalk {
        bound: &bound,
        uses: Uses::default(),
        deep,
        aliases: HashMap::new(),
        assigned: HashSet::new(),
    };
    w.block(&decl.body);
    if deep {
        let mut sites: Vec<String> = decl.params.iter().map(|p| p.name.clone()).collect();
        crate::compiler::collect_frame_binds(&decl.body, &mut sites);
        for (alias, root) in &w.aliases {
            if sites.iter().filter(|n| *n == alias).count() > 1
                || w.assigned.contains(alias)
                || w.uses.escaped.contains(alias)
            {
                w.uses.lost.insert(root.clone());
            }
        }
    }
    w.uses
}

/// What the summary pass reads of one fn declaration.
pub(super) struct Scan {
    /// TICKET-190: every use of every bare name, for the param-escape summary.
    pub uses: Uses,
    /// The scanned fn is a generator: calling it stores its arguments without running the body.
    is_generator: bool,
    params: Vec<String>,
}

/// TICKET-254 — peel the place links (`.f`, `[i]`) off `expr` and return the value the chain reads
/// from, with the count of links peeled. A subscript `is_slice` calls a builtin slice is no place
/// link: it builds a new value, so the peel stops there and that value is the base.
pub(super) fn place_base<'a>(
    expr: &'a Expr,
    is_slice: &impl Fn(crate::ast::Span) -> bool,
) -> (&'a Expr, usize) {
    match &expr.kind {
        ExprKind::Field { obj, .. } => {
            let (base, n) = place_base(obj, is_slice);
            (base, n + 1)
        }
        ExprKind::Index { obj, index, .. } => {
            if index.as_ref().is_some_and(|i| is_slice(i.span)) {
                (expr, 0)
            } else {
                let (base, n) = place_base(obj, is_slice);
                (base, n + 1)
            }
        }
        ExprKind::Ident(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => (expr, 0),
    }
}

/// TICKET-254 — `expr` is a call `obj.method(..)`: its receiver and method name.
pub(super) fn method_call_parts(expr: &Expr) -> Option<(&Expr, &str)> {
    match &expr.kind {
        ExprKind::Call { callee, .. } => field_parts(callee),
        ExprKind::Ident(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Field { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => None,
    }
}

/// `expr` is a field read `obj.name`: its object and the field name.
fn field_parts(expr: &Expr) -> Option<(&Expr, &str)> {
    match &expr.kind {
        ExprKind::Field { obj, name, .. } => Some((obj, name)),
        ExprKind::Ident(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => None,
    }
}

/// The object a place link (`.f`, `[i]`) reads from, and whether the link is a subscript, or `None`
/// when `expr` is no place link.
pub(super) fn place_parent(expr: &Expr) -> Option<(&Expr, bool)> {
    match &expr.kind {
        ExprKind::Field { obj, .. } => Some((obj, false)),
        ExprKind::Index { obj, .. } => Some((obj, true)),
        ExprKind::Ident(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => None,
    }
}

pub(super) fn scan(decl: &FnDecl) -> Scan {
    Scan {
        uses: uses_of(decl),
        is_generator: decl.is_generator,
        params: decl.params.iter().map(|p| p.name.clone()).collect(),
    }
}

impl Checker {
    pub(super) fn infer_fn_summaries(&mut self, stmts: &[Stmt]) {
        let scans: HashMap<String, Scan> = stmts
            .iter()
            .filter(|stmt| !self.skips_body(stmt))
            .filter_map(|stmt| {
                if let StmtKind::Fn(decl) = &stmt.kind {
                    Some((decl.name.clone(), scan(decl)))
                } else {
                    None
                }
            })
            .collect();
        #[cfg(any(test, feature = "test-api"))]
        {
            self.writer_scans += scans.len();
        }
        for (name, summary) in self.infer_scan_escapes(&scans) {
            if let Some(sig) = self.functions.get_mut(&name) {
                sig.summary = summary;
            }
        }
    }

    /// The least fixed point of every scanned top-level fn's [`FnSummary`]: a param escapes once
    /// [`Self::root_escapes`] says so under this pass's summaries.
    fn infer_scan_escapes(&self, scans: &HashMap<String, Scan>) -> HashMap<String, FnSummary> {
        let mut summaries: HashMap<String, FnSummary> = scans
            .iter()
            .map(|(name, scan)| {
                // A generator escapes every param; any other fn starts from "nothing escapes"
                // and only grows.
                let summary = FnSummary {
                    escapes: vec![scan.is_generator; scan.params.len()],
                    is_generator: scan.is_generator,
                };
                (name.clone(), summary)
            })
            .collect();
        loop {
            let old = summaries.clone();
            let mut changed = false;
            for (name, scan) in scans.iter().filter(|(_, scan)| !scan.is_generator) {
                let params = self.functions.get(name).map(|s| s.params.clone());
                let callee = |c: &ArgCallee, j: usize| self.arg_escapes(c, j, Some(&old));
                for (i, p) in scan.params.iter().enumerate() {
                    if old[name].escapes[i] {
                        continue;
                    }
                    let tys: Vec<Ty> = params
                        .as_ref()
                        .and_then(|ps| ps.get(i))
                        .cloned()
                        .into_iter()
                        .collect();
                    if self.root_escapes(&scan.uses, p, &tys, &callee)
                        && let Some(summary) = summaries.get_mut(name)
                    {
                        summary.escapes[i] = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        summaries
    }

    /// A nested fn's summary: every param escapes (a nested callee declines).
    pub(super) fn bind_nested_fn_summary(&mut self, decl: &FnDecl) {
        let summary = FnSummary {
            escapes: vec![true; decl.params.len()],
            is_generator: decl.is_generator,
        };
        if let Some(scope) = self.fn_write_scopes.last_mut() {
            scope.insert(decl.name.clone(), summary);
        }
    }

    pub(super) fn named_fn_summary(&self, callee: &Expr) -> Option<(String, FnSummary)> {
        match &callee.kind {
            ExprKind::Ident(name) => {
                if let Some(i) = self.owning_scope(name) {
                    // Only the scope that owns the binding: a local value shadowing a nested fn
                    // is a value callee and has no summary (TICKET-190 review).
                    self.fn_write_scopes
                        .get(i)
                        .and_then(|scope| scope.get(name).cloned())
                        .map(|summary| (name.clone(), summary))
                } else {
                    self.functions
                        .get(name)
                        .map(|sig| (name.clone(), sig.summary.clone()))
                }
            }
            ExprKind::Field { obj, name, .. } => {
                if let ExprKind::Ident(module) = &obj.kind
                    && !self.head_is_value(module)
                    && let Some(mid) = self.imported_modules.get(module)
                    && let Some(msig) = self.module_sigs.get(mid)
                {
                    msig.certain_fn(name)
                        .map(|sig: &FnSig| (name.clone(), sig.summary.clone()))
                } else {
                    None
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        }
    }

    /// TICKET-190 — does argument `j` of callee `c` escape the call? `local` holds this pass's
    /// same-module summaries during the fixed point. A missing, shadowed or value callee escapes,
    /// as do a generator's arguments, an index at or past `escapes.len()` and a variadic slot.
    pub(super) fn arg_escapes(
        &self,
        c: &ArgCallee,
        j: usize,
        local: Option<&HashMap<String, FnSummary>>,
    ) -> bool {
        let (sig, summary) = match c {
            ArgCallee::Fn(f) => {
                if self.lookup(f).is_some() || !self.functions.contains_key(f) {
                    return true;
                }
                let Some(sig) = self.functions.get(f) else {
                    return true;
                };
                (sig, local.and_then(|l| l.get(f)).unwrap_or(&sig.summary))
            }
            ArgCallee::Module(m, f) => {
                if self.head_is_value(m) {
                    return true;
                }
                let Some(sig) = self
                    .imported_modules
                    .get(m)
                    .and_then(|mid| self.module_sigs.get(mid))
                    .and_then(|ms| ms.certain_fn(f))
                else {
                    return true;
                };
                (sig, &sig.summary)
            }
        };
        summary.is_generator
            || sig.summary.is_generator
            || sig.variadic.is_some_and(|v| j >= v)
            || summary.escapes.get(j).copied().unwrap_or(true)
    }

    /// TICKET-190 — the one reader of [`Uses`]: can root `name`, of every type in `tys`, escape
    /// the body? It escapes when it is used as a plain value, when its type is unknown, when a
    /// kept use fails its type rule, or when `callee` lets its argument escape.
    pub(super) fn root_escapes(
        &self,
        uses: &Uses,
        name: &str,
        tys: &[Ty],
        callee: &dyn Fn(&ArgCallee, usize) -> bool,
    ) -> bool {
        uses.matched.contains(name) || self.root_leaves(uses, name, tys, callee)
    }

    /// [`Self::root_escapes`] without the `matched` rule: a frame verdict follows a matched
    /// root's binders as aliases instead.
    pub(super) fn root_leaves(
        &self,
        uses: &Uses,
        name: &str,
        tys: &[Ty],
        callee: &dyn Fn(&ArgCallee, usize) -> bool,
    ) -> bool {
        if uses.escaped.contains(name) || tys.is_empty() {
            return true;
        }
        uses.kept
            .iter()
            .filter(|(n, _)| n == name)
            .any(|(_, u)| match u {
                RootUse::Arg(c, j) => callee(c, *j),
                RootUse::Recv(_)
                | RootUse::Field(_)
                | RootUse::Index
                | RootUse::Op
                | RootUse::Iter => tys.iter().any(|t| !self.use_keeps_root(u, t)),
            })
    }

    /// TICKET-240 Rule G — is the whole graph of frame slot `name` private? The caller already
    /// knows every binding of it is all-fresh and its root stays in the frame. This adds: no
    /// alias was lost, the root is never an operator operand or an argument of a named callee,
    /// every store into the graph takes a fresh or markless value (and index), every method on a
    /// view is a native one whose result is markless, and every other view ends in a markless
    /// value. Dropping one of these turns a false fault into a lost write.
    pub(super) fn deep_private(
        &mut self,
        uses: &Uses,
        acc: &crate::checker::GenFrameAcc,
        name: &str,
    ) -> bool {
        if uses.lost.contains(name)
            || uses
                .kept
                .iter()
                .any(|(n, u)| n == name && matches!(u, RootUse::Op | RootUse::Arg(..)))
        {
            return false;
        }
        let markless = |id: crate::ast::NodeId| {
            acc.expr_tys
                .get(&id.0)
                .is_some_and(|t| !ret_may_hold_receiver(t, &Ty::Unknown))
        };
        for (_, u) in uses.deep.iter().filter(|(n, _)| n == name) {
            let stored: Vec<&Expr> = match u {
                DeepUse::Read { view } => {
                    if !markless(*view) {
                        return false;
                    }
                    continue;
                }
                DeepUse::Recv { recv, call, args } => {
                    let native = acc
                        .expr_tys
                        .get(&recv.0)
                        .is_some_and(|t| native_receiver(t.scalar()).is_some());
                    if !native || !markless(*call) {
                        return false;
                    }
                    args.iter().collect()
                }
                DeepUse::Store { value, index } => {
                    std::iter::once(&**value).chain(index.as_deref()).collect()
                }
            };
            for e in stored {
                if !markless(e.id) && !self.fresh_shape(e, 0, false).is_all() {
                    return false;
                }
            }
        }
        true
    }

    /// The type rule of one kept use: its result cannot alias a root of type `t`.
    pub(super) fn use_keeps_root(&self, u: &RootUse, t: &Ty) -> bool {
        let t = t.scalar();
        match u {
            // A native method keeps its receiver only when its declared return type cannot hold it.
            RootUse::Recv(m) => native_receiver(t).is_some_and(|(key, own)| {
                self.structs
                    .get(key)
                    .and_then(|info| info.methods.get(m))
                    .is_some_and(|sig| !ret_may_hold_receiver(&sig.ret, &own))
            }),
            RootUse::Field(f) => match t {
                Ty::Tuple(_) => true,
                Ty::Struct(key, _) => self
                    .struct_shape(key)
                    .is_some_and(|info| info.fields.iter().any(|(n, _)| n == f)),
                Ty::Int
                | Ty::Float
                | Ty::Bool
                | Ty::Str
                | Ty::Bytes
                | Ty::ByteArray
                | Ty::Nil
                | Ty::List(_)
                | Ty::Map(..)
                | Ty::Set(_)
                | Ty::Func { .. }
                | Ty::Enum(..)
                | Ty::Param(_)
                | Ty::Result(..)
                | Ty::Option(_)
                | Ty::Channel(_)
                | Ty::Shared(_)
                | Ty::Atomic(_)
                | Ty::AtomicInt
                | Ty::RwShared(_)
                | Ty::Executor
                | Ty::Socket
                | Ty::Listener
                | Ty::Writer
                | Ty::Reader
                | Ty::Ptr
                | Ty::Width(_)
                | Ty::Protocol(..)
                | Ty::Module(_)
                | Ty::Unknown
                | Ty::Var(_) => false,
            },
            RootUse::Index => matches!(
                t,
                Ty::List(_) | Ty::Map(..) | Ty::Str | Ty::Bytes | Ty::ByteArray | Ty::Tuple(_)
            ),
            RootUse::Op => matches!(
                t,
                Ty::Int
                    | Ty::Float
                    | Ty::Bool
                    | Ty::Str
                    | Ty::Bytes
                    | Ty::List(_)
                    | Ty::Map(..)
                    | Ty::Set(_)
                    | Ty::ByteArray
                    | Ty::Tuple(_)
            ),
            RootUse::Iter => matches!(
                t,
                Ty::List(_) | Ty::Map(..) | Ty::Set(_) | Ty::Str | Ty::Bytes | Ty::ByteArray
            ),
            RootUse::Arg(..) => false,
        }
    }
}
