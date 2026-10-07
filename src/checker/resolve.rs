//! TICKET-222 (R1) — the one writer of the resolutions table. What an expression path (`f`,
//! `lib.f`, `a.b.f`, `T.m`, `Bx[int].make`, `E.A`) denotes is decided by ONE rule table,
//! [`Checker::classify_path`], and written ONCE per NodeId by [`Checker::resolve_path`]. The call
//! side, the value side and the compiler read that answer and never re-decide. A path keeps ONE
//! kind whatever position reads it: a module fn is `Fn`, a type method `MethodFn`, a payload
//! variant `VariantFn`; the compiler derives the opcode from the position.
//!
//! [`Checker::commit_resolution`] is private here and has exactly four callers: `resolve_path`,
//! `record_pattern_head`, `record_index_call` and `record_decode`. A new path form adds a rule to
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

    /// What the path `e` denotes, from the one rule table, memoised per NodeId on the recording
    /// walk. A non-recording walk (a SYNTH id, the generic-arg prepass, `resolving_returns`)
    /// classifies afresh and writes only `callee_diverges` (DEC-180, DEC-025). `None` when `e` is
    /// no path this table classifies; nothing is written then.
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
            ExprKind::Field { obj, name, .. } if !crate::ast::is_tuple_index(name) => {
                self.classify_field(obj, name)
            }
            _ => None,
        }
    }

    fn classify_ident(&self, e: &Expr, n: &str) -> Option<Resolution> {
        // A default provider `desugar` synthesized (`$def$…`), unspellable by a user.
        if n.starts_with(crate::desugar::PROVIDER_PREFIX)
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
        ) || self.functions.contains_key(n)
        {
            // `value_head_resolution` asks `slot_holds_fn_decl`, the one fn-slot test (DEC-201).
            return Some(self.value_head_resolution(n));
        }
        if n == "None" {
            return Some(Resolution::Variant {
                enum_key: "Option".to_string(),
                variant: n.to_string(),
            });
        }
        if n == "print" || is_firstclass_builtin_fn(n) {
            return Some(Resolution::Builtin(n.to_string()));
        }
        None
    }

    fn classify_field(&self, obj: &Expr, name: &str) -> Option<Resolution> {
        if let ExprKind::Ident(t) = &obj.kind {
            // Through a type parameter: a bound's instance method is a path value; anything else
            // is the static-witness call (`T.make()`), which reports a miss itself.
            if self.shadowing_type_param(t) {
                return Some(match self.param_member_fn(t, name) {
                    Some(pf) => pf.res?,
                    None => Resolution::WitnessStatic(t.clone()),
                });
            }
            // std.json's decode: its entry is the descriptor `record_decode` writes (DEC-214).
            if self.json_decode_member(t, name) {
                return None;
            }
            if self.module_fn(t, name).is_some()
                && let Resolution::ModuleMember { module, name } = self.member_resolution(obj, name)
            {
                return Some(Resolution::Fn { module, name });
            }
        }
        if let Some((th, _)) = self.peel_type_path(obj)
            && th.kind != TypeHeadKind::Protocol
            && !th.native_handle
        {
            if let Some(v) = self.variants.get(&(th.key.clone(), name.to_string()))
                && v.payload.is_empty()
            {
                return Some(Resolution::Variant {
                    enum_key: th.key.clone(),
                    variant: name.to_string(),
                });
            }
            if let Some(r) = self.type_member_fn(&th, None, name).and_then(|pf| pf.res) {
                return Some(r);
            }
        }
        Some(self.member_resolution(obj, name))
    }

    /// The call side's writer until its dispatch reads `resolve_path` (TICKET-222 step 5).
    pub(super) fn record_resolution(&mut self, id: crate::ast::NodeId, r: Resolution, span: Span) {
        self.commit_resolution(id, r, span);
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

    /// Record that the call `call_id` reads its bracket as an index, then calls the element
    /// (`fs[k](10)`, DEC-210). Written on the CALL node, which is no path.
    pub(super) fn record_index_call(&mut self, call_id: crate::ast::NodeId, span: Span) {
        self.commit_resolution(call_id, Resolution::IndexCall, span);
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
        match crate::json_decode::from_ty(target, &shape, &mut Vec::new()) {
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
