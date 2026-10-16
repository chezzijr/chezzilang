//! Checker: capture signatures, scopes, declarations, lookup, name heads and type heads.
//! Split out of `src/checker/setup.rs`. `super::*` is the parent module.

use super::setup::BindSite;
use super::setup::HeadBinding;
use super::setup::TypeHead;
use super::setup::TypeHeadKind;
use super::setup::TypeRead;
use super::setup::alias_type_head;
use super::setup::open_binding_message;
use super::*;

impl Checker {
    /// Capture this module's public surface (own top-level fns/values/types) after checking.
    pub(super) fn capture_sig(&self, stmts: &[Stmt]) -> ModuleSig {
        let mut sig = ModuleSig::default();
        // TICKET-196: one export record per module slot, from its `GlobalBinding`. An extern fn is a
        // module global exactly like a top-level `fn` (the compiler binds it into the module's global
        // slot: `src/compiler/mod.rs` reserves the slot and emits `MakeCffi` + `DefineGlobalSlot`), so
        // it exports the same way — `c.strlen` / `import strlen from c` resolve through `sig.members`
        // (with `certain_fn` set), never `sig.types` (an extern fn is not a type, so it must not
        // enter the ctor-shadowing path).
        for (name, g) in &self.globals {
            let exported = g.decls.iter().any(|(k, _, _)| {
                matches!(
                    k,
                    globals::DeclKind::Fn
                        | globals::DeclKind::Hoisted
                        | globals::DeclKind::Let
                        | globals::DeclKind::ConstLet
                )
            });
            if !exported {
                continue;
            }
            let certain_fn = (g.decls.len() == 1
                && matches!(
                    g.decls[0].0,
                    globals::DeclKind::Fn | globals::DeclKind::Hoisted
                ))
            .then(|| self.functions.get(name).cloned())
            .flatten()
            .map(|mut fsig| {
                for effect in &mut fsig.summary.writes {
                    if let fn_writes::WriteRoot::Global(gname) = &effect.root {
                        effect.global_ty = self.lookup(gname);
                    }
                }
                fsig
            });
            let ty = match &certain_fn {
                Some(f) => Some(fn_value_ty(f)),
                None => self
                    .lookup(name)
                    .or_else(|| self.functions.get(name).map(fn_value_ty)),
            };
            let Some(ty) = ty else {
                continue;
            };
            sig.members.insert(
                name.clone(),
                MemberSig {
                    ty,
                    certain_fn,
                    is_const: g.is_const(),
                    redeclared: g.redeclared(),
                },
            );
        }
        for s in stmts {
            match &s.kind {
                StmtKind::Struct { name, .. } => {
                    sig.types.insert(name.clone());
                    // The LAYOUT lives under the runtime key (bare unless disambiguated); the sig is
                    // keyed by the BARE name (importers look up by bare member name + their own
                    // `type_key`).
                    let key = self.bare_key(name);
                    if let Some(info) = self.structs.get(&key) {
                        let mut info = info.clone();
                        // Carry the decl docstring onto the EXPORTED sig (editor hover) so an
                        // importer's `from M import S` / `x: S` hover surfaces it across the boundary.
                        info.doc = self.name_docs.get(name).cloned();
                        sig.struct_defs.insert(name.clone(), info);
                    }
                }
                StmtKind::Enum { name, .. } => {
                    sig.types.insert(name.clone());
                    let key = self.bare_key(name);
                    if let Some(variant_names) = self.enums.get(&key) {
                        let type_params =
                            self.enum_type_params.get(&key).cloned().unwrap_or_default();
                        let variants = variant_names
                            .iter()
                            .filter_map(|v| self.variants.get(&(key.clone(), v.clone())).cloned())
                            .collect();
                        sig.enum_defs.insert(
                            name.clone(),
                            EnumSigInfo {
                                variant_names: variant_names.clone(),
                                type_params,
                                variants,
                                methods: self.enum_methods.get(&key).cloned().unwrap_or_default(),
                                doc: self.name_docs.get(name).cloned(),
                            },
                        );
                    }
                }
                StmtKind::TypeAlias { name, .. } => {
                    sig.types.insert(name.clone());
                    if let Some(body) = self.aliases.get(name) {
                        // Resolve the alias body in THIS (the defining) module's scope so an
                        // importer carries the right underlying type (incl. an FFI width license).
                        let resolved = self.resolve_ty_ro(body);
                        // Also resolve the body to a WIDTH-BEARING CType in this defining scope, so a
                        // cross-module `type Len = int32` exports `int32` (not `Ty::Int`). This is the
                        // channel the real width travels through a `from`-import / `module.Alias` hop.
                        let ctype = self.resolve_ctype(body);
                        let licensed = self.ffi_alias_ok.contains(name);
                        // If the alias embeds FFI widths but is NOT licensed, find the first width
                        // the defining module did not import — an importer must reject the alias
                        // rather than launder the un-imported width.
                        let unlicensed_width = if licensed {
                            None
                        } else {
                            let mut widths = Vec::new();
                            Self::collect_width_names(body, &mut widths);
                            widths.into_iter().find(|w| self.bound_builtin(w).is_none())
                        };
                        sig.type_aliases.insert(
                            name.clone(),
                            AliasSig {
                                body: resolved,
                                licensed,
                                unlicensed_width,
                                ctype,
                            },
                        );
                    }
                }
                StmtKind::Protocol { name, .. } => {
                    // A reserved protocol is never exported: `prebuilt_protocols` stays the live
                    // source for its 21 names (mirrored, drift-guarded, in `std/prelude.chz`), so
                    // exporting it here would put a second, sig-borne copy into `sig.types`.
                    if !(is_reserved_protocol(name) && self.current_module_is_stdlib)
                        && let Some(info) = self.protocol_shape(name).cloned()
                    {
                        let doc = self.name_docs.get(name).cloned();
                        Self::export_protocol(&mut sig, name, info, doc);
                    }
                }
                StmtKind::Let { .. }
                | StmtKind::Assign { .. }
                | StmtKind::Fn(_)
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
                | StmtKind::Import(_)
                | StmtKind::Extern { .. }
                | StmtKind::Native(_)
                | StmtKind::NativeStruct { .. }
                | StmtKind::NativeEnum { .. }
                | StmtKind::NativeType { .. }
                | StmtKind::Assert { .. }
                | StmtKind::Expr(_) => {}
            }
        }
        sig
    }

    // ===== scopes =====

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.fn_write_scopes.push(HashMap::new());
        self.loop_vars.push(std::collections::HashSet::new());
        self.const_decls.push(std::collections::HashSet::new());
        self.hole_rejected.push(std::collections::HashSet::new());
        self.capture_table.push(HashMap::new());
        self.written_captures.push(HashMap::new());
    }
    pub(super) fn pop_scope(&mut self) {
        // TICKET-139 (W14-2) — settle the popped scope's keyword calls BEFORE the scope goes: a
        // keyword call through a `kw_certain` binding is legal only if the binding is never written,
        // and a write may come after the call, so the verdict waits for the binding's own scope end.
        // Module scope pops through here too (a module binding written from a fn body). The three
        // tables are keyed `(scope_idx, name)` and the next pushed scope reuses this index, so they
        // drain here.
        let top = self.scopes.len().saturating_sub(1);
        let pending = std::mem::take(&mut self.kw_pending);
        for (mut keys, kw_use) in pending {
            let written = keys
                .iter()
                .find(|k| k.0 >= top && self.kw_written.contains(*k))
                .cloned();
            keys.retain(|k| k.0 < top);
            if written.is_none() && !keys.is_empty() {
                self.kw_pending.push((keys, kw_use));
                continue;
            }
            match kw_use {
                super::globals::KwUse::Keyword(span) => {
                    if let Some(key) = written {
                        self.error(span, super::globals::kw_ambiguous_msg(&key.1));
                    }
                }
                super::globals::KwUse::GenStamp(call, crossing, span) if written.is_none() => {
                    crate::checker::record_call_table_entry(
                        &mut self.gen_crossings.calls,
                        &mut self.table_conflicts,
                        call,
                        crossing,
                        "generator crossing",
                        span,
                    );
                }
                _ => {}
            }
        }
        self.kw_certain.retain(|k, _| k.0 < top);
        self.kw_written.retain(|k| k.0 < top);
        self.scopes.pop();
        self.fn_write_scopes.pop();
        self.loop_vars.pop();
        self.const_decls.pop();
        self.hole_rejected.pop();
        self.capture_table.pop();
        self.written_captures.pop();
    }
    /// Record `name` (already declared in the current scope) as a `const T` binding. At module
    /// scope it writes nothing: module const-ness is `GlobalBinding::is_const` (TICKET-186).
    pub(super) fn declare_const(&mut self, name: &str) {
        if self.scopes.len() == 1 {
            return;
        }
        if let Some(set) = self.const_decls.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name` an in-scope `const T` binding (innermost binding wins, shadowing-aware)? A `const`
    /// captured by a nested fn stays const inside the closure body — the enclosing scope is still on
    /// the stack, so this resolves through it. A module global asks its record (TICKET-186).
    pub(super) fn is_const_decl(&self, name: &str) -> bool {
        match self.owning_scope(name) {
            Some(0) => self.globals.get(name).is_some_and(|g| g.is_const()),
            Some(i) => self.const_decls[i].contains(name),
            None => false,
        }
    }
    /// TICKET-238 -- did the hole check reject the binding `name` resolves to?
    pub(super) fn is_poisoned(&self, name: &str) -> bool {
        self.owning_scope(name)
            .is_some_and(|i| self.hole_rejected[i].contains(name))
    }
    /// TICKET-238 -- the statement read a rejected binding and an error exists: every further
    /// error of this statement is a cascade of that one.
    pub(super) fn after_hole_error(&self) -> bool {
        self.poison_reads != self.stmt_mark.poison && !self.errors.is_empty()
    }
    /// TICKET-238 -- THE hole check: the type a statement stores for `name` has no hole
    /// (`Ty::has_hole`). On a hole it reports once per statement and returns the error sentinel;
    /// the `bool` means "poisoned" (the caller marks the binding in `hole_rejected`). It closes
    /// no frame: a lambda param is declared while its statement still solves. A desugar temp
    /// (`OPT_RECV_PREFIX`) holds a consumed operand and is not judged.
    pub(super) fn closed_binding_ty(&mut self, name: &str, ty: Ty, site: BindSite) -> (Ty, bool) {
        let t = self.pinning_value_ty(&ty);
        if name.starts_with(super::pattern::OPT_RECV_PREFIX) {
            return (t, false);
        }
        if t.has_hole() {
            if self.errors.len() <= self.stmt_mark.errors {
                let span = self.stmt_mark.span;
                self.error(span, open_binding_message(name, &t, site));
            }
            return (Ty::Unknown, true);
        }
        let poisoned = t.is_unknown() && self.poison_reads != self.stmt_mark.poison;
        (t, poisoned)
    }
    pub(super) fn declare(&mut self, name: &str, ty: Ty) {
        self.declare_at(name, ty, BindSite::Other);
    }
    /// Bind `name` in the innermost scope, through the hole check.
    pub(super) fn declare_at(&mut self, name: &str, ty: Ty, site: BindSite) {
        let (ty, poisoned) = self.closed_binding_ty(name, ty, site);
        self.declare_unchecked(name, ty);
        if poisoned && let Some(set) = self.hole_rejected.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// `declare` without the hole check, for a name whose type a signature owns: the nested `fn`
    /// statement's own name (`infer_nested_fn_ret` and `finalize_ret` judge its return type) and a
    /// declared fn's params in `check_fn_body_inner` (`fn_sig` resolved and judged their annotations).
    pub(super) fn declare_unchecked(&mut self, name: &str, ty: Ty) {
        if let Some(set) = self.hole_rejected.last_mut() {
            set.remove(name);
        }
        // TICKET-139 (W14-2) — a same-scope re-declaration can share the runtime slot, so it counts
        // as a write for the keyword-call gate (`kw_written`).
        if self.scopes.last().is_some_and(|s| s.contains_key(name)) {
            self.kw_written
                .insert((self.scopes.len() - 1, name.to_string()));
        }
        self.scopes.last_mut().unwrap().insert(name.to_string(), ty);
        // Re-declaring a name (e.g. `:=` shadowing a loop var in the same scope) yields a fresh,
        // mutable binding — clear any loop-var mark so assignment to it isn't wrongly rejected.
        if let Some(set) = self.loop_vars.last_mut() {
            set.remove(name);
        }
        // Same rule for a `const` binding: a re-declaration in the same scope is a fresh binding
        // (mutable unless `declare_const` re-marks it), so clear any stale const mark first.
        if let Some(set) = self.const_decls.last_mut() {
            set.remove(name);
        }
        // Same rule for a `from`-imported global: re-declaring it at MODULE scope (`COUNT := COUNT + 1`)
        // hands the name back to this module, so the from-import rebind gate (`imported_values`, keyed
        // by bare name) must stop firing — the binding it names is gone. Module scope only (the sole
        // scope the gate consults); a fn-local shadow leaves the module binding intact. `bind_import`
        // inserts AFTER its own `declare`, so its entry survives.
        if self.scopes.len() == 1 {
            self.imported_values.remove(name);
        }
    }
    pub(super) fn lookup(&self, name: &str) -> Option<Ty> {
        self.owning_scope(name)
            .map(|i| self.scopes[i][name].clone())
    }
    /// TICKET-089 — `e` is a NULLARY read of a `Shared`/`RwShared`/`Atomic` box bound to a bare name
    /// (`s.get()`, `r.get()`, `a.load()`): the value it returns is a deep copy, so a write to it is
    /// thrown away (`docs/concurrency.md` §6). Returns `(box name, reader, payload type, write-through
    /// method)`. Bare-`Ident` receivers only, so the box type comes from `lookup` with no re-inference
    /// (which would double-report the receiver's diagnostics) — `boxes[0].get()` stays a ceiling.
    /// `RwShared.read(f)` takes a closure, so it is not this shape.
    pub(super) fn read_temporary_of_box(
        &self,
        e: &Expr,
    ) -> Option<(String, &'static str, Ty, &'static str)> {
        let ExprKind::Call { callee, args, .. } = &e.kind else {
            return None;
        };
        let ExprKind::Field {
            obj, name: reader, ..
        } = &callee.kind
        else {
            return None;
        };
        let ExprKind::Ident(name) = &obj.kind else {
            return None;
        };
        if !args.is_empty() {
            return None;
        }
        let (reader, fix, payload) = match (self.lookup(name)?, reader.as_str()) {
            (Ty::Shared(p), "get") => ("get", "update", *p),
            (Ty::RwShared(p), "get") => ("get", "write", *p),
            (Ty::Atomic(p), "load") => ("load", "store", *p),
            _ => return None,
        };
        Some((name.clone(), reader, payload, fix))
    }
    /// Does scope `i` hold a binding of `name` visible from here? The ONE raw membership test over
    /// `scopes` (TICKET-183). A module global seeded before any body is walked
    /// (`seed_module_globals`) is visible in every fn/closure body, but a top-level statement sees
    /// it only once the walk reaches its first let — module scope is order-free for bodies and
    /// lexical for statements.
    pub(super) fn scope_has(&self, i: usize, name: &str) -> bool {
        self.scopes[i].contains_key(name)
            && !(i == 0
                && !self.in_fn_body
                && self.globals.get(name).is_some_and(|g| g.unreached()))
    }
    /// Which scope owns `name` (innermost binding wins, shadowing-aware)? The ONE innermost-first
    /// walk over `scopes` (TICKET-183): every "which scope owns this name" question calls it, so the
    /// top-level lexical rule in [`Checker::scope_has`] applies to all of them.
    pub(super) fn owning_scope(&self, name: &str) -> Option<usize> {
        (0..self.scopes.len())
            .rev()
            .find(|&i| self.scope_has(i, name))
    }
    /// The one answer to what a bare head names, over `owning_scope`: a binding in scope >= 1, then
    /// a type parameter (DEC-108), then scope 0, then unbound (TICKET-180).
    pub(super) fn head_binding(&self, name: &str) -> HeadBinding {
        match self.owning_scope(name) {
            Some(i) if i > 0 => HeadBinding::Local,
            _ if self.type_params.contains_key(name) => HeadBinding::TypeParam,
            Some(_) if matches!(self.scopes[0].get(name), Some(Ty::Module(_))) => {
                HeadBinding::Module
            }
            Some(_) => HeadBinding::Global,
            None => HeadBinding::Unbound,
        }
    }
    /// Does a binding hide the type named `name` at a type-head site (`T.m()`, `E.V`)? Every
    /// binding does, a module-scope one and a whole-module import included (CPython rebinding).
    pub(super) fn head_hides_type(&self, name: &str) -> bool {
        matches!(
            self.head_binding(name),
            HeadBinding::Local | HeadBinding::Global | HeadBinding::Module
        )
    }
    /// Is the module name `name` a value here (a module-head site yields to it)? A whole-module
    /// import is not a value.
    pub(super) fn head_is_value(&self, name: &str) -> bool {
        matches!(
            self.head_binding(name),
            HeadBinding::Local | HeadBinding::Global
        )
    }
    /// THE one answer to "does this head expression name a user type here, which kind, under which
    /// key" (TICKET-204). The binding question is `head_hides_type`/`head_is_value` (DEC-180); a
    /// type head site reads this and filters on `kind`, never a type table. A name is one kind of
    /// type (a second declaration is `type 'A' is already defined`), so the kind order is total.
    pub(super) fn type_head(&self, head: &Expr) -> Option<TypeHead> {
        match &head.kind {
            ExprKind::Ident(n) => self.bare_type_head(n),
            ExprKind::Field { obj, name, .. } => match obj.kind.as_ident() {
                Some(m) => self.qualified_type_head(m, name),
                None => None,
            },
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        }
    }
    /// The bare half of [`Checker::type_head`]: a declared type, else a type alias of one.
    pub(super) fn bare_type_head(&self, name: &str) -> Option<TypeHead> {
        if self.head_hides_type(name) {
            return None;
        }
        let kind = if self.struct_names.contains(name) {
            TypeHeadKind::Struct
        } else if self.enum_names.contains(name) {
            TypeHeadKind::Enum
        } else if self.protocols.contains_key(&self.user_protocol_key(name)) {
            TypeHeadKind::Protocol
        } else {
            return alias_type_head(name, name.to_string(), self.alias_body_ty(name)?);
        };
        let key = if kind == TypeHeadKind::Protocol {
            self.user_protocol_key(name)
        } else {
            self.bare_key(name)
        };
        Some(TypeHead {
            kind,
            key,
            name: name.to_string(),
            spelled: name.to_string(),
            pinned: None,
            native_handle: false,
        })
    }
    /// U10: THE answer to "does bare `name`, read as a value, name a type". An alias is a type
    /// whatever its body; `Self` is the enclosing type of an inherent method.
    pub(super) fn bare_type_read(&self, name: &str) -> Option<TypeRead> {
        if let Some(th) = self.bare_type_head(name) {
            return Some(TypeRead::Head(th));
        }
        if self.head_hides_type(name) {
            return None;
        }
        if self.aliases.contains_key(name) || self.imported_alias_tys.contains_key(name) {
            return Some(TypeRead::Alias(name.to_string()));
        }
        if name == "Self" && self.current_self_ty.is_some() {
            return Some(TypeRead::SelfType);
        }
        (BuiltinType::global(name).is_some() || self.bound_builtin(name).is_some())
            .then(|| TypeRead::Builtin(name.to_string()))
    }
    /// U10: the `module.Name` half of [`Checker::bare_type_read`]: a type the whole-module import
    /// declares, else an exported alias of any body.
    pub(super) fn qualified_type_read(&self, module: &str, name: &str) -> Option<TypeRead> {
        if let Some(th) = self.qualified_type_head(module, name) {
            return Some(TypeRead::Head(th));
        }
        if self.head_is_value(module) {
            return None;
        }
        let mid = self.imported_modules.get(module)?;
        let sig = self.module_sigs.get(mid)?;
        if sig.type_aliases.contains_key(name) {
            return Some(TypeRead::Alias(format!("{module}.{name}")));
        }
        // A builtin type the module owns (`std.ffi`'s `ptr` and widths).
        sig.builtins
            .contains(name)
            .then(|| TypeRead::Builtin(format!("{module}.{name}")))
    }
    /// The `module.Name` half of [`Checker::type_head`]: a type the whole-module import `module`
    /// declares, else an exported alias of one.
    pub(super) fn qualified_type_head(&self, module: &str, name: &str) -> Option<TypeHead> {
        if self.head_is_value(module) {
            return None;
        }
        let mid = self.imported_modules.get(module)?;
        let sig = self.module_sigs.get(mid)?;
        let spelled = format!("{module}.{name}");
        let kind = if sig.struct_defs.contains_key(name) {
            TypeHeadKind::Struct
        } else if sig.enum_defs.contains_key(name) {
            TypeHeadKind::Enum
        } else if sig.protocol_defs.contains_key(name) {
            TypeHeadKind::Protocol
        } else {
            return alias_type_head(name, spelled, self.qualified_alias_ty(module, name)?);
        };
        Some(TypeHead {
            kind,
            key: self.type_key(mid, name),
            name: name.to_string(),
            spelled,
            pinned: None,
            // The same test the import loop uses to skip a handle.
            native_handle: kind == TypeHeadKind::Struct && sig.builtins.contains(name),
        })
    }
    /// The type `value` has with no expected type, read as a pinning use, or `None` when it errs
    /// or is unknown. Speculative: every diagnostic and side table is rolled back, and
    /// `pinning_value_ty` runs inside that window because the rollback drops a pending carrier
    /// (DEC-228).
    pub(super) fn unhinted_value_ty(&mut self, value: &Expr) -> Option<Ty> {
        let hint = self.expected_hint.take();
        let owner = self.hint_owner.take();
        let mark = self.diag_mark();
        let t = self.infer_value(value);
        let t = self.pinning_value_ty(&t);
        let erred = self.errors.len() != mark.errors;
        self.diag_rollback(mark);
        self.expected_hint = hint;
        self.hint_owner = owner;
        (!erred && !t.is_unknown()).then_some(t)
    }
    /// The literal `None`, the same test `is_inline_default` uses in `desugar`.
    pub(super) fn is_none_lit(e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::NoneLit)
    }
    /// The slot a literal `None` shares with the sibling `value`: `value`'s type under one `?`, or
    /// `value`'s own type when it is already a carrier. `None` when that type is not concrete.
    pub(super) fn none_slot_from(&mut self, value: &Expr) -> Option<Ty> {
        let t = self.unhinted_value_ty(value)?;
        let slot = merge_unknown(&Ty::Option(Box::new(Ty::Unknown)), &t);
        ty_fully_concrete(&slot).then_some(slot)
    }
    /// The slot of a literal or an `if` chain that holds a literal `None` beside other values, read
    /// from the first sibling that is not `None`. The syntax test runs first because the read costs
    /// a `diag_mark`. A site whose branches have binders calls [`Checker::branch_slot`] instead.
    pub(super) fn none_sibling_slot(&mut self, vals: &[&Expr]) -> Option<Ty> {
        if !vals.iter().any(|e| Self::is_none_lit(e)) {
            return None;
        }
        let first = vals.iter().find(|e| !Self::is_none_lit(e))?;
        self.none_slot_from(first)
    }
    /// [`Checker::none_sibling_slot`] for a site whose branches bring their own binders (a `match`
    /// arm's pattern, a `recover:` tail branch's statements): called inside each branch, after its
    /// binders are in scope. The first branch that is not `None` decides `slot`.
    pub(super) fn branch_slot(
        &mut self,
        beside: bool,
        slot: &mut Option<Ty>,
        value: &Expr,
    ) -> Option<Ty> {
        if beside && slot.is_none() && !Self::is_none_lit(value) {
            *slot = self.none_slot_from(value);
        }
        slot.clone()
    }
    /// `unify` for a call argument: a `?x` that meets a parameter whose solved type is still open
    /// is read as the pinning use it is, so `unify`'s fill arm never lifts an unpinned `?x` by a
    /// second layer. A bare `Unknown` parameter of a `fn` type is not open here.
    pub(super) fn unify_arg(&mut self, decl: &Ty, actual: &Ty, map: &mut HashMap<String, Ty>) {
        let open = match super::subst(decl, map) {
            Ty::Func { params, ret, .. } => ret.has_hole() || params.iter().any(Ty::has_hole),
            want @ Ty::Int
            | want @ Ty::Float
            | want @ Ty::Bool
            | want @ Ty::Str
            | want @ Ty::Bytes
            | want @ Ty::ByteArray
            | want @ Ty::Nil
            | want @ Ty::List(_)
            | want @ Ty::Map(..)
            | want @ Ty::Set(_)
            | want @ Ty::BuiltinFn { .. }
            | want @ Ty::Tuple(_)
            | want @ Ty::Struct(..)
            | want @ Ty::Enum(..)
            | want @ Ty::Param(_)
            | want @ Ty::Result(..)
            | want @ Ty::Option(_)
            | want @ Ty::Channel(_)
            | want @ Ty::Shared(_)
            | want @ Ty::Atomic(_)
            | want @ Ty::AtomicInt
            | want @ Ty::RwShared(_)
            | want @ Ty::Executor
            | want @ Ty::Socket
            | want @ Ty::Listener
            | want @ Ty::Writer
            | want @ Ty::Reader
            | want @ Ty::Ptr
            | want @ Ty::Width(_)
            | want @ Ty::Protocol(..)
            | want @ Ty::Module(_)
            | want @ Ty::Unknown
            | want @ Ty::Var(_) => want.has_hole(),
        };
        if open {
            let actual = self.pinning_value_ty(actual);
            super::unify(decl, &actual, map);
        } else if actual.has_hole() {
            // The mirror case, on the same statement: the parameter was solved to an unpinned `?x`
            // and this argument is open (`pair(?7, None)`). The open sibling is the pinning use,
            // so the `?x` takes its default and the `None` then meets a concrete slot.
            let want = super::subst(decl, map);
            if super::tyvar::has_var(&want) {
                let _ = self.pinning_value_ty(&want);
            }
            super::unify(decl, actual, map);
        } else {
            // A `?x` under a declared carrier (`o: T?`) takes that carrier, so `T` is its payload.
            self.solve(&super::subst(decl, map), actual);
            super::unify(decl, &self.zonk(actual), map);
        }
    }
}
