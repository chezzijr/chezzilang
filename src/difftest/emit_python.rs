//! Render a `Program` as Python source.
//!
//! The emitted Python calls a fixed *shim* prelude that implements Chezzi's **specified**
//! behaviour for the handful of by-design surface/semantic differences: value stringifying
//! (`true`/`false`/`nil`, raw nested strings, Chezzi float formatting) and integer
//! `/`,`%` (truncate-toward-zero, sign-of-dividend). The shim mirrors the *spec*, while the
//! Chezzi source uses the real *implementation* — so any divergence in stdout is a genuine
//! deviation of the implementation from its own contract, never a by-design difference.

use super::ast::*;
use super::emit_chezzi::float_lit;

/// Prelude prepended to every emitted Python program.
pub const SHIM: &str = r#"import math as _math
def _chz_div(a, b):
    q = abs(a) // abs(b)
    return q if (a < 0) == (b < 0) else -q
def _chz_mod(a, b):
    return a - _chz_div(a, b) * b
def _chz_str(v):
    if v is True: return "true"
    if v is False: return "false"
    if v is None: return "nil"
    if isinstance(v, float):
        if v != v: return "NaN"
        if v == float("inf"): return "inf"
        if v == float("-inf"): return "-inf"
        return ("%.1f" % v) if v.is_integer() else repr(v)
    if isinstance(v, str): return v
    if isinstance(v, list): return "[" + ", ".join(_chz_repr(x) for x in v) + "]"
    if isinstance(v, dict): return "{" + ", ".join(_chz_repr(k) + ": " + _chz_repr(x) for k, x in v.items()) + "}"
    return str(v)
def _chz_repr(v):
    return v if isinstance(v, str) else _chz_str(v)
def _chz_print(*a):
    print(*[_chz_str(x) for x in a])
"#;

pub fn emit(p: &Program) -> String {
    let mut e = Emitter { out: String::new() };
    e.out.push_str(SHIM);
    e.out.push('\n');
    for f in &p.funcs {
        e.func(f);
        e.out.push('\n');
    }
    e.block(&p.main, 0);
    e.out
}

struct Emitter {
    out: String,
}

impl Emitter {
    fn indent(&mut self, n: usize) {
        for _ in 0..n {
            self.out.push_str("    ");
        }
    }

    fn func(&mut self, f: &Func) {
        self.out.push_str("def ");
        self.out.push_str(&f.name);
        self.out.push('(');
        for (i, (name, _)) in f.params.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.out.push_str(name);
        }
        self.out.push_str("):\n");
        self.block(&f.body, 1);
    }

    fn block(&mut self, b: &Block, depth: usize) {
        if b.is_empty() {
            self.indent(depth);
            self.out.push_str("pass\n");
            return;
        }
        for s in b {
            self.stmt(s, depth);
        }
    }

    fn stmt(&mut self, s: &Stmt, depth: usize) {
        self.indent(depth);
        match s {
            Stmt::Let { name, init, .. } => {
                self.out.push_str(name);
                self.out.push_str(" = ");
                self.expr(init);
                self.out.push('\n');
            }
            Stmt::Assign { name, op, value } => {
                self.out.push_str(name);
                self.out.push_str(match op {
                    AssignOp::Set => " = ",
                    AssignOp::Add => " += ",
                    AssignOp::Sub => " -= ",
                    AssignOp::Mul => " *= ",
                });
                self.expr(value);
                self.out.push('\n');
            }
            Stmt::If { cond, then, els } => {
                self.out.push_str("if ");
                self.expr(cond);
                self.out.push_str(":\n");
                self.block(then, depth + 1);
                if let Some(els) = els {
                    self.indent(depth);
                    self.out.push_str("else:\n");
                    self.block(els, depth + 1);
                }
            }
            Stmt::While { cond, body } => {
                self.out.push_str("while ");
                self.expr(cond);
                self.out.push_str(":\n");
                self.block(body, depth + 1);
            }
            Stmt::ForRange {
                var,
                start,
                end,
                body,
            } => {
                self.out.push_str("for ");
                self.out.push_str(var);
                self.out.push_str(" in range(");
                self.expr(start);
                self.out.push_str(", ");
                self.expr(end);
                self.out.push_str("):\n");
                self.block(body, depth + 1);
            }
            Stmt::Print(args) => {
                self.out.push_str("_chz_print(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(a);
                }
                self.out.push_str(")\n");
            }
            Stmt::Return(e) => {
                self.out.push_str("return");
                if let Some(e) = e {
                    self.out.push(' ');
                    self.expr(e);
                }
                self.out.push('\n');
            }
            Stmt::Eval(e) => {
                self.expr(e);
                self.out.push('\n');
            }
        }
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::IntLit(n) => self.out.push_str(&n.to_string()),
            Expr::BoolLit(b) => self.out.push_str(if *b { "True" } else { "False" }),
            Expr::StrLit(s) => {
                self.out.push('"');
                self.out.push_str(s);
                self.out.push('"');
            }
            Expr::FloatLit(f) => self.out.push_str(&float_lit(*f)),
            Expr::Var(name) => self.out.push_str(name),
            Expr::Unary { op, e, .. } => {
                self.out.push('(');
                self.out.push_str(match op {
                    UnOp::Neg => "-",
                    UnOp::Not => "not ",
                });
                self.expr(e);
                self.out.push(')');
            }
            Expr::Bin { op, ty, l, r } => self.bin(*op, ty, l, r),
            Expr::Call { name, args, .. } => {
                self.out.push_str(name);
                self.out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(a);
                }
                self.out.push(')');
            }
            Expr::ListLit { items, .. } => {
                self.out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(it);
                }
                self.out.push(']');
            }
            Expr::MapLit { entries, .. } => {
                self.out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(k);
                    self.out.push_str(": ");
                    self.expr(v);
                }
                self.out.push('}');
            }
            Expr::Index { base, idx, .. } => {
                self.out.push('(');
                self.expr(base);
                self.out.push('[');
                self.expr(idx);
                self.out.push_str("])");
            }
            Expr::Len(base) => {
                self.out.push_str("len(");
                self.expr(base);
                self.out.push(')');
            }
        }
    }

    /// Integer `/` and `%` route through the shim (Chezzi semantics); everything else is the
    /// matching native Python operator.
    fn bin(&mut self, op: BinOp, ty: &Ty, l: &Expr, r: &Expr) {
        if *ty == Ty::Int && matches!(op, BinOp::Div | BinOp::Mod) {
            self.out.push_str(if op == BinOp::Div {
                "_chz_div("
            } else {
                "_chz_mod("
            });
            self.expr(l);
            self.out.push_str(", ");
            self.expr(r);
            self.out.push(')');
            return;
        }
        self.out.push('(');
        self.expr(l);
        self.out.push(' ');
        self.out.push_str(match op {
            BinOp::Add | BinOp::Concat => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::And => "and",
            BinOp::Or => "or",
        });
        self.out.push(' ');
        self.expr(r);
        self.out.push(')');
    }
}
