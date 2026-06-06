//! Abstract syntax tree (M2): the typed node shapes the parser produces from the lexer's
//! `Tok` stream. `Stmt` and `Expr` carry a `Span` (from the lexer) so later phases — and
//! parse errors — can point at exact source locations. Declarations (`FnDecl`, `Field`, …)
//! live inside the statement that owns them and inherit its span.
//!
//! Everything derives `Debug` so `chezzi ast` can pretty-print a tree with `{:#?}`.

pub use crate::lexer::Span;

/// A whole parsed source file: a flat sequence of top-level statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub stmts: Vec<Stmt>,
}

/// A block is just a list of statements (a function body, an `if` arm, a loop body, …).
pub type Block = Vec<Stmt>;

// ===== statements =====

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// `x := expr` (ty = None) or `name: Type = expr` (ty = Some). `names` holds one binding for a
    /// plain/typed let, or two-or-more for a destructuring let (`a, b := pair()`); a destructuring
    /// let always has `ty = None`.
    Let {
        names: Vec<String>,
        ty: Option<Type>,
        value: Expr,
    },
    /// `target op value`, e.g. `count += 1`, `x = 3`.
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
    },
    /// A top-level or method function definition.
    Fn(FnDecl),
    /// `struct Name:` with fields and (optionally) methods.
    Struct {
        name: String,
        fields: Vec<Field>,
        methods: Vec<FnDecl>,
    },
    /// `enum Name:` with its variants.
    Enum {
        name: String,
        variants: Vec<Variant>,
    },
    /// `if` / `else if` / `else`. Each `(cond, body)` is one branch; `else if` adds another
    /// branch; a final bare `else` is `else_block`.
    If {
        branches: Vec<(Expr, Block)>,
        else_block: Option<Block>,
    },
    /// `for var in iter:` — `iter` may be a range (`0..10`) or any iterable expression. `vars` holds
    /// one binding for the common form, or two (`for k, v in m:`) to destructure a map's entries.
    For {
        vars: Vec<String>,
        iter: Expr,
        body: Block,
    },
    /// `while cond:`.
    While {
        cond: Expr,
        body: Block,
    },
    /// `match scrutinee:` with its arms.
    Match {
        scrutinee: Expr,
        arms: Vec<MatchArm>,
    },
    /// `return` with an optional value.
    Return(Option<Expr>),
    /// `break` — exit the innermost enclosing loop. Carries only its `Span` (via `Stmt`).
    Break,
    /// `continue` — skip to the next iteration of the innermost enclosing loop.
    Continue,
    /// An `import …` statement.
    Import(Import),
    /// A bare expression used as a statement, e.g. `print(x)`.
    Expr(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignOp {
    Eq,      // =
    PlusEq,  // +=
    MinusEq, // -=
}

/// A function definition — used both for top-level `fn`s and `struct` methods.
#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Type>, // None ⇒ returns nothing
    pub body: Block,
}

/// A function or closure parameter. `ty` is `None` for closure params, whose types are inferred.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Option<Type>,
}

/// A struct field: `name: Type`.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
}

/// An enum variant: `Circle(int)` → payload `[int]`; `Point` → payload `[]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub payload: Vec<Type>,
}

/// One arm of a `match`: `pattern: body`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Block,
}

/// A match pattern. Variant patterns with optional sub-patterns (`Circle(r)`, `Ok(v)`, `Point`,
/// `None`, `Cons(h, Some(t))`), tuple patterns (`(a, b)`, `(0, x)` — gap #15), literal patterns
/// (`0`, `"a"`, `true`) against int/str/bool scrutinees, a plain binding name in a sub-position,
/// and a `_` wildcard catch-all. Sub-patterns nest arbitrarily (`Variant`/`Tuple` bindings are
/// themselves `Pattern`s).
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// A binding name in a sub-position (a variant payload slot or tuple element), e.g. the `h` and
    /// `t` in `Cons(h, t)` or the `a`/`b` in `(a, b)`. A bare identifier at the *top* of an arm is a
    /// nullary `Variant` instead (it names a variant like `None`).
    Ident(String),
    Variant {
        name: String,
        bindings: Vec<Pattern>,
    },
    /// A tuple pattern, e.g. `(a, b)` or `(0, _)`. Two-or-more elements (matches a tuple value).
    Tuple(Vec<Pattern>),
    /// An int/str/bool literal pattern. Float is intentionally excluded (float equality footgun).
    Literal(LitPattern),
    /// The `_` catch-all arm.
    Wildcard,
}

/// A literal value usable as a `match` pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum LitPattern {
    Int(i64),
    Str(String),
    Bool(bool),
}

/// The four import forms (syntax.md §12).
#[derive(Debug, Clone, PartialEq)]
pub enum Import {
    /// `import a.b.c` or `import a.b.c as alias`.
    Module {
        path: Vec<String>,
        alias: Option<String>,
    },
    /// `import n1, n2 as a, … from a.b` — each name with an optional alias.
    From {
        path: Vec<String>,
        names: Vec<(String, Option<String>)>,
    },
}

// ===== types =====

/// A type annotation: `int`, `str`, `Point`, or a generic `list[int]`, `map[K, V]`, `Result[int]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Named(String),
    Generic(String, Vec<Type>),
    Func { params: Vec<Type>, ret: Box<Type> },
    /// `(T1, T2, …)` — a tuple type (always ≥2 elements; a 1-element `(T)` unwraps to `T`).
    Tuple(Vec<Type>),
}

// ===== expressions =====

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(String), // raw contents; interpolation parsing deferred
    Bool(bool),
    Ident(String),
    /// `[a, b, c]`
    List(Vec<Expr>),
    /// `(a, b, …)` — a tuple literal (always ≥2 elements).
    Tuple(Vec<Expr>),
    /// `{k: v, …}` — insertion-ordered map literal. Each pair is `(key, value)`.
    Map(Vec<(Expr, Expr)>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `start..end` (end-exclusive).
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },
    /// `callee(args…)`
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `obj.name`
    Field {
        obj: Box<Expr>,
        name: String,
    },
    /// `obj[index]`
    Index {
        obj: Box<Expr>,
        index: Box<Expr>,
    },
    /// postfix `expr?` — error propagation.
    Try(Box<Expr>),
    /// `fn(params) [-> ret]: body` — an anonymous function; body is a single expression.
    Closure {
        params: Vec<Param>,
        ret: Option<Type>,
        body: Box<Expr>,
    },
    /// Expression-position `match` (`x := match s:` with `pattern: expr` arms). Distinct from the
    /// statement form (`StmtKind::Match`, block arms): every arm body is a single value-expression
    /// and the whole `match` evaluates to the chosen arm's value.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchExprArm>,
    },
    /// Expression-position `if` (`if c: a else: b`) — inline, `else` mandatory; evaluates to the
    /// taken branch's value. Distinct from the statement form (`StmtKind::If`, block bodies).
    IfElse {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Box<Expr>,
    },
}

/// One arm of an expression-position `match`: `pattern: value-expr`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchExprArm {
    pub pattern: Pattern,
    pub body: Expr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Neg, // -x
    Not, // not x
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Eq,
    NotEq,
    And,
    Or,
    // Bitwise (int-only) — gap #13.
    BitAnd, // &
    BitOr,  // |
    BitXor, // ^
    Shl,    // <<
    Shr,    // >>
}
