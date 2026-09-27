//! Proven writes made by statically named functions. Function values remain opaque.

use super::{ChainLink, Checker, FnSig, ModuleSig, Ty};
use crate::ast::{DeferTarget, Expr, ExprKind, FnDecl, SpawnTarget, Stmt, StmtKind};
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
    locals: HashSet<String>,
    params: Vec<String>,
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
    let mut out = Scan {
        direct: Vec::new(),
        calls: Vec::new(),
        locals: HashSet::new(),
        params: decl.params.iter().map(|p| p.name.clone()).collect(),
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

    fn record(&mut self, expr: &Expr, kind: WriteKind, operation: String) {
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
        for stmt in body {
            self.stmt(stmt);
        }
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
            }
            StmtKind::Let { value, .. } => self.expr(value),
            StmtKind::Expr(expr) | StmtKind::Return(Some(expr)) | StmtKind::Yield(expr) => {
                self.expr(expr)
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    self.expr(cond);
                    self.block(body);
                }
                if let Some(body) = else_block {
                    self.block(body);
                }
            }
            StmtKind::For { iter, body, .. } => {
                self.expr(iter);
                self.block(body);
            }
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::Parallel { body }
            | StmtKind::Spawn(SpawnTarget::Block(body))
            | StmtKind::Defer(DeferTarget::Block(body)) => self.block(body),
            StmtKind::Spawn(SpawnTarget::Call(expr)) | StmtKind::Defer(DeferTarget::Call(expr)) => {
                self.expr(expr)
            }
            StmtKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.block(&arm.body);
                }
            }
            StmtKind::Assert { cond, msg } => {
                self.expr(cond);
                if let Some(msg) = msg {
                    self.expr(msg);
                }
            }
            StmtKind::Fn(_) => {}
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
                    ExprKind::Ident(name)
                        if !self.locals.contains(name) && !self.params.contains(name) =>
                    {
                        self.calls.push(CallEdge {
                            callee: name.clone(),
                            args: args.clone(),
                        });
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
            ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => self.expr(expr),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Compare { operands, .. } => {
                for operand in operands {
                    self.expr(operand);
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
        let mut summaries: HashMap<String, Vec<FnWrite>> = HashMap::new();
        for (name, scan) in &scans {
            let direct = scan
                .direct
                .iter()
                .filter_map(|effect| {
                    let mut effect = effect.clone();
                    if let WriteRoot::Capture(root) = &effect.root {
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
            for (name, scan) in &scans {
                for edge in &scan.calls {
                    let callee = old
                        .get(&edge.callee)
                        .cloned()
                        .or_else(|| self.functions.get(&edge.callee).map(|s| s.writes.clone()))
                        .unwrap_or_default();
                    for effect in callee {
                        if let Some(mapped) = Self::map_write(
                            scan,
                            &effect,
                            &edge.args,
                            true,
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
        for (name, writes) in summaries {
            if let Some(sig) = self.functions.get_mut(&name) {
                sig.writes = writes;
            }
        }
    }

    pub(super) fn bind_nested_fn_writes(&mut self, decl: &FnDecl) {
        let scan = scan(decl);
        let mut writes: Vec<FnWrite> = scan
            .direct
            .iter()
            .filter_map(|effect| {
                let mut effect = effect.clone();
                if let WriteRoot::Capture(root) = &effect.root {
                    let scope = self.owning_scope(root)?;
                    if scope == 0 {
                        effect.root = WriteRoot::Global(root.clone());
                    }
                }
                Some(effect)
            })
            .collect();
        loop {
            let old = writes.clone();
            for edge in &scan.calls {
                let callee = if edge.callee == decl.name {
                    old.clone()
                } else {
                    self.fn_write_scopes
                        .iter()
                        .rev()
                        .find_map(|s| s.get(&edge.callee).cloned())
                        .or_else(|| self.functions.get(&edge.callee).map(|s| s.writes.clone()))
                        .unwrap_or_default()
                };
                for effect in callee {
                    if let Some(mapped) =
                        Self::map_write(&scan, &effect, &edge.args, false, &self.module_global_lets)
                        && !writes.contains(&mapped)
                    {
                        writes.push(mapped);
                    }
                }
            }
            if writes.len() == old.len() {
                break;
            }
        }
        if let Some(scope) = self.fn_write_scopes.last_mut() {
            scope.insert(decl.name.clone(), writes);
        }
    }

    fn map_write(
        scan: &Scan,
        effect: &FnWrite,
        args: &[Expr],
        top_level: bool,
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
                if scan.locals.contains(name) || scan.params.contains(name) {
                    return None;
                }
                if top_level {
                    if !globals.contains(name) {
                        return None;
                    }
                    mapped.root = WriteRoot::Global(name.clone());
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
                    && !self.is_local_binding(module)
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
