//! TICKET-225 — the one constant evaluator. The checker's overflow lint, its width check (an untyped
//! constant meeting an `int8`/`float32` slot) and the compiler's peephole fold all read it, so a
//! constant has one value everywhere. Int arithmetic follows the VM (`src/vm/arith.rs`): `+ - * /`
//! and `<<` overflow, `%` wraps, and a zero divisor or a shift amount outside `0..64` stays a
//! runtime fault (`None` here, never folded).

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
    /// The innermost node whose int arithmetic overflows, and the VM's name for its op.
    Overflow(Span, &'static str),
    NotConst,
}

/// One int binary op with the VM's semantics. `Some(Err(name))` is an overflow the VM reports as
/// `integer overflow in {name}`; `None` is a runtime fault of another kind (a zero divisor, a shift
/// amount outside `0..64`) or an op that is not int arithmetic.
pub fn int_binop(op: BinaryOp, a: i64, b: i64) -> Option<Result<i64, &'static str>> {
    let checked = |r: Option<i64>, name| Some(r.ok_or(name));
    match op {
        BinaryOp::Add => checked(a.checked_add(b), "Add"),
        BinaryOp::Sub => checked(a.checked_sub(b), "Sub"),
        BinaryOp::Mul => checked(a.checked_mul(b), "Mul"),
        BinaryOp::Div | BinaryOp::Mod if b == 0 => None,
        BinaryOp::Div => checked(a.checked_div(b), "Div"),
        BinaryOp::Mod => Some(Ok(a.wrapping_rem(b))),
        BinaryOp::BitAnd => Some(Ok(a & b)),
        BinaryOp::BitOr => Some(Ok(a | b)),
        BinaryOp::BitXor => Some(Ok(a ^ b)),
        BinaryOp::Shl | BinaryOp::Shr if !(0..64).contains(&b) => None,
        BinaryOp::Shl => {
            let v = a << (b as u32);
            Some(if (v >> (b as u32)) == a {
                Ok(v)
            } else {
                Err("Shl")
            })
        }
        BinaryOp::Shr => Some(Ok(a >> (b as u32))),
        _ => None,
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
        _ => None,
    }
}

/// Fold a constant expression: int and float literals under unary `-` and the binary ops
/// [`int_binop`] / [`float_binop`] fold. Mixed int/float operands are `NotConst`. Any other node
/// kind is `NotConst` without descending, so a caller runs one scan per maximal arithmetic tree.
/// `visits` counts the nodes entered, once each, so a test can pin that the scan is linear.
pub fn eval(e: &Expr, visits: &mut usize) -> Fold {
    *visits += 1;
    match &e.kind {
        ExprKind::Int(n) => Fold::Value(Const::Int(*n)),
        ExprKind::Float(f) => Fold::Value(Const::Float(*f)),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr,
        } => match eval(expr, visits) {
            Fold::Value(Const::Int(v)) => match v.checked_neg() {
                Some(n) => Fold::Value(Const::Int(n)),
                None => Fold::Overflow(e.span, "negation"),
            },
            Fold::Value(Const::Float(f)) => Fold::Value(Const::Float(-f)),
            other => other,
        },
        ExprKind::Binary { op, lhs, rhs } => {
            let l = eval(lhs, visits);
            let r = eval(rhs, visits);
            match (l, r) {
                (o @ Fold::Overflow(..), _) | (_, o @ Fold::Overflow(..)) => o,
                (Fold::Value(Const::Int(a)), Fold::Value(Const::Int(b))) => {
                    match int_binop(*op, a, b) {
                        Some(Ok(v)) => Fold::Value(Const::Int(v)),
                        Some(Err(name)) => Fold::Overflow(e.span, name),
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
        _ => Fold::NotConst,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `int_binop` against the real VM: each op x each operand pair runs as `a OP b` on runtime
    /// values inside `recover:`, and the VM's value or fault must be what `int_binop` predicts.
    #[test]
    fn consteval_matches_vm_arith() {
        let ops = [
            (BinaryOp::Add, "+"),
            (BinaryOp::Sub, "-"),
            (BinaryOp::Mul, "*"),
            (BinaryOp::Div, "/"),
            (BinaryOp::Mod, "%"),
            (BinaryOp::BitAnd, "&"),
            (BinaryOp::BitOr, "|"),
            (BinaryOp::BitXor, "^"),
            (BinaryOp::Shl, "<<"),
            (BinaryOp::Shr, ">>"),
        ];
        let vals = [0, 1, -1, i64::MAX, i64::MIN, 63, 64];
        let lit = |v: i64| {
            if v == i64::MIN {
                "(0 - 9223372036854775807 - 1)".to_string()
            } else {
                v.to_string()
            }
        };
        let mut src = String::new();
        for (i, (_, sym)) in ops.iter().enumerate() {
            src += &format!(
                "fn t{i}(a: int, b: int):\n    r := recover: a {sym} b\n    match r:\n        Ok(v): print(str(v))\n        Err(e): print(\"F \" + e.message())\n"
            );
        }
        let mut want = Vec::new();
        for (i, (op, _)) in ops.iter().enumerate() {
            for a in vals {
                for b in vals {
                    src += &format!("t{i}({}, {})\n", lit(a), lit(b));
                    want.push((*op, a, b, int_binop(*op, a, b)));
                }
            }
        }
        let out = crate::vm::run_capture(&src).expect("the program runs");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), want.len(), "one line per op pair");
        let mut red = Vec::new();
        for ((op, a, b, w), got) in want.iter().zip(&lines) {
            let agrees = match w {
                Some(Ok(v)) => *got == v.to_string(),
                Some(Err(name)) => {
                    got.starts_with("F ") && got.contains(&format!("integer overflow in {name}"))
                }
                None => got.starts_with("F ") && !got.contains("integer overflow"),
            };
            if !agrees {
                red.push(format!("{op:?}({a}, {b}): int_binop {w:?}, vm {got:?}"));
            }
        }
        assert!(red.is_empty(), "{}", red.join("\n"));
    }

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
        assert!(matches!(v("1 << 63", &mut n), Fold::Overflow(_, "Shl")));
        assert!(matches!(
            v("(9223372036854775807 + 1) * 2", &mut n),
            Fold::Overflow(s, "Add") if s.col > 1
        ));
        // A runtime fault of another kind, mixed operands, and a non-constant leaf are not folded.
        assert_eq!(v("1 / 0", &mut n), Fold::NotConst);
        assert_eq!(v("1 << 64", &mut n), Fold::NotConst);
        assert_eq!(v("1 + 2.0", &mut n), Fold::NotConst);
        assert_eq!(v("len([1]) + 1", &mut n), Fold::NotConst);
    }
}
