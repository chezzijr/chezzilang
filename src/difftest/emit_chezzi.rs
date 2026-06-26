//! Render a `Program` as Chezzi source using native operators and `print`.
//!
//! Every binary/unary expression is fully parenthesized so evaluation order is explicit and
//! cannot diverge from the Python rendering through precedence differences.

use super::ast::*;

pub fn emit(p: &Program) -> String {
    let mut e = Emitter { out: String::new() };
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
        self.out.push_str("fn ");
        self.out.push_str(&f.name);
        self.out.push('(');
        for (i, (name, ty)) in f.params.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            self.out.push_str(name);
            self.out.push_str(": ");
            self.out.push_str(&ty_str(ty));
        }
        self.out.push_str(") -> ");
        self.out.push_str(&ty_str(&f.ret));
        self.out.push_str(":\n");
        self.block(&f.body, 1);
    }

    fn block(&mut self, b: &Block, depth: usize) {
        if b.is_empty() {
            // Chezzi needs a body; `pass`-equivalent is an empty print? Use a no-op.
            self.indent(depth);
            self.out.push_str("_ := 0\n");
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
                self.out.push_str(" := ");
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
                self.out.push_str("print(");
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
            Expr::BoolLit(b) => self.out.push_str(if *b { "true" } else { "false" }),
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
            Expr::Bin { op, l, r, .. } => {
                self.out.push('(');
                self.expr(l);
                self.out.push(' ');
                self.out.push_str(binop_str(*op));
                self.out.push(' ');
                self.expr(r);
                self.out.push(')');
            }
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
                if entries.is_empty() {
                    // empty map literal — Chezzi `{}` is an empty map
                    self.out.push_str("{}");
                    return;
                }
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
                self.out.push('(');
                self.expr(base);
                self.out.push_str(").len()");
            }
        }
    }
}

fn ty_str(t: &Ty) -> String {
    match t {
        Ty::Int => "int".into(),
        Ty::Bool => "bool".into(),
        Ty::Str => "str".into(),
        Ty::Float => "float".into(),
        Ty::List(e) => format!("List[{}]", ty_str(e)),
        Ty::Map(k, v) => format!("Map[{}, {}]", ty_str(k), ty_str(v)),
    }
}

fn binop_str(op: BinOp) -> &'static str {
    match op {
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
    }
}

/// Render a float literal so it always parses as a float in Chezzi (needs a `.` or exponent).
pub fn float_lit(f: f64) -> String {
    if f == f.trunc() && f.is_finite() {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}
