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
    /// `struct Name:` (or `struct Name[A, B]:`) with fields and (optionally) methods.
    Struct {
        name: String,
        type_params: Vec<TypeParam>,
        fields: Vec<Field>,
        methods: Vec<FnDecl>,
    },
    /// `protocol Name:` (or `protocol Name[T]:`) — a structural interface: a list of method
    /// signatures (no bodies). A type satisfies it by having matching methods (Go-style; no explicit
    /// `implements`). `type_params` is empty for a bare protocol; for `protocol Container[T]` the
    /// method signatures may reference `T`, and a bound names concrete args (`[X: Container[int]]`).
    Protocol {
        name: String,
        type_params: Vec<TypeParam>,
        methods: Vec<MethodSig>,
    },
    /// `enum Name:` (or `enum Name[T]:`) with its variants. `type_params` is empty for a
    /// non-generic enum; for `enum Tree[T]` a variant payload may reference `T`. Type-erased like
    /// generic structs — the parameters matter only to the checker.
    Enum {
        name: String,
        type_params: Vec<TypeParam>,
        variants: Vec<Variant>,
    },
    /// `type Name = <type>` — a transparent type alias (`Name` is interchangeable with the aliased
    /// type everywhere; structural, not a distinct nominal type).
    TypeAlias {
        name: String,
        ty: Type,
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
    /// `defer <call>` (form 1) or `defer:` block (form 2) — register cleanup to run when the
    /// enclosing block/frame exits (normal return, `?` short-circuit, `break`/`continue`, or panic),
    /// in LIFO order. Form 1's receiver + arguments are evaluated at the `defer` statement (Go
    /// semantics); form 2's free variables are snapshotted by value at the `defer` point. The
    /// deferred body itself runs at scope exit.
    Defer(DeferTarget),
    /// `parallel:` — a structured-concurrency nursery. Spawned children run to completion at the
    /// block's dedent (the join barrier); the first child error aborts the rest and propagates.
    Parallel { body: Block },
    /// `spawn <call>` (form 1) or `spawn:` block (form 2) — register a task on the innermost
    /// enclosing `parallel:` nursery. Legal only inside a `parallel:` (checker-enforced). Under the
    /// sequential executor the task is registered here and run at the nursery's dedent.
    Spawn(SpawnTarget),
    /// `wait:` — Chezzi's `select`. Race the `recv`s in `arms`: block until whichever channel has a
    /// value first, bind it (per the arm's [`WaitTarget`]), and run that arm's body. Source-order
    /// poll (deterministic, not Go-random); a closed+empty arm is skipped; `else_block` (optional,
    /// last) is the non-blocking fallback. Recv-only — unbounded channels never block a `send`. See
    /// `docs/concurrency.md §6d`.
    Wait {
        arms: Vec<WaitArm>,
        else_block: Option<Block>,
    },
    /// `break` — exit the innermost enclosing loop. Carries only its `Span` (via `Stmt`).
    Break,
    /// `continue` — skip to the next iteration of the innermost enclosing loop.
    Continue,
    /// An `import …` statement.
    Import(Import),
    /// A bare expression used as a statement, e.g. `print(x)`.
    Expr(Expr),
}

/// The target of a `defer` statement: a single call (`defer f(x)` / `defer obj.m(x)`) or an
/// indented block (`defer:`). Mirrors [`SpawnTarget`]: form 1 sidesteps the single-expression
/// closure limit for the common case; form 2 allows a multi-statement cleanup body that runs
/// top-to-bottom at scope exit (LIFO relative to other `defer`s). Unlike `spawn`, the block runs in
/// the same task, so its captured free variables are not read-only.
#[derive(Debug, Clone, PartialEq)]
pub enum DeferTarget {
    /// `defer f(args)` / `defer obj.m(args)` — the expression must be a call (checker-enforced).
    Call(Expr),
    /// `defer:` followed by an indented block.
    Block(Block),
}

/// The target of a `spawn` statement: a named call (`spawn f(x)`) or an anonymous indented block
/// (`spawn:`). Form 1 sidesteps the single-expression closure limit for the common case; form 2
/// allows a multi-statement task body.
#[derive(Debug, Clone, PartialEq)]
pub enum SpawnTarget {
    /// `spawn f(args)` — the expression must be a call (parser-enforced).
    Call(Expr),
    /// `spawn:` followed by an indented block.
    Block(Block),
}

/// One arm of a `wait:` — `<target> (:= | =) <chan>.recv() : <body>`. The RHS is required (parser-
/// enforced) to be a bare `.recv()` on `chan`; `chan` is the channel expression, evaluated once.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitArm {
    pub target: WaitTarget,
    pub chan: Expr,
    pub body: Block,
    pub span: Span,
}

/// Where a `wait` arm delivers the received value: a fresh arm-scoped binding (`v :=`), an existing
/// outer lvalue (`result =`), or discarded (`_`). Mirrors the `:=`/`=`/`_` split of ordinary `let`/
/// assignment; arm bodies are plain lexical sub-scopes (not closures), so `=` mutation is normal.
#[derive(Debug, Clone, PartialEq)]
pub enum WaitTarget {
    Bind(String),
    Assign(Expr),
    Discard,
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
    /// Generic type parameters: `fn max[T: Comparable](…)`. Empty for non-generic fns/methods.
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    pub ret: Option<Type>, // None ⇒ returns nothing
    pub body: Block,
}

/// A generic type parameter declaration: `T`, `T: Comparable`, `T: Add + Mul`, or a parameterized
/// bound `S: Iterator[T]`. `bounds` lists the protocols the instantiating type must satisfy
/// (empty = unbounded).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    pub bounds: Vec<Bound>,
}

/// A single protocol bound on a type parameter. `args` is empty for a bare bound (`Comparable`) and
/// non-empty for a parameterized one (`Iterator[T]` → name `Iterator`, args `[Named("T")]`). Only
/// `Iterator` consumes its args today (element-type recovery); other protocols ignore them.
#[derive(Debug, Clone, PartialEq)]
pub struct Bound {
    pub name: String,
    pub args: Vec<Type>,
}

/// A protocol method signature — like an [`FnDecl`] but body-less. `Self` (as `Type::Named("Self")`)
/// inside the params/ret refers to the conforming type.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodSig {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Option<Type>,
}

/// A function or closure parameter. `ty` is `None` for closure params, whose types are inferred.
/// `default` is `Some` for a defaulted param (`x: int = 10`). The default may be any expression that
/// does not reference another parameter (the desugar pass enforces this and fills it in at omitting
/// call sites). Free functions and struct methods allow defaults; closures do not.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Option<Type>,
    pub default: Option<Expr>,
}

/// A struct field: `name: Type`, optionally with a default (`name: Type = expr`). A defaulted field
/// is filled in by the desugar pass when a constructor call omits it. The default may be any
/// expression that does not reference another field.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub default: Option<Expr>,
}

/// An enum variant: `Circle(int)` → payload `[int]`; `Point` → payload `[]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub name: String,
    pub payload: Vec<Type>,
}

/// One arm of a `match`: `pattern [if guard]: body`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// Optional `if <expr>` guard. The arm matches only if the pattern binds AND the guard (a bool,
    /// evaluated with the pattern's bindings in scope) is true; otherwise control falls through to
    /// the next arm. A guarded arm is never irrefutable (does not contribute to exhaustiveness).
    pub guard: Option<Expr>,
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
    /// A half-open integer range pattern `start..end` — matches an `int` value `v` when
    /// `start <= v < end`. Int-only; refutable (never irrefutable / never makes a match exhaustive).
    Range { start: i64, end: i64 },
    /// The `_` catch-all arm.
    Wildcard,
    /// An or-pattern `p1 | p2 | ...` — matches if ANY alternative matches (first match wins).
    /// Always holds two-or-more alternatives (a single primary parses to that primary unchanged).
    /// Every alternative must bind the same set of variables with unifiable types; the agreed set
    /// is declared once. Irrefutable iff every alternative is.
    Or(Vec<Pattern>),
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
    /// `{a, b, c}` — set literal (≥1 element; empty `{}` is a map, empty set is `set()`).
    Set(Vec<Expr>),
    /// A comprehension: `[elem for vars in iter if guard]` (list), `{elem for …}` (set), or
    /// `{key: elem for …}` (map). One `for` clause, optional `if` guard. `vars` is one binding, or
    /// two for `for k, v in m`. `key` is `Some` only for the map form. Evaluates by iterating
    /// `iter` (any iterable — like a `for` loop), binding `vars`, and collecting `elem` (skipping
    /// rows where `guard` is false) into a fresh list / set / map.
    Comprehension {
        kind: CompKind,
        key: Option<Box<Expr>>,
        elem: Box<Expr>,
        vars: Vec<String>,
        iter: Box<Expr>,
        guard: Option<Box<Expr>>,
    },
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
    /// `callee(args…)`, optionally with explicit call-site type arguments `callee[T, …](args…)`.
    /// `type_args` is empty for an ordinary call; non-empty only for `name[Types](…)` where `name`
    /// is a (generic) function / struct / enum-variant constructor. Type-erased at runtime — the
    /// engines ignore `type_args`; only the checker consumes them (to pin inference).
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        /// Call-site named arguments (`f(x=1)`), parsed alongside positional `args`. The desugar pass
        /// (run in `resolver::build_graph`) resolves these against the callee's params, producing a
        /// fully positional `args` list and clearing `named`. So the checker and both engines only
        /// ever see `named` empty — they read `args` and ignore this field.
        named: Vec<(String, Expr)>,
        type_args: Vec<Type>,
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
    /// `obj[start..end]` — half-open slice. Emitted when the subscript is a `..` range (distinct
    /// from `Index` so the `Slice` protocol dispatches separately from `Index`).
    Slice {
        obj: Box<Expr>,
        start: Box<Expr>,
        end: Box<Expr>,
    },
    /// postfix `expr?` — error propagation.
    Try(Box<Expr>),
    /// Optional chaining `obj?.name` (field) or `obj?.name(args)` (method). `obj` is an `Option[T]`:
    /// `None` short-circuits to `None`, `Some(v)` applies the access to `v` and re-wraps in `Some`.
    /// A **carrier** node produced by the parser and lowered to a `Match` by the desugar pass
    /// (`resolver::build_graph`), so the checker and engines never see it.
    OptChain {
        obj: Box<Expr>,
        name: String,
        /// `Some(args)` ⇒ method call `obj?.name(args)`; `None` ⇒ field access `obj?.name`.
        call: Option<OptCall>,
    },
    /// Null-coalescing `lhs ?? rhs`. `lhs` is an `Option[T]`: `Some(v)` ⇒ `v`, `None` ⇒ `rhs`.
    /// A **carrier** node lowered to a `Match` by the desugar pass; never reaches checker/engines.
    NullCoalesce {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `module.decode[Type](arg)` — type-directed JSON decode (M8). `obj` is the json-module
    /// expression (so the engine can reach its `parse`), `ty` is the target type to decode into,
    /// `arg` is the source string. Evaluates to `Result[ty]`. Scoped to the `.decode[T](…)` shape;
    /// not general call-site type arguments.
    DecodeCall {
        obj: Box<Expr>,
        ty: Type,
        arg: Box<Expr>,
    },
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
    /// `recover: <block>` — a panic-recovery boundary. Runs the block; any runtime fault occurring
    /// transitively beneath it is caught and converted to `Err(Error)`, otherwise the block's
    /// trailing-expression value is wrapped in `Ok`. Evaluates to `Result[T, Error]`.
    Recover(Block),
}

/// The call part of an optional-chained method call `obj?.name(args)` — see [`ExprKind::OptChain`].
#[derive(Debug, Clone, PartialEq)]
pub struct OptCall {
    pub args: Vec<Expr>,
    pub named: Vec<(String, Expr)>,
    pub type_args: Vec<Type>,
}

/// One arm of an expression-position `match`: `pattern [if guard]: value-expr`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchExprArm {
    pub pattern: Pattern,
    /// Optional `if <expr>` guard — see [`MatchArm::guard`].
    pub guard: Option<Expr>,
    pub body: Expr,
}

/// Which collection a comprehension builds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompKind {
    List,
    Set,
    Map,
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
