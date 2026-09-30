//! TICKET-186 — one record per module slot. Everything the checker knows about a top-level name
//! beyond its type lives in [`GlobalBinding`]; its type lives in `scopes[0]`.

use super::*;

/// What declared a module slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeclKind {
    Let,
    ConstLet,
    Fn,
    Import,
    /// An `extern` fn or a `native` decl.
    Hoisted,
}

/// The one record per module slot (TICKET-186), built by `collect_module_globals` before any body
/// is walked. A new fact about a module slot goes here, never into a new name-keyed set on
/// `Checker`.
///
/// Kept outside it, on purpose:
/// - `scopes[0]` stays the one type store: every `lookup` reads `scopes[i]` at every depth.
/// - `fn_reads` records readers and is rolled back by `DiagMark` (DEC-157).
/// - `kw_written`/`kw_pending` record assignment writes, which are walk-order and settled at
///   `pop_scope` at every depth.
/// - `empty_coll_sites`/`carrier_pins` serve locals at every depth; for a global their pins land
///   in the type store the first let no longer wipes.
#[derive(Debug, Clone, Default)]
pub(super) struct GlobalBinding {
    /// Every top-level declaration of the name, in source order, with its statement span.
    pub(super) decls: Vec<(DeclKind, Span)>,
    /// `seed_module_globals` typed it before any body was walked (TICKET-183).
    pub(super) seeded: bool,
    /// The walk has reached its first let: top-level statements see it from here on.
    pub(super) reached: bool,
    /// `report_untyped_globals` reported it as an initialization cycle.
    pub(super) cycle: bool,
}

impl GlobalBinding {
    pub(super) fn is_const(&self) -> bool {
        self.decls.iter().any(|(k, _)| *k == DeclKind::ConstLet)
    }
    pub(super) fn has_let(&self) -> bool {
        self.decls
            .iter()
            .any(|(k, _)| matches!(k, DeclKind::Let | DeclKind::ConstLet))
    }
    pub(super) fn first_let(&self) -> Option<Span> {
        self.decls
            .iter()
            .find(|(k, _)| matches!(k, DeclKind::Let | DeclKind::ConstLet))
            .map(|(_, s)| *s)
    }
    pub(super) fn redeclared(&self) -> bool {
        self.decls.len() > 1
    }
    /// Seeded, and the walk has not reached its first let: hidden from top-level statements.
    pub(super) fn unreached(&self) -> bool {
        self.seeded && !self.reached
    }
}

impl Checker {
    /// Build `self.globals` from the module's top-level statements, in source order.
    pub(super) fn collect_module_globals(&mut self, stmts: &[Stmt]) {
        self.globals.clear();
        for s in stmts {
            let mut add = |name: &str, kind: DeclKind| {
                if name != "_" {
                    self.globals
                        .entry(name.to_string())
                        .or_default()
                        .decls
                        .push((kind, s.span));
                }
            };
            match &s.kind {
                StmtKind::Let {
                    names, is_const, ..
                } => {
                    let kind = if *is_const {
                        DeclKind::ConstLet
                    } else {
                        DeclKind::Let
                    };
                    for n in names {
                        add(n, kind);
                    }
                }
                StmtKind::Fn(d) => add(&d.name, DeclKind::Fn),
                StmtKind::Import(Import::Module { path, alias, .. }) => {
                    let name = alias
                        .clone()
                        .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                    add(&name, DeclKind::Import);
                }
                StmtKind::Import(Import::From { names, .. }) => {
                    for (member, alias) in names {
                        add(alias.as_ref().unwrap_or(member), DeclKind::Import);
                    }
                }
                StmtKind::Extern { fns, .. } => {
                    for f in fns {
                        add(&f.name, DeclKind::Hoisted);
                    }
                }
                StmtKind::Native(d) => add(&d.name, DeclKind::Hoisted),
                _ => {}
            }
        }
        // Module const-ness is decided here, once per slot: a module global is one storage slot,
        // so a `const` declaration next to any other declaration of the name is an error, in
        // either order (owner decision 2026-09-30, JavaScript's let/const rule). This runs outside
        // every speculative walk, so it needs no `inferring_ret` gate.
        let mut reports: Vec<(Span, String)> = self
            .globals
            .iter()
            .filter(|(_, g)| g.is_const() && g.redeclared())
            .map(|(name, g)| {
                let msg = if g.decls[0].0 == DeclKind::ConstLet {
                    format!("cannot re-declare const binding '{name}' (a const cannot be rebound — not even with ':=' or a new typed let)")
                } else {
                    format!("'{name}' is declared both const and plain at module scope — a module global is one storage slot, so it cannot be const at one line and rebindable at another (declare it once)")
                };
                (g.decls[1].1, msg)
            })
            .collect();
        reports.sort_by_key(|(s, _)| (s.line, s.col));
        for (span, msg) in reports {
            self.error(span, msg);
        }
    }

    /// The `reached` bit of every record, for a speculative walk to restore.
    pub(super) fn save_reached(&self) -> Vec<(String, bool)> {
        self.globals
            .iter()
            .map(|(n, g)| (n.clone(), g.reached))
            .collect()
    }

    pub(super) fn restore_reached(&mut self, saved: Vec<(String, bool)>) {
        for (n, r) in saved {
            if let Some(g) = self.globals.get_mut(&n) {
                g.reached = r;
            }
        }
    }

    /// Mark `name`'s first let as reached, if it is a seeded global.
    pub(super) fn reach_global(&mut self, name: &str) {
        if let Some(g) = self.globals.get_mut(name) {
            g.reached = true;
        }
    }
}
