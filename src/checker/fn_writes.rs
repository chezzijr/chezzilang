//! Proven writes made by statically named functions. Function values remain opaque.

use super::{ChainLink, Checker, FnSig, Ty};
use crate::ast::{BinaryOp, DeferTarget, Expr, ExprKind, FnDecl, SpawnTarget, Stmt, StmtKind};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum WriteRoot {
    Param(usize),
    Capture(String),
    Global(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum WriteKind {
    Store,
    Method(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct FnWrite {
    pub root: WriteRoot,
    pub path: Vec<ChainLink>,
    pub kind: WriteKind,
    pub operation: String,
    pub global_ty: Option<Ty>,
}

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

/// A statically named function's summary: its proven writes and, per declared param, whether the
/// argument can escape the call (TICKET-190). Both come from one least fixed point.
#[derive(Clone, Debug, Default)]
pub(super) struct FnSummary {
    pub writes: Vec<FnWrite>,
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
    /// An operand of an arithmetic, comparison or unary operator, or a compound assignment target.
    Op,
    /// `for v in n`.
    Iter,
    /// Positional argument `j` of a statically named callee.
    Arg(ArgCallee, usize),
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
        _ => true,
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
        _ => None,
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
            _ => None,
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
            ExprKind::Field { obj, name, .. } => match &obj.kind {
                ExprKind::Ident(n) => self.keep(n, RootUse::Field(name.clone())),
                _ => self.path(obj),
            },
            ExprKind::Index { obj, index, .. } => {
                match &obj.kind {
                    ExprKind::Ident(n) => self.keep(n, RootUse::Index),
                    _ => self.path(obj),
                }
                if let Some(index) = index {
                    self.value(index);
                }
            }
            ExprKind::Ident(n) if self.aliases.contains_key(n) => {}
            _ => self.value(e),
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
                                _ => None,
                            },
                        });
                        self.path(target);
                    }
                    _ => self.escape_free(target),
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
            StmtKind::For {
                vars, iter, body, ..
            } => {
                // TICKET-240: a loop over a view binds views of the same root.
                if self.deep
                    && let Some(root) = self.view_root(iter)
                {
                    for v in vars {
                        self.alias(v, &root);
                    }
                }
                match &iter.kind {
                    ExprKind::Ident(n) => self.keep(n, RootUse::Iter),
                    ExprKind::Field { .. } | ExprKind::Index { .. } => self.path(iter),
                    _ => self.value(iter),
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
                        crate::compiler::pattern_binds(&arm.pattern, &mut binders);
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
            _ => self
                .uses
                .escaped
                .extend(crate::compiler::free_names_of_block(
                    std::slice::from_ref(stmt),
                    &HashSet::new(),
                )),
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
            ExprKind::Unary { expr, .. } => self.operand(expr),
            ExprKind::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                rhs,
            }
            | ExprKind::NullCoalesce { lhs, rhs, .. } => {
                self.value(lhs);
                self.value(rhs);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.operand(lhs);
                self.operand(rhs);
            }
            ExprKind::Compare { operands, .. } => {
                for o in operands {
                    self.operand(o);
                }
            }
            ExprKind::Range { start, end } => {
                self.value(start);
                self.value(end);
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
                        _ => {
                            self.value(obj);
                            None
                        }
                    },
                    _ => {
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
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => {
                // A slice of a view is a new container over the view's children.
                self.deep_use(obj, || DeepUse::Read { view: e.id });
                self.indexed(obj);
                for b in [start, end, step].into_iter().flatten() {
                    self.value(b);
                }
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
            ExprKind::Recover(body) => self.block(body),
            ExprKind::ElseGuard { value, body, .. } => {
                self.value(value);
                self.block(body);
            }
            // A closure, comprehension, interpolation, `?`, `?.` or anything else.
            _ => self.escape_free(e),
        }
    }

    fn operand(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Ident(n) => {
                if self.aliases.contains_key(n) {
                    self.deep_use(e, || DeepUse::Read { view: e.id });
                }
                self.keep(n, RootUse::Op)
            }
            _ => self.value(e),
        }
    }

    fn indexed(&mut self, obj: &Expr) {
        match &obj.kind {
            ExprKind::Ident(n) => self.keep(n, RootUse::Index),
            _ => self.value(obj),
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

#[derive(Clone)]
pub(super) struct CallEdge {
    pub callee: String,
    pub args: Vec<Expr>,
    /// The callee is a bare name no scope of the scanned fn binds: a module slot.
    pub module_slot: bool,
}

pub(super) struct Scan {
    pub direct: Vec<FnWrite>,
    pub calls: Vec<CallEdge>,
    nested: HashMap<String, Scan>,
    /// TICKET-190: every use of every bare name, for the param-escape summary.
    pub uses: Uses,
    /// The scanned fn is a generator: calling it stores its arguments without running the body.
    is_generator: bool,
    visible_fns: Vec<HashMap<String, Option<String>>>,
    locals: HashSet<String>,
    params: Vec<String>,
    /// How many conditional constructs (`if`/`match` arm/loop body/short-circuit right side) enclose
    /// the node being walked.
    cond: usize,
    /// A possible early exit (`return`, `break`, `continue`, `?`) has been walked. The statement side
    /// derives from `flow::stmt` (TICKET-184): set after any statement that escapes or cannot fall
    /// through. Its divergence oracle is `false`, because fn_writes runs before any body is checked.
    left: bool,
}

pub(super) fn chain(expr: &Expr) -> Option<(String, Vec<ChainLink>)> {
    match &expr.kind {
        ExprKind::Ident(name) => Some((name.clone(), Vec::new())),
        ExprKind::Field { obj, name, .. } => {
            let (root, mut path) = chain(obj)?;
            path.push(ChainLink::Field(name.clone()));
            Some((root, path))
        }
        ExprKind::Index { obj, .. } => {
            let (root, mut path) = chain(obj)?;
            path.push(ChainLink::Index);
            Some((root, path))
        }
        _ => None,
    }
}

pub(super) fn scan(decl: &FnDecl) -> Scan {
    scan_with_visible(decl, Vec::new())
}

fn scan_with_visible(decl: &FnDecl, visible_fns: Vec<HashMap<String, Option<String>>>) -> Scan {
    let mut out = Scan {
        direct: Vec::new(),
        calls: Vec::new(),
        nested: HashMap::new(),
        uses: uses_of(decl),
        is_generator: decl.is_generator,
        visible_fns,
        locals: HashSet::new(),
        params: decl.params.iter().map(|p| p.name.clone()).collect(),
        cond: 0,
        left: false,
    };
    out.collect_locals(&decl.body);
    out.block(&decl.body);
    out
}

impl Scan {
    fn collect_locals(&mut self, body: &[Stmt]) {
        for stmt in body {
            match &stmt.kind {
                StmtKind::Let { names, .. } => self.locals.extend(names.iter().cloned()),
                StmtKind::Fn(decl) => {
                    self.locals.insert(decl.name.clone());
                }
                StmtKind::If {
                    branches,
                    else_block,
                } => {
                    for (_, body) in branches {
                        self.collect_locals(body);
                    }
                    if let Some(body) = else_block {
                        self.collect_locals(body);
                    }
                }
                StmtKind::For { body, .. }
                | StmtKind::While { body, .. }
                | StmtKind::Parallel { body }
                | StmtKind::Spawn(SpawnTarget::Block(body))
                | StmtKind::Defer(DeferTarget::Block(body)) => self.collect_locals(body),
                StmtKind::Match { arms, .. } => {
                    for arm in arms {
                        self.collect_locals(&arm.body);
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn root(&self, name: &str) -> Option<WriteRoot> {
        if self.locals.contains(name) {
            return None;
        }
        self.params
            .iter()
            .position(|p| p == name)
            .map(WriteRoot::Param)
            .or_else(|| Some(WriteRoot::Capture(name.to_string())))
    }

    /// Layer A reports a write only when every path through the function reaches it (D4, TICKET-179).
    /// A write under a condition or after a possible early exit is left to layer C.
    fn certain(&self) -> bool {
        self.cond == 0 && !self.left
    }

    /// Walk `f` inside one more conditional construct.
    fn conditional(&mut self, f: impl FnOnce(&mut Self)) {
        self.cond += 1;
        f(self);
        self.cond -= 1;
    }

    fn record(&mut self, expr: &Expr, kind: WriteKind, operation: String) {
        if !self.certain() {
            return;
        }
        if let Some((name, path)) = chain(expr)
            && let Some(root) = self.root(&name)
            && (!matches!(root, WriteRoot::Param(_))
                || !path.is_empty()
                || matches!(kind, WriteKind::Method(_)))
        {
            self.direct.push(FnWrite {
                root,
                path,
                kind,
                operation,
                global_ty: None,
            });
        }
    }

    fn block(&mut self, body: &[Stmt]) {
        self.visible_fns.push(HashMap::new());
        for stmt in body {
            self.stmt(stmt);
            let f = super::flow::stmt(stmt, &|_| false);
            self.left |= !f.falls_through || f.escapes();
        }
        self.visible_fns.pop();
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Assign { target, value, .. } => {
                let op = match &target.kind {
                    ExprKind::Field { name, .. } => name.clone(),
                    ExprKind::Index { .. } => "set_index".to_string(),
                    _ => "assign".to_string(),
                };
                self.record(target, WriteKind::Store, op);
                self.expr(value);
                // TICKET-189: a rebound parameter names a new value from here on, at any depth (a
                // conditional rebind too, so a later write is no longer certain to reach the
                // argument). Writes walked before the rebind stay recorded.
                if let ExprKind::Ident(name) = &target.kind
                    && self.params.contains(name)
                {
                    self.locals.insert(name.clone());
                }
                if let ExprKind::Ident(name) = &target.kind {
                    if let Some(scope) = self
                        .visible_fns
                        .iter_mut()
                        .rev()
                        .find(|scope| scope.contains_key(name))
                    {
                        scope.insert(name.clone(), None);
                    } else if let Some(scope) = self.visible_fns.last_mut() {
                        scope.insert(name.clone(), None);
                    }
                }
            }
            StmtKind::Let { names, value, .. } => {
                self.expr(value);
                if let Some(scope) = self.visible_fns.last_mut() {
                    for name in names {
                        scope.insert(name.clone(), None);
                    }
                }
            }
            StmtKind::Expr(expr) | StmtKind::Yield(expr) | StmtKind::Return(Some(expr)) => {
                self.expr(expr)
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                // Only the first condition runs on every path.
                if let Some((cond, _)) = branches.first() {
                    self.expr(cond);
                }
                self.conditional(|s| {
                    for (i, (cond, body)) in branches.iter().enumerate() {
                        if i > 0 {
                            s.expr(cond);
                        }
                        s.block(body);
                    }
                    if let Some(body) = else_block {
                        s.block(body);
                    }
                });
            }
            StmtKind::For {
                vars, iter, body, ..
            } => {
                self.expr(iter);
                let prior: Vec<_> = vars
                    .iter()
                    .map(|name| (name.clone(), self.locals.insert(name.clone())))
                    .collect();
                self.conditional(|s| s.block(body));
                for (name, existed) in prior {
                    if !existed {
                        self.locals.remove(&name);
                    }
                }
            }
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.conditional(|s| s.block(body));
            }
            StmtKind::Parallel { body }
            | StmtKind::Spawn(SpawnTarget::Block(body))
            | StmtKind::Defer(DeferTarget::Block(body)) => self.block(body),
            StmtKind::Spawn(SpawnTarget::Call(expr)) | StmtKind::Defer(DeferTarget::Call(expr)) => {
                self.expr(expr)
            }
            StmtKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                self.conditional(|s| {
                    for arm in arms {
                        s.block(&arm.body);
                    }
                });
            }
            StmtKind::Assert { cond, msg } => {
                self.expr(cond);
                if let Some(msg) = msg {
                    self.expr(msg);
                }
            }
            StmtKind::Fn(decl) => {
                // Nested names become visible at their declaration and leave with the block.
                let key = format!(
                    "{}@{}:{}:{}",
                    decl.name, decl.name_span.file, decl.name_span.line, decl.name_span.col
                );
                if let Some(scope) = self.visible_fns.last_mut() {
                    scope.insert(decl.name.clone(), Some(key.clone()));
                }
                let child = scan_with_visible(decl, self.visible_fns.clone());
                self.nested.insert(key, child);
            }
            _ => {}
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Call {
                callee,
                args,
                named,
                bracket,
                ..
            } => {
                match &callee.kind {
                    ExprKind::Field { obj, name, .. } => {
                        self.record(obj, WriteKind::Method(name.clone()), name.clone());
                    }
                    ExprKind::Ident(name) if self.certain() => {
                        let binding = self
                            .visible_fns
                            .iter()
                            .rev()
                            .find_map(|scope| scope.get(name));
                        if let Some(Some(key)) = binding {
                            self.calls.push(CallEdge {
                                callee: key.clone(),
                                args: args.clone(),
                                module_slot: false,
                            });
                        } else if binding.is_none()
                            && !self.locals.contains(name)
                            && !self.params.contains(name)
                        {
                            self.calls.push(CallEdge {
                                callee: name.clone(),
                                args: args.clone(),
                                module_slot: true,
                            });
                        }
                    }
                    _ => {}
                }
                for arg in args {
                    self.expr(arg);
                }
                if let Some(b) = bracket {
                    self.expr(b);
                }
                for (_, arg) in named {
                    self.expr(arg);
                }
            }
            ExprKind::Field { obj, .. } | ExprKind::Index { obj, .. } => self.expr(obj),
            ExprKind::List(items, _) | ExprKind::Tuple(items) | ExprKind::Set(items) => {
                for item in items {
                    self.expr(item);
                }
            }
            ExprKind::Map(pairs) => {
                for (key, value) in pairs {
                    self.expr(key);
                    self.expr(value);
                }
            }
            ExprKind::Unary { expr, .. } => self.expr(expr),
            ExprKind::Try(expr) => {
                self.expr(expr);
                self.left = true;
            }
            ExprKind::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                rhs,
                ..
            }
            | ExprKind::NullCoalesce { lhs, rhs, .. } => {
                self.expr(lhs);
                self.conditional(|s| s.expr(rhs));
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Compare { operands, .. } => {
                // A chain `a < b < c` evaluates `c` only when `a < b` holds.
                for (i, operand) in operands.iter().enumerate() {
                    if i < 2 {
                        self.expr(operand);
                    } else {
                        self.conditional(|s| s.expr(operand));
                    }
                }
            }
            ExprKind::Closure { .. } => {}
            _ => {}
        }
    }
}

impl Checker {
    pub(super) fn infer_fn_writers(&mut self, stmts: &[Stmt]) {
        let scans: HashMap<String, Scan> = stmts
            .iter()
            .filter_map(|stmt| {
                if let StmtKind::Fn(decl) = &stmt.kind {
                    Some((decl.name.clone(), scan(decl)))
                } else {
                    None
                }
            })
            .collect();
        let saved_body_facts = std::mem::replace(&mut self.body_facts_pass, true);
        let summaries = self.infer_scan_writes(&scans, &HashMap::new(), true);
        self.body_facts_pass = saved_body_facts;
        for (name, summary) in summaries {
            if let Some(sig) = self.functions.get_mut(&name) {
                sig.summary = summary;
            }
        }
    }

    /// The least fixed point of every scanned fn's [`FnSummary`]: writes grow through call edges;
    /// for a top-level fn a param escapes once [`Self::root_escapes`] says so under this pass's
    /// summaries. A nested fn's params all escape (nested callees decline).
    fn infer_scan_writes(
        &self,
        scans: &HashMap<String, Scan>,
        known: &HashMap<String, FnSummary>,
        top_level: bool,
    ) -> HashMap<String, FnSummary> {
        let mut summaries: HashMap<String, FnSummary> = HashMap::new();
        for (name, scan) in scans {
            let direct = scan
                .direct
                .iter()
                .filter_map(|effect| {
                    let mut effect = effect.clone();
                    if top_level && let WriteRoot::Capture(root) = &effect.root {
                        if !self.globals.get(root).is_some_and(|g| g.has_let()) {
                            return None;
                        }
                        effect.root = WriteRoot::Global(root.clone());
                    }
                    Some(effect)
                })
                .collect();
            // A generator escapes every param, and so does a nested fn; a top-level fn starts
            // from "nothing escapes" and only grows.
            let escapes = vec![scan.is_generator || !top_level; scan.params.len()];
            summaries.insert(
                name.clone(),
                FnSummary {
                    writes: direct,
                    escapes,
                    is_generator: scan.is_generator,
                },
            );
        }
        loop {
            let old = summaries.clone();
            let mut changed = false;
            for (name, scan) in scans {
                let mut visible = known.clone();
                visible.extend(old.clone());
                let nested = self.infer_scan_writes(&scan.nested, &visible, false);
                for edge in &scan.calls {
                    let local_callee = nested.get(&edge.callee);
                    // TICKET-201: a redeclared module slot holds no known fn in a body.
                    let callee = if edge.module_slot && !self.slot_holds_fn_decl(&edge.callee) {
                        Vec::new()
                    } else {
                        local_callee
                            .map(|s| s.writes.clone())
                            .or_else(|| old.get(&edge.callee).map(|s| s.writes.clone()))
                            .or_else(|| known.get(&edge.callee).map(|s| s.writes.clone()))
                            .or_else(|| {
                                self.functions
                                    .get(&edge.callee)
                                    .map(|s| s.summary.writes.clone())
                            })
                            .unwrap_or_default()
                    };
                    for effect in callee {
                        if let Some(mapped) = Self::map_write(
                            scan,
                            &effect,
                            &edge.args,
                            top_level,
                            local_callee.is_some(),
                            &self.globals,
                        ) && let Some(summary) = summaries.get_mut(name)
                            && !summary.writes.contains(&mapped)
                        {
                            summary.writes.push(mapped);
                            changed = true;
                        }
                    }
                }
                if top_level && !scan.is_generator {
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
            }
            if !changed {
                break;
            }
        }
        summaries
    }

    pub(super) fn bind_nested_fn_writes(&mut self, decl: &FnDecl) {
        let scan = scan(decl);
        let mut known = HashMap::new();
        for scope in &self.fn_write_scopes {
            known.extend(scope.clone());
        }
        let scans = HashMap::from([(decl.name.clone(), scan)]);
        let mut summary = self
            .infer_scan_writes(&scans, &known, false)
            .remove(&decl.name)
            .unwrap_or_default();
        summary.writes.retain_mut(|effect| {
            if let WriteRoot::Capture(root) = &effect.root {
                let Some(scope) = self.owning_scope(root) else {
                    return false;
                };
                if scope == 0 {
                    effect.root = WriteRoot::Global(root.clone());
                }
            }
            true
        });
        if let Some(scope) = self.fn_write_scopes.last_mut() {
            scope.insert(decl.name.clone(), summary);
        }
    }

    fn map_write(
        scan: &Scan,
        effect: &FnWrite,
        args: &[Expr],
        top_level: bool,
        nested_callee: bool,
        globals: &HashMap<String, super::globals::GlobalBinding>,
    ) -> Option<FnWrite> {
        let has_let = |n: &str| globals.get(n).is_some_and(|g| g.has_let());
        let mut mapped = effect.clone();
        match &effect.root {
            WriteRoot::Param(i) => {
                let (root, mut prefix) = chain(args.get(*i)?)?;
                mapped.root = scan.root(&root)?;
                mapped.global_ty = None;
                if top_level && let WriteRoot::Capture(name) = &mapped.root {
                    if !has_let(name) {
                        return None;
                    }
                    mapped.root = WriteRoot::Global(name.clone());
                }
                prefix.extend(mapped.path);
                mapped.path = prefix;
            }
            WriteRoot::Capture(name) => {
                if nested_callee {
                    mapped.root = scan.root(name)?;
                } else if scan.locals.contains(name) || scan.params.contains(name) {
                    return None;
                }
                if top_level && let WriteRoot::Capture(root) = &mapped.root {
                    if !has_let(root) {
                        return None;
                    }
                    mapped.root = WriteRoot::Global(root.clone());
                }
            }
            WriteRoot::Global(_) => {}
        }
        Some(mapped)
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
                        .filter(|_| self.slot_holds_fn_decl(name))
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
            _ => None,
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
                if self.lookup(f).is_some() || !self.slot_holds_fn_decl(f) {
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
                _ => tys.iter().any(|t| !self.use_keeps_root(u, t)),
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
    fn use_keeps_root(&self, u: &RootUse, t: &Ty) -> bool {
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
                _ => false,
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
