//! Abstract IR for generated programs — the single source of truth that both the Chezzi
//! and Python emitters render. Expressions carry their static `Ty` so the emitters are
//! total and the generator can bound integer magnitudes / avoid divide-by-zero statically.
//!
//! The IR is deliberately a *cross-language safe subset*: only constructs that have a
//! well-defined, identical meaning in Chezzi and (via the shim) Python.

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Int,
    Bool,
    Str,
    Float,
    List(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
    Concat, // string / list concatenation via `+`
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg, // -x  (int/float)
    Not, // not x  (bool)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Set, // =
    Add, // +=
    Sub, // -=
    Mul, // *=
}

#[derive(Clone, Debug)]
pub enum Expr {
    IntLit(i64),
    BoolLit(bool),
    StrLit(String),
    FloatLit(f64),
    Var(String),
    Unary {
        op: UnOp,
        ty: Ty,
        e: Box<Expr>,
    },
    Bin {
        op: BinOp,
        ty: Ty, // result type
        l: Box<Expr>,
        r: Box<Expr>,
    },
    Call {
        name: String,
        ret: Ty,
        args: Vec<Expr>,
    },
    ListLit {
        elem: Ty,
        items: Vec<Expr>,
    },
    MapLit {
        k: Ty,
        v: Ty,
        entries: Vec<(Expr, Expr)>,
    },
    Index {
        ret: Ty,
        base: Box<Expr>,
        idx: Box<Expr>,
    },
    Len(Box<Expr>),
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let {
        name: String,
        ty: Ty,
        init: Expr,
    },
    Assign {
        name: String,
        op: AssignOp,
        value: Expr,
    },
    If {
        cond: Expr,
        then: Block,
        els: Option<Block>,
    },
    While {
        cond: Expr,
        body: Block,
    },
    ForRange {
        var: String,
        start: Expr,
        end: Expr,
        body: Block,
    },
    Print(Vec<Expr>),
    Return(Option<Expr>),
    Eval(Expr),
}

pub type Block = Vec<Stmt>;

#[derive(Clone, Debug)]
pub struct Func {
    pub name: String,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Program {
    pub funcs: Vec<Func>,
    pub main: Block,
}
