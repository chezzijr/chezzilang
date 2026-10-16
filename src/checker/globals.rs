//! TICKET-186 — one record per module slot. Everything the checker knows about a top-level name
//! beyond its type lives in [`GlobalBinding`]; its type lives in `scopes[0]`.

use super::setup::BindSite;
use super::*;

/// What declared a module slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeclKind {
    Let,
    ConstLet,
    Fn,
    /// A whole-module or from-import; a from-import carries the facts of its home slot.
    Import(ImportFacts),
    /// An `extern` fn or a `native` decl.
    Hoisted,
    /// A `struct`, `enum`, `type` or `protocol` (the word its "already defined" message names).
    /// It binds the type namespace only, so it holds no slot.
    Type(&'static str),
}

/// What an importer knows about the home slot of a from-imported name (TICKET-196), read from the
/// home module's `MemberSig`, and which namespaces the import binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ImportFacts {
    pub(super) is_const: bool,
    pub(super) redeclared: bool,
    /// It binds a value: a fn, a let, a variant, a module, or a name the home sig does not type.
    pub(super) value: bool,
    /// It binds a type: a struct, enum, alias or protocol, a module, or a name the home sig does
    /// not type.
    pub(super) ty: bool,
}

/// The two namespaces of a module (Rust's): a type and a value of one name coexist, and a call
/// reaches the value (TICKET-029: `fn Path` beside `struct Path` is its constructor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Value,
    Type,
}

impl DeclKind {
    /// Does this declaration bind `ns`? A let binds the value namespace too, but a second binding
    /// of a let's name is judged by `reject_redeclare` on its type, so the clash rule skips lets.
    fn declares(self, ns: Namespace) -> bool {
        match (self, ns) {
            (DeclKind::Fn | DeclKind::Hoisted, Namespace::Value) => true,
            (DeclKind::Type(_), Namespace::Type) => true,
            (DeclKind::Import(f), Namespace::Value) => f.value,
            (DeclKind::Import(f), Namespace::Type) => f.ty,
            (DeclKind::Let | DeclKind::ConstLet, _)
            | (DeclKind::Fn | DeclKind::Hoisted, Namespace::Type)
            | (DeclKind::Type(_), Namespace::Value) => false,
        }
    }

    /// The word of a "`<word>` '`<name>`' is already defined" message.
    fn word(self) -> &'static str {
        match self {
            DeclKind::Type(w) => w,
            DeclKind::Let
            | DeclKind::ConstLet
            | DeclKind::Fn
            | DeclKind::Hoisted
            | DeclKind::Import(_) => "function",
        }
    }
}

/// The one record per module slot (TICKET-186), built by `collect_module_globals` before any body
/// is walked. A new fact about a module slot goes here, never into a new name-keyed set on
/// `Checker`.
///
/// Kept outside it, on purpose:
/// - `scopes[0]` stays the one type store: every `lookup` reads `scopes[i]` at every depth.
/// - `kw_written`/`kw_pending` record assignment writes, which are walk-order and settled at
///   `pop_scope` at every depth.
#[derive(Debug, Clone, Default)]
pub(super) struct GlobalBinding {
    /// Every top-level declaration of the name's slot, in source order, with its statement span.
    pub(super) decls: Vec<(DeclKind, Span)>,
    /// Every top-level `struct`/`enum`/`type`/`protocol` of the name. A type holds no slot, so
    /// only `clashes` reads these.
    pub(super) types: Vec<(DeclKind, Span)>,
    /// `seed_module_globals` typed it before any body was walked (TICKET-183).
    pub(super) seeded: bool,
    /// The walk has reached its first let: top-level statements see it from here on.
    pub(super) reached: bool,
    /// `report_untyped_globals` reported it as an initialization cycle.
    pub(super) cycle: bool,
}

/// Why `Checker::labels_certain` denies a keyword call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KwDeny {
    /// The binding is not certain to hold one known function.
    NotOneFn,
    /// The module slot is declared more than once and this code may run after any of them.
    Redeclared,
}

/// What a `kw_pending` entry waits on. Both settle at the binding's `pop_scope` against
/// `kw_written`, because a write may come after the call.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum KwUse {
    /// A keyword call through the binding; a written binding makes it a compile error.
    Keyword(Span),
    /// A generator creation stamp (TICKET-190), recorded into `gen_crossings.calls` only when the
    /// binding is never written: a written binding may hold another fn at the call.
    GenStamp((usize, u32), CallCrossing, Span),
}

/// The denial for a keyword call through a name that may hold functions with different labels.
pub(super) fn kw_ambiguous_msg(name: &str) -> String {
    format!(
        "keyword arguments through '{name}' are ambiguous: '{name}' is reassigned, so it may hold a function with different parameter names; pass the arguments positionally"
    )
}

/// The one keyword-deny text, for a callee `name` (`None`: not a bare name).
pub(super) fn kw_deny_msg(name: Option<&str>, deny: KwDeny) -> String {
    match (name, deny) {
        (Some(name), KwDeny::Redeclared) => kw_ambiguous_msg(name),
        _ => "keyword arguments through a function value need a binding that holds one known function (`g := some_fn`, a closure literal, or a nested `fn`, never reassigned); this callee may hold any function of its type, whose parameter names can differ, so pass the arguments positionally".to_string(),
    }
}

impl GlobalBinding {
    pub(super) fn is_const(&self) -> bool {
        self.decls.iter().any(|(k, _)| match k {
            DeclKind::ConstLet => true,
            DeclKind::Import(f) => f.is_const,
            DeclKind::Let | DeclKind::Fn | DeclKind::Hoisted | DeclKind::Type(_) => false,
        })
    }
    /// A from-import of a slot its home module declares more than once.
    pub(super) fn imported_redeclared(&self) -> bool {
        self.decls
            .iter()
            .any(|(k, _)| matches!(k, DeclKind::Import(f) if f.redeclared))
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
    /// The one "already defined" rule for a module's top level (Go's `redeclared in this block`,
    /// Rust's E0428/E0255, per namespace): a second declaration of a name in a namespace a prior
    /// one binds is an error at the later of the two. Two imports are `note_import_bind`'s ("is
    /// already imported"), a let is `reject_redeclare`'s, and an import no one wrote (`Span::NOWHERE`,
    /// the prelude's) is shadowed like a builtin.
    fn clashes(&self, name: &str) -> Vec<(Span, String)> {
        let mut all: Vec<(DeclKind, Span)> =
            self.decls.iter().chain(&self.types).copied().collect();
        all.sort_by_key(|(_, s)| (s.line, s.col));
        let mut out = Vec::new();
        for ns in [Namespace::Value, Namespace::Type] {
            let mut prior: Option<DeclKind> = None;
            for &(kind, span) in &all {
                if !kind.declares(ns) || span.is_nowhere() {
                    continue;
                }
                let Some(first) = prior else {
                    prior = Some(kind);
                    continue;
                };
                let msg = match (first, kind) {
                    (DeclKind::Import(_), DeclKind::Import(_)) => continue,
                    (DeclKind::Import(_), _) | (_, DeclKind::Import(_)) => {
                        format!("'{name}' is already imported")
                    }
                    _ => format!("{} '{name}' is already defined", kind.word()),
                };
                out.push((span, msg));
            }
        }
        out
    }

    /// Seeded, and the walk has not reached its first let: hidden from top-level statements.
    pub(super) fn unreached(&self) -> bool {
        self.seeded && !self.reached
    }
}

impl Checker {
    /// Build `self.globals` from the module's top-level statements, in source order.
    /// `imports` pairs each from-import with its home module, whose sig is already in
    /// `module_sigs` (dependencies are checked first).
    pub(super) fn collect_module_globals(&mut self, stmts: &[Stmt], imports: &[ResolvedImport]) {
        self.globals.clear();
        for s in stmts {
            let mut add = |name: &str, kind: DeclKind, span: Span| {
                if !crate::ast::is_blank_name(name) {
                    let g = self.globals.entry(name.to_string()).or_default();
                    let list = match kind {
                        DeclKind::Type(_) => &mut g.types,
                        DeclKind::Let
                        | DeclKind::ConstLet
                        | DeclKind::Fn
                        | DeclKind::Import(_)
                        | DeclKind::Hoisted => &mut g.decls,
                    };
                    list.push((kind, span));
                }
            };
            match &s.kind {
                StmtKind::Let {
                    target, is_const, ..
                } => {
                    let kind = if *is_const {
                        DeclKind::ConstLet
                    } else {
                        DeclKind::Let
                    };
                    for n in &target.binder_names() {
                        add(n, kind, s.span);
                    }
                }
                StmtKind::Fn(d) => add(&d.name, DeclKind::Fn, s.span),
                StmtKind::Import(Import::Module { path, alias, .. }) => {
                    let name = alias
                        .clone()
                        .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                    let facts = ImportFacts {
                        value: true,
                        ty: true,
                        ..ImportFacts::default()
                    };
                    add(&name, DeclKind::Import(facts), s.span);
                }
                StmtKind::Import(Import::From { names, .. }) => {
                    let home = imports
                        .iter()
                        .find(|ri| ri.span == s.span)
                        .and_then(|ri| self.module_sigs.get(&ri.target));
                    for (member, alias) in names {
                        let slot = home.and_then(|sig| sig.member(member));
                        let is_type = home.is_some_and(|sig| sig.types.contains(member));
                        let facts = ImportFacts {
                            is_const: slot.is_some_and(|m| m.is_const),
                            redeclared: slot.is_some_and(|m| m.redeclared),
                            value: slot.is_some() || !is_type,
                            ty: is_type || slot.is_none(),
                        };
                        add(
                            alias.as_ref().unwrap_or(member),
                            DeclKind::Import(facts),
                            s.span,
                        );
                    }
                }
                // A variant is a value and a constructor head, so it binds both namespaces (Rust
                // E0255 for `use E::Red;` + `struct Red;`). It is a constant, as a pattern reads it
                // too, so a let cannot re-bind it (Rust E0530).
                StmtKind::Import(Import::Variants { names, .. }) => {
                    let facts = ImportFacts {
                        is_const: true,
                        value: true,
                        ty: true,
                        ..ImportFacts::default()
                    };
                    for (member, alias) in names {
                        add(
                            alias.as_ref().unwrap_or(member),
                            DeclKind::Import(facts),
                            s.span,
                        );
                    }
                }
                StmtKind::Extern { fns, .. } => {
                    for f in fns {
                        add(&f.name, DeclKind::Hoisted, f.span);
                    }
                }
                StmtKind::Native(d) => add(&d.name, DeclKind::Hoisted, s.span),
                StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::TypeAlias { name, .. } => add(name, DeclKind::Type("type"), s.span),
                StmtKind::Protocol { name, .. } => add(name, DeclKind::Type("protocol"), s.span),
                StmtKind::Assign { .. }
                | StmtKind::If { .. }
                | StmtKind::For { .. }
                | StmtKind::While { .. }
                | StmtKind::Match { .. }
                | StmtKind::Return(_)
                | StmtKind::Yield(_)
                | StmtKind::Defer(_)
                | StmtKind::Parallel { .. }
                | StmtKind::Spawn(_)
                | StmtKind::Wait { .. }
                | StmtKind::Break
                | StmtKind::Continue
                | StmtKind::Pass
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
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
                let first_const = match g.decls[0].0 {
                    DeclKind::ConstLet => true,
                    DeclKind::Import(f) => f.is_const,
                    DeclKind::Let | DeclKind::Fn | DeclKind::Hoisted | DeclKind::Type(_) => {
                        false
                    }
                };
                let msg = if first_const {
                    format!("cannot re-declare const binding '{name}' (a const cannot be rebound — not even with ':=' or a new typed let)")
                } else {
                    format!("'{name}' is declared both const and plain at module scope — a module global is one storage slot, so it cannot be const at one line and rebindable at another (declare it once)")
                };
                (g.decls[1].1, msg)
            })
            .collect();
        reports.extend(self.globals.iter().flat_map(|(name, g)| g.clashes(name)));
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

    /// Is the top-level let at `stmt_span` the first let of the seeded global `name`?
    pub(super) fn first_let_refines(&self, name: &str, stmt_span: Span) -> bool {
        self.scopes.len() == 1
            && self
                .globals
                .get(name)
                .is_some_and(|g| g.seeded && g.first_let() == Some(stmt_span))
    }

    /// The first let of a seeded global writes the slot the bodies above it already read
    /// (TICKET-186, K4): merge `declared` into the seed in `scopes[0]` and return `true`.
    /// It never calls `declare`, `reject_redeclare` or `declare_const`: those are for a
    /// fresh binding, and this is not one. Returns `false` when this is not that let, or when
    /// `declared` is not a refinement of the seed (a retype; the caller's `reject_redeclare`
    /// reports it against the seed).
    pub(super) fn refine_first_let(
        &mut self,
        name: &str,
        declared: Ty,
        stmt_span: Span,
        site: BindSite,
    ) -> bool {
        let Some(merged) = self.first_let_merge(name, &declared, stmt_span) else {
            return false;
        };
        // TICKET-238 -- the merged type is the one this let stores, so it is the one judged.
        let (merged, poisoned) = self.closed_binding_ty(name, merged, site);
        self.reach_global(name);
        if poisoned {
            self.hole_rejected[0].insert(name.to_string());
        } else {
            self.hole_rejected[0].remove(name);
        }
        self.scopes[0].insert(name.to_string(), merged);
        true
    }

    /// The type `refine_first_let` would write, or `None` when it would return `false`.
    pub(super) fn first_let_merge(&self, name: &str, declared: &Ty, stmt_span: Span) -> Option<Ty> {
        if !self.first_let_refines(name, stmt_span) {
            return None;
        }
        let prev = self.scopes[0].get(name).cloned().unwrap_or(Ty::Unknown);
        let mut merged = if prev.is_unknown() {
            declared.clone()
        } else {
            merge_unknown(&prev, declared)
        };
        // TICKET-201: labels are equality-neutral, so `merge_unknown` keeps the seed's; the seed
        // was computed for bodies, and the let's own value names its labels here (K6).
        if let (Ty::Func { labels, .. }, Ty::Func { labels: own, .. }) = (&mut merged, declared) {
            *labels = own.clone();
        }
        (merge_unknown(declared, &merged) == merged).then_some(merged)
    }

    /// May the code being checked run after a LATER top-level declaration? True in a fn or
    /// closure body (it runs when called), a `defer:` block (it runs at scope exit) and a `spawn:`
    /// block (it runs concurrently). False at top level and in a `parallel:` body, which run in
    /// source order. A `defer f(..)` call form evaluates its callee at the `defer` (c26), so it is
    /// not a deferred block. Also true in the global seed, the global typing pass and the
    /// write-summary fixpoint (`body_facts_pass`): they compute facts that bodies read.
    pub(super) fn runs_after_later_decls(&self) -> bool {
        self.in_fn_body || self.in_defer_block || self.in_spawn_block || self.body_facts_pass
    }

    /// TICKET-201 — the one answer to "does module slot `name` hold its `fn` declaration here?".
    /// A slot declared once does. A redeclared slot does only at a top-level statement of the main
    /// walk before the walk reaches the slot's first let; a body, a `defer:`/`spawn:` block and a
    /// body-facts pass never see it as holding the declaration. `self.functions` is a table of
    /// declarations: read it as slot content only through this.
    pub(super) fn slot_holds_fn_decl(&self, name: &str) -> bool {
        self.functions.contains_key(name)
            && match self.globals.get(name) {
                Some(g) if g.redeclared() => !self.runs_after_later_decls() && !g.reached,
                _ => true,
            }
    }

    /// The one decider for keyword-label certainty (TICKET-186): may a keyword call bind `name`'s
    /// labels, and under which `kw_certain` key? A body may run before or after any declaration
    /// of a module slot, so where `runs_after_later_decls` holds, a slot declared more than once
    /// (fn, import, let, extern, native) is never certain: it holds different functions at
    /// different times. A top-level statement runs in source order and keeps the lexical answer
    /// (cells c22, c23, c26). `kw_certain` is asked first, so every denial it made before keeps
    /// its message.
    pub(super) fn labels_certain(&self, name: &str) -> Result<Vec<(usize, String)>, KwDeny> {
        let body_redeclared =
            self.runs_after_later_decls() && self.globals.get(name).is_some_and(|g| g.redeclared());
        match self.owning_scope(name) {
            Some(s) if s >= 1 => {
                let key = (s, name.to_string());
                if let Some(deps) = self.kw_certain.get(&key) {
                    Ok(std::iter::once(key.clone())
                        .chain(deps.iter().cloned())
                        .collect())
                } else {
                    Err(KwDeny::NotOneFn)
                }
            }
            Some(_) => {
                let key = (0, name.to_string());
                if !self.kw_certain.contains_key(&key) {
                    if self
                        .globals
                        .get(name)
                        .is_some_and(|g| g.imported_redeclared())
                    {
                        Err(KwDeny::Redeclared)
                    } else {
                        Err(KwDeny::NotOneFn)
                    }
                } else if body_redeclared {
                    Err(KwDeny::Redeclared)
                } else {
                    Ok(std::iter::once(key.clone())
                        .chain(self.kw_certain[&key].iter().cloned())
                        .collect())
                }
            }
            // A fn reached by name through `functions`.
            None if self.functions.contains_key(name) && !self.slot_holds_fn_decl(name) => {
                Err(KwDeny::Redeclared)
            }
            None => Ok(vec![(0, name.to_string())]),
        }
    }

    /// Mark `name`'s first let as reached, if it is a seeded global.
    pub(super) fn reach_global(&mut self, name: &str) {
        if let Some(g) = self.globals.get_mut(name) {
            g.reached = true;
        }
    }
}
