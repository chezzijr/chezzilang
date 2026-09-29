//! Proven writes made by statically named functions. Function values remain opaque.

use super::{ChainLink, Checker, FnSig, ModuleSig, Ty};
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

#[derive(Clone)]
pub(super) struct CallEdge {
    pub callee: String,
    pub args: Vec<Expr>,
}

pub(super) struct Scan {
    pub direct: Vec<FnWrite>,
    pub calls: Vec<CallEdge>,
    nested: HashMap<String, Scan>,
    visible_fns: Vec<HashMap<String, Option<String>>>,
    locals: HashSet<String>,
    params: Vec<String>,
    /// How many conditional constructs (`if`/`match` arm/loop body/short-circuit right side) enclose
    /// the node being walked.
    cond: usize,
    /// A possible early exit (`return`, `break`, `continue`, `?`) has been walked.
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
            StmtKind::Expr(expr) | StmtKind::Yield(expr) => self.expr(expr),
            StmtKind::Return(value) => {
                if let Some(expr) = value {
                    self.expr(expr);
                }
                self.left = true;
            }
            StmtKind::Break | StmtKind::Continue => self.left = true,
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
                            });
                        } else if binding.is_none()
                            && !self.locals.contains(name)
                            && !self.params.contains(name)
                        {
                            self.calls.push(CallEdge {
                                callee: name.clone(),
                                args: args.clone(),
                            });
                        }
                    }
                    _ => {}
                }
                for arg in args {
                    self.expr(arg);
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
        let summaries = self.infer_scan_writes(&scans, &HashMap::new(), true);
        for (name, writes) in summaries {
            if let Some(sig) = self.functions.get_mut(&name) {
                sig.writes = writes;
            }
        }
    }

    fn infer_scan_writes(
        &self,
        scans: &HashMap<String, Scan>,
        known: &HashMap<String, Vec<FnWrite>>,
        top_level: bool,
    ) -> HashMap<String, Vec<FnWrite>> {
        let mut summaries: HashMap<String, Vec<FnWrite>> = HashMap::new();
        for (name, scan) in scans {
            let direct = scan
                .direct
                .iter()
                .filter_map(|effect| {
                    let mut effect = effect.clone();
                    if top_level && let WriteRoot::Capture(root) = &effect.root {
                        if !self.module_global_lets.contains(root) {
                            return None;
                        }
                        effect.root = WriteRoot::Global(root.clone());
                    }
                    Some(effect)
                })
                .collect();
            summaries.insert(name.clone(), direct);
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
                    let callee = local_callee
                        .cloned()
                        .or_else(|| old.get(&edge.callee).cloned())
                        .or_else(|| known.get(&edge.callee).cloned())
                        .or_else(|| self.functions.get(&edge.callee).map(|s| s.writes.clone()))
                        .unwrap_or_default();
                    for effect in callee {
                        if let Some(mapped) = Self::map_write(
                            scan,
                            &effect,
                            &edge.args,
                            top_level,
                            local_callee.is_some(),
                            &self.module_global_lets,
                        ) && let Some(writes) = summaries.get_mut(name)
                            && !writes.contains(&mapped)
                        {
                            writes.push(mapped);
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
        let mut writes = self
            .infer_scan_writes(&scans, &known, false)
            .remove(&decl.name)
            .unwrap_or_default();
        writes.retain_mut(|effect| {
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
            scope.insert(decl.name.clone(), writes);
        }
    }

    fn map_write(
        scan: &Scan,
        effect: &FnWrite,
        args: &[Expr],
        top_level: bool,
        nested_callee: bool,
        globals: &HashSet<String>,
    ) -> Option<FnWrite> {
        let mut mapped = effect.clone();
        match &effect.root {
            WriteRoot::Param(i) => {
                let (root, mut prefix) = chain(args.get(*i)?)?;
                mapped.root = scan.root(&root)?;
                mapped.global_ty = None;
                if top_level && let WriteRoot::Capture(name) = &mapped.root {
                    if !globals.contains(name) {
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
                    if !globals.contains(root) {
                        return None;
                    }
                    mapped.root = WriteRoot::Global(root.clone());
                }
            }
            WriteRoot::Global(_) => {}
        }
        Some(mapped)
    }

    pub(super) fn named_fn_writes(&self, callee: &Expr) -> Option<(String, Vec<FnWrite>)> {
        match &callee.kind {
            ExprKind::Ident(name) => {
                if self.lookup(name).is_some() {
                    self.fn_write_scopes
                        .iter()
                        .rev()
                        .find_map(|scope| scope.get(name).cloned())
                        .map(|writes| (name.clone(), writes))
                } else {
                    self.functions
                        .get(name)
                        .map(|sig| (name.clone(), sig.writes.clone()))
                }
            }
            ExprKind::Field { obj, name, .. } => {
                if let ExprKind::Ident(module) = &obj.kind
                    && !self.head_is_value(module)
                    && let Some(mid) = self.imported_modules.get(module)
                    && let Some(ModuleSig { functions, .. }) = self.module_sigs.get(mid)
                {
                    functions
                        .get(name)
                        .map(|sig: &FnSig| (name.clone(), sig.writes.clone()))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
