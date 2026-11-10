//! TICKET-222 (R1) — the one decider and the one writer of the resolutions table. What an
//! expression path (`f`, `lib.f`, `a.b.f`, `T.m`, `Bx[int].make`, `E.A`) denotes is decided by ONE
//! rule table, [`Checker::classify_path`], and written ONCE per NodeId by
//! [`Checker::resolve_path`], which returns the answer. The call side (`infer_call_dispatch`), the
//! value side (`infer_ident`, `infer_field`, the type-applied fn value) and the compiler take their
//! branch from that answer and never re-decide. A path keeps ONE kind whatever position reads it:
//! a module fn is `Fn`, a type method `MethodFn`, a payload variant `VariantFn`; the compiler
//! derives the opcode from the position.
//!
//! [`Checker::commit_resolution`] is private here and has exactly four callers: `resolve_path`,
//! `record_pattern_head`, `record_bracket_reading` and `record_decode`. A new path form adds a rule to
//! `classify_path`; never add a `commit_resolution` call elsewhere or a `record_*` helper outside
//! this file.

use super::setup::{HeadBinding, TypeHeadKind};
use super::*;

impl Checker {
    /// The one table write: fills `callee_diverges` on every walk, and the resolutions table on
    /// the recording walk only. A second, different write for one NodeId is a checker bug
    /// (`debug_assert` here, `reject_table_conflicts` in release).
    fn commit_resolution(&mut self, id: crate::ast::NodeId, r: Resolution, span: Span) {
        // Every walk (inference passes included) records divergence, before the main-pass guard.
        if id.0 != crate::ast::NodeId::SYNTH.0 {
            let d = self.resolution_diverges(&r);
            self.callee_diverges
                .insert((self.graph_module_idx, id.0), d);
        }
        if !self.records_node(id) {
            return;
        }
        // One writer per NodeId: a second, different write means a second decider came back.
        if let Some(prev) = self.resolutions.get(&(self.graph_module_idx, id.0)) {
            debug_assert!(
                *prev == r,
                "NodeId {} resolved twice: {prev:?} then {r:?}",
                id.0
            );
        }
        crate::checker::record_call_table_entry(
            &mut self.resolutions,
            &mut self.table_conflicts,
            (self.graph_module_idx, id.0),
            r,
            "name resolution",
            span,
        );
    }

    /// What the path `e` denotes, from the one rule table, memoised per NodeId
    /// on the recording walk. A non-recording walk (a SYNTH id, the generic-arg prepass,
    /// `resolving_returns`) classifies afresh and writes only `callee_diverges` (DEC-180,
    /// DEC-025). `None` when `e` names nothing this table classifies (a type parameter, a type
    /// path's miss, std.json's decode); nothing is written then. Every reader matches on the answer.
    pub(super) fn resolve_path(&mut self, e: &Expr) -> Option<Resolution> {
        if self.records_node(e.id)
            && let Some(r) = self.resolutions.get(&(self.graph_module_idx, e.id.0))
        {
            return Some(r.clone());
        }
        let r = self.classify_path(e)?;
        self.commit_resolution(e.id, r.clone(), e.span);
        Some(r)
    }

    /// THE rule table: what the path `e` names. Only an `Ident` or a non-tuple `Field` is a path;
    /// a bracket node is never classified, its head is. Every input is an existing `&self` decider.
    pub(super) fn classify_path(&self, e: &Expr) -> Option<Resolution> {
        match &e.kind {
            ExprKind::Ident(n) => self.classify_ident(e, n),
            ExprKind::NoneLit => {
                self.imported_variants
                    .get(crate::lexer::NONE)
                    .map(|iv| Resolution::Variant {
                        enum_key: iv.head.key.clone(),
                        variant: iv.variant.clone(),
                    })
            }
            ExprKind::Field { obj, name, .. } if !crate::ast::is_tuple_index(name) => {
                // A full module path (`a.b` of `import a.b`) no binding hides names the module.
                if let Some(p) = self.full_module_path(e) {
                    return Some(self.value_head_resolution(&p));
                }
                self.classify_field(obj, name)
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
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
            | ExprKind::Field { .. }
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

    fn classify_ident(&self, e: &Expr, n: &str) -> Option<Resolution> {
        if let Some(th) = self.self_type_head(n)
            && th.kind == TypeHeadKind::Struct
        {
            return Some(Resolution::StructCtor(th.key));
        }
        // A default provider `desugar` synthesized (`$def$…`), unspellable by a user.
        if n.starts_with(crate::base::provider_name::PROVIDER_PREFIX)
            && (e.id.0 == crate::ast::NodeId::SYNTH.0 || !self.functions.contains_key(n))
        {
            return Some(Resolution::Provider);
        }
        // A type parameter shadows every same-named item here; its readers report the shadow.
        if self.shadowing_type_param(n) {
            return None;
        }
        if matches!(
            self.head_binding(n),
            HeadBinding::Local | HeadBinding::Global | HeadBinding::Module
        ) {
            return Some(self.value_head_resolution(n));
        }
        if self.functions.contains_key(n) {
            // A module binds the name once, so the fn declaration is the slot (DEC-201).
            return Some(self.value_head_resolution(n));
        }
        // A bare variant: a name an `import V from Enum` binds (the prelude's four included).
        if let Some(iv) = self.imported_variants.get(n) {
            let (enum_key, variant) = (iv.head.key.clone(), iv.variant.clone());
            let arity = self
                .variants
                .get(&(enum_key.clone(), variant.clone()))
                .map_or(0, |v| v.payload.len());
            return Some(if arity == 0 {
                Resolution::Variant { enum_key, variant }
            } else {
                Resolution::VariantFn { enum_key, variant }
            });
        }
        // A struct this module sees beats a builtin of its name (only keywords are reserved): a
        // user `struct int` makes `int(4)` its constructor.
        if let Some(key) = self.struct_ctor_key(n) {
            return Some(Resolution::StructCtor(key));
        }
        if let Some(b) = self.builtin_callee(n) {
            return Some(Resolution::Builtin(b));
        }
        // A value call through a module-level binding outside the scope stack (an imported value).
        Some(Resolution::Global {
            module: self.graph_module_idx,
            name: n.to_string(),
        })
    }

    /// The struct a bare `n(..)` constructs: a bare-resolvable struct (`struct_names`) or an alias
    /// of one. A module binds the name once, so no same-named fn replaces the constructor.
    fn struct_ctor_key(&self, n: &str) -> Option<String> {
        if !self.struct_names.contains(n) && self.alias_struct_head(n).is_none() {
            return None;
        }
        let key = self
            .alias_struct_head(n)
            .map(|(k, _)| k)
            .unwrap_or_else(|| self.bare_key(n));
        self.structs.contains_key(&key).then_some(key)
    }

    fn classify_field(&self, obj: &Expr, name: &str) -> Option<Resolution> {
        // Through a type parameter: a bound's instance method is a path value; anything else
        // is the static-witness call (`T.make()`), which reports a miss itself.
        if let Some(t) = self.type_param_head(obj) {
            return Some(match self.param_member_fn(t, name) {
                Some(pf) => pf.res?,
                None => Resolution::WitnessStatic(t.to_string()),
            });
        }
        if let Some(t) = self.head_name(obj) {
            // std.json's decode: its entry is the descriptor `record_decode` writes (DEC-214).
            if self.json_decode_member(&t, name) {
                return None;
            }
            if let Some(r) = self.qualified_ctor(&t, name) {
                return Some(r);
            }
            if self.module_fn(&t, name).is_some()
                && let Resolution::ModuleMember { module, name } = self.member_resolution(obj, name)
            {
                return Some(Resolution::Fn { module, name });
            }
        }
        // A type path names a member of its type or nothing: `None` is a miss (or a protocol's
        // method, which is no item), and its readers report it.
        if let Some((th, _)) = self.peel_type_path(obj) {
            if th.kind == TypeHeadKind::Protocol {
                return None;
            }
            if th.native_handle {
                // A native type's static method (`List.from_iter`) is a member of the path.
                if let Some(pf) = self.type_member_fn(&th, None, name) {
                    return pf.res;
                }
                // A native handle's instance method has no proto, so it is no path value; called,
                // it is a static call on the type, which reports it.
                return self
                    .structs
                    .get(&th.key)
                    .is_some_and(|info| info.methods.contains_key(name))
                    .then(|| Resolution::MethodFn {
                        type_key: th.key.clone(),
                        method: name.to_string(),
                    });
            }
            if let Some(v) = self.variants.get(&(th.key.clone(), name.to_string()))
                && v.payload.is_empty()
            {
                return Some(Resolution::Variant {
                    enum_key: th.key.clone(),
                    variant: name.to_string(),
                });
            }
            return self.type_member_fn(&th, None, name).and_then(|pf| pf.res);
        }
        Some(self.member_resolution(obj, name))
    }

    /// `m.name(..)` constructing through a whole-module import `m`: a qualified struct (not a
    /// reserved native type), an exported struct alias, or a native module's builtin constructor
    /// (`c.Shared(0)`, `time.timer(100)`).
    fn qualified_ctor(&self, m: &str, name: &str) -> Option<Resolution> {
        if !self.head_is_value(m)
            && let Some(mid) = self.imported_modules.get(m)
            && let Some(sig) = self.module_sigs.get(mid)
        {
            if !sig.builtins.contains(name) && sig.struct_defs.contains_key(name) {
                return Some(Resolution::StructCtor(self.type_key(mid, name)));
            }
            if sig.builtins.contains(name)
                && let Some(b) =
                    Builtin::from_name(name).filter(|b| b.lowering() == Lowering::Opcode)
            {
                return Some(Resolution::Builtin(b));
            }
        }
        if let Some(Ty::Struct(key, _)) = self.qualified_alias_ty(m, name) {
            return Some(Resolution::StructCtor(key));
        }
        None
    }

    /// Record a pattern head (`PatBinding`, `PatStruct`, a pattern `Variant`). A pattern is no
    /// expression path; its head is decided where the pattern binds.
    pub(super) fn record_pattern_head(
        &mut self,
        id: crate::ast::NodeId,
        r: Resolution,
        span: Span,
    ) {
        debug_assert!(
            matches!(
                r,
                Resolution::PatBinding | Resolution::PatStruct(_) | Resolution::Variant { .. }
            ),
            "not a pattern head: {r:?}"
        );
        self.commit_resolution(id, r, span);
    }

    /// Record which reading the call `call_id` takes of its one-arg bracket (DEC-210): the index,
    /// then the call of the element (`fs[k](10)`), or the callee's type arguments (`f[int](1)`).
    /// Written on the CALL node, which is no path.
    pub(super) fn record_bracket_reading(
        &mut self,
        call_id: crate::ast::NodeId,
        index: bool,
        span: Span,
    ) {
        let r = if index {
            Resolution::IndexCall
        } else {
            Resolution::TypeArgsCall
        };
        self.commit_resolution(call_id, r, span);
    }

    /// Build `target`'s decode descriptor and record it on `id` as `Resolution::Decode`. `false`
    /// when it reported why `target` is not decodable. Written only at a final `Pinned` verdict
    /// (DEC-214).
    pub(super) fn record_decode(
        &mut self,
        id: crate::ast::NodeId,
        target: &Ty,
        span: Span,
    ) -> bool {
        // One decision for what is decodable and what the VM decodes: the descriptor built here is
        // the diagnostic when it fails and the compiler's `Op::JsonDecode` operand when it succeeds.
        // Each field's default is the fill `S(...)` takes for it (decodable structs are non-generic,
        // so no type arguments), so a missing key and an omitted argument share one default.
        let shape = |key: &str| {
            self.structs.get(key).map(|s| {
                s.fields
                    .iter()
                    .enumerate()
                    .map(|(i, (name, ty))| {
                        let fill = s
                            .field_slots
                            .as_ref()
                            .and_then(|sl| sl.get(i))
                            .and_then(|sl| sl.default.as_ref())
                            .and_then(|d| self.default_fill(d, 0));
                        (name.clone(), ty.clone(), fill)
                    })
                    .collect()
            })
        };
        match crate::checker::json_decode::from_ty(target, &shape, &mut Vec::new()) {
            Ok(desc) => {
                self.commit_resolution(id, Resolution::Decode(desc), span);
                true
            }
            Err(msg) if msg.is_empty() => true,
            Err(msg) => {
                self.error(span, msg);
                false
            }
        }
    }
}
