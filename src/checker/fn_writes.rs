//! Checker: the place and operator facts the temporary-write warning reads (TICKET-247, TICKET-254):
//! bound call slots, operator aliasing, native receivers and containers, place links.

use super::{Checker, Ty};
use crate::ast::{BinaryOp, Expr, ExprKind, UnaryOp};

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

/// TICKET-247 — what the result of an operator node is to its operands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum OperandAlias {
    /// A new object that holds each operand (`?x`, `!x`).
    Holds,
    /// A value the operator computes. It is no operand when the operand's type has no user
    /// operator ([`Checker::op_keeps_root`] owns that type rule).
    Fresh,
    /// One of the operands itself (`a and b`, `a or b`, `a ?? b`).
    Child,
}

/// TICKET-247 — the one decider of "can the result of this operator node be, or hold, an
/// operand?". `Checker::fresh_shape` reads it. Each operator enum is matched
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

/// The native method table that owns a receiver type's methods.
pub(super) fn native_receiver(t: &Ty) -> Option<&'static str> {
    match t {
        Ty::List(_) => Some("List"),
        Ty::Map(..) => Some("Map"),
        Ty::Set(_) => Some("Set"),
        Ty::Str => Some("str"),
        Ty::Bytes => Some("bytes"),
        Ty::ByteArray => Some("bytearray"),
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

impl Checker {
    /// TICKET-247 — the result of an operator [`aliases_operand`] calls `Fresh` holds no operand of
    /// type `t`: a type with no user operator.
    pub(super) fn op_keeps_root(&self, t: &Ty) -> bool {
        matches!(
            t.scalar(),
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
        )
    }
}
