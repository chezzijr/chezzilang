//! TICKET-225 — the one constant evaluator. The checker's overflow lint, its width check (an untyped
//! constant meeting an `int8`/`float32` slot) and the compiler's peephole fold all read it, so a
//! constant has one value everywhere. TICKET-268: it is also the one owner of an int op's value and
//! fault. The VM's int ops call [`int_binop`]; a zero divisor or a shift amount outside `0..64` is a
//! fault the checker reports when the right operand is a constant ([`operand_fault`]).

use super::{BinaryOp, Expr, ExprKind, Span, UnaryOp};

/// A folded constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
}

/// The result of [`eval`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fold {
    Value(Const),
    /// The innermost node whose int arithmetic always faults, and its fault.
    Fault(Span, IntFault),
    NotConst,
}

/// The op an int overflow fault names. A binary op is named by its `HOOKS` protocol.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntOp {
    Bin(BinaryOp),
    Neg,
    Abs,
    Lcm,
    Gcd,
}

/// The run-time fault of one int op.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntFault {
    Overflow(IntOp),
    DivByZero,
    ModByZero,
    ShiftRange(i64),
}

impl IntFault {
    /// The fault's run-time text. `protocol` names a binary op: the caller passes its `HOOKS`
    /// protocol, which `chezzi-base` owns (`base::hooks::int_fault_message`). `negation` has a
    /// `HOOKS` row (`Neg`) but keeps its own name; `abs`, `lcm` and `gcd` have no row.
    pub fn message(self, protocol: impl FnOnce(BinaryOp) -> &'static str) -> String {
        match self {
            IntFault::Overflow(op) => {
                let name = match op {
                    IntOp::Bin(b) => protocol(b),
                    IntOp::Neg => "negation",
                    IntOp::Abs => "abs",
                    IntOp::Lcm => "lcm",
                    IntOp::Gcd => "gcd",
                };
                format!("integer overflow in {name}")
            }
            IntFault::DivByZero => "division by zero".to_string(),
            IntFault::ModByZero => "modulo by zero".to_string(),
            IntFault::ShiftRange(b) => format!("shift amount {b} out of range (0..64)"),
        }
    }
}

/// The fault the right operand `b` gives `a OP b` for every int `a`: a zero divisor, or a shift
/// amount outside `0..64`. [`int_binop`] reads it first.
#[inline(always)]
pub fn operand_fault(op: BinaryOp, b: i64) -> Option<IntFault> {
    match op {
        BinaryOp::Div if b == 0 => Some(IntFault::DivByZero),
        BinaryOp::Mod if b == 0 => Some(IntFault::ModByZero),
        BinaryOp::Shl | BinaryOp::Shr if !(0..64).contains(&b) => Some(IntFault::ShiftRange(b)),
        BinaryOp::Div
        | BinaryOp::Mod
        | BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq
        | BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::In => None,
    }
}

/// Int unary `-`: `-MIN` overflows.
#[inline]
pub fn int_neg(a: i64) -> Result<i64, IntFault> {
    a.checked_neg().ok_or(IntFault::Overflow(IntOp::Neg))
}

/// One int binary op with the VM's semantics, the one owner of its value and its fault.
/// `Some(Err(fault))` is the fault the VM raises; `None` is an op that is not int arithmetic.
#[inline(always)]
pub fn int_binop(op: BinaryOp, a: i64, b: i64) -> Option<Result<i64, IntFault>> {
    if let Some(f) = operand_fault(op, b) {
        return Some(Err(f));
    }
    let overflow = || IntFault::Overflow(IntOp::Bin(op));
    match op {
        BinaryOp::Add => Some(a.checked_add(b).ok_or_else(overflow)),
        BinaryOp::Sub => Some(a.checked_sub(b).ok_or_else(overflow)),
        BinaryOp::Mul => Some(a.checked_mul(b).ok_or_else(overflow)),
        BinaryOp::Div => Some(a.checked_div(b).ok_or_else(overflow)),
        // `MIN % -1` is mathematically 0: `wrapping_rem` gives it, where `checked_rem` faults.
        BinaryOp::Mod => Some(Ok(a.wrapping_rem(b))),
        BinaryOp::BitAnd => Some(Ok(a & b)),
        BinaryOp::BitOr => Some(Ok(a | b)),
        BinaryOp::BitXor => Some(Ok(a ^ b)),
        BinaryOp::Shl => {
            let v = a << (b as u32);
            Some(if (v >> (b as u32)) == a {
                Ok(v)
            } else {
                Err(overflow())
            })
        }
        BinaryOp::Shr => Some(Ok(a >> (b as u32))),
        BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq
        | BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::In => None,
    }
}

/// One float binary op (`+ - * / %`, IEEE). A zero divisor is not folded (`None`).
pub fn float_binop(op: BinaryOp, a: f64, b: f64) -> Option<f64> {
    match op {
        BinaryOp::Add => Some(a + b),
        BinaryOp::Sub => Some(a - b),
        BinaryOp::Mul => Some(a * b),
        BinaryOp::Div | BinaryOp::Mod if b == 0.0 => None,
        BinaryOp::Div => Some(a / b),
        BinaryOp::Mod => Some(a % b),
        BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq
        | BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::And
        | BinaryOp::Or
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr
        | BinaryOp::In => None,
    }
}

/// Fold a constant expression: int and float literals under unary `-` and the binary ops
/// [`int_binop`] / [`float_binop`] fold. Mixed int/float operands are `NotConst`. Any other node
/// kind is `NotConst` without descending, so a caller runs one scan per maximal arithmetic tree.
/// `visits` counts the nodes entered, once each, so a test can pin that the scan is linear.
pub fn eval(e: &Expr, visits: &mut usize) -> Fold {
    eval_noting(e, visits, &mut |_, _, _| {})
}

/// [`eval`], and `note(span, op, fault)` for each `Binary` node whose left operand is not a constant
/// and whose constant right operand faults for every int left operand ([`operand_fault`]). The
/// checker reports a noted node once it knows the left operand is an `int`.
pub fn eval_noting(
    e: &Expr,
    visits: &mut usize,
    note: &mut dyn FnMut(Span, BinaryOp, IntFault),
) -> Fold {
    *visits += 1;
    match &e.kind {
        ExprKind::Int(n) => Fold::Value(Const::Int(*n)),
        ExprKind::Float(f) => Fold::Value(Const::Float(*f)),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => match eval_noting(expr, visits, note) {
            Fold::Value(Const::Int(v)) => match int_neg(v) {
                Ok(n) => Fold::Value(Const::Int(n)),
                Err(f) => Fold::Fault(e.span, f),
            },
            Fold::Value(Const::Float(f)) => Fold::Value(Const::Float(-f)),
            other @ Fold::Fault(..) | other @ Fold::NotConst => other,
        },
        ExprKind::Binary { op, lhs, rhs } => {
            let l = eval_noting(lhs, visits, note);
            let r = eval_noting(rhs, visits, note);
            if let (Fold::NotConst, Fold::Value(Const::Int(b))) = (l, r)
                && let Some(f) = operand_fault(*op, b)
            {
                note(e.span, *op, f);
            }
            match (l, r) {
                (o @ Fold::Fault(..), _) | (_, o @ Fold::Fault(..)) => o,
                (Fold::Value(Const::Int(a)), Fold::Value(Const::Int(b))) => {
                    match int_binop(*op, a, b) {
                        Some(Ok(v)) => Fold::Value(Const::Int(v)),
                        Some(Err(f)) => Fold::Fault(e.span, f),
                        None => Fold::NotConst,
                    }
                }
                (Fold::Value(Const::Float(a)), Fold::Value(Const::Float(b))) => {
                    match float_binop(*op, a, b) {
                        Some(v) => Fold::Value(Const::Float(v)),
                        None => Fold::NotConst,
                    }
                }
                _ => Fold::NotConst,
            }
        }
        ExprKind::Str(_)
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
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Field { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => Fold::NotConst,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_expr(src: &str) -> Expr {
        let toks = crate::lexer::tokenize(&format!("x := {src}\n")).unwrap();
        let m = crate::parser::parse(toks).unwrap();
        match m.stmts.into_iter().next().unwrap().kind {
            super::super::StmtKind::Let { value, .. } => value,
            k => panic!("not a let: {k:?}"),
        }
    }

    #[test]
    fn eval_folds_every_pure_op_and_reports_the_innermost_overflow() {
        let mut n = 0;
        let v = |s: &str, n: &mut usize| eval(&parse_expr(s), n);
        assert_eq!(v("1 << 8", &mut n), Fold::Value(Const::Int(256)));
        assert_eq!(v("127 | (127 + 1)", &mut n), Fold::Value(Const::Int(255)));
        assert_eq!(v("0 - 1", &mut n), Fold::Value(Const::Int(-1)));
        assert_eq!(v("3e38 + 3e38", &mut n), Fold::Value(Const::Float(6e38)));
        assert_eq!(v("-3e38 * 2.0", &mut n), Fold::Value(Const::Float(-6e38)));
        assert!(matches!(
            v("1 << 63", &mut n),
            Fold::Fault(_, IntFault::Overflow(IntOp::Bin(BinaryOp::Shl)))
        ));
        assert!(matches!(
            v("(9223372036854775807 + 1) * 2", &mut n),
            Fold::Fault(s, IntFault::Overflow(IntOp::Bin(BinaryOp::Add))) if s.col > 1
        ));
        // A zero divisor and a shift amount outside `0..64` are faults, not values.
        assert!(matches!(
            v("1 / 0", &mut n),
            Fold::Fault(_, IntFault::DivByZero)
        ));
        assert!(matches!(
            v("1 % 0", &mut n),
            Fold::Fault(_, IntFault::ModByZero)
        ));
        assert!(matches!(
            v("1 << 64", &mut n),
            Fold::Fault(_, IntFault::ShiftRange(64))
        ));
        assert!(matches!(
            v("1 >> -1", &mut n),
            Fold::Fault(_, IntFault::ShiftRange(-1))
        ));
        // A float zero divisor is legal (IEEE), mixed operands and a non-constant leaf are not folded.
        assert_eq!(v("1.0 / 0.0", &mut n), Fold::NotConst);
        assert_eq!(v("1 + 2.0", &mut n), Fold::NotConst);
        assert_eq!(v("len([1]) + 1", &mut n), Fold::NotConst);
    }

    #[test]
    fn eval_noting_notes_a_constant_operand_that_faults_for_every_left_operand() {
        let mut n = 0;
        let mut notes = Vec::new();
        let y = parse_expr("y / 0");
        let f = eval_noting(&y, &mut n, &mut |_, op, f| notes.push((op, f)));
        assert_eq!(f, Fold::NotConst);
        assert_eq!(notes, vec![(BinaryOp::Div, IntFault::DivByZero)]);
        // A fully constant node is a `Fault`, not a note.
        let mut notes = Vec::new();
        let k = parse_expr("1 / 0");
        eval_noting(&k, &mut n, &mut |_, op, f| notes.push((op, f)));
        assert!(notes.is_empty());
    }
}
