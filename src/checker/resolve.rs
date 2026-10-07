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
//! `record_pattern_head`, `record_index_call` and `record_decode`. A new path form adds a rule to
//! `classify_path`; never add a `commit_resolution` call elsewhere or a `record_*` helper outside
//! this file.

use super::setup::{HeadBinding, TypeHeadKind};
use super::*;

/// The builtin constructors and fns a bare call names (`infer_named_call`'s builtin arms).
const BUILTIN_CALLEES: &[&str] = &[
    "Ok",
    "Some",
    "Err",
    "print",
    "panic",
    "range",
    "int",
    "float",
    "bool",
    "str",
    "ord",
    "chr",
    "List",
    "Set",
    "Map",
    "bytearray",
    "bytes",
    "Channel",
    "Shared",
    "RwShared",
    "Atomic",
    "timer",
    "Executor",
    "AtomicInt",
];

/// Where a path is read. It changes one rule of [`Checker::classify_path`] only: a struct's raw
/// constructor inside its same-named fn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PathPos {
    /// The callee of a call (`P(..)`).
    Callee,
    /// Every other read (`f := P`, `ap(P, 1)`, `P == g`).
    Value,
}

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

    /// What the path `e` denotes at position `pos`, from the one rule table, memoised per NodeId
    /// on the recording walk. A non-recording walk (a SYNTH id, the generic-arg prepass,
    /// `resolving_returns`) classifies afresh and writes only `callee_diverges` (DEC-180,
    /// DEC-025). `None` when `e` names nothing this table classifies (a type parameter, a type
    /// path's miss, std.json's decode); nothing is written then. Every reader matches on the answer.
    pub(super) fn resolve_path(&mut self, e: &Expr, pos: PathPos) -> Option<Resolution> {
        if self.records_node(e.id)
            && let Some(r) = self.resolutions.get(&(self.graph_module_idx, e.id.0))
        {
            return Some(r.clone());
        }
        let r = self.classify_path(e, pos)?;
        self.commit_resolution(e.id, r.clone(), e.span);
        Some(r)
    }

    /// THE rule table: what the path `e` names. Only an `Ident` or a non-tuple `Field` is a path;
    /// a bracket node is never classified, its head is. Every input is an existing `&self` decider.
    pub(super) fn classify_path(&self, e: &Expr, pos: PathPos) -> Option<Resolution> {
        match &e.kind {
            ExprKind::Ident(n) => self.classify_ident(e, n, pos),
            ExprKind::Field { obj, name, .. } if !crate::ast::is_tuple_index(name) => {
                self.classify_field(obj, name)
            }
            _ => None,
        }
    }

    fn classify_ident(&self, e: &Expr, n: &str, pos: PathPos) -> Option<Resolution> {
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
        ) {
            return Some(self.value_head_resolution(n));
        }
        // The one rule that reads the position: inside `fn P` of a module declaring `struct P`,
        // the CALLEE `P(..)` is the struct's raw constructor and a VALUE read of `P` is the fn
        // (DEC-029/055/172). Rust draws the same line with its two namespaces: with
        // `struct P { x: i64 }` and `fn P(x: i64) -> P`, inside `fn P` the struct expression
        // `P { x }` constructs and `let f = P;` is the fn (`rustc --edition 2021`, run:
        // `P { x: 104 }`). Chezzi's callee `P(x=..)` is that struct-expression spelling.
        let ctor = self.struct_ctor_key(n);
        if pos == PathPos::Callee
            && let Some(key) = &ctor
            && self.raw_ctor_owner.as_deref() == Some(key.as_str())
        {
            return Some(Resolution::StructCtor(key.clone()));
        }
        if self.functions.contains_key(n) {
            // `value_head_resolution` asks `slot_holds_fn_decl`, the one fn-slot test (DEC-201).
            return Some(self.value_head_resolution(n));
        }
        if n == "None" {
            return Some(Resolution::Variant {
                enum_key: "Option".to_string(),
                variant: n.to_string(),
            });
        }
        if n == "print" || is_firstclass_builtin_fn(n) || BUILTIN_CALLEES.contains(&n) {
            return Some(Resolution::Builtin(n.to_string()));
        }
        if let Some(key) = ctor {
            return Some(Resolution::StructCtor(key));
        }
        // A value call through a module-level binding outside the scope stack (an imported value).
        Some(Resolution::Global {
            module: self.graph_module_idx,
            name: n.to_string(),
        })
    }

    /// The struct a bare `n(..)` constructs: a bare-resolvable struct (`struct_names`) or an alias
    /// of one, unless a same-named fn replaces its constructor outside its own body.
    fn struct_ctor_key(&self, n: &str) -> Option<String> {
        if !self.struct_names.contains(n) && self.alias_struct_head(n).is_none() {
            return None;
        }
        let key = self
            .alias_struct_head(n)
            .map(|(k, _)| k)
            .unwrap_or_else(|| self.bare_key(n));
        ((self.raw_ctor_owner.as_deref() == Some(key.as_str()) || !self.functions.contains_key(n))
            && self.structs.contains_key(&key))
        .then_some(key)
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
            if let Some(r) = self.qualified_ctor(t, name) {
                return Some(r);
            }
            if self.module_fn(t, name).is_some()
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
                // A native handle's method has no proto, so it is no path value; called, it is a
                // static call on the type.
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
    /// reserved native type, not replaced by a same-named fn slot), an exported struct alias, or a
    /// native module's builtin constructor (`c.Shared(0)`, `time.timer(100)`).
    fn qualified_ctor(&self, m: &str, name: &str) -> Option<Resolution> {
        if !self.head_is_value(m)
            && let Some(mid) = self.imported_modules.get(m)
            && let Some(sig) = self.module_sigs.get(mid)
        {
            if self.qualified_builtin_ty(name, &[]).is_none()
                && sig.struct_defs.contains_key(name)
                && !sig.member(name).is_some_and(MemberSig::holds_fn)
            {
                return Some(Resolution::StructCtor(self.type_key(mid, name)));
            }
            if sig.types.contains(name) && Self::qualified_native_ctor(name) {
                return Some(Resolution::Builtin(name.to_string()));
            }
        }
        if let Some(Ty::Struct(key, _)) = self.qualified_alias_ty(m, name)
            && !self.module_declares_fn(m, name)
        {
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
