// checker::pattern — split out of checker/mod.rs. `super::*` == the `checker` module.
// Pattern / match-arm binding and or-pattern consistency.

use super::resolve::PathPos;
use super::setup::{HeadBinding, TypeHead, TypeHeadKind};
use super::*;
use crate::ast::CarrierTag;
use crate::ast::consteval;

/// TICKET-225: whether `e` folds to a constant (`300`, `1 << 8`, `3e38 + 3e38`).
pub(super) fn is_const_expr(e: &Expr) -> bool {
    matches!(consteval::eval(e, &mut 0), consteval::Fold::Value(_))
}

/// The one diagnostic for a range used where it has no runtime value. It names every legal position
/// AND the materialization escape hatch — the `range(a, b)` builtin, which really does return a
/// `List[int]` (so `List(0..3)` is rejected and `Set(range(0, 3))` is the way).
pub(super) const RANGE_NOT_A_VALUE: &str = "a range is only valid as the iterable of a `for` loop or comprehension, as a slice receiver, \
     or as a `match` pattern — use `range(a, b)` to materialize a `List[int]`";

/// What a bare, payload-free name in a pattern is. `Checker::bare_pattern_name` is the one
/// classifier; every pattern walker reads it.
pub(super) enum BareName {
    /// A variant an `import V from Enum` (or the prelude) binds bare.
    Imported { key: String, variant: String },
    /// A variant no import binds: it must be written qualified.
    Variant,
    /// A `const` binding in scope: a pattern neither compares against it nor rebinds it.
    Const,
    /// A fresh name: the default arm, binding the whole value.
    Binder,
}

impl BareName {
    pub(super) fn is_variant(&self) -> bool {
        matches!(self, BareName::Imported { .. } | BareName::Variant)
    }
}

impl Checker {
    pub(super) fn bare_pattern_name(&self, name: &str) -> BareName {
        if let Some(iv) = self.imported_variants.get(name) {
            BareName::Imported {
                key: iv.head.key.clone(),
                variant: iv.variant.clone(),
            }
        } else if self.variant_owners.contains_key(name) {
            BareName::Variant
        } else if self.is_const_decl(name) {
            BareName::Const
        } else {
            BareName::Binder
        }
    }

    /// The variant a `?` / `!` pattern head means in the enum keyed `key`: the one map from a
    /// carrier tag to a variant.
    pub(super) fn carrier_variant_of_key(tag: CarrierTag, key: &str) -> Option<&'static str> {
        match (key, tag) {
            ("Option", CarrierTag::Present) => Some("Some"),
            ("Result", CarrierTag::Present) => Some("Ok"),
            ("Result", CarrierTag::Error) => Some("Err"),
            _ => None,
        }
    }

    /// The carrier pattern head that means the variant `variant`, read back from
    /// `carrier_variant_of_key`; `None` for `None` and for a user variant name.
    pub(super) fn carrier_tag_of_variant(variant: &str) -> Option<CarrierTag> {
        [CarrierTag::Present, CarrierTag::Error]
            .into_iter()
            .find(|tag| {
                ["Option", "Result"]
                    .iter()
                    .any(|key| Self::carrier_variant_of_key(*tag, key) == Some(variant))
            })
    }

    /// The `(enum key, variant)` a `?` / `!` head means over a value of type `ty`, or why it
    /// cannot match one.
    pub(super) fn carrier_variant(
        tag: CarrierTag,
        ty: &Ty,
    ) -> Result<(&'static str, &'static str), String> {
        let key = match ty {
            Ty::Option(_) => Some("Option"),
            Ty::Result(..) => Some("Result"),
            _ => None,
        };
        if let Some(key) = key
            && let Some(variant) = Self::carrier_variant_of_key(tag, key)
        {
            return Ok((key, variant));
        }
        Err(match (tag, key) {
            (CarrierTag::Error, Some(_)) => {
                format!("`!e` matches an error, and {ty} has none; write `None`")
            }
            (CarrierTag::Error, None) => {
                format!("`!e` matches the error of a `T!E` value, found {ty}")
            }
            (CarrierTag::Present, _) => {
                format!("`?v` matches a present `T?` or a successful `T!E`, found {ty}")
            }
        })
    }

    /// A carrier pattern over a value of type `ty`, as the one-payload variant pattern it means,
    /// on the same head id: the variant path then checks, records and binds it like the long
    /// form. `None` after reporting why the head cannot match `ty` (an un-inferable `ty` is the
    /// caller's to report).
    fn carrier_as_variant(
        &mut self,
        tag: CarrierTag,
        inner: &Pattern,
        id: crate::ast::NodeId,
        ty: &Ty,
        span: Span,
    ) -> Option<Pattern> {
        if ty.is_unknown() {
            return None;
        }
        match Self::carrier_variant(tag, ty) {
            Ok((_, variant)) => Some(Pattern::Variant {
                name: variant.to_string(),
                id,
                bindings: vec![inner.clone()],
                enum_name: None,
                module_name: None,
            }),
            Err(msg) => {
                self.error(span, msg);
                None
            }
        }
    }

    /// Declare every name `pattern` would bind as `Unknown`, after its head was rejected, so the
    /// arm body does not cascade into `unknown name` errors.
    fn declare_pattern_names_unknown(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident(name, _, _) => {
                if matches!(self.bare_pattern_name(name), BareName::Binder) {
                    self.declare(name, Ty::Unknown);
                }
            }
            Pattern::Variant { bindings: subs, .. } | Pattern::Tuple(subs) | Pattern::Or(subs) => {
                for sub in subs {
                    self.declare_pattern_names_unknown(sub);
                }
            }
            Pattern::Carrier { inner, .. } => self.declare_pattern_names_unknown(inner),
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
        }
    }

    /// Bind a bare pattern name to `ty`: the one place a pattern declares a name. A constant is
    /// rejected and declares nothing; the arm stays a catch-all (`true`), so no second error
    /// follows. `hover` is the binding token's own span, where the caller has one.
    fn bind_pattern_name(
        &mut self,
        id: crate::ast::NodeId,
        name: &str,
        ty: Ty,
        span: Span,
        hover: Option<Span>,
    ) -> bool {
        if matches!(self.bare_pattern_name(name), BareName::Const) {
            self.error(
                span,
                format!(
                    "`{name}` is a constant; to compare write `x if x == {name}`, to bind use a new name"
                ),
            );
            return true;
        }
        if let Some(h) = hover {
            self.hover_record_at(h, &ty, HoverKind::Local, None);
        }
        self.record_pattern_head(id, Resolution::PatBinding, span);
        self.declare(name, ty);
        true
    }

    /// The variant a pattern head `name` names in the enum `ekey`. A bare name an `import V from
    /// Enum` binds is the variant that import names, and only in that import's enum (`None` for
    /// any other enum); a qualified head, or a name no import binds, names itself.
    pub(super) fn pattern_variant_name(
        &self,
        bare: bool,
        name: &str,
        ekey: Option<&str>,
    ) -> Option<String> {
        match self.imported_variants.get(name).filter(|_| bare) {
            Some(iv) => (ekey == Some(iv.head.key.as_str())).then(|| iv.variant.clone()),
            None => Some(name.to_string()),
        }
    }

    /// Record a bare imported variant head (`Some(x)`) or its qualified spelling (`Option.None`)
    /// that no scrutinee type confirms: an un-inferable or literal scrutinee. The checker has
    /// already reported the arm unless it is inside a rolled-back carrier walk; the lowering tests
    /// the variant tag either way.
    fn record_imported_pattern_variant(
        &mut self,
        id: crate::ast::NodeId,
        enum_name: &Option<String>,
        name: &str,
        span: Span,
    ) {
        let Some(iv) = self.imported_variants.get(name) else {
            return;
        };
        if enum_name.as_deref().is_none_or(|e| e == iv.head.name) {
            let (key, variant) = (iv.head.key.clone(), iv.variant.clone());
            self.record_variant(id, &key, &variant, span);
        }
    }

    /// Type-check a *nested* sub-pattern (a variant payload slot or tuple element — gap #15) against
    /// its expected type `ty`, declaring any bindings into the current scope. Returns whether the
    /// sub-pattern is **irrefutable** (matches every value of `ty`): a binding/wildcard is, a
    /// literal/variant is not, a tuple is iff all its elements are.
    pub(super) fn bind_subpattern(&mut self, pattern: &Pattern, ty: &Ty, span: Span) -> bool {
        match pattern {
            Pattern::Wildcard => true,
            Pattern::Ident(name, bind_span, id) => {
                // A nested bare identifier names a *built-in* nullary variant of the matched type (a
                // refutable variant match — `Some(None)`, `Ok(Err(e))`), or a fresh binding. User
                // variants must be written qualified (handled below), never resolved bare here.
                let class = self.bare_pattern_name(name);
                if let BareName::Imported {
                    key: ikey,
                    variant: ivar,
                } = &class
                {
                    if Self::scrutinee_enum(ty) == Some(ikey.as_str())
                        && let Some(vmap) = self.variants_of(ty)
                        && let Some(payload) = vmap.get(ivar)
                    {
                        if payload.is_empty() {
                            // A nullary imported variant of `ty`: a refutable match, binds nothing.
                            self.record_variant(*id, ikey, ivar, span);
                            return false;
                        }
                        // A non-nullary variant used without its payload — needs `Name(...)`.
                        self.error(
                            span,
                            format!("variant '{name}' of {ty} requires its payload — write '{name}(...)'"),
                        );
                        return false;
                    }
                    // A built-in variant name that ISN'T a variant of `ty` cannot be a binding: the
                    // compiler routes it by the variant registry (a `MatchArm` test), so it would trap
                    // on the VM. Reject it here at check time instead.
                    if !ty.is_unknown() {
                        self.error(span, format!("'{name}' is not a variant of {ty}"));
                        return false;
                    }
                    // Over an un-inferable slot the nullary `None` is still the variant (a
                    // refutable test that binds nothing); a payload variant name binds.
                    if self
                        .variants
                        .get(&(ikey.clone(), ivar.clone()))
                        .is_some_and(|v| v.payload.is_empty())
                    {
                        self.record_variant(*id, ikey, ivar, span);
                        return false;
                    }
                }
                // A variant that is not imported must be written qualified — never resolved bare,
                // never silently a binding (the bare→binding trap). Reject with a hint to the
                // qualified form.
                if matches!(class, BareName::Variant) {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                    return false;
                }
                // EDITOR HOVER: a pattern binding (`n` in `Col.Val(n)`, `a`/`b` in `(a, b)`) is a
                // NAME, not an `Expr` the probe visits — record its decl-site hover at the binding
                // token's OWN span (`bind_span`, not the arm-level `span`), exactly as the for-loop
                // uses `var_spans`. No-op unless a probe is armed → zero overhead on normal checks.
                self.bind_pattern_name(*id, name, ty.clone(), span, Some(*bind_span))
            }
            Pattern::Or(alts) => self.bind_or_alternatives(alts, ty, span),
            Pattern::Carrier { tag, inner, id } => {
                if ty.is_unknown() {
                    self.error(
                        span,
                        "cannot match a variant pattern on a value of un-inferable type; annotate it"
                            .to_string(),
                    );
                }
                match self.carrier_as_variant(*tag, inner, *id, ty, span) {
                    Some(variant) => self.bind_subpattern(&variant, ty, span),
                    None => {
                        self.declare_pattern_names_unknown(inner);
                        false
                    }
                }
            }
            Pattern::Literal(lit) => {
                let lit_ty = lit_pattern_ty(lit);
                if !ty.is_unknown() && &lit_ty != ty.scalar() {
                    self.error(
                        span,
                        format!("literal of type {lit_ty} cannot match a value of type {ty}"),
                    );
                }
                false
            }
            Pattern::Range { .. } => {
                self.reject_empty_range(pattern, span);
                // A range sub-pattern is int-only and always refutable.
                if !ty.is_unknown() && ty.scalar() != &Ty::Int {
                    self.error(
                        span,
                        format!("range pattern cannot match a value of type {ty}"),
                    );
                }
                false
            }
            Pattern::Tuple(subs) => match ty {
                Ty::Tuple(tys) => {
                    if tys.len() != subs.len() {
                        self.error(
                            span,
                            format!(
                                "tuple pattern has {} element(s), but the value has {}",
                                subs.len(),
                                tys.len()
                            ),
                        );
                    }
                    let mut irref = true;
                    for (sub, t) in subs.iter().zip(tys.iter()) {
                        irref &= self.bind_subpattern(sub, t, span);
                    }
                    irref
                }
                Ty::Unknown => {
                    // §4.1 — a STRUCTURAL sub-pattern (here a tuple) over an un-inferable element/
                    // payload would destructure a value whose shape we can't prove → it traps at
                    // runtime on a wrong shape (a trailing `_` cannot rescue it). Reject; annotate the
                    // enclosing param. Still bind the sub-patterns (as Unknown) so the arm body does
                    // not cascade into spurious "unknown name" errors.
                    self.error(
                        span,
                        "cannot match a tuple pattern on a value of un-inferable type; annotate it"
                            .to_string(),
                    );
                    for sub in subs {
                        self.bind_subpattern(sub, &Ty::Unknown, span);
                    }
                    false
                }
                other => {
                    self.error(
                        span,
                        format!("tuple pattern cannot match a value of type {other}"),
                    );
                    for sub in subs {
                        self.bind_subpattern(sub, &Ty::Unknown, span);
                    }
                    false
                }
            },
            Pattern::Variant {
                id,
                name,
                bindings,
                enum_name,
                module_name,
            } => {
                // A USER struct sub-pattern (L2): `Line(Point(x, y), _)` binds a nested struct field
                // positionally. Checked BEFORE `check_pattern_qualifier` + the enum path — a struct
                // qualifier is a MODULE binder, not an enum, so the enum-qualifier validation would
                // otherwise mis-fire (`enum 'geo' has no variant 'Point'`) on a valid `geo.Point(..)`.
                // The constructor must name the struct (bare or module-qualified, via
                // `resolve_struct_ctor`); a qualifier that is NOT a module (an ENUM-name collision like
                // `E.Point`) is a clean reject here, NOT a VM crash — the compiler cannot lower it (bug
                // #4). Irrefutable iff every sub-pattern is (a struct has one constructor).
                if let Some(fields) = self.struct_fields_of(ty) {
                    let Ty::Struct(sname, _) = ty else {
                        unreachable!("struct_fields_of returned Some for a non-struct")
                    };
                    let ctor = self.resolve_struct_ctor(
                        sname,
                        name,
                        enum_name.as_deref(),
                        module_name.as_deref(),
                    );
                    if let Err(msg) = &ctor {
                        self.error(span, msg.clone());
                    } else {
                        self.record_pattern_head(*id, Resolution::PatStruct(sname.clone()), span);
                        if fields.len() != bindings.len() {
                            self.error(
                                span,
                                format!(
                                    "struct '{}' binds {} field(s), but {} given",
                                    crate::compiler::bare_display(sname),
                                    fields.len(),
                                    bindings.len()
                                ),
                            );
                        }
                    }
                    let mut sub_irref = true;
                    for (b, t) in bindings.iter().zip(fields.iter()) {
                        sub_irref &= self.bind_subpattern(b, t, span);
                    }
                    return ctor.is_ok() && fields.len() == bindings.len() && sub_irref;
                }
                let qualifier_reported = self.check_pattern_qualifier(
                    module_name,
                    enum_name,
                    name,
                    Self::scrutinee_enum(ty).map(|k| (k, ty)),
                    span,
                );
                match self.variants_of(ty) {
                    Some(vmap) => {
                        // A nested variant sub-pattern is irrefutable ONLY when its enum has exactly
                        // one variant (so naming it covers the whole domain) AND every payload
                        // sub-pattern is itself irrefutable. `Some(Some(v))` (2-variant Option) or
                        // `Some(0)` (literal payload) stays refutable; `Outer.Wrap(Inner.Only(x))`
                        // over single-variant enums is irrefutable and may close its parent variant.
                        let single_variant = vmap.len() == 1;
                        let vname = self.pattern_variant_name(
                            enum_name.is_none() && module_name.is_none(),
                            name,
                            Self::scrutinee_enum(ty),
                        );
                        match vname.as_ref().and_then(|v| vmap.get(v)) {
                            Some(payload) => {
                                if let (Some(ekey), Some(v)) = (Self::scrutinee_enum(ty), &vname) {
                                    self.record_variant(*id, ekey, v, span);
                                }
                                if payload.len() != bindings.len() {
                                    self.error(
                                        span,
                                        format!(
                                            "variant '{name}' binds {} value(s), but {} given",
                                            payload.len(),
                                            bindings.len()
                                        ),
                                    );
                                }
                                let mut sub_irref = true;
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    sub_irref &= self.bind_subpattern(b, t, span);
                                }
                                single_variant && sub_irref
                            }
                            None => {
                                if !qualifier_reported {
                                    self.error(span, format!("'{name}' is not a variant of {ty}"));
                                }
                                for b in bindings {
                                    self.bind_subpattern(b, &Ty::Unknown, span);
                                }
                                false
                            }
                        }
                    }
                    None if ty.is_unknown() => {
                        // §4.1 — a STRUCTURAL sub-pattern (here an enum/variant) over an un-inferable
                        // element/payload tests a variant tag against a value whose type we can't
                        // prove → it traps at runtime on a wrong shape (a trailing `_` cannot rescue
                        // it). Reject; annotate the enclosing param. Still bind the payload (as
                        // Unknown) so the arm body does not cascade into "unknown name" errors.
                        self.error(
                            span,
                            "cannot match a variant pattern on a value of un-inferable type; annotate it"
                                .to_string(),
                        );
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                        false
                    }
                    None => {
                        self.error(
                            span,
                            format!("variant pattern '{name}' cannot match a value of type {ty}"),
                        );
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                        false
                    }
                }
            }
        }
    }

    /// Bind the alternatives of an or-pattern in a *sub-pattern* position against `ty`, enforcing
    /// that every alternative binds the EXACT same set of names with unifiable types, then declaring
    /// the agreed set once into the current scope. Returns `true` iff ANY alternative is
    /// irrefutable. Bounded by the finite pattern tree (recursion only descends sub-patterns).
    pub(super) fn bind_or_alternatives(&mut self, alts: &[Pattern], ty: &Ty, span: Span) -> bool {
        // An or-pattern is irrefutable iff ANY alternative is irrefutable (one alt that always
        // matches makes the whole or-pattern always match) — OR, not AND.
        let mut irref = false;
        let mut binders: Vec<(usize, std::collections::BTreeMap<String, Ty>)> = Vec::new();
        for (i, alt) in alts.iter().enumerate() {
            self.push_scope();
            let alt_irref = self.bind_subpattern(alt, ty, span);
            irref |= alt_irref;
            // Snapshot the names this alternative introduced (its scratch scope's top frame).
            let snap: std::collections::BTreeMap<String, Ty> = self
                .scopes
                .last()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect();
            self.pop_scope();
            binders.push((i, snap));
        }
        self.enforce_or_consistency(&binders, span);
        irref
    }

    /// Enforce that all alternatives' binder snapshots agree on the bound-name set + unifiable types,
    /// then declare the agreed names once into the current (real) scope. `binders[0]` is the
    /// reference set; mismatches are reported once, clearly, and the first set is still declared so
    /// the arm body type-checks (no cascading "unknown name" errors).
    pub(super) fn enforce_or_consistency(
        &mut self,
        binders: &[(usize, std::collections::BTreeMap<String, Ty>)],
        span: Span,
    ) {
        if binders.is_empty() {
            return;
        }
        let (_, first) = &binders[0];
        for (_, other) in &binders[1..] {
            if first.keys().ne(other.keys()) {
                let left: Vec<&str> = first.keys().map(|s| s.as_str()).collect();
                let right: Vec<&str> = other.keys().map(|s| s.as_str()).collect();
                self.error(
                    span,
                    format!(
                        "or-pattern alternatives must bind the same variables: left binds {{{}}}, right binds {{{}}}",
                        left.join(", "),
                        right.join(", "),
                    ),
                );
                break;
            }
            // Same key set — check per-name type compatibility (in either direction).
            for (name, lt) in first.iter() {
                if let Some(rt) = other.get(name)
                    && !self.join_ty(lt, rt)
                    && !self.join_ty(rt, lt)
                {
                    self.error(
                        span,
                        format!("or-pattern binds '{name}' as {lt} in one alternative and {rt} in another"),
                    );
                }
            }
        }
        // Declare the agreed set once into the real scope.
        for (name, ty) in first.iter() {
            self.declare(name, ty.clone());
        }
    }

    /// Reject an empty range pattern. `start..end` is half-open (`start <= v < end`), so
    /// `start >= end` matches nothing on any run; rustc rejects the same shapes (E0579).
    fn reject_empty_range(&mut self, pattern: &Pattern, span: Span) {
        if let Pattern::Range { start, end } = pattern
            && start >= end
        {
            self.error(
                span,
                format!(
                    "empty range pattern '{start}..{end}': the lower bound must be less than the upper bound (a range pattern matches start <= v < end)"
                ),
            );
        }
    }

    /// Push a scope and bind one arm's pattern, recording coverage + diagnostics. Returns `true` if
    /// this arm is **irrefutable** (a `_` wildcard, or a tuple of irrefutable sub-patterns — either
    /// makes the match exhaustive). The caller must `pop_scope` after the arm body.
    pub(super) fn bind_match_arm(
        &mut self,
        pattern: &Pattern,
        kind: &MatchKind,
        span: Span,
        covered: &mut std::collections::HashSet<String>,
        guarded: bool,
    ) -> bool {
        // A wildcard binds nothing and is valid in every mode.
        if let Pattern::Wildcard = pattern {
            self.push_scope();
            return true;
        }
        // A carrier pattern is the variant pattern it means over this scrutinee; the variant path
        // below checks it. Only the duplicate-arm text differs: it prints the pattern as written.
        if let Pattern::Carrier { tag, inner, id } = pattern {
            let scrut = match kind {
                MatchKind::Variants { scrut, .. } => scrut.clone(),
                MatchKind::Literal(ty) => ty.clone(),
                MatchKind::Tuple(tys) => Ty::Tuple(tys.clone()),
                MatchKind::Struct { label, targs, .. } => Ty::Struct(label.clone(), targs.clone()),
                // Reported upstream (`reconstruct_unknown_kind`).
                MatchKind::Skip => Ty::Unknown,
            };
            let Some(variant) = self.carrier_as_variant(*tag, inner, *id, &scrut, span) else {
                self.push_scope();
                self.declare_pattern_names_unknown(inner);
                return false;
            };
            let Pattern::Variant { name, .. } = &variant else {
                unreachable!("carrier_as_variant builds a Variant")
            };
            let duplicate = covered.remove(name);
            if duplicate {
                self.error(
                    span,
                    format!("duplicate match arm '{}'", tag.pattern_text()),
                );
            }
            let irref = self.bind_match_arm(&variant, kind, span, covered, guarded);
            if duplicate {
                covered.insert(name.clone());
            }
            return irref;
        }
        // An or-pattern at the top of an arm: bind each alternative into a scratch scope (threading
        // coverage so `Red | Green | Blue` closes the variant domain), enforce that all alternatives
        // bind the same names with unifiable types, then declare the agreed set into the arm scope.
        // Irrefutable iff ANY alternative is (e.g. `1 | _` is irrefutable via `_`). OR, not AND.
        if let Pattern::Or(alts) = pattern {
            self.push_scope(); // the arm scope the caller pops
            let mut irref = false;
            let mut binders: Vec<(usize, std::collections::BTreeMap<String, Ty>)> = Vec::new();
            for (i, alt) in alts.iter().enumerate() {
                // Recurse: this pushes a scratch scope, threads `covered`, binds the alternative.
                // Thread `guarded` unchanged: a top-level guard makes EVERY alternative refutable,
                // so a guarded `E.A(0) | E.B` closes nothing; an unguarded one lets each alternative
                // decide its own payload-irrefutability independently.
                let alt_irref = self.bind_match_arm(alt, kind, span, covered, guarded);
                let snap: std::collections::BTreeMap<String, Ty> = self
                    .scopes
                    .last()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                self.pop_scope(); // discard the scratch scope (we re-declare into the arm scope)
                irref |= alt_irref;
                binders.push((i, snap));
            }
            self.enforce_or_consistency(&binders, span);
            return irref;
        }
        // Reject a name bound more than once within this (non-Or, non-Wildcard) pattern, e.g. `(x, x)`
        // or `E.V(a, a)` — Rust's rule. Emitted (not early-returned) so the arm body still checks on
        // the last binding, avoiding cascade errors. Or-alternatives are checked when this fn recurses
        // on each alt above, so a duplicate inside one alt is still caught.
        // A bare ident is a real binder UNLESS it names a (refutable) nullary variant — the built-in
        // `Ok`/`Err`/`Some`/`None` or a user enum variant — which binds nothing (see `bind_subpattern`).
        // So `(None, None, None)` isn't falsely flagged as a duplicate binding.
        let is_binder = |name: &str| matches!(self.bare_pattern_name(name), BareName::Binder);
        if let Some(dup) = first_duplicate_binder(pattern, &is_binder) {
            self.error(
                span,
                format!("identifier '{dup}' is bound more than once in this pattern"),
            );
        }
        self.reject_empty_range(pattern, span);
        match kind {
            MatchKind::Skip => {
                // Un-inferable scrutinee with only binding/`_` arms: accept the pattern shape
                // permissively, binding everything as `Unknown`. Still scope so the caller can
                // `pop_scope` uniformly. (A structural arm here was already rejected upstream by §4.1.)
                self.push_scope();
                match pattern {
                    Pattern::Variant {
                        id,
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } => {
                        // Un-inferable scrutinee (Skip): no enum to validate the qualifier against.
                        self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                        // A bare name (no qualifier, no payload) that is NOT a known variant is an
                        // irrefutable binding catch-all — `n:` binds the scrutinee like `_` and
                        // closes the match, exactly as a concretely-typed scrutinee does (the parser
                        // models every bare pattern name as a nullary `Variant`). Declaring it +
                        // returning irrefutable keeps the un-inferable path consistent with the
                        // typed-`Literal` path; treating it as a refutable variant instead would both
                        // leave the binding undeclared (`unknown name`) and wrongly report the match
                        // non-exhaustive.
                        let is_known_variant = self.bare_pattern_name(name).is_variant();
                        if enum_name.is_none()
                            && module_name.is_none()
                            && bindings.is_empty()
                            && !is_known_variant
                        {
                            return self.bind_pattern_name(*id, name, Ty::Unknown, span, None);
                        }
                        // A structural arm over an un-inferable scrutinee is rejected upstream
                        // (`reconstruct_unknown_kind`), except inside a rolled-back carrier walk,
                        // whose lowering names only the built-in variants.
                        self.record_imported_pattern_variant(*id, enum_name, name, span);
                        covered.insert(name.clone());
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                    Pattern::Tuple(subs) => {
                        for s in subs {
                            self.bind_subpattern(s, &Ty::Unknown, span);
                        }
                    }
                    Pattern::Carrier { inner, .. } => self.declare_pattern_names_unknown(inner),
                    _ => {}
                }
            }
            MatchKind::Variants {
                label,
                variants,
                scrut,
            } => {
                self.push_scope();
                match pattern {
                    Pattern::Variant {
                        id,
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } => {
                        // TICKET-107 (W12-11) — a bare non-variant name binds the whole scrutinee
                        // (Rust's identifier-pattern rule; Chezzi requires `E.Variant`, so it can
                        // never be a variant); returning `true` makes it irrefutable, so an
                        // unguarded one closes the match and warns later arms exactly like `_`, and
                        // a guarded one closes nothing; `exh_lower` already lowers it to `Pat::Wild`
                        // and the compiler already binds the whole value.
                        if enum_name.is_none()
                            && module_name.is_none()
                            && bindings.is_empty()
                            && !self.bare_pattern_name(name).is_variant()
                        {
                            return self.bind_pattern_name(*id, name, scrut.clone(), span, None);
                        }
                        let qualifier_reported = self.check_pattern_qualifier(
                            module_name,
                            enum_name,
                            name,
                            Some((label.as_str(), scrut)),
                            span,
                        );
                        let vname = self.pattern_variant_name(
                            enum_name.is_none() && module_name.is_none(),
                            name,
                            Some(label.as_str()),
                        );
                        let payload = vname.as_ref().and_then(|v| variants.get(v)).cloned();
                        let name = vname.as_deref().unwrap_or(name);
                        if payload.is_some() {
                            self.record_variant(*id, &label.clone(), name, span);
                        }
                        if payload.is_none() && !qualifier_reported {
                            self.error(span, format!("'{name}' is not a variant of {scrut}"));
                        }
                        // Bind the payload FIRST, accumulating whether every sub-pattern is
                        // irrefutable (a wildcard or plain binding). A literal/range/nested-variant
                        // sub-pattern (e.g. `Some(0)`, `P.Pair(0, y)`) makes the payload refutable,
                        // so the arm covers only part of the variant's domain — it must NOT close it.
                        let mut payload_irref = true;
                        match &payload {
                            Some(payload) => {
                                if payload.len() != bindings.len() {
                                    self.error(
                                        span,
                                        format!(
                                            "variant '{name}' binds {} value(s), but {} given",
                                            payload.len(),
                                            bindings.len()
                                        ),
                                    );
                                }
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    payload_irref &= self.bind_subpattern(b, t, span);
                                }
                            }
                            None => {
                                for b in bindings {
                                    payload_irref &= self.bind_subpattern(b, &Ty::Unknown, span);
                                }
                            }
                        }
                        // A variant is `covered` ONLY when an arm for it is BOTH unguarded AND has an
                        // all-irrefutable payload (docs/syntax.md §8: a guarded arm is never
                        // irrefutable). Duplicate-arm detection fires only against a PRIOR fully
                        // closing arm, so a guard-then-fallback on the same variant is legal.
                        if covered.contains(name) {
                            self.error(span, format!("duplicate match arm '{name}'"));
                        } else if !guarded && payload_irref {
                            covered.insert(name.to_string());
                        }
                    }
                    Pattern::Literal(_) => self.error(
                        span,
                        format!(
                            "cannot match a literal against {}",
                            crate::compiler::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Range { .. } => self.error(
                        span,
                        format!(
                            "cannot match a range against {}",
                            crate::compiler::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Tuple(_) => self.error(
                        span,
                        format!(
                            "cannot match a tuple against {}",
                            crate::compiler::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Ident(..)
                    | Pattern::Wildcard
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("ident/wildcard/or/carrier handled elsewhere")
                    }
                }
            }
            MatchKind::Literal(ty) => {
                self.push_scope();
                match pattern {
                    Pattern::Literal(lit) => {
                        let lit_ty = lit_pattern_ty(lit);
                        if &lit_ty != ty {
                            self.error(
                                span,
                                format!(
                                    "literal of type {lit_ty} cannot match scrutinee of type {ty}"
                                ),
                            );
                        }
                        // Exact-duplicate literal-arm detection, mirroring the enum-variant
                        // `covered`/`guarded` logic above: a literal closed by a PRIOR UNGUARDED arm
                        // makes any later same-literal arm dead → a `duplicate match arm` error (was
                        // silently accepted; enum-variant dups already erred — this closes the
                        // inconsistency). A GUARDED arm never closes, so `1 if c: … / 1: …` stays
                        // legal. Keyed with a `:`-bearing prefix so it can never collide with a
                        // variant name (identifiers have no `:`). Range subsumption is out of scope.
                        use crate::ast::LitPattern;
                        let key = match lit {
                            LitPattern::Int(n) => format!("lit:i{n}"),
                            LitPattern::Str(s) => format!("lit:s{s}"),
                            LitPattern::Bool(b) => format!("lit:b{b}"),
                        };
                        if covered.contains(&key) {
                            let shown = match lit {
                                LitPattern::Int(n) => n.to_string(),
                                LitPattern::Str(s) => format!("\"{s}\""),
                                LitPattern::Bool(b) => b.to_string(),
                            };
                            self.error(span, format!("duplicate match arm '{shown}'"));
                        } else if !guarded {
                            covered.insert(key);
                        }
                    }
                    Pattern::Range { .. } => {
                        // A range pattern is int-only; reject against str/bool scrutinees.
                        if ty != &Ty::Int {
                            self.error(
                                span,
                                format!("range pattern cannot match scrutinee of type {ty}"),
                            );
                        }
                    }
                    // int/str/bool have no nullary variants, so a bare top-level identifier here is a
                    // binding capturing the whole scrutinee value (irrefutable catch-all). The parser
                    // emits it as `Variant { bindings: [] }`; reinterpret it as a binding — UNLESS the
                    // name is a registered variant (e.g. `None`). The compiler routes by the variant
                    // registry, so a colliding name would trap on the VM; reject
                    // it here at check time instead. (Rename the binding to fix.)
                    Pattern::Variant {
                        id,
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } if bindings.is_empty() => {
                        // A *qualified* `Enum.Variant` is unambiguously a variant, never a binding —
                        // validate the qualifier and reject it against an int/str/bool scrutinee (a
                        // variant cannot match a literal-typed value). Falls through the bare path
                        // below otherwise.
                        if enum_name.is_some() {
                            // int/str/bool scrutinee: no enum to validate against (the variant is
                            // rejected below regardless).
                            self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                            self.error(span, format!("cannot match a variant against {ty}"));
                            return false;
                        }
                        // Match the compiler's variant registry: user enums PLUS the built-in
                        // Result/Option variants (which the checker special-cases elsewhere).
                        if self.bare_pattern_name(name).is_variant() {
                            self.error(
                                span,
                                format!(
                                    "'{name}' is a variant name and cannot bind a scrutinee of type {ty}; rename the binding"
                                ),
                            );
                            return false;
                        }
                        return self.bind_pattern_name(*id, name, ty.clone(), span, None);
                    }
                    Pattern::Variant {
                        id,
                        bindings,
                        enum_name,
                        name,
                        module_name,
                    } => {
                        self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                        self.error(span, format!("cannot match a variant against {ty}"));
                        self.record_imported_pattern_variant(*id, enum_name, name, span);
                        // Still bind the payload sub-patterns (as Unknown) so the arm body doesn't
                        // cascade into spurious "unknown name" errors — notably the desugared `?.`
                        // case, where the payload binding is an internal `__opt` temp the user can't
                        // see. (The `cannot match` error already flags the real problem.)
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span);
                        }
                    }
                    Pattern::Tuple(_) => {
                        self.error(span, format!("cannot match a tuple against {ty}"))
                    }
                    Pattern::Ident(..)
                    | Pattern::Wildcard
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("ident/wildcard/or/carrier handled elsewhere")
                    }
                }
            }
            MatchKind::Tuple(tys) => {
                self.push_scope();
                if let Pattern::Tuple(subs) = pattern {
                    if tys.len() != subs.len() {
                        self.error(
                            span,
                            format!(
                                "tuple pattern has {} element(s), but the value has {}",
                                subs.len(),
                                tys.len()
                            ),
                        );
                    }
                    let mut irref = true;
                    for (sub, t) in subs.iter().zip(tys.iter()) {
                        irref &= self.bind_subpattern(sub, t, span);
                    }
                    return irref;
                }
                // TICKET-139/W14-36 + DEC-107: a BARE payload-free unqualified non-variant name is a
                // whole-scrutinee catch-all on a tuple too (`rest:`), the same predicate the struct
                // arm uses — irrefutable, and the name binds the whole tuple.
                if let Pattern::Variant {
                    id,
                    name,
                    bindings,
                    enum_name: None,
                    module_name: None,
                } = pattern
                    && bindings.is_empty()
                {
                    if self.bare_pattern_name(name).is_variant() {
                        self.error(
                            span,
                            format!(
                                "'{name}' is a variant name and cannot bind a scrutinee of type {}; rename the binding",
                                Ty::Tuple(tys.clone())
                            ),
                        );
                        return false;
                    }
                    return self.bind_pattern_name(*id, name, Ty::Tuple(tys.clone()), span, None);
                }
                self.error(
                    span,
                    "a tuple scrutinee requires a tuple pattern (or `_`)".to_string(),
                );
            }
            MatchKind::Struct {
                label,
                fields,
                targs,
            } => {
                self.push_scope();
                match pattern {
                    Pattern::Variant {
                        id,
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } => {
                        let shown = crate::compiler::bare_display(label.as_str());
                        // The constructor spelling: BARE `Point` OR module-qualified `geo.Point` (the
                        // only spelling for a whole-module-imported struct — the bare name isn't in
                        // scope). `resolve_struct_ctor` accepts either against the scrutinee's identity;
                        // a qualifier that is not a module (`E.Point`, an enum-name collision) or a
                        // 3-part path is a clean reject, never a mis-bind (bugs #1, #2, #4).
                        let ctor = self.resolve_struct_ctor(
                            label,
                            name,
                            enum_name.as_deref(),
                            module_name.as_deref(),
                        );
                        // A BARE non-constructor name binding nothing is a whole-scrutinee catch-all
                        // (`other:`), mirroring the literal/tuple bare-ident path — irrefutable, closes
                        // the match. A bare name that IS a variant collides and is rejected here
                        // (same rule as the `MatchKind::Literal` path).
                        let is_bare = enum_name.is_none() && module_name.is_none();
                        if is_bare && ctor.is_err() && bindings.is_empty() {
                            if self.bare_pattern_name(name).is_variant() {
                                self.error(
                                    span,
                                    format!(
                                        "'{name}' is a variant name and cannot bind a scrutinee of type {shown}; rename the binding"
                                    ),
                                );
                                return false;
                            }
                            let ty = Ty::Struct(label.clone(), targs.clone());
                            return self.bind_pattern_name(*id, name, ty, span, None);
                        }
                        // A constructor pattern: the name must be the struct's own name, and the
                        // field count must match (a clean checker error, never a runtime panic).
                        let is_ctor = ctor.is_ok();
                        if is_ctor {
                            self.record_pattern_head(
                                *id,
                                Resolution::PatStruct(label.clone()),
                                span,
                            );
                        }
                        if let Err(msg) = &ctor {
                            self.error(span, msg.clone());
                        } else if fields.len() != bindings.len() {
                            self.error(
                                span,
                                format!(
                                    "struct '{shown}' binds {} field(s), but {} given",
                                    fields.len(),
                                    bindings.len()
                                ),
                            );
                        }
                        // Bind each positional field. A struct has ONE constructor, so a `label(..)`
                        // arm whose every sub-pattern is irrefutable is itself irrefutable and closes
                        // the match; a literal/nested-refutable field (`Point(0, y)`) keeps it open.
                        let mut irref = is_ctor && fields.len() == bindings.len();
                        for (b, t) in bindings.iter().zip(fields.iter()) {
                            irref &= self.bind_subpattern(b, t, span);
                        }
                        // Duplicate-arm detection (bug #3): a struct has ONE constructor, so an
                        // UNGUARDED irrefutable arm CLOSES the match — a later constructor arm is dead
                        // code, exactly like a repeated enum-variant/literal arm. Keyed on the struct
                        // identity (`label`), which never collides with a literal key or a sibling
                        // variant name (a struct match's `covered` holds only struct labels).
                        if is_ctor {
                            if covered.contains(label) {
                                self.error(span, format!("duplicate match arm '{shown}'"));
                            } else if !guarded && irref {
                                covered.insert(label.clone());
                            }
                        }
                        return irref;
                    }
                    Pattern::Literal(_) => {
                        self.error(span, format!("cannot match a literal against {label}"))
                    }
                    Pattern::Range { .. } => {
                        self.error(span, format!("cannot match a range against {label}"))
                    }
                    Pattern::Tuple(_) => {
                        self.error(span, format!("cannot match a tuple against {label}"))
                    }
                    Pattern::Ident(..)
                    | Pattern::Wildcard
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("ident/wildcard/or/carrier handled elsewhere")
                    }
                }
            }
        }
        false
    }

    /// W8-40: warn on a `match` arm made unreachable by an earlier arm. `dead` is `has_wildcard` READ
    /// ONE ARM LATER — `has_wildcard` already means "an earlier arm was irrefutable AND unguarded"
    /// (see the three call sites), so a guarded catch-all (`_ if c:`) never silences a later arm: its
    /// guard may fail at runtime, so every arm after it stays live. Range subsumption (`0..10` then
    /// `5`) is deliberately out of scope — the exhaustiveness walk has no interval arithmetic.
    fn warn_unreachable_arm(&mut self, dead: bool, span: Span) {
        if dead {
            self.warn(
                span,
                "unreachable match arm: an earlier arm already matches every value".to_string(),
            );
        }
    }

    /// `bool` is the ONE closed literal domain — `int`/`str` are infinite and still need a `_`.
    /// `covered` only ever receives an UNGUARDED arm's key (see the `else if !guarded` gate above),
    /// so a guarded `true if c:` never closes the domain. Feeds `has_wildcard` in the three arm
    /// loops, which is also what `warn_unreachable_arm` reads one arm later — closing the domain
    /// this way both removes the false rejection and warns on a following `_`, matching rustc.
    fn bool_domain_closed(kind: &MatchKind, covered: &std::collections::HashSet<String>) -> bool {
        matches!(kind, MatchKind::Literal(Ty::Bool))
            && covered.contains("lit:btrue")
            && covered.contains("lit:bfalse")
    }

    /// Report a non-exhaustive match.
    /// - Variants mode: missing variants, unless a `_` wildcard was seen.
    /// - Literal mode: int/str literal domains are open, so a `_` wildcard is *required*. `bool`'s
    ///   two-value domain is closed by unguarded `true` and `false` arms (see `bool_domain_closed`),
    ///   which reach here as `has_wildcard` — so a bool match covering both values takes the
    ///   early `has_wildcard` return above and never reaches this arm.
    /// - Skip mode: un-inferable scrutinee, no exhaustiveness check.
    pub(super) fn check_exhaustive(
        &mut self,
        kind: &MatchKind,
        covered: &std::collections::HashSet<String>,
        has_wildcard: bool,
        help: Option<String>,
        span: Span,
    ) {
        if has_wildcard {
            return;
        }
        match kind {
            MatchKind::Skip => {}
            MatchKind::Variants {
                label,
                variants,
                scrut,
            } => {
                // A carrier prints as its type and its missing variants as patterns (`!_`); a
                // user enum keeps its name and its variant names.
                let carrier = matches!(scrut, Ty::Option(_) | Ty::Result(..));
                let mut missing: Vec<String> = variants
                    .keys()
                    .filter(|v| !covered.contains(*v))
                    .map(|v| match Self::carrier_tag_of_variant(v) {
                        Some(tag) if carrier => tag.pattern_text().to_string(),
                        _ => v.clone(),
                    })
                    .collect();
                if !missing.is_empty() {
                    missing.sort();
                    let shown = if carrier {
                        scrut.to_string()
                    } else {
                        crate::compiler::bare_display(label.as_str())
                    };
                    self.error_help(
                        span,
                        format!(
                            "non-exhaustive match on {shown}: missing {}",
                            missing.join(", ")
                        ),
                        help,
                    );
                }
            }
            MatchKind::Literal(_) => {
                self.error_help(
                    span,
                    "non-exhaustive match: add a `_` arm".to_string(),
                    help,
                );
            }
            MatchKind::Tuple(_) => {
                // A tuple match is exhaustive only via an irrefutable arm (a `_`, or a tuple of
                // all-binding sub-patterns). `has_wildcard` already captured that.
                self.error_help(
                    span,
                    "non-exhaustive match: add a `_` arm".to_string(),
                    help,
                );
            }
            MatchKind::Struct { .. } => {
                // A struct has ONE constructor, so a single all-binding `Point(x, y)` arm is
                // irrefutable and closes the match (`has_wildcard` already captured that). Reaching
                // here means every arm was refutable (a literal/nested field like `Point(0, y)`) with
                // no `_` — non-exhaustive.
                self.error_help(
                    span,
                    "non-exhaustive match: add a `_` arm".to_string(),
                    help,
                );
            }
        }
    }

    /// Resolve a struct pattern's constructor spelling against the scrutinee struct identity `label`
    /// (L2). A BARE `Point` resolves via `bare_key`; a QUALIFIED `mod.Point` resolves the module binder
    /// to the struct's identity key — symmetric with qualified construction (`geo.Point(3, 4)`), and the
    /// only spelling for a whole-module-imported struct (the bare name isn't in scope). `Ok(())` when it
    /// names `label`; `Err(msg)` (already BARE-rendered — never the `::` identity key) otherwise. A
    /// 3-part `a.b.Point` is rejected (structs are two-level). Shared by the top-level `MatchKind::Struct`
    /// arm and the nested-struct sub-pattern arm so the two lower identically.
    pub(super) fn resolve_struct_ctor(
        &self,
        label: &str,
        name: &str,
        enum_name: Option<&str>,
        module_name: Option<&str>,
    ) -> Result<(), String> {
        let shown = crate::compiler::bare_display(label);
        if module_name.is_some() {
            return Err(format!(
                "struct patterns use two-level paths; write `{shown}(...)` or `<module>.{shown}(...)`"
            ));
        }
        match enum_name {
            None => {
                if self.bare_key(name) == label
                    || self
                        .alias_struct_head(name)
                        .is_some_and(|(k, _)| k == label)
                {
                    Ok(())
                } else {
                    Err(format!("'{name}' is not a constructor of {shown}"))
                }
            }
            Some(q) => {
                let Some(mid) = self.imported_modules.get(q) else {
                    return Err(format!(
                        "'{q}' is not a module; write `{shown}(...)` or `<module>.{shown}(...)`"
                    ));
                };
                if self.type_key(mid, name) == label
                    || matches!(self.qualified_alias_ty(q, name), Some(Ty::Struct(k, _)) if k == label)
                {
                    Ok(())
                } else {
                    Err(format!("'{q}.{name}' is not a constructor of {shown}"))
                }
            }
        }
    }

    /// `wait:` — Chezzi's `select` (§6d). Each arm's channel expr must be a `Channel[T]`; the arm's
    /// target binds (`:=`)/assigns (`=`)/discards (`_`) the element `T`. `wait` is a runtime race, not
    /// a type match, so it is **not** exhaustive — no coverage analysis, ≥1 arm is the only structural
    /// rule (parser-enforced). Each arm body is its own lexical sub-scope (like a `match` arm).
    pub(super) fn check_wait(&mut self, arms: &[WaitArm], else_block: Option<&Block>) {
        for arm in arms {
            self.push_scope();
            match &arm.kind {
                WaitArmKind::Recv { target, chan } => {
                    let elem = match self.infer(chan) {
                        Ty::Channel(e) => *e,
                        Ty::Unknown => Ty::Unknown,
                        other => {
                            self.error(
                                chan.span,
                                format!("a wait arm must recv from a Channel, found {other}"),
                            );
                            Ty::Unknown
                        }
                    };
                    match target {
                        WaitTarget::Bind(name) => self.declare(name, elem),
                        WaitTarget::Discard => {}
                    }
                }
                WaitArmKind::Send { call } => self.check_wait_send(call),
            }
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        if let Some(b) = else_block {
            self.check_block(b);
        }
    }

    /// A send-`wait:` arm must be exactly `chan.send(value)` with `chan: Channel[T]` and `value: T`.
    /// Anything else (`try_send`, a non-`send` call, a bare non-call expr) is rejected with the list
    /// of legal arm forms — the parser is lenient, so this is the sole gate on send-arm shape. When
    /// the shape IS `chan.send(value)`, inferring the call reuses the ordinary channel-`send` checks
    /// (element-type match + sendability) so a send arm and a plain `ch.send(v)` type-check identically.
    fn check_wait_send(&mut self, call: &Expr) {
        // Decompose the required shape `<recv>.send(<1 positional arg>)`, no named args.
        if let ExprKind::Call {
            callee,
            args,
            named,
            ..
        } = &call.kind
            && let ExprKind::Field { obj, name, .. } = &callee.kind
            && name == "send"
            && args.len() == 1
            && named.is_empty()
        {
            // The receiver MUST be a `Channel[T]`. A user type that merely HAS a `send` method
            // would type-check clean, but the compiler lowers a send-arm as a raw channel op and
            // `op_wait_poll` calls `channel_core` on the handle — a non-channel receiver hits an
            // `unreachable!` VM panic (the checker-superset-of-compiler soundness class). Gate it
            // here, mirroring the recv-arm's `Ty::Channel(e)` guard, before the ordinary call infer.
            // Infer the receiver for its TYPE only — snapshot + truncate its errors, because the
            // `self.infer(call)` below re-infers the same `obj` sub-expression and would re-report
            // them, doubling a diagnostic (e.g. an undefined receiver → two "undefined variable"s).
            // Mirrors the RwShared `read` recovery-only re-inference idiom.
            let mark = self.diag_mark();
            let recv_ty = self.infer(obj);
            self.diag_rollback(mark);
            match recv_ty {
                Ty::Channel(_) | Ty::Unknown => {}
                other => {
                    self.error(
                        obj.span,
                        format!("a wait send arm must send to a Channel, found {other}"),
                    );
                    return;
                }
            }
            // Receiver is a channel — infer the whole call to surface element-type/arg errors (and the
            // receiver's own errors, reported exactly once here) so a send arm and a plain
            // `ch.send(v)` type-check identically.
            self.infer(call);
            return;
        }
        // Not `chan.send(value)` — surface any nested-expr errors, then list the legal arm forms.
        self.infer(call);
        self.error(
            call.span,
            "a wait arm must be a recv (`x := ch.recv()`), a send (`ch.send(v)`), a timer, \
             or `else`"
                .to_string(),
        );
    }

    pub(super) fn check_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchArm]) {
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut exh = self.exh_new(&kind);
        let mut arm_pattern_error = false;
        for arm in arms {
            self.warn_unreachable_arm(
                has_wildcard,
                arm.body.first().map_or(scrutinee.span, |s| s.span),
            );
            // PERSISTENT refine-on-first-use (see `check_block`): a STATEMENT-`match` arm mirrors an
            // if/else statement body — a refine-on-first-use pin of an OUTER empty collection inside
            // one arm PERSISTS across sibling arms and past the match (Option B: a cross-arm element-
            // type conflict is a hard error). No snapshot/restore here, so the pin `repin` wrote to
            // the binding's OWNING scope survives `pop_scope` (which only removes the arm's binders).
            // The EXPRESSION-position matcher `infer_match` keeps its barrier — value-arms stay
            // independent.
            // The mark spans the arm PATTERN only, never the guard or the body: an unrelated error
            // inside an arm must not hide a genuinely non-exhaustive match.
            let pat_mark = self.errors.len();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                arm.span,
                &mut covered,
                arm.guard.is_some(),
            );
            arm_pattern_error |= self.errors.len() > pat_mark;
            // The guard is type-checked with the arm's bindings in scope. A guarded arm is never
            // irrefutable — its guard may fail at runtime — so it can't make the match exhaustive.
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            has_wildcard |= Self::bool_domain_closed(&kind, &covered);
            has_wildcard |= self.exh_add(&mut exh, &arm.pattern, arm.guard.is_some());
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        let help = self.exh_help(&exh);
        if !arm_pattern_error {
            self.check_exhaustive(&kind, &covered, has_wildcard, help, scrutinee.span);
        }
    }

    /// Infer an expression-position `match`: bind each arm, infer its value, and unify the arm
    /// types into one result. Exhaustiveness is still enforced.
    pub(super) fn infer_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[crate::ast::MatchExprArm],
        owned: bool,
    ) -> Ty {
        // Capture + clear the expected-type hint before the scrutinee/guards (the hint is for the
        // arm BODIES, the tail values). It is re-installed before each arm body below — every arm
        // is equally the value, and `infer_call` drains the single take()-once slot, so without
        // re-installing per arm only the first-inferred arm would get the hint (branch-order bug).
        let hint = self.expected_hint.take();
        let had_hint = hint.is_some();
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut exh = self.exh_new(&kind);
        let mut arm_pattern_error = false;
        let mut arm_tys: Vec<(Span, Ty)> = Vec::new();
        for arm in arms {
            self.warn_unreachable_arm(has_wildcard, arm.body.span);
            // No refine-on-first-use barrier here: a pin made in a value arm PERSISTS, exactly like
            // statement position. See the note above `Checker::is_unrefined_empty_coll`.
            let pat_mark = self.errors.len();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                arm.span,
                &mut covered,
                arm.guard.is_some(),
            );
            arm_pattern_error |= self.errors.len() > pat_mark;
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            has_wildcard |= Self::bool_domain_closed(&kind, &covered);
            has_wildcard |= self.exh_add(&mut exh, &arm.pattern, arm.guard.is_some());
            match &hint {
                Some(h) => self.install_hint(&arm.body, h.clone(), owned),
                None => self.expected_hint = None,
            }
            let t = self.infer(&arm.body);
            self.pop_scope();
            arm_tys.push((arm.body.span, t));
        }
        self.expected_hint = None;
        self.hint_owner = None;
        let mut result = None;
        for (sp, t) in arm_tys {
            result = Some(self.unify_branch(result, t, sp, hint.as_ref()));
        }
        let help = self.exh_help(&exh);
        if !arm_pattern_error {
            self.check_exhaustive(&kind, &covered, has_wildcard, help, scrutinee.span);
        }
        let res = result.unwrap_or(Ty::Unknown);
        if had_hint {
            res
        } else {
            self.default_expr_result_e(res)
        }
    }

    /// Infer an expression-position `if c: a else: b`: condition is bool, the two branches unify.
    pub(super) fn infer_if_else(
        &mut self,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
        owned: bool,
    ) -> Ty {
        self.infer_if_else_chain(cond, then, els, owned)
    }

    /// Chain-aware body of `infer_if_else`: an `elif` desugars to a nested `IfElse` in `els`, and the
    /// nested `els` sub-chain is inferred by a DIRECT recursive call here (not generic `infer`).
    fn infer_if_else_chain(&mut self, cond: &Expr, then: &Expr, els: &Expr, owned: bool) -> Ty {
        // Capture + clear the expected-type hint before the condition: the hint is for the branch
        // VALUES (tail position), not the bool condition. Re-install it for EACH branch — both are
        // equally the tail value, and `infer_call` drains the single slot via `take()`, so without
        // re-installing it the second-inferred branch would lose the hint and a generic ctor there
        // would deadlock (acceptance would depend on branch order).
        let hint = self.expected_hint.take();
        let had_hint = hint.is_some();
        self.expect_bool(cond, "if condition");
        // No refine-on-first-use barrier here: a pin made in a branch VALUE persists, exactly like
        // statement position. See the note above `Checker::is_unrefined_empty_coll`.
        // Each branch value owns the hint when this if owns it (TICKET-227): it wraps on its own.
        match &hint {
            Some(h) => self.install_hint(then, h.clone(), owned),
            None => self.expected_hint = None,
        }
        let t_then = self.infer(then);
        match &hint {
            Some(h) => self.install_hint(els, h.clone(), owned),
            None => self.expected_hint = None,
        }
        // A nested-`IfElse` `els` is the `elif` tail — recurse DIRECTLY (it owns the hint exactly
        // when this chain does); any other `els` is the final leaf, inferred normally.
        let t_els = if let ExprKind::IfElse {
            cond: c2,
            then: t2,
            els: e2,
        } = &els.kind
        {
            // The `elif` node itself never wraps: its own branches did.
            self.hint_owner = None;
            self.infer_if_else_chain(c2, t2, e2, owned)
        } else {
            self.infer(els)
        };
        self.expected_hint = None;
        self.hint_owner = None;
        let acc = self.unify_branch(None, t_then, then.span, hint.as_ref());
        let res = self.unify_branch(Some(acc), t_els, els.span, hint.as_ref());
        if had_hint {
            res
        } else {
            self.default_expr_result_e(res)
        }
    }

    /// Default an UNANNOTATED if/match-expression's folded `Result` error slot to the built-in
    /// `Error` protocol — matching the return-inference E-default and the `T!`/`Result[T]` shorthand
    /// (docs/syntax.md) — WHEN the slot is un-pinned (`Unknown`) or its payload satisfies `Error`. A
    /// concrete non-`Error` payload is PRESERVED (see the arm below: no post-hoc re-check exists here,
    /// so laundering it into `Error` would be unsound). E.g. `x := if c: Ok(1) else: Ok(2)` folds to
    /// `Result[int, Unknown]` (no `Err` branch) and `x := if c: Ok(1) else: Err("e")` folds to
    /// `Result[int, Unknown]` too (the fold keeps the `Ok` branch's E-`Unknown`) — both normalize to
    /// an `Error` slot. Applied ONLY without an expected-type hint (an annotated
    /// `x: Result[str, str] = if …` keeps its declared E) and ONLY to the top-level `Result` — it
    /// does NOT reject a residual `Unknown` (binding position stays lenient: `x := if c: None else:
    /// None` is as legal as `x := None`). The T-slot / deeper order-dependent branch merge is
    /// intentionally out of scope here (`unify_branch` keeps its `compatible`-based fold untouched).
    fn default_expr_result_e(&self, t: Ty) -> Ty {
        match t {
            // An UNANNOTATED if/match-expression's `Result` error slot defaults to the `Error`
            // protocol when un-pinned (`Unknown`) OR the pinned payload satisfies `Error` AND IS
            // SENDABLE — matching the return-inference E-default (`sig.rs fill_ret`). A concrete
            // payload that does NOT satisfy `Error`, OR satisfies `Error` but is NOT sendable (the
            // `Error` existential is sendable like every protocol), is PRESERVED: unlike the
            // return path there is no post-hoc assignability re-check here, so forcing `Error` would
            // launder a non-Error (or non-sendable) value into the `Error` existential (`match x:
            // Err(e): e.message()` would check-pass then fault at runtime). Fires only on the
            // no-hint path (an explicit `x: Result[str, str] = if …` keeps its declared E).
            Ty::Result(v, e)
                if e.is_unknown()
                    || (self.assignable(&Ty::error_proto(), &e) && self.sendable(&e)) =>
            {
                Ty::Result(v, Box::new(Ty::error_proto()))
            }
            other => other,
        }
    }

    /// Fold one branch's type into a match/if expression's running result type. The first concrete
    /// branch sets the type; a later incompatible branch is a real error (and yields `Unknown` to
    /// suppress cascades). `Unknown` branches never override a concrete result. `hint` is the
    /// statically known expected type at this position (an annotated binding, a call argument, a
    /// declared return) — it is read ONLY on a `compatible` mismatch, and ONLY through `assignable`,
    /// never folded into the accumulator (so an existing diagnostic like `Sq and int` stays that,
    /// not `Sh and int`). An int branch beside a float branch is an error (D3: no int→float widening).
    pub(super) fn unify_branch(
        &mut self,
        acc: Option<Ty>,
        t: Ty,
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        match acc {
            None => t,
            Some(prev) => {
                if self.join_ty(&prev, &t) {
                    if prev.is_unknown() { t } else { prev }
                } else if let Some(h) = hint
                    && ty_fully_concrete(h)
                    && self.assignable(h, &prev)
                    && self.assignable(h, &t)
                {
                    h.clone()
                } else {
                    self.error(
                        span,
                        format!(
                            "branches have incompatible types: {prev} and {t}{}",
                            float_fix_note_join(&prev, &t)
                        ),
                    );
                    Ty::Unknown
                }
            }
        }
    }

    // ===== expression inference =====

    /// Type-check an interpolated string literal's `{...}` fragment expressions. The string is
    /// parsed into chunks by the SHARED `crate::interpolation` parser (the very one the compiler
    /// emits from — so the checker and the compiler can never disagree on how a string is chunked),
    /// and every fragment `Expr` is run through the normal `infer_value` path: undefined names,
    /// type/method/arity mismatches, and void-call fragments all surface here as compile errors
    /// instead of slipping past `check` to panic the compiler (`global_slot`) or fault at runtime.
    ///
    /// A malformed interpolation (unterminated `{`, bad format spec) is reported as an error; we
    /// then stop (the compiler treats the same malformed string as fatal). Format-spec *validation*
    /// stays the compiler's job — we discard the parsed spec and only infer the expression.
    ///
    /// Span: a fragment expr is parsed from the `{…}` substring via `lexer::tokenize_frag`, which
    /// re-lexes it against the literal's `PosMap` — so every fragment token span is the char's REAL
    /// physical source position (line and column), past real newlines, `\n` escapes and any nesting
    /// depth alike. Nothing is re-anchored on the way out: a fragment error points at the EXPRESSION
    /// (where CPython carets inside an f-string), and two fragments can never share a
    /// witness/keyword/carrier table key, because two distinct source chars are two distinct
    /// positions by construction. Always returns `Ty::Str`.
    pub(super) fn check_interpolation(&mut self, raw: &crate::ast::StrLit, span: Span) -> Ty {
        match crate::interpolation::parse_interpolation(raw, span) {
            Ok(chunks) => self.check_interp_chunks(&chunks, span),
            Err(e) => {
                // `e.span`, not `span`: a fragment's lex error carries the offending char's real
                // position, and that is the one an editor squiggles. Errors about the literal as a
                // whole set `e.span == span` anyway, so this is a strict improvement (M24-7).
                self.error(e.span, e.message);
                Ty::Str
            }
        }
    }

    /// Check an already-parsed interpolation's chunks — the desugared [`ExprKind::Interp`] path, and
    /// the body of [`Self::check_interpolation`]'s fallback. Always returns `Ty::Str`.
    pub(super) fn check_interp_chunks(&mut self, chunks: &[crate::ast::Chunk], span: Span) -> Ty {
        // A value+keyword call inside a `{…}` fragment is keyed by (string span, fragment ordinal).
        // That pair used to be what kept two fragments whose first named-arg value shared a
        // fragment-relative column off one table slot; since M24-6 a fragment's spans are real
        // physical positions, so the pair is belt-and-braces (see `WitnessKey`'s doc).
        // Save/restore for nested interpolations. The compiler keeps the identical pair.
        let saved_ctx = self.kw_frag_ctx;
        let saved_ord = self.kw_frag_ord;
        let mut ord = 0usize;
        for chunk in chunks {
            if let crate::ast::Chunk::Expr(e, spec, fields) = chunk {
                self.kw_frag_ctx = span;
                self.kw_frag_ord = ord;
                // No re-anchoring: a fragment is re-lexed with the literal's absolute line AND
                // column, so its own span is a real source position and a fragment error points at
                // the EXPRESSION, exactly where CPython points inside an f-string (measured on
                // 3.14.6: `print(f"hello {f'inner {nope} x'} world")` carets `nope` itself, not the
                // literal). Three anchors lived here across this milestone — one on a cloned root,
                // one beside the AST — and each was a workaround for the column being fake; with a
                // real column there is nothing left to anchor, and the checker finally agrees with
                // the compiler, which never re-anchored.
                let ty = self.infer_value(e);
                // The spec's nested width/precision fields evaluate after the value, width first
                // (the order `compile_interp` emits them). Each must be an `int`; `Unknown` keeps
                // the runtime backstop (`fmtspec::field_from_int`).
                if let Some(fs) = spec {
                    let mut slots = [(fs.dyn_width, "width"), (fs.dyn_precision, "precision")]
                        .into_iter()
                        .filter(|(dynamic, _)| *dynamic)
                        .map(|(_, what)| what);
                    for field in fields {
                        let fty = self.infer_value(field);
                        if let Some(what) = slots.next()
                            && !matches!(fty, Ty::Int | Ty::Unknown)
                        {
                            self.error(
                                field.span,
                                format!(
                                    "format spec: a nested {what} field must be an int, found {fty}"
                                ),
                            );
                        }
                    }
                }
                // Static format-spec/value-type check: a CONCRETE static type is checked at COMPILE
                // time (same wording the runtime backstop would emit — single-sourced in
                // `fmtspec`); only `Unknown`, a generic `Param(T)` and a protocol existential keep
                // the runtime backstop (see `format_spec_kind`).
                if let Some(fs) = spec
                    && let Some((kind, text_form)) = self.format_spec_kind(&ty)
                    && let Err(msg) = crate::fmtspec::spec_valid_for_scalar(fs, kind)
                {
                    // TICKET-124 (W13-18) / TICKET-142 (W14-19): a value that renders as its text
                    // form goes through the runtime's `FmtArg::Other` → `render_str` path — the same
                    // string-format rules a scalar `Str` follows — so a spec that fails those rules
                    // is provably wrong here too.
                    if text_form {
                        self.error(span, format!("{msg} ({ty} is formatted as its text form)"));
                    } else {
                        self.error(span, msg);
                    }
                }
                ord += 1;
            }
        }
        self.kw_frag_ctx = saved_ctx;
        self.kw_frag_ord = saved_ord;
        Ty::Str
    }

    /// The [`crate::fmtspec::ScalarKind`] a format spec on a value of static type `ty` is checked
    /// against, plus whether the value renders as its text form (for the diagnostic). `None` keeps
    /// the runtime backstop: `Unknown`, a generic `Param(T)`, a protocol existential, a module.
    fn format_spec_kind(&self, ty: &Ty) -> Option<(crate::fmtspec::ScalarKind, bool)> {
        use crate::fmtspec::ScalarKind;
        match scalar_kind_of(ty) {
            Some(kind) => Some((kind, false)),
            None if renders_as_text(ty) => Some((ScalarKind::Str, true)),
            None => None,
        }
    }

    /// Infer an expression that is used in **value position** (assignment RHS, a call/collection
    /// argument, a binary/unary operand, an index/range bound, …). `nil` is a return-only / void
    /// type, never a writable value: a void call's result must not silently propagate into a binding
    /// or another expression. So if the expr is exactly `Ty::Nil`, report it and degrade to `Unknown`
    /// (suppressing the cascade). A bare void call AS A STATEMENT keeps using plain `infer` (legal),
    /// as does a fn/closure RETURN expr (returning nil just makes a void fn — not "using nil").
    pub(super) fn infer_value(&mut self, expr: &Expr) -> Ty {
        let ty = self.infer(expr);
        if let Ty::Module(m) = &ty {
            let m = m.clone();
            self.error(
                expr.span,
                format!("module '{m}' is not a value — a module is only a qualifier, write '{m}.<member>'"),
            );
            return Ty::Unknown;
        }
        if ty == Ty::Nil {
            self.error(
                expr.span,
                "expression returns no value (None) and cannot be used as a value".to_string(),
            );
            return Ty::Unknown;
        }
        ty
    }

    /// TICKET-227 — install `hint` as the expected type of `e`. With `slot`, `e` OWNS it: only
    /// `infer` of `e` may wrap `e` into the hint's carrier. Without, the hint is a seed (it guides
    /// inference and never wraps). A synthesized node never owns a hint.
    pub(super) fn install_hint(&mut self, e: &Expr, hint: Ty, slot: bool) {
        self.expected_hint = Some(hint);
        self.hint_owner = (slot && e.id.0 != crate::ast::NodeId::SYNTH.0).then_some(e.id);
    }

    /// TICKET-227 — infer `e` in value position as the value of a typed slot `slot`: `e` owns the
    /// slot's type and wraps into it where [`Self::meet_slot`] says so.
    pub(super) fn infer_value_in(&mut self, e: &Expr, slot: &Ty) -> Ty {
        self.install_hint(e, slot.clone(), true);
        let t = self.infer_value(e);
        self.expected_hint = None;
        self.hint_owner = None;
        t
    }

    pub(super) fn infer(&mut self, expr: &Expr) -> Ty {
        // TICKET-225: a var bound since a type was stored reads as its binding.
        if self.tyvars.borrow().any()
            && let Some(h) = &self.expected_hint
        {
            self.expected_hint = Some(self.zonk(h));
        }
        // TICKET-227: the slot this node owns, if the hint was installed FOR it. Taken here, so
        // no child ever sees the owner.
        let owner = self.hint_owner.take();
        let slot = match owner {
            Some(o) if o.0 == expr.id.0 => self.expected_hint.clone(),
            _ => None,
        };
        self.hint_owned = slot.is_some();
        let ty = self.infer_kind(expr);
        let ty = if self.tyvars.borrow().any() {
            self.zonk(&ty)
        } else {
            ty
        };
        // EDITOR HOVER probe: record this expr's type if its leaf/field anchor is the cursor token.
        // No-op (one `Option` check) unless a probe is armed. Children infer before parents and only
        // LEAF kinds record, so a parent expression never overwrites the smaller symbol's type.
        if self.hover_probe.is_some() {
            self.hover_record_expr(expr, &ty);
        }
        match &slot {
            Some(s) => self.meet_slot(s, expr, ty),
            None => ty,
        }
    }

    /// TICKET-227 (D3) — THE one place an implicit wrap is decided: the value `value` of type `ty`
    /// meets the typed slot `slot` it owns. A plain `T` at a `T?`/`T!E` slot records a wrap and
    /// takes the slot's type; anything else keeps its own type, and the slot's `assignable` compare
    /// reports a misfit. Called only by [`Self::infer`].
    fn meet_slot(&mut self, slot: &Ty, value: &Expr, ty: Ty) -> Ty {
        match self.wrap_mode(slot, &ty) {
            Some(w) => {
                self.record_wrap(value.id, w, value.span);
                slot.clone()
            }
            None => ty,
        }
    }

    /// TICKET-142 (W14-33): the dispatch every expression inference passes through. Wraps
    /// [`Self::infer_kind_inner`] with the constant-overflow check: at the root of each maximal
    /// arithmetic (`Binary`/`Unary`) tree, run ONE `consteval::eval` over the whole tree and report
    /// each overflow once. A child of a `Binary`/`Unary` sees `arith_parent` and skips (its parent's
    /// scan already entered it); a child of any other node (a call argument under a `+`) starts its
    /// own tree. Each node is scanned at most once, so the check is linear even on a
    /// `MAX_AST_DEPTH` chain. Must not touch `hint_owned` (the inner fn takes it first).
    /// A walk with `inferring_ret` set rolls back its diagnostics and `const_overflow_seen`, so it
    /// skips the scan and the real walk reports each overflow once (TICKET-183).
    pub(super) fn infer_kind(&mut self, expr: &Expr) -> Ty {
        let covered = self.arith_parent;
        let is_arith = matches!(expr.kind, ExprKind::Unary { .. } | ExprKind::Binary { .. });
        if is_arith
            && !covered
            && !self.inferring_ret
            && let consteval::Fold::Overflow(sp, op) =
                consteval::eval(expr, &mut self.const_scan_visits)
            && self.const_overflow_seen.insert(sp)
        {
            self.error(
                sp,
                format!(
                    "integer overflow in {op}: this constant expression does not fit in int (i64)"
                ),
            );
        }
        if !covered {
            self.const_meets_slot(expr, None);
        }
        self.arith_parent = is_arith;
        let ty = self.infer_kind_inner(expr);
        self.arith_parent = covered;
        // TICKET-218: a C width is a SLOT tag; a value read through any expression is its scalar
        // (owner rules 1 and 3: an `int8` value is an `int`, arithmetic on it yields `int`).
        match ty {
            Ty::Width(_) => ty.scalar().clone(),
            t => t,
        }
    }

    /// TICKET-225 (R5, Go's untyped constants) — the one place a constant meets its slot. A literal
    /// or a constant expression (`1 << 8`, `3e38 + 3e38`) is checked against the C width of the
    /// expected type it is inferred under: `constant 256 does not fit int8 (-128..127)`. `slot` is
    /// `None` from `infer_kind`, which reads `expected_hint`; an assignment passes its target type,
    /// which no hint carries (`infer` reads an lvalue as its scalar). Skipped while inferring a
    /// return (DEC-183) or in the generic-arg prepass; one report per span.
    pub(super) fn const_meets_slot(&mut self, expr: &Expr, slot: Option<&Ty>) {
        use crate::ast::consteval::{Const, Fold};
        if self.inferring_ret
            || self.generic_arg_prepass
            || !matches!(
                expr.kind,
                ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
            )
        {
            return;
        }
        let Some(w) = slot
            .or(self.expected_hint.as_ref())
            .and_then(|h| h.width())
            .cloned()
        else {
            return;
        };
        let float = w == crate::native::cffi::CType::Float32;
        let shown = match consteval::eval(expr, &mut self.const_scan_visits) {
            Fold::Value(Const::Float(f)) if float && !w.fits_f64(f) => format!("{f:e}"),
            Fold::Value(Const::Int(k)) if !float && !w.fits_int(k, true) => k.to_string(),
            _ => return,
        };
        if self.const_overflow_seen.insert(expr.span) {
            let name = w.width_name().unwrap_or("?");
            let range = w.range_text();
            self.error(
                expr.span,
                format!("constant {shown} does not fit {name} {range}"),
            );
        }
    }

    /// TICKET-225 / TICKET-227: infer `e` as the value of the typed slot `slot` (a constant meets
    /// the slot's width; a plain value wraps into a carrier slot); `None` infers with no hint. For
    /// value sites with a statement-tail slot (yield, inline body, closure body).
    pub(super) fn infer_in_slot(&mut self, e: &Expr, slot: Option<Ty>) -> Ty {
        match slot {
            Some(s) => {
                self.install_hint(e, s, true);
                let t = self.infer(e);
                self.expected_hint = None;
                self.hint_owner = None;
                t
            }
            None => self.infer(e),
        }
    }

    fn infer_kind_inner(&mut self, expr: &Expr) -> Ty {
        let owned = std::mem::take(&mut self.hint_owned);
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(raw) => self.check_interpolation(raw, expr.span),
            // The desugared form: fragments are real children, already normalized (named/default/
            // variadic args). `Str` above is only the brace-free or malformed remainder.
            ExprKind::Interp(chunks) => self.check_interp_chunks(chunks, expr.span),
            ExprKind::RawStr(_) => Ty::Str, // verbatim `str`, no interpolation to check
            ExprKind::Bytes(_) => Ty::Bytes,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Pass => Ty::Nil,
            ExprKind::Ident(name) => self.infer_ident(expr, name, expr.span),
            ExprKind::List(items, _) => {
                // Consume any expected-type hint (a `List[E]` slot: an annotated `let`, a call
                // arg, a return position — or the synthesized variadic list). `take()` so the
                // hint drives THIS literal's element type and never leaks into a nested element
                // call. `None` keeps the ordinary bottom-up inference.
                let hint = self
                    .expected_hint
                    .take()
                    .map(|t| Self::sink_payload(&t).clone());
                self.infer_list(items, hint.as_ref())
            }
            ExprKind::Tuple(items) => {
                // TICKET-124 (W13-13): consume any expected-type hint (a `(Box[Named], int)` slot),
                // same `take()`-then-project contract as `List`/`Map` above, so a nested ctor
                // literal inside the tuple sees its own element's hint instead of the bare
                // bottom-up type.
                let hint = self.expected_hint.take();
                let tys = match hint {
                    Some(Ty::Tuple(hs)) if hs.len() == items.len() => items
                        .iter()
                        .zip(&hs)
                        .map(|(e, h)| {
                            if ty_concrete_but(h, &|n| self.rigid_param(n, &[])) {
                                self.infer_arg(e, Some(h))
                            } else {
                                self.infer_value(e)
                            }
                        })
                        .collect(),
                    _ => items.iter().map(|e| self.infer_value(e)).collect(),
                };
                Ty::Tuple(tys)
            }
            // Same `take()`-then-project contract as the `List` arm above. Without the `take()` the
            // OUTER `Map[str, List[int]]` stayed in the slot and was consumed — wasted — by the first
            // entry's own inference.
            ExprKind::Map(entries) => {
                let hint = self
                    .expected_hint
                    .take()
                    .map(|t| Self::sink_payload(&t).clone());
                self.infer_map(entries, hint.as_ref())
            }
            ExprKind::Set(elems) => {
                let hint = self
                    .expected_hint
                    .take()
                    .map(|t| Self::sink_payload(&t).clone());
                self.infer_set(elems, hint.as_ref())
            }
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.infer_comprehension(*kind, key.as_deref(), elem, clauses),
            ExprKind::Unary { op, expr: inner } => self.infer_unary(expr, *op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs),
            ExprKind::Compare { operands, ops } => self.infer_compare_chain(operands, ops),
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => self.infer_slice(
                obj,
                start.as_deref(),
                end.as_deref(),
                step.as_deref(),
                expr.span,
            ),
            // A range has NO runtime value in any engine: the compiler lowers `a..b` only as a
            // `for`/comprehension iterable (a counting loop) or a slice receiver (materialize +
            // slice), and rejects it everywhere else. This arm is reached from every VALUE position
            // (assign RHS, call arg, collection element, binary operand, method receiver, index
            // object, return, generic bound arg, interpolation, pipe) — so typing it as `List[int]`
            // laundered a whole class of programs that check clean and then FAIL TO COMPILE at run
            // time. Reject here instead; the sanctioned positions never reach `infer_kind`:
            // `for_bindings` (sig.rs) matches `ExprKind::Range` syntactically for BOTH iterable
            // forms, `infer_slice` special-cases a range receiver, and `case a..b:` is a
            // `Pattern::Range` (a different AST node). Keeps the checker's accepted set a subset of
            // what the compiler can lower (see the backstop in compiler/mod.rs).
            ExprKind::Range { start, end } => {
                self.expect_int(start, "range bound");
                self.expect_int(end, "range bound");
                self.error(expr.span, RANGE_NOT_A_VALUE);
                // `Unknown` (not `list[int]`) so the rejection doesn't cascade into a second,
                // misleading diagnostic that names a type the range never had.
                Ty::Unknown
            }
            ExprKind::Call {
                callee,
                args,
                named,
                type_args,
                bracket,
            } => self.infer_call(
                callee,
                args,
                named,
                type_args,
                bracket.as_deref(),
                expr.span,
                expr.id,
            ),
            ExprKind::Field {
                obj,
                name,
                name_span,
            } => self.infer_field(expr, obj, name, *name_span),
            ExprKind::Index { obj, index, types } => {
                self.infer_index(expr, obj, index.as_deref(), types)
            }
            ExprKind::Try(inner) => self.infer_try(inner, expr.span),
            // W7-43 — optional-chaining `?.` / null-coalescing `??` are CARRIER nodes: the checker
            // types the operand, picks the lowering, then clone-lowers and infers the clone. The
            // choice needs the operand's TYPE, which is why desugar no longer lowers them; the
            // picked mode is recorded in the `CarrierTable` so the type-blind compiler agrees.
            ExprKind::OptChain { obj, name_span, .. } => {
                self.infer_opt_chain(expr, obj, *name_span, expr.span)
            }
            ExprKind::NullCoalesce { lhs, op_span, .. } => {
                self.infer_null_coalesce(expr, lhs, *op_span)
            }
            ExprKind::Closure { params, ret, body } => {
                // No expected type at the generic `infer` seam — free-closure inference (sources
                // #2/#3) and the ambiguity check happen inside `infer_closure`.
                self.infer_closure(params, ret.as_ref(), body, None)
            }
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms, owned),
            ExprKind::IfElse { cond, then, els } => self.infer_if_else(cond, then, els, owned),
            ExprKind::Recover(block) => self.infer_recover(block),
        }
    }

    /// Record `(ty, kind)` as the hover result if `span` is the armed probe position (entry module,
    /// first hit wins). The single place a probe hit is committed; both the expr-leaf path and the
    /// let-binding path funnel through here. A no-op when no probe is armed.
    pub(super) fn hover_record_at(
        &mut self,
        span: Span,
        ty: &Ty,
        kind: HoverKind,
        doc: Option<String>,
    ) {
        let Some((pl, pc)) = self.hover_probe else {
            return;
        };
        if self.hover_result.is_some() || self.current_module_id != self.hover_entry {
            return;
        }
        if span.line == pl && span.col == pc {
            self.hover_result = Some((ty.clone(), kind, doc));
        }
    }

    /// PART B — like [`Self::hover_record_at`], but for an occurrence of a NAMED binding. When the
    /// probe lands on an occurrence whose recorded type still carries an `Unknown`-in-slot (a
    /// not-yet-refined empty collection), DON'T lock `hover_result` to that provisional type; instead
    /// stash the binding's `(name, kind, doc)` in `hover_pending`. The end-of-scope finalize then looks
    /// up the binding's FINAL (refined) type and writes it to `hover_result`, so an earlier occurrence
    /// of `b` (its `b := []` decl or any use before the refining `b.push(0)`) shows `List[int]`, not
    /// `List[Unknown]`. A concrete (fully-known) type records immediately like `hover_record_at`.
    /// Probe-gated; entirely inert off the hover probe → behavior-neutral.
    pub(super) fn hover_record_binding(
        &mut self,
        span: Span,
        ty: &Ty,
        name: &str,
        kind: HoverKind,
        doc: Option<String>,
    ) {
        let Some((pl, pc)) = self.hover_probe else {
            return;
        };
        if self.hover_result.is_some() || self.current_module_id != self.hover_entry {
            return;
        }
        if span.line == pl && span.col == pc {
            if contains_unknown_in_slot(ty) {
                // defer: the binding may be refined later; resolve to its final type at the end-of-scope
                // seam that OWNS it. Record that owning scope (reverse walk, like `repin`/`drop_empty_site`)
                // so an intervening inner fn/method `check_fn_body` seam doesn't finalize it prematurely
                // (correctness-0). Fall back to the innermost scope if the binding isn't declared yet
                // (a decl-site hover recorded before `declare`) — at top level that is the module scope.
                let owning = self
                    .owning_scope(name)
                    .unwrap_or(self.scopes.len().saturating_sub(1));
                self.hover_pending = Some((owning, name.to_string(), kind, doc));
            } else {
                self.hover_result = Some((ty.clone(), kind, doc));
            }
        }
    }

    /// PART B — at end-of-scope (fn body / module, BEFORE `pop_scope`), if the probe deferred onto an
    /// unrefined-empty binding (`hover_pending` set) and no concrete hover landed elsewhere, resolve
    /// the binding's FINAL (now-refined) type from its owning scope and commit it to `hover_result`.
    /// A no-op off the probe (`hover_pending` stays `None`).
    pub(super) fn finalize_hover_pending(&mut self) {
        if self.hover_result.is_some() {
            return; // a concrete hover already landed elsewhere
        }
        // Only resolve at the seam that OWNS the pending binding (the scope about to be popped). A
        // pending binding owned by an ENCLOSING scope (`owning < idx`) is still refinable after this
        // pop — leave it for that scope's own finalize, else an intervening inner fn/method seam would
        // lock it to the still-unrefined `List[Unknown]` (correctness-0). Mirrors `finalize_empty_coll_sites`.
        let idx = self.scopes.len().saturating_sub(1);
        let owns_here = matches!(&self.hover_pending, Some((owning, ..)) if *owning >= idx);
        if owns_here && let Some((_owning, name, kind, doc)) = self.hover_pending.take() {
            let ty = self.lookup(&name).unwrap_or(Ty::Unknown);
            self.hover_result = Some((ty, kind, doc));
        }
    }

    /// Editor hover for a `from M import T` user type (struct/enum). Computes the effective
    /// doc — the type's own decl docstring carried across the module boundary, else a `kind (from
    /// module)` fallback — then (1) seeds `name_docs[bind]` so a later bare (`x: T`) / generic-head
    /// (`x: T[..]`) annotation use surfaces the same doc (those arms read `name_docs`), and (2) records
    /// the import-line token hover at `name_span`. Both halves are probe-gated no-ops off the hover
    /// probe (`name_docs` is editor-tooling-only and entry-module-scoped), so this is behavior-neutral.
    pub(super) fn record_imported_type_hover(
        &mut self,
        bind: &str,
        name_span: Span,
        ty: &Ty,
        own_doc: Option<&str>,
        kind_word: &str,
        path: &[String],
    ) {
        if self.hover_probe.is_none() {
            return;
        }
        let doc = own_doc
            .map(str::to_string)
            .unwrap_or_else(|| format!("{kind_word} (from {})", path.join(".")));
        self.name_docs.insert(bind.to_string(), doc.clone());
        self.hover_record_at(name_span, ty, HoverKind::Type, Some(doc));
    }

    /// Editor hover for a per-name import of a native/reserved TYPE (`import Shared from
    /// std.concurrency`, `import Socket from std.net`, `import ptr from std.ffi`, …). These branches
    /// license the name via the per-module sets and short-circuit BEFORE the user-struct import arm
    /// that records a hover, so the import-line token would otherwise show nothing. Records that
    /// token hover with the type's `builtin_type_doc` blurb (else a `(from <module>)` fallback) and
    /// its resolved native `Ty` for display. Probe-gated no-op off the hover probe; unlike a user
    /// `.chz` type NO `name_docs` seeding is needed — the bare/annotation use already resolves its
    /// doc through `builtin_type_doc` in the `Type::Named`/`Type::Generic` hover arms.
    pub(super) fn record_native_type_import_hover(
        &mut self,
        member: &str,
        name_span: Span,
        path: &[String],
    ) {
        if self.hover_probe.is_none() {
            return;
        }
        let ty = self
            .qualified_builtin_ty(member, &[])
            .unwrap_or(Ty::Unknown);
        let doc = builtin_type_doc(member)
            .unwrap_or_else(|| format!("{member} (from {})", path.join(".")));
        self.hover_record_at(name_span, &ty, HoverKind::Type, Some(doc));
    }

    /// The DISPLAY-only signature for a reserved callable builtin, for editor hover + value-position
    /// typing. The eight MIGRATED universe builtins (`ord`/`chr`/`panic`/`int`/`float`/`str`/`bytes`/
    /// `bytearray`) source their sig from `std/prelude.chz` via [`Checker::native_prelude_sigs`]; the
    /// still-synthetic `print` + container/runtime ctors fall through to [`builtin_container_sig`].
    /// Covers exactly [`RESERVED_CALLABLE`] (drift-guarded). Not used for direct-call typing (the
    /// `infer_named_call` arms handle that) — only hover + the first-class value form.
    pub(super) fn builtin_sig(&self, name: &str) -> Option<FnSig> {
        if let Some(sig) = self.native_prelude_sigs.get(name) {
            return Some(sig.clone());
        }
        builtin_container_sig(name)
    }

    /// Hover-record a LEAF expression (identifier / literal) or a field-name access. Non-leaf kinds
    /// (Binary/Index/Call/…) are skipped so hovering `a` in `a[0]` reports `a`'s type, not the element
    /// type — the parent never overwrites the child. The field-name access anchors on `name_span` (the
    /// field-name token), not the receiver-start `expr.span`, so `a.b.c` resolves the hovered segment.
    pub(super) fn hover_record_expr(&mut self, expr: &Expr, ty: &Ty) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            // An interpolated literal hovers as one `str` literal, like its un-desugared `Str` form
            // (its fragments record their own hovers when inferred).
            | ExprKind::Interp(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bytes(_)
            | ExprKind::Bool(_) => self.hover_record_at(expr.span, ty, HoverKind::Literal, None),
            ExprKind::Ident(name) => {
                // doc source mirrors the resolution: a `let`-bound local/global → `name_docs`; a free
                // fn → its `FnSig::doc`; a bare type/ctor name used as a value → `name_docs`. All keyed
                // by simple name, entry-module-scoped (safe — hover only fires in the entry module).
                if self.lookup(name).is_some() {
                    // Only a TRUE module-top-level binding (resolves at scope 0) owns its `name_docs`
                    // entry; a shadowing param/local of the same name has no doc of its own and must
                    // NOT borrow the global's (`name_docs` is keyed by bare name).
                    let at_top_level = self.owning_scope(name) == Some(0);
                    let doc = if at_top_level {
                        self.name_docs.get(name).cloned()
                    } else {
                        None
                    };
                    // PART B: a use of a binding whose recorded type is still an unrefined empty
                    // collection defers to the binding's final (refined) type via `hover_record_binding`.
                    self.hover_record_binding(expr.span, ty, name, HoverKind::Local, doc);
                } else if let Some(sig) = self.functions.get(name) {
                    self.hover_record_at(expr.span, ty, HoverKind::Func, sig.doc.clone());
                } else {
                    let doc = self.name_docs.get(name).cloned();
                    self.hover_record_at(expr.span, ty, HoverKind::Other, doc);
                }
            }
            ExprKind::Field { name_span, .. } => {
                self.hover_record_at(*name_span, ty, HoverKind::Field, None);
            }
            _ => {}
        }
    }

    /// Build a DISPLAY-only `Ty::Func` for a by-name call callee (free fn, struct constructor, or a
    /// reserved builtin via [`builtin_sig`]), for editor hover. `None` only for bare enum variants —
    /// they carry no recordable signature, so hover stays `None`. Pure read of the fn/struct/builtin
    /// tables: emits no error and changes no checking decision (it is only ever called under the hover
    /// probe). The free-fn branch displays a generic fn's declared signature verbatim (`FnSig`
    /// params/ret stay `Ty::Param(T)` → "fn(T, T) -> T"); the struct branch mirrors `name_is_generic`'s
    /// module-keyed `bare_key` lookup and renders fields → `Struct` ("fn(int, int) -> Vec2"); the
    /// builtin branch returns a canonical display sig ("fn(int) -> List[int]" for `range`).
    pub(super) fn callee_display_ty(&self, name: &str) -> Option<Ty> {
        if let Some(sig) = self.functions.get(name) {
            return Some(Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
                labels: crate::checker::FnLabels::default(),
            });
        }
        // A RESERVED builtin container/handle (`List`/`Map`/`Set`/`Channel`/`Shared`/…) now ALSO has a
        // `self.structs` entry — for its harvested METHOD table — but it is NOT a nominal struct: its
        // ctor callee must display the FLAT `builtin_container_sig` shape (`fn(?) -> List[?]`), NOT a
        // struct-ctor sig synthesized from the (empty) field list. Skip the struct branch for these so
        // they fall through to `builtin_sig` below (mirrors `resolve_type`'s reserved-type guard).
        if builtin_container_sig(name).is_none()
            && let Some(info) = self.structs.get(&self.bare_key(name))
        {
            let params: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
            let targs: Vec<Ty> = info
                .type_params
                .iter()
                .map(|tp| Ty::Param(tp.name.clone()))
                .collect();
            return Some(Ty::Func {
                params,
                ret: Box::new(Ty::Struct(name.to_string(), targs)),
                labels: FnLabels::default(),
            });
        }
        // A free / constructor builtin (`print`/`range`/`List`/`Channel`/…): a DISPLAY-only signature
        // from `builtin_sig` (the inference arms aren't a single queryable sig). Reserved names can't
        // be user-shadowed, so this never collides with the fn/struct tables above.
        if let Some(sig) = self.builtin_sig(name) {
            return Some(Ty::Func {
                params: sig.params,
                ret: Box::new(sig.ret),
                labels: crate::checker::FnLabels::default(),
            });
        }
        None
    }

    /// `recover: <block>` yields `Result[T, Error]` where `T` is the type of the block's trailing
    /// expression (or `nil`). Non-final statements are checked for their effects.
    pub(super) fn infer_recover(&mut self, block: &Block) -> Ty {
        // A `recover:` block is a value, not a control-flow target: `return`/`break`/`continue` that
        // would escape it are rejected. `?` is fine — it propagates normally.
        if let Some((span, kw)) = self.block_flow(block).first_escape() {
            self.error(
                span,
                format!("'{kw}' is not allowed inside a recover block"),
            );
        }
        self.push_scope();
        self.recover_depth += 1;
        let mut value_ty = Ty::Nil;
        if let Some((last, init)) = block.split_last() {
            for stmt in init {
                self.check_stmt(stmt);
            }
            match &last.kind {
                StmtKind::Expr(e) => value_ty = self.infer(e),
                // A trailing statement-form `match` whose every arm produces a value is the block's
                // value expression (docs/syntax.md): its unified arm type becomes the `Result[T]` T.
                // The `crate::ast` predicate is the SAME one the compiler uses to decide whether to
                // push the arm value vs `Op::Nil`, so the two stages can never drift. A non-total /
                // non-value-arm `match` falls to the `_` arm below (checked for effects, tail stays
                // `nil`) exactly as before.
                StmtKind::Match { scrutinee, arms } if crate::ast::match_tail_is_value(arms) => {
                    value_ty = self.infer_recover_tail_match(scrutinee, arms);
                }
                // A trailing statement-form `if/else` whose every branch (and the `else`) produces a
                // value behaves identically — the unified branch type becomes T.
                StmtKind::If {
                    branches,
                    else_block,
                } if crate::ast::if_tail_is_value(branches, else_block) => {
                    value_ty = self.infer_recover_tail_if(branches, else_block);
                }
                _ => self.check_stmt(last),
            }
            // A `recover:` whose tail provably diverges (a statement-form `match` whose every arm
            // `panic`s, `while true:`, all-branch-returning `if/else`, a trailing `exit`/`panic`)
            // yields no normal value, so its `Ok` payload is bottom (`Unknown`), not `nil` — exactly
            // like the direct `recover: panic(...)` form, which `infer`s `panic` to `Unknown` via the
            // `Expr` arm above. Without this, a diverging *statement* tail leaves `value_ty = Nil` and
            // its `Ok(v)` is wrongly nil-banned in value position. Guarding on `== Ty::Nil` keeps every
            // concrete-tail recover (`recover: 5` -> `int`) and non-diverging statement tail
            // (`recover: x := 5` -> `Result[nil]`) untouched. Reuses the sound, conservative
            // `flow::stmt` summary (divergence read from the resolved callee).
            if value_ty == Ty::Nil && !flow::stmt(last, &|e| self.call_diverges(e)).falls_through {
                value_ty = Ty::Unknown;
            }
        }
        self.recover_depth -= 1;
        self.pop_scope();
        Ty::result(value_ty)
    }

    /// Fold one recover-TAIL arm/branch type into the running block value type WITHOUT erroring on a
    /// mismatch — the crucial difference from `unify_branch`. A statement-form `match`/`if` whose every
    /// arm merely *ends in* an `Expr` (the syntactic `match_tail_is_value` predicate, shared with the
    /// compiler) can still have genuinely heterogeneous arm types: a void `print(...)` arm (`nil`) mixed
    /// with an `int` arm, or `str` vs `int`. Such a tail has no single value type, so — per the feature's
    /// design contract ("do not force a value where there isn't one") — it FALLS BACK to `Result[nil]`,
    /// value dropped, exactly as before this feature, instead of being rejected. `acc == None` means "not
    /// uniform yet decided"; once this returns `None` the caller latches non-uniform and types the block
    /// `nil`. `Unknown` arms (a `panic`) never break uniformity (they were already skipped by
    /// `unify_branch`), so `[100, panic(...)]` still types `Result[int]`.
    ///
    /// SOUNDNESS (why the compiler needs no matching gate): the compiler always compiles the tail as a
    /// VALUE (pushing each arm's real value → `Ok(<real value>)` at runtime), but when this returns
    /// non-uniform the block is typed `Result[nil]`, and the nil-in-value-position ban makes a
    /// `nil`-typed `Ok(v)` binding UNUSABLE in every value context (interpolation, list literal, call
    /// arg, arithmetic). So the heterogeneous runtime payload can never be observed — observationally
    /// identical to the pre-feature `Result[nil]` value-drop, with no checker/runtime divergence.
    fn fold_recover_tail(&self, acc: Option<Ty>, t: Ty) -> Option<Ty> {
        match acc {
            None => Some(t),
            Some(prev) => {
                if self.join_ty(&prev, &t) {
                    Some(if prev.is_unknown() { t } else { prev })
                } else {
                    None
                }
            }
        }
    }

    /// A statement-form `match` in `recover:` TAIL position, used as the block's value expression.
    /// Structurally IS `check_match` (same `bind_match_arm` / guard `expect_bool` / exhaustiveness),
    /// but each arm body is split into init statements (checked for effects) + a trailing value
    /// expression, and the trailing types are folded via `fold_recover_tail` into the block's `T`. Only
    /// reached when [`crate::ast::match_tail_is_value`] holds (every arm body ends in an `Expr`), so
    /// `split_last` always yields an `Expr` tail. Uses the statement-form PERSISTENT refine-on-first-
    /// use (no snapshot/restore) exactly like `check_match`; refinement is checker-only (no engine
    /// effect). Scoped to the recover tail — `match` typing elsewhere is untouched. Genuinely
    /// heterogeneous arms fall back to `Result[nil]` (see `fold_recover_tail`) rather than erroring.
    fn infer_recover_tail_match(&mut self, scrutinee: &Expr, arms: &[crate::ast::MatchArm]) -> Ty {
        let pats: Vec<&Pattern> = arms.iter().map(|a| &a.pattern).collect();
        let kind = self.match_kind(scrutinee, &pats);
        let mut covered = std::collections::HashSet::new();
        let mut has_wildcard = false;
        let mut exh = self.exh_new(&kind);
        let mut arm_pattern_error = false;
        let mut result: Option<Ty> = None;
        let mut uniform = true;
        for arm in arms {
            self.warn_unreachable_arm(
                has_wildcard,
                arm.body.first().map_or(scrutinee.span, |s| s.span),
            );
            let pat_mark = self.errors.len();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                arm.span,
                &mut covered,
                arm.guard.is_some(),
            );
            arm_pattern_error |= self.errors.len() > pat_mark;
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            has_wildcard |= Self::bool_domain_closed(&kind, &covered);
            has_wildcard |= self.exh_add(&mut exh, &arm.pattern, arm.guard.is_some());
            // `match_tail_is_value` guarantees a non-empty body with a trailing `Expr`.
            let (last, init) = arm
                .body
                .split_last()
                .expect("match_tail_is_value guarantees a non-empty arm body");
            for stmt in init {
                self.check_stmt(stmt);
            }
            // Always infer every arm's trailing expr (surfaces intra-arm errors); only the CROSS-arm
            // fold is gated on `uniform` so heterogeneous arms fall back to nil instead of erroring.
            let t = match &last.kind {
                StmtKind::Expr(e) => self.infer(e),
                _ => {
                    self.check_stmt(last);
                    Ty::Nil
                }
            };
            self.pop_scope();
            if uniform {
                match self.fold_recover_tail(result.take(), t) {
                    Some(u) => result = Some(u),
                    None => uniform = false,
                }
            }
        }
        let help = self.exh_help(&exh);
        if !arm_pattern_error {
            self.check_exhaustive(&kind, &covered, has_wildcard, help, scrutinee.span);
        }
        if uniform {
            result.unwrap_or(Ty::Nil)
        } else {
            Ty::Nil
        }
    }

    /// A statement-form `if/else` in `recover:` TAIL position, used as the block's value expression.
    /// Mirrors the statement-`If` checker (`sig.rs`) + `check_block`'s per-branch push/pop PERSISTENT
    /// refine, but each branch body (and the `else`) is split into init statements + a trailing value
    /// expression whose types fold via `fold_recover_tail` into `T`. Only reached when
    /// [`crate::ast::if_tail_is_value`] holds (has an `else` and every branch/else body ends in an
    /// `Expr`). Scoped to the recover tail — `if` typing elsewhere is untouched. Genuinely
    /// heterogeneous branches fall back to `Result[nil]` (see `fold_recover_tail`) rather than erroring.
    fn infer_recover_tail_if(
        &mut self,
        branches: &[(Expr, Block)],
        else_block: &Option<Block>,
    ) -> Ty {
        let mut result: Option<Ty> = None;
        let mut uniform = true;
        for (cond, body) in branches {
            self.expect_bool(cond, "if condition");
            let (t, _span) = self.infer_recover_tail_block(body);
            if uniform {
                match self.fold_recover_tail(result.take(), t) {
                    Some(u) => result = Some(u),
                    None => uniform = false,
                }
            }
        }
        // `if_tail_is_value` guarantees `else_block.is_some()`.
        if let Some(body) = else_block {
            let (t, _span) = self.infer_recover_tail_block(body);
            if uniform {
                match self.fold_recover_tail(result.take(), t) {
                    Some(u) => result = Some(u),
                    None => uniform = false,
                }
            }
        }
        if uniform {
            result.unwrap_or(Ty::Nil)
        } else {
            Ty::Nil
        }
    }

    /// Check a statement block used in recover TAIL position and return its trailing value type +
    /// the trailing expression's span (for `unify_branch` diagnostics). Mirrors `check_block`'s
    /// push/pop PERSISTENT refine; init statements are checked for effects, the trailing `Expr` is
    /// the value (`nil` if the block does not end in one — the caller's predicate rules that out).
    fn infer_recover_tail_block(&mut self, block: &Block) -> (Ty, Span) {
        self.push_scope();
        let out = if let Some((last, init)) = block.split_last() {
            for stmt in init {
                self.check_stmt(stmt);
            }
            match &last.kind {
                StmtKind::Expr(e) => (self.infer(e), last.span),
                _ => {
                    self.check_stmt(last);
                    (Ty::Nil, last.span)
                }
            }
        } else {
            (Ty::Nil, Span::default())
        };
        self.pop_scope();
        out
    }

    /// M24 — PERMANENT WALL, not a v1 limit: a generic fn that takes hidden witness arguments
    /// (`wparams`, from [`FnSig::witness_params`]) may not become a function VALUE. A `Ty::Func`
    /// erases which declaration it came from, so no witness can ever be recovered at the eventual
    /// indirect call — the value would be called one argument short. Rejected at the READ, where the
    /// name is still known. Every path that can hand back a `Ty::Func` for a named fn routes through
    /// here: the bare read (`g := reset`), the turbofish read (`reset[Counter]`, incl. as a HOF
    /// argument), and a cross-module member read (`lib.reset`). Returns `true` if it rejected.
    pub(super) fn reject_witness_fn_value(
        &mut self,
        name: &str,
        wparams: &[String],
        span: Span,
    ) -> bool {
        if wparams.is_empty() {
            return false;
        }
        self.error(
            span,
            format!(
                "'{name}' cannot be used as a function value: its bound on {} requires a static \
                 protocol method, which needs the concrete type — a function value erases it. \
                 Call '{name}' directly, or pass a factory closure instead (e.g. \
                 `fn make[T](mk: fn() -> T) -> T`)",
                wparams.join(", ")
            ),
        );
        true
    }

    /// W7-42r shape (b) — reject a VALUE read of an imported name that sits ABOVE that name's own
    /// `import`. Imports are HOISTED (`check_module` runs `bind_import` for every import before the
    /// `check_stmt` loop), so the name is in `scopes[0]` from line 1 whatever line the `import` is
    /// on, and a read above it silently resolves to the imported binding — which a later
    /// module-scope `let` then refills, handing a closure typed against the import a value of the
    /// let's type (`f := fn() -> str: x` / `x := 1` / `import COUNT as x from lib.st` printed `1`,
    /// check-clean). The W7-42 re-declaration rule cannot cover it: its import gate is deliberately
    /// one-way (source-EARLIER import only), and inverting it would reject the sound
    /// `module_scope_redeclare_over_hoisted_import_ok`. The forward READ is the error instead.
    ///
    /// Deliberately also rejects programs that are TECHNICALLY SOUND today (`print(COUNT)` above
    /// `import COUNT from lib` works, because of the hoist): it reads as a use-before-definition,
    /// and both owning ancestors refuse it — CPython raises `NameError`, Go will not even parse an
    /// `import` after a declaration. Kept narrow on purpose: VALUE/CALLABLE reads only (a bare type
    /// name resolves through `bare_types`/`resolve_type`, never here), and NOT Go's full "imports
    /// before all code" rule, which would be a grammar change.
    ///
    /// A DEFERRED read — a top-level `fn` body above the `import` — is rejected too, and there the
    /// ancestors SPLIT (measured): CPython accepts it, because the body runs after the import; Go
    /// still refuses, because it will not take a late `import` at all. We follow Go, because the
    /// hoist makes the sound and the unsound case indistinguishable AT THE READ SITE: the same
    /// `COUNT` in the same position is fine until some later `let` refills the slot, and the reader
    /// cannot see which it is. Do not loosen this to "only immediate reads".
    ///
    /// "Still the import's binding" is the exact test the W7-42 rule uses (`sig.rs`): a module-scope
    /// `declare` clears `imported_values` (`setup.rs:1774`), so once a `let` has handed the name
    /// back to this module the read is that let's, not the import's. For a from-imported FN the
    /// caller passes `is_import = true` and the `import_binds` lookup below is the whole gate — a
    /// same-module top-level `fn` is never in `import_binds`, and must not be: it is legitimately
    /// position-independent (`compiler/mod.rs:1404`, `desugar/mod.rs:689`). "Above its import" is
    /// the same directional `import_binds` span comparison, in the opposite direction — total for
    /// SHADOWING is the CALLER's job, and each caller already carries it: the value arm passes
    /// `is_import` only when no scope above 0 holds the name (see `infer_ident`), while both
    /// `functions`-arm callers are reached only after `lookup(name)` came back `None` — a
    /// parameter/local/loop/block binding of that name would have resolved there first, so a
    /// from-imported fn shadowed by an inner binding never reaches this gate at all.
    /// the same two reasons (imports are top-level only; no statement separator, so no positional
    /// tie). Writes need no counterpart: `check_assign` already rejects an assignment to a
    /// from-imported global at ANY position ("cannot assign to 'x' imported from module …"), and a
    /// whole-module bind is a `Ty::Module` that no value is assignable to.
    pub(super) fn reject_read_above_import(&mut self, name: &str, is_import: bool, span: Span) {
        if !is_import {
            return;
        }
        let Some(imp) = self.import_binds.get(name).copied() else {
            return;
        };
        if (span.line, span.col) < (imp.line, imp.col) {
            self.error(
                span,
                format!(
                    "'{name}' is used before its `import` on line {} (imports are hoisted, so this \
                     reads the imported binding — move the `import` above this line)",
                    imp.line
                ),
            );
        }
    }

    /// TICKET-187 — the one pin-or-reject rule for a GENERIC fn read as a value, shared by every
    /// read: same-module and from-imported bare names (Scope A in `infer_ident`) and qualified
    /// `m.f` (the `Ty::Module` field read). `Some(ty)` when the rule decided (pinned, or rejected
    /// as `Ty::Unknown`); `None` to fall through to the rigid `fn(T) -> T`, whose assignability
    /// diagnostic is the accurate one there. A rigid callee `T` must never reach a caller's scope,
    /// because `Ty::Param` compares by name (G1).
    pub(super) fn generic_fn_value_ty(
        &mut self,
        node: crate::ast::NodeId,
        name: &str,
        sig: &FnSig,
        spelling: &str,
        span: Span,
    ) -> Option<Ty> {
        if sig.type_params.is_empty() {
            return None;
        }
        let type_params = sig.type_params.clone();
        let declared = fn_value_ty(sig);
        // A hint of `Unknown` DETERMINES NOTHING and must count as no hint at all, or the rule
        // cancels itself on an INFERRED return type (`fn get(): return id`): the
        // return-inference pass reads the body, takes the `Ty::Unknown` this arm returns as the
        // inferred return, and the real pass re-checks the same `return id` against a
        // `Some(Unknown)` hint — turning a reject into a silent ACCEPT (measured: `g := get();
        // g(1)` printed `1`, check-clean).
        let hint = match &self.expected_hint {
            Some(Ty::Unknown) | None => None,
            Some(h) => Some(h.clone()),
        };
        let verdict = match &hint {
            Some(h) => {
                pin_generic_fn_value(&type_params, &declared, h, &|n| self.rigid_param(n, &[]))
            }
            // No hint at all: nothing in this position can determine anything.
            None => FnValuePin::Undetermined,
        };
        match verdict {
            FnValuePin::Pinned(map, refined) => {
                self.enforce_bounds(&type_params, &type_params, &map, span);
                // A method value's receiver `where` bound; empty for a fn.
                self.enforce_bounds(&sig.where_bounds, &type_params, &map, span);
                return Some(refined);
            }
            // …the value can never be formed. Go refuses exactly this spelling, at the READ:
            // `cannot use generic function id without instantiation` (and, in argument
            // position, `in call to takeBool, cannot infer T`). Chezzi used to accept it and
            // blame the eventual call ("argument 1 of 'closure': expected T, found int" — a
            // `closure` the user never wrote, naming a `T` there is no way to act on), or
            // accept it silently when the value was never called. The
            // witness wall above wins first — its advice differs (a turbofish does not help).
            // …gated by the hint's own parameter positions (see `fn_slot_params_concrete`):
            // a hint that is not concrete there cannot answer the question, so the rigid
            // arm's assignability diagnostic owns it. No hint at all still reports (`g := id`).
            //
            // TICKET-225 (R5, amends DEC-197): the read is no longer the final word. It takes one
            // type variable per param, and any later use in its frame pins them; the frame verdict
            // (`close_tyvar_frame`) reports a read still unpinned, with this same message.
            FnValuePin::Undetermined
                if hint
                    .as_ref()
                    .is_none_or(|h| fn_slot_params_concrete(h, &|n| self.rigid_param(n, &[]))) =>
            {
                return Some(self.defer_generic_fn_value(node, name, sig, spelling, span, false));
            }
            // Not this rule's business (see [`FnValuePin::Skip`]) — fall through to the rigid
            // `fn(T) -> T` arm and let the existing assignability diagnostic, which is the
            // accurate one there, speak.
            _ => {}
        }
        None
    }

    /// TICKET-187 — is `e` a read of a GENERIC fn-like path as a value, and which one. Returns the
    /// display name, the sig with any head args (written or alias-pinned) substituted, and the
    /// instantiation hint. A view over [`Self::path_fn`] (TICKET-204): `Bx[int].put` and `B.put`
    /// answer with only `put`'s own `U` free, exactly as `Bx.put` is re-pinned.
    pub(super) fn generic_fn_value_sig(&self, e: &Expr) -> Option<(String, FnSig, String)> {
        self.path_fn(e)
            .and_then(|pf| self.pin_path_head(pf))
            .filter(|pf| !pf.sig.type_params.is_empty())
            .map(|pf| (pf.display, pf.sig, pf.spelling))
    }

    /// The path value `T.m` for an in-scope type param `T`: a bound's INSTANCE method `m`, as a fn
    /// taking the receiver first (Rust's `T::m`). `None` for a static requirement (call-only,
    /// through `infer_witness_static_call`) and a miss. Protocol methods take no own type params,
    /// so the value is never generic.
    pub(super) fn param_member_fn(&self, tname: &str, name: &str) -> Option<PathFn> {
        let (bound, msig, map) = self.bound_method(tname, name, &|s| !s.is_static)?;
        let mut inst = subst_sig(&msig, &map);
        inst.ret = self.bound_method_ret(&bound, name, &msig, &map);
        let sig = method_value_sig(&inst, &[], Ty::Param(tname.to_string()));
        let arity = sig.params.len();
        Some(PathFn {
            display: format!("{tname}.{name}"),
            head_spelled: tname.to_string(),
            spelling: format!("{tname}.{name}"),
            sig,
            head_decl: Vec::new(),
            head_params: 0,
            head_args: None,
            head_pinned: None,
            res: Some(Resolution::ParamMethodFn {
                method: name.to_string(),
                arity,
            }),
        })
    }

    /// The `m.f` half of [`Self::path_fn`]: `m` is a whole-module import here and `f` is one of its
    /// fns.
    pub(super) fn module_fn(&self, m: &str, name: &str) -> Option<(String, FnSig)> {
        if !matches!(self.head_binding(m), HeadBinding::Module) {
            return None;
        }
        if self.json_decode_member(m, name) {
            return Some((format!("{m}.{name}"), json_decode_sig()));
        }
        let msig = self.module_sigs.get(self.imported_modules.get(m)?)?;
        let sig = msig.certain_fn(name)?;
        Some((format!("{m}.{name}"), sig.clone()))
    }

    /// [`Self::module_fn`] when `f` is GENERIC.
    fn generic_module_fn(&self, m: &str, name: &str) -> Option<(String, FnSig)> {
        self.module_fn(m, name)
            .filter(|(_, sig)| !sig.type_params.is_empty())
    }

    /// THE one answer to "is this path a fn-like item read as a value, with which signature"
    /// (TICKET-204, Rust's path-value rule): a same-module or from-imported fn no local shadows,
    /// `m.f` on a whole-module import, or a type member — a static method, an instance method named
    /// through its type (receiver first), or a payload variant. Generic or not; the caller filters.
    pub(super) fn path_fn(&self, e: &Expr) -> Option<PathFn> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if !matches!(
                    self.head_binding(name),
                    HeadBinding::Unbound | HeadBinding::Global
                ) || self.lookup(name).is_some()
                {
                    return None;
                }
                if let Some(sig) = self
                    .functions
                    .get(name)
                    .filter(|_| self.slot_holds_fn_decl(name))
                {
                    return Some(PathFn::of_fn(name.clone(), sig.clone()));
                }
                // A bare imported variant constructor, through the head its import stored.
                let iv = self.imported_variants.get(name)?;
                self.type_member_fn(&iv.head, None, &iv.variant)
            }
            ExprKind::Field { obj, name, .. } => {
                // A type parameter shadows a module or type of that name here, as in a call.
                if let ExprKind::Ident(t) = &obj.kind
                    && self.shadowing_type_param(t)
                {
                    return self.param_member_fn(t, name);
                }
                if let ExprKind::Ident(m) = &obj.kind
                    && let Some((display, sig)) = self.module_fn(m, name)
                {
                    return Some(PathFn::of_fn(display, sig));
                }
                let (th, head_args) = self.peel_type_path(obj)?;
                self.type_member_fn(&th, head_args, name)
            }
            _ => None,
        }
    }

    /// The type head of a member path's receiver `obj`, with its written type arguments: `Bx`,
    /// `Bx[int]`, `vlib.R2[int, str]`, or an alias `B`. An alias given type arguments (`A[int]`)
    /// is still a type path; its readers reject it through [`Self::written_head_args`].
    pub(super) fn peel_type_path(&self, obj: &Expr) -> Option<(TypeHead, Option<WrittenTypeArgs>)> {
        let (head, args) = match crate::ast::type_application(obj) {
            Some(app) => (app.head, Some((app.args, app.args_span))),
            None => (obj, None),
        };
        let th = self.type_head(head)?;
        Some((th, args))
    }

    /// The fn-like member `name` of type head `th`: a payload variant (a constructor fn) or a
    /// method (an instance method takes its receiver first, keyword `self`). `None` for a nullary
    /// variant, a protocol, a miss, and a native handle, whose methods have no proto.
    pub(super) fn type_member_fn(
        &self,
        th: &TypeHead,
        head_args: Option<WrittenTypeArgs>,
        name: &str,
    ) -> Option<PathFn> {
        if th.native_handle {
            return None;
        }
        let key = &th.key;
        let params_of = |tps: &[TyParam]| {
            tps.iter()
                .map(|tp| Ty::Param(tp.name.clone()))
                .collect::<Vec<_>>()
        };
        let method = |tps: Vec<TyParam>, msig: &FnSig, recv: Ty| {
            let res = Resolution::MethodFn {
                type_key: key.clone(),
                method: name.to_string(),
            };
            (method_value_sig(msig, &tps, recv), tps, res)
        };
        let (mut sig, head_decl, res) = match th.kind {
            TypeHeadKind::Enum => {
                let tps = self.enum_type_params.get(key).cloned().unwrap_or_default();
                let recv = Ty::enum_ty(key.clone(), params_of(&tps));
                if let Some(v) = self.variants.get(&(key.clone(), name.to_string())) {
                    if v.payload.is_empty() {
                        return None;
                    }
                    let res = Resolution::VariantFn {
                        enum_key: key.clone(),
                        variant: name.to_string(),
                        arity: v.payload.len(),
                    };
                    let mut sig = FnSig::plain(v.payload.clone(), recv);
                    sig.type_params = tps.clone();
                    (sig, tps, res)
                } else {
                    method(tps, self.enum_methods.get(key)?.get(name)?, recv)
                }
            }
            TypeHeadKind::Struct => {
                let info = self.struct_shape(key)?;
                let tps = info.type_params.clone();
                let recv = Ty::Struct(key.clone(), params_of(&tps));
                method(tps, info.methods.get(name)?, recv)
            }
            TypeHeadKind::Protocol => return None,
        };
        // A head param the instantiated sig no longer names (`Box[int].make2`) leaves the sig but
        // stays in `head_decl`, so written head args still arity-check against the declaration.
        let mut occurring = Vec::new();
        for t in sig.params.iter().chain(std::iter::once(&sig.ret)) {
            ty_collect_params(t, None, &mut occurring);
        }
        let own = sig.type_params.split_off(head_decl.len());
        let kept: Vec<TyParam> = head_decl
            .iter()
            .filter(|tp| occurring.contains(&tp.name))
            .cloned()
            .collect();
        let head_params = kept.len();
        sig.type_params = kept.iter().cloned().chain(own.iter().cloned()).collect();
        let head_text = match &head_args {
            Some((args, _)) => format!(
                "{}[{}]",
                th.spelled,
                args.iter()
                    .map(|t| self.resolve_ty_ro(t).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => th.spelled.clone(),
        };
        let display = format!("{head_text}.{name}");
        let hint_head = if head_args.is_none() && th.pinned.is_none() {
            fn_spelling(&th.spelled, &kept)
        } else {
            head_text
        };
        Some(PathFn {
            display,
            head_spelled: th.spelled.clone(),
            sig,
            head_decl,
            head_params,
            head_args,
            head_pinned: th.pinned.clone(),
            res: Some(res),
            spelling: fn_spelling(&format!("{hint_head}.{name}"), &own),
        })
    }

    /// The one decision for a type head given type arguments, in every position: an alias
    /// (`pinned` is `Some`, even `Some([])` for a non-generic alias) given written args reports
    /// that it already fixes its arguments and answers `None`. Else the written args when there are
    /// any, else the alias's pinned args, else none. A new site calls this; it never tests `pinned`.
    pub(super) fn written_head_args(
        &mut self,
        spelled: &str,
        decl_params: usize,
        pinned: Option<Vec<Ty>>,
        written: Vec<Ty>,
        span: Span,
    ) -> Option<Vec<Ty>> {
        if head_args_clash(pinned.as_deref(), decl_params, !written.is_empty()) {
            self.error(
                span,
                format!(
                    "type alias '{spelled}' already fixes its type arguments; write the aliased type to pass your own"
                ),
            );
            return None;
        }
        if written.is_empty() {
            Some(pinned.unwrap_or_default())
        } else {
            Some(written)
        }
    }

    /// How many type params the struct or enum keyed `key` declares.
    pub(super) fn type_param_count(&self, key: &str) -> usize {
        self.type_params_of(key).map_or(0, |tps| tps.len())
    }

    /// The type params the struct or enum keyed `key` declares; `None` for no such type.
    pub(super) fn type_params_of(&self, key: &str) -> Option<Vec<TyParam>> {
        if let Some(info) = self.struct_shape(key) {
            return Some(info.type_params.clone());
        }
        self.enum_type_params.get(key).cloned()
    }

    /// The one place head args become a substitution for a `&self` reader: the alias-pinned args,
    /// else the written ones (resolved with `resolve_ty_ro`), else none (`pf` unchanged). `None` on
    /// an arity mismatch, which [`Self::path_fn_value_ty`] reports at the read.
    fn pin_path_head(&self, mut pf: PathFn) -> Option<PathFn> {
        if head_args_clash(
            pf.head_pinned.as_deref(),
            pf.head_decl.len(),
            pf.head_args.is_some(),
        ) {
            return None;
        }
        let args: Vec<Ty> = if let Some(p) = &pf.head_pinned {
            p.clone()
        } else if let Some((a, _)) = &pf.head_args {
            a.iter().map(|t| self.resolve_ty_ro(t)).collect()
        } else {
            return Some(pf);
        };
        if args.len() != pf.head_decl.len() {
            return None;
        }
        let map: HashMap<String, Ty> = pf
            .head_decl
            .iter()
            .map(|tp| tp.name.clone())
            .zip(args)
            .collect();
        let mut sig = subst_sig(&pf.sig, &map);
        sig.type_params = sig.type_params.split_off(pf.head_params);
        pf.spelling = fn_spelling(&pf.display, &sig.type_params);
        pf.sig = sig;
        pf.head_params = 0;
        pf.head_decl = Vec::new();
        pf.head_args = None;
        pf.head_pinned = None;
        Some(pf)
    }

    /// The value type of fn-like path `pf` read here, with its own written type args `own_args`
    /// (a turbofish), if any: the witness wall, the head and own arity checks, the bounds over the
    /// type's declared params followed by the item's own (DEC-202), then the substituted fn type —
    /// or, with params left free, the pin-or-reject rule of [`Self::generic_fn_value_ty`].
    pub(super) fn path_fn_value_ty(
        &mut self,
        node: crate::ast::NodeId,
        pf: PathFn,
        own_args: Option<WrittenTypeArgs>,
        span: Span,
    ) -> Ty {
        // M24 — the fn-as-value wall: pinning the type params does NOT recover the witness (the pin
        // is checker-only, the runtime value is the same erased function).
        if self.reject_witness_fn_value(&pf.display, &pf.sig.witness_params, span) {
            return Ty::Unknown;
        }
        let own = pf.sig.type_params[pf.head_params..].to_vec();
        let mut map = HashMap::new();
        let mut arity_ok = true;
        let written: Vec<Ty> = match &pf.head_args {
            Some((args, aspan)) => args.iter().map(|t| self.resolve_type(t, *aspan)).collect(),
            None => Vec::new(),
        };
        let Some(head) = self.written_head_args(
            &pf.head_spelled,
            pf.head_decl.len(),
            pf.head_pinned.clone(),
            written,
            span,
        ) else {
            return Ty::Unknown;
        };
        if pf.head_args.is_some() {
            arity_ok &= head.len() == pf.head_decl.len();
            map.extend(self.seed_targs(&pf.head_spelled, &pf.head_decl, &head, span));
        } else {
            for (tp, t) in pf.head_decl.iter().zip(head) {
                map.insert(tp.name.clone(), t);
            }
        }
        if let Some((args, aspan)) = &own_args {
            let resolved: Vec<Ty> = args.iter().map(|t| self.resolve_type(t, *aspan)).collect();
            arity_ok &= resolved.len() == own.len();
            // `seed_targs` emits the clean "'name' expects N type argument(s), found M".
            map.extend(self.seed_targs(&pf.display, &own, &resolved, span));
        }
        if !arity_ok {
            return Ty::Unknown;
        }
        let tps: Vec<TyParam> = pf.head_decl.iter().chain(own.iter()).cloned().collect();
        self.enforce_bounds(&tps, &tps, &map, span);
        // A conditional method's receiver `where` bound (`where T: Add`), as its call form enforces.
        self.enforce_bounds(&pf.sig.where_bounds, &tps, &map, span);
        let mut sig = subst_sig(&pf.sig, &map);
        sig.type_params.retain(|tp| !map.contains_key(&tp.name));
        if sig.type_params.is_empty() {
            return fn_value_ty(&sig);
        }
        let spelling = if map.is_empty() {
            pf.spelling.clone()
        } else {
            fn_spelling(&pf.display, &sig.type_params)
        };
        self.generic_fn_value_ty(node, &pf.display, &sig, &spelling, span)
            .unwrap_or_else(|| fn_value_ty(&sig))
    }

    /// THE one checker resolution of a type-applied fn value `head[T…]`, either carrier
    /// (`ast::type_application`): `pair[str, int]`, `lib.idt[int]`, `Bx[int].put[str]`. `None` when
    /// the head is no fn-like path with type params of its own.
    pub(super) fn infer_type_applied_fn_value(&mut self, e: &Expr) -> Option<Ty> {
        let app = crate::ast::type_application(e)?;
        let head = app.head;
        let written = Some((app.args.clone(), app.args_span));
        match self.resolve_path(head, PathPos::Value) {
            // A fn-like path: its own type params take the written arguments. A fn head resolves to
            // `Resolution::Fn`, the one fact the compiler erases on (DEC-197).
            Some(
                r @ (Resolution::Fn { .. }
                | Resolution::MethodFn { .. }
                | Resolution::VariantFn { .. }
                | Resolution::ParamMethodFn { .. }),
            ) => {
                let pf = self.path_fn(head)?;
                if pf.sig.type_params.len() == pf.head_params {
                    return None;
                }
                // The compiler loads a module fn's head as a value (`Compiler::resolution` (5)).
                if let Resolution::Fn { .. } = r
                    && let ExprKind::Field { obj: m, .. } = &head.kind
                {
                    self.infer(m);
                }
                Some(self.path_fn_value_ty(e.id, pf, written, head.span))
            }
            // TICKET-214: std.json's decode, which the table leaves unnamed — `T` is written, so
            // the value always records its descriptor (or reports a target that does not decode).
            None => {
                let ExprKind::Field { obj: m, name, .. } = &head.kind else {
                    return None;
                };
                let ExprKind::Ident(mn) = &m.kind else {
                    return None;
                };
                if !self.json_decode_member(mn, name) {
                    return None;
                }
                self.infer(m);
                let pf = self.path_fn(head)?;
                let ty = self.path_fn_value_ty(e.id, pf, written, head.span);
                Some(self.record_decode_value(head.id, ty, head.span))
            }
            _ => None,
        }
    }

    /// The members a type path names but never yields, reported at `name_span`: a protocol
    /// method and a native handle's method. Read by the value read ([`Self::type_member_value`])
    /// and the call read (`infer_static_call`), so both refuse the same paths. `true` when it
    /// reported.
    pub(super) fn type_member_refusal(
        &mut self,
        th: &TypeHead,
        name: &str,
        name_span: Span,
    ) -> bool {
        let spelled = th.spelled.clone();
        if th.kind == TypeHeadKind::Protocol {
            let has = self
                .protocols
                .get(&th.key)
                .is_some_and(|p| p.methods.iter().any(|(m, _)| m == name));
            if !has {
                return false;
            }
            self.error(
                name_span,
                format!(
                    "'{name}' is a method of protocol '{spelled}' -- a protocol method is not a \
                      value: name it through a concrete type (`<Type>.{name}`, which takes the \
                      receiver first) or wrap it (`fn(x): x.{name}()`)"
                ),
            );
            return true;
        }
        if th.native_handle {
            if !self
                .structs
                .get(&th.key)
                .is_some_and(|info| info.methods.contains_key(name))
            {
                return false;
            }
            self.error(
                name_span,
                format!(
                    "'{name}' is a method of the native type '{spelled}' -- \
                     a native method is not a value: call it on a value (`x.{name}(…)`) or wrap \
                     it in a closure (`fn(x): x.{name}()`)"
                ),
            );
            return true;
        }
        false
    }

    /// A type path `Head.name` / `Head[T…].name` read as a value (TICKET-204): a nullary variant, a
    /// fn-like member through [`Self::type_member_fn`], or one of the refusals (a protocol method,
    /// a native method, a missing variant). `None` when `obj` is no type head or `name` is no member
    /// this decides; the caller falls through to the ordinary field path.
    fn type_member_value(
        &mut self,
        obj: &Expr,
        name: &str,
        name_span: Span,
        res: Option<&Resolution>,
    ) -> Option<Ty> {
        let (th, head_args) = self.peel_type_path(obj)?;
        let spelled = th.spelled.clone();
        if self.type_member_refusal(&th, name, name_span) {
            return Some(Ty::Unknown);
        }
        let key = th.key.clone();
        match res {
            Some(Resolution::Variant { .. }) => {
                // A nullary variant is a value of the enum: explicit head args resolve and
                // arity-check, an alias head pins its own, a bare head leaves them Unknown.
                let tps = self.enum_type_params.get(&key).cloned().unwrap_or_default();
                let written: Vec<Ty> = match &head_args {
                    Some((targs, _)) => targs
                        .iter()
                        .map(|t| self.resolve_type(t, obj.span))
                        .collect(),
                    None => Vec::new(),
                };
                let Some(head) = self.written_head_args(
                    &spelled,
                    self.type_param_count(&key),
                    th.pinned.clone(),
                    written,
                    obj.span,
                ) else {
                    return Some(Ty::Unknown);
                };
                let args = if head_args.is_some() {
                    self.seed_targs(&spelled, &tps, &head, obj.span);
                    head
                } else if head.len() == tps.len() {
                    head
                } else {
                    vec![Ty::Unknown; tps.len()]
                };
                Some(Ty::enum_ty(key, args))
            }
            Some(Resolution::MethodFn { .. } | Resolution::VariantFn { .. }) => {
                let pf = self.type_member_fn(&th, head_args, name)?;
                Some(self.path_fn_value_ty(obj.id, pf, None, name_span))
            }
            // A miss on an enum: no such variant. A struct's miss reads `obj` as a value below,
            // which reports the type.
            None if th.kind == TypeHeadKind::Enum && !th.native_handle => {
                // A declared enum is named bare, as the call path names it; an alias as written.
                let ename = if th.pinned.is_some() {
                    &spelled
                } else {
                    &th.name
                };
                let names = self.variant_names(&key);
                self.error_help(
                    name_span,
                    format!("enum '{ename}' has no variant '{name}'"),
                    suggest::did_you_mean(name, &names),
                );
                Some(Ty::Unknown)
            }
            _ => None,
        }
    }

    /// The one message for a type name read as a value, bare, imported, qualified or type-applied
    /// (TICKET-204). It decides nothing about the head; it formats from `th`.
    fn type_not_value(&mut self, th: &TypeHead, span: Span) {
        let spelled = &th.spelled;
        let msg = if th.native_handle {
            format!("'{spelled}' is a type, not a value")
        } else {
            match th.kind {
                TypeHeadKind::Struct => format!(
                    "'{spelled}' is a type, not a value — constructors are not values: call it \
                      (`{spelled}(…)`) or wrap it in a closure"
                ),
                TypeHeadKind::Enum => format!(
                    "'{spelled}' is a type, not a value — use one of its variants \
                      (`{spelled}.<Variant>`)"
                ),
                TypeHeadKind::Protocol => format!("'{spelled}' is a protocol, not a value"),
            }
        };
        self.error(span, msg);
    }

    /// THE ONE diagnostic for "this read of generic fn `name` cannot become a function value here",
    /// shared by both positions that can reach the verdict: the immediate read (`infer_ident`, whose
    /// expected-type hint either determines the params or does not) and the DEFERRED end-of-call
    /// check on a generic method's argument ([`Checker::close_tyvar_frame`]).
    /// One rule, one sentence — the whole point of the extension is that a binding and an argument
    /// stop giving one function two verdicts.
    pub(super) fn reject_undetermined_generic_fn_value(
        &mut self,
        name: &str,
        decl: &FnSig,
        spelling: &str,
        span: Span,
    ) {
        let type_params = &decl.type_params;
        // Render the wanted shape from the fn's OWN signature with each undetermined parameter shown
        // as a `<T>` placeholder, so the advice fits this declaration instead of a made-up one.
        // `fn_value_ty` so a fn with DEFAULTED params does not advertise a stricter arity than a
        // plain fn read gives (`fn rep[T](x: T, n: int = 2)` — a non-generic `g := rep; g(1)` works,
        // so the suggested type must permit it too).
        let holes: HashMap<String, Ty> = type_params
            .iter()
            .map(|tp| (tp.name.clone(), Ty::Param(format!("<{}>", tp.name))))
            .collect();
        let sig = subst(&fn_value_ty(decl), &holes);
        let names = type_params
            .iter()
            .map(|tp| tp.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let sig = sig.to_string();
        // TICKET-204: a turbofish takes every parameter at once (`pair[<A>, <B>]`), on any fn-like
        // path (`R1[<T>].L`, `Bx[<T>].put[<U>]`); `spelling` is the hint `path_fn` built.
        let turbofish = format!("instantiate it (`{spelling}`), or ");
        // Which parameters actually OCCUR in the signature? Only those can be reached by giving the
        // position a concrete function type. One that appears NOWHERE (`pred[T](n: int) -> bool`,
        // reachable from a HOF slot) renders no `<…>` hole at all, so the "write a real type in place
        // of each `<…>`" tail would point at nothing and the suggested `fn(int) -> bool` is the type
        // the position ALREADY has — advice the user has, by construction, already followed. Testing
        // the RENDERED text is the exact question: does the advice contain a hole for this parameter?
        let absent = type_params
            .iter()
            .map(|tp| tp.name.as_str())
            .filter(|n| !sig.contains(&format!("<{n}>")))
            .collect::<Vec<_>>();
        // WORDING IS LOAD-BEARING. Say what is true of THIS read — the parameters are not determined
        // HERE — never "nothing determines them", and never "annotate the binding". The rule also
        // fires in positions that DO carry a concrete type Chezzi simply does not thread into
        // `expected_hint` (a parameter/field DEFAULT value: `fn run(f: fn(int) -> int = id)`, whose
        // slot pins T=int by inspection), and in positions with no binding to annotate at all
        // (`print(id)`, a `yield`, a list/map element, a HOF argument). Both earlier phrasings were
        // then FACTUALLY FALSE, and the second told a user who had already written
        // `xs: List[fn(int) -> int] = [id]` to do the thing they had done. Naming the POSITION keeps
        // the sentence true everywhere and still points at the fix.
        let advice = if absent.is_empty() {
            format!(
                "{turbofish}give this position a concrete function type (`{sig}`), writing a real \
                 type in place of each `<…>`"
            )
        } else {
            // The genuinely-unused parameter. A type at this position can never reach it, so the only
            // honest remedies are the turbofish and deleting it.
            let plural = if absent.len() == 1 { "s" } else { "" };
            format!(
                "{} appear{plural} nowhere in its signature (`{sig}`), so no type at this position \
                 can reach {}: {turbofish}drop the unused type parameter{}",
                absent.join(", "),
                if absent.len() == 1 { "it" } else { "them" },
                if absent.len() == 1 { "" } else { "s" },
            )
        };
        self.error(
            span,
            format!(
                "'{name}' is generic and {names} {} not determined here, so it cannot become \
                 a function value — {advice}",
                if type_params.len() == 1 { "is" } else { "are" }
            ),
        );
    }

    pub(super) fn infer_ident(&mut self, e: &Expr, name: &str, span: Span) -> Ty {
        // BARE-VALUE position, and the same shadowing rule (`Checker::shadowing_type_param`): a type
        // parameter shadows a same-named FUNCTION or module GLOBAL for the whole body, so `g := foo`
        // and `LIM + 1` must not quietly read the outer one while `foo()` / `LIM.m()` resolve to the
        // parameter. FIRST, because `lookup` reaches module globals (scope 0) — an inner LOCAL still
        // wins, which is what `shadowing_type_param` excludes. Go, the one-namespace ancestor, is the
        // reference: reading a type parameter as a value is *"foo (type) is not an expression"*.
        // The rule table leaves exactly that name unnamed; every arm below reads its answer.
        let Some(res) = self.resolve_path(e, PathPos::Value) else {
            return self.type_param_shadow_error(
                name,
                "a type parameter is a type, not a value — it is erased at runtime, so there is nothing to read",
                span,
            );
        };
        // A binding a scope holds. What the name means does not depend on whether its type is
        // known here. (`Global` is also the answer for a name no scope binds: the diagnostics below.)
        if let Resolution::Local | Resolution::Global { .. } | Resolution::Module(_) = res
            && let Some(ty) = self.lookup(name)
        {
            // TICKET-183 — a body reads a module global declared below it through the type
            // `seed_module_globals` gave it. An `Unknown` in that type (an un-annotated empty
            // collection, a value of un-inferable type) is pinned by walk-order code this body cannot
            // see, so decline rather than guess: ask for an annotation.
            if self.in_fn_body
                && !self.inferring_ret
                && self.globals.get(name).is_some_and(|g| g.unreached())
                && self.owning_scope(name) == Some(0)
                && (ty.is_unknown() || contains_unknown_in_slot(&ty))
            {
                if self.globals.get(name).is_some_and(|g| g.cycle) {
                    return Ty::Unknown;
                }
                self.error(
                    span,
                    format!(
                        "'{name}' is declared below this function and its type is not known here ({ty}) -- annotate its declaration (`{name}: <type> = ...`)"
                    ),
                );
                return Ty::Unknown;
            }
            // …and only when the read actually RESOLVED to the module-global slot the import fills.
            // `imported_values`/`Ty::Module` are keyed by BARE NAME and say nothing about the scope
            // the read resolved in, but `lookup` walks innermost-first: a parameter, fn-local `:=`,
            // loop variable, or block-scope binding that shadows the name owns this read, so the
            // gate's own sentence ("this reads the imported binding") is false there and firing is a
            // FALSE REJECT (`fn circumference(pi: float, ...)` above `import pi from std.math`).
            // Any scope above 0 holding the name means it is not the import's binding.
            let shadowed = self.scopes.iter().skip(1).any(|s| s.contains_key(name));
            let is_import = !shadowed
                && (self.imported_values.contains_key(name) || matches!(ty, Ty::Module(_)));
            self.reject_read_above_import(name, is_import, span);
            // A function-local binding captured by an enclosing `spawn:` task crosses the airlock as
            // a copy; a *non-sendable* one (e.g. a captured closure that's then called) can't, so
            // reading it inside the task is an error — the read-side counterpart to the reassignment
            // gate. Module globals/imports are excluded (`is_local_capture`): they resolve in every
            // task like free functions, so reading an imported module here is fine.
            if self.is_local_capture(name) && !self.sendable(&ty) {
                self.error(
                    span,
                    format!(
                        "cannot use non-sendable captured binding '{name}' of type {ty} inside a \
                         spawned task (captures cross the airlock — communicate via a Channel or Shared)"
                    ),
                );
            }
            return ty;
        }
        // A module-level fn slot. A slot a later `:=` redeclares answers `Global` (the compiler
        // loads the slot) and is still typed by its fn declaration here.
        if let Resolution::Fn { .. } | Resolution::Global { .. } = res
            && let Some(sig) = self.functions.get(name)
        {
            let sig = sig.clone();
            let type_params = sig.type_params.clone();
            // M24 — the fn-as-value wall, at the BARE read (`g := reset`): both for the Scope-A pin
            // below and for the rigid fallback after it.
            let wparams = sig.witness_params.clone();
            // W7-42r: this expression's type is now fixed against the fn's signature, so a later
            // module-scope `name := …` would retype the ONE slot underneath it (see `fn_reads`).
            self.record_fn_read(name);
            // …and a FROM-IMPORTED fn read above its own `import` is the same use-before-import the
            // value arm rejects (`g := h` above `import h from lib.fns`). Leaving it accepted gave
            // two verdicts for one user-visible concept; both ancestors reject it too (CPython:
            // `NameError`; Go refuses the late `import`). `import_binds` is the whole gate — a
            // same-module top-level `fn` is not in it and stays position-independent.
            self.reject_read_above_import(name, true, span);
            if self.reject_witness_fn_value(name, &wparams, span) {
                return Ty::Unknown;
            }
            // Scope A — a GENERIC fn referenced in value position (a bare `Name`, NOT the callee of a
            // direct call) whose type params can be PINNED from an expected `fn(..) -> ..` hint (a
            // `let` annotation, a HOF param, or a return position — all delivered via the
            // `expected_hint` slot), and its refusal half: a hint that CANNOT determine them. Both
            // verdicts come from the one shared derivation, [`pin_generic_fn_value`], which the
            // deferred argument-position check asks too. Runtime is generic-ERASED — the value is just
            // the underlying function, so an indirect call already works; the pin is checker-only.
            //
            // SOUNDNESS: `unify` is first-binding-wins + a silent no-op on mismatch, so an
            // unsatisfiable hint (`g: fn(str) -> int = ident`) binds `T=str` (the param position wins)
            // and we return the CONCRETE `fn(str) -> str` — NEVER `expected` — leaving the existing
            // assignability / arg / return check to reject it against `fn(str) -> int`.
            if !type_params.is_empty()
                && let Some(ty) = self.generic_fn_value_ty(
                    e.id,
                    name,
                    &sig,
                    &fn_spelling(name, &type_params),
                    span,
                )
            {
                return ty;
            }
            // A user fn's value type carries its param NAMES as labels, so `g := greet` yields a
            // labelled function value and `g(name="Bob")` resolves through it — and its call slots,
            // so `f := g; f()` fills the defaults and packs a variadic like a direct call.
            return fn_value_ty(&sig);
        }
        // A first-class universe builtin fn used in value position (`f := ord`, HOF arg, bare
        // `defer print(...)`). Typed as the dedicated `Ty::BuiltinFn` from `builtin_sig` — a genuine
        // callable that is sendable (crosses the spawn airlock) yet, unlike `Ty::Unknown`, is rejected
        // by `expect_bool` (so `if print:` is a type error, not a VM/interp divergence). Only the four
        // first-class fns; type/ctor names fall through to the "unknown/not first-class" arms below
        // (uniform with `f := Point`).
        //
        // A body reaches here only for a name no scope holds: every module global is seeded into
        // scope 0 before any body (TICKET-183, TICKET-180). At top level a name whose `:=` is below
        // the read is the builtin, as in CPython; the compiler loads it from the checker's `Builtin`
        // record, never from the not-yet-initialized slot.
        // `print`'s VALUE form is a FIXED 1-arg function, NOT its variadic call signature: the
        // variadic + `sep=`/`end=` shapes need the specialized `CallPrint`/`CallPrintSep` opcodes,
        // which are unreachable through a bound value (`p := print`). So force the canonical 1-arg
        // `Ty::BuiltinFn` here rather than the harvested variadic sig from `builtin_sig` — this is the
        // design-sanctioned split (the call authority is the variadic prelude decl; the value form is
        // fixed).
        if let Resolution::Builtin(b) = &res
            && b == "print"
        {
            return Ty::BuiltinFn {
                params: vec![Ty::Unknown],
                ret: Box::new(Ty::Nil),
            };
        }
        if let Resolution::Builtin(b) = &res
            && is_firstclass_builtin_fn(b)
            && let Some(sig) = self.builtin_sig(name)
        {
            return Ty::BuiltinFn {
                params: sig.params,
                ret: Box::new(sig.ret),
            };
        }
        // A bare imported variant: the nullary one is its enum's value, a payload one is the
        // variant constructor as a fn value (DEC-225 pins or rejects its type parameters).
        if let Resolution::Variant { enum_key, .. } = &res {
            return self.enum_ty_unknown_args(enum_key);
        }
        if let Resolution::VariantFn { .. } = res
            && let Some(pf) = self.path_fn(e)
        {
            return self.path_fn_value_ty(e.id, pf, None, span);
        }
        // A type name read as a value (`f := Box`, TICKET-204).
        if let Some(th) = self.bare_type_head(name) {
            self.type_not_value(&th, span);
            return Ty::Unknown;
        }
        // A bare user-variant name used as a value (`Red`, `Leaf`) is no longer allowed — variants are
        // scoped under their enum and must be written qualified (`Color.Red`, `Tree.Leaf`).
        if self.variant_owners.contains_key(name) {
            let hint = self.qualify_hint(name);
            self.error(span, hint);
            return Ty::Unknown;
        }
        // A bare use of a name that is a type declared in some (un-imported) module — typically a
        // constructor like `Point(1)` whose module wasn't `from`-imported. Hint how to import it.
        // An IMPORTED scalar alias (`import Count from m; Count(3)`) is already imported, so the
        // hint would be false; it falls through to the plain "unknown name", like a local one.
        if self.types_by_name.contains_key(name) && !self.imported_alias_tys.contains_key(name) {
            self.error(span, self.unknown_type_msg(name));
            return Ty::Unknown;
        }
        // A bare name two un-aliased imports both bind (`import a.math` + `import b.math`).
        if let Some(msg) = self.ambiguous_bind_msg(name) {
            self.error(span, msg);
            return Ty::Unknown;
        }
        // The head of an imported dotted path used where no full path resolved (`pkg.other.X` after
        // `import pkg.deep`): the head `pkg` is a path PREFIX, not a bound name. Narrow: fires ONLY
        // for a literal import path head, never a genuine typo.
        if self.import_paths.iter().any(|(p, _, _)| p[0] == name) {
            self.error(
                span,
                format!(
                    "'{name}' is not a bound name — a full path `{name}.<module>.<Name>` needs `import {name}.<module>` in this file"
                ),
            );
            return Ty::Unknown;
        }
        // TICKET-142 (W14-32): `_` is the blank identifier — never declared, so it cannot be read
        // (Go: `cannot use _ as value or type`). A loop variable / parameter named `_` still binds
        // and resolves in the first arm above.
        if name == "_" {
            self.error(
                span,
                "cannot use '_' as a value — '_' is the blank identifier; `_ := e` and `_ = e` discard e",
            );
            return Ty::Unknown;
        }
        let names = self.in_scope_names();
        self.error_help(
            span,
            format!("unknown name '{name}'"),
            suggest::did_you_mean(name, &names),
        );
        Ty::Unknown
    }

    /// A bare collection literal at a `T?` / `T!E` sink coerces to `Some(v)` / `Ok(v)` (W8-21), so the
    /// literal's real expected type is the carrier's PAYLOAD, not the carrier. Without this unwrap the
    /// element hint stopped at the carrier and nothing reached the items — the same laundering the
    /// expected-type propagation exists to close, just one sink shape further out. Measured before:
    /// `fn mk() -> List[List[int]]?: return [empty_list(), ["x"]]` was check-clean at rc=0 and
    /// `xs[1][0] + 1` faulted at run time, while the identical body at a bare `-> List[List[int]]`
    /// was correctly rejected. It also un-breaks two FALSE REJECTIONS that predate the propagation:
    /// `fn opt() -> List[Shape]?: return [C(), S()]` was *list elements differ: C vs S* where the
    /// bare `-> List[Shape]` accepts it, and `xs: List[float]? = [1, 2]` was *cannot assign
    /// List[int] to variable of type List[float]?* where the bare annotation widens.
    ///
    /// Terminates: every step strictly descends into a smaller type.
    fn sink_payload(t: &Ty) -> &Ty {
        let mut cur = t;
        loop {
            match cur {
                Ty::Option(inner) | Ty::Result(inner, _) => cur = inner,
                _ => return cur,
            }
        }
    }

    pub(super) fn infer_list(&mut self, items: &[Expr], expected: Option<&Ty>) -> Ty {
        // EXPECTED-TYPE-DIRECTED path: when the slot type is a concrete `List[E]` (an annotated
        // `let xs: List[Any] = …`, a `List[E]` call arg — INCLUDING the synthesized variadic list
        // for `...xs: E` — or a `List[E]` return), drive `E` down onto each element instead of
        // unifying siblings bottom-up. A heterogeneous literal whose every element is assignable to
        // `E` then types as `List[E]`: the element-homogeneity rule is bypassed because the declared
        // element type already sanctions the mix. This is what makes the `Any` top type the honest
        // variadic element type — `fn f(...xs: Any)` called `f(1, "a", true)` (and the equivalent
        // `xs: List[Any] = [1, "a", true]`) collapse to a `List[Any]` and check clean, since every
        // value satisfies the empty `Any` protocol. Falls back to bottom-up inference when `E` is not
        // satisfied-by-all, preserving the existing "list elements differ" diagnostic for a
        // genuinely mistyped literal.
        // EXPECTED-TYPE PROPAGATION: drive the declared ELEMENT type onto each item, so a generic
        // call in element position is pinned by the slot it fills (`a: List[List[int]] =
        // [empty()]` binds `empty`'s `T` to `int`). The `take()` at the `ExprKind::List` arm stops
        // the OUTER `List[List[int]]` leaking into an element unchanged; handing each item its own
        // element type is the correct hop, not that leak, and `infer_if_else_chain` already
        // re-installs a hint per arm the same way.
        //
        // Not cosmetic: an `Unknown`-carrying element used to LAUNDER the whole literal. Measured
        // before, `a: List[List[int]] = [empty(), ["x"]]` was check-clean at rc=0 — the `all()` gate
        // below passed the `List[Unknown]` element, bottom-up then found `List[Unknown]` compatible
        // with `List[str]`, and the literal typed as `List[List[Unknown]]`, assignable to anything —
        // so `a[1][0] + 1` reached the runtime as *cannot apply Add to str and int*. The same
        // literal WITHOUT the generic call (`[["x"]]`) was correctly rejected.
        let elem_expected = match expected {
            Some(Ty::List(e)) if !e.is_unknown() => Some((**e).clone()),
            _ => None,
        };
        let tys: Vec<Ty> = items
            .iter()
            .map(|it| match &elem_expected {
                Some(e) => self.infer_value_in(it, e),
                None => self.infer_value(it),
            })
            .collect();

        // TICKET-032 A2 — CLOSED. This gate used to be all-or-nothing: one element not assignable to
        // `e` abandoned the expected path entirely and fell through to the bottom-up homogeneity
        // below, where an `Unknown`-CORED sibling laundered the whole literal (`compatible(List[
        // Unknown], List[str])` is true, so nothing fired and the literal typed as `List[List[
        // Unknown]]`, assignable to anything) — measured, `a: List[List[int]] = [empty().reversed(),
        // ["x"]]` then `a[1][0] + 1` was check-clean at rc=0 and faulted at run time with *cannot
        // apply Add to str and int*. Reporting per ELEMENT instead of falling through closes it: the
        // diagnostic moves from the assignment to the offending element (DEC-036 licenses the caret
        // move; DEC-007, which forbade it, is superseded). Returning the DECLARED element type
        // unconditionally is what suppresses the old assignment-level cascade.
        if let Some(Ty::List(e)) = expected
            && !e.is_unknown()
            && !items.is_empty()
        {
            for (t, item) in tys.iter().zip(items) {
                if !t.is_unknown() && !self.assignable(e, t) {
                    let [e_s, t_s] = Ty::render_distinct([e, t]);
                    let note = self.protocol_note(e, t);
                    self.error(
                        item.span,
                        format!(
                            "list element: expected {e_s}, found {t_s}{note}{}",
                            float_fix_note(e, t)
                        ),
                    );
                }
            }
            return Ty::list((**e).clone());
        }
        // Bottom-up homogeneity over the item types. A mixed literal is the ordinary heterogeneity
        // error — `[1, 2.5]` has no type context to adapt to, so it is rejected (D3: write `1.0`).
        let mut elem = Ty::Unknown;
        for (t, item) in tys.iter().zip(items) {
            if elem.is_unknown() {
                elem = t.clone();
            } else if !t.is_unknown() && !self.join_ty(&elem, t) {
                let [elem_s, t_s] = Ty::render_distinct([&elem, t]);
                self.error(
                    item.span,
                    format!(
                        "list elements differ: {elem_s} vs {t_s}{}",
                        float_fix_note_join(&elem, t)
                    ),
                );
            }
        }
        Ty::list(elem)
    }

    /// Infer the type of a map literal `{k: v, …}`. Keys must share one (hashable) type, values
    /// another; heterogeneity and non-hashable keys are errors. Empty `{}` → `map[?, ?]`.
    pub(super) fn infer_set(&mut self, elems: &[Expr], expected: Option<&Ty>) -> Ty {
        // Drive the declared ELEMENT type onto each item, exactly as `infer_list` does.
        let elem_expected = match expected {
            Some(Ty::Set(e)) if !e.is_unknown() => Some((**e).clone()),
            _ => None,
        };
        let mut elem = Ty::Unknown;
        for e in elems {
            let et = match &elem_expected {
                Some(x) => self.infer_value_in(e, x),
                None => self.infer_value(e),
            };
            if !et.is_unknown()
                && let Some(why) = self.key_ty_reject(&et)
                && self.pending_key_reject.as_deref() != Some(why.as_str())
            {
                self.error(e.span, format!("set element type {why}"));
            }
            match &elem_expected {
                // W12-7 (TICKET-106): mirror `infer_map`'s expected-type path — check each element
                // is ASSIGNABLE to the declared element type (so a protocol-satisfying struct literal
                // is accepted) instead of accumulating and comparing elements to each other.
                Some(x) => {
                    if !et.is_unknown() && !self.assignable(x, &et) {
                        let [x_s, et_s] = Ty::render_distinct([x, &et]);
                        self.error(e.span, format!("set element: expected {x_s}, found {et_s}"));
                    }
                }
                None => {
                    if elem.is_unknown() {
                        elem = et;
                    } else if !et.is_unknown() && !self.join_ty(&elem, &et) {
                        let [elem_s, et_s] = Ty::render_distinct([&elem, &et]);
                        self.error(e.span, format!("set elements differ: {elem_s} vs {et_s}"));
                    }
                }
            }
        }
        Ty::set(elem_expected.unwrap_or(elem))
    }

    pub(super) fn infer_map(&mut self, entries: &[(Expr, Expr)], expected: Option<&Ty>) -> Ty {
        // Drive the declared KEY and VALUE types onto each entry, exactly as `infer_list` does — and
        // for the same reason: an `Unknown`-carrying value laundered the whole literal. Measured
        // before, `m: Map[str, List[int]] = {"k": empty(), "j": ["x"]}` was check-clean at rc=0 and
        // `m["j"][0] + 1` reached the runtime as *cannot apply Add to str and int*.
        let (key_expected, val_expected) = match expected {
            Some(Ty::Map(k, v)) => (
                (!k.is_unknown()).then(|| (**k).clone()),
                (!v.is_unknown()).then(|| (**v).clone()),
            ),
            _ => (None, None),
        };
        // Infer keys+values in source order first, then run the homogeneity checks in that same order.
        let mut key_tys: Vec<Ty> = Vec::with_capacity(entries.len());
        let mut val_tys: Vec<Ty> = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            let kt = match &key_expected {
                Some(x) => self.infer_value_in(k, x),
                None => self.infer_value(k),
            };
            key_tys.push(kt);
            let vt = match &val_expected {
                Some(x) => self.infer_value_in(v, x),
                None => self.infer_value(v),
            };
            val_tys.push(vt);
        }

        // TICKET-032 A2 — CLOSED, the `infer_list` twin: an expected key/value type is reported per
        // ENTRY instead of falling through to bottom-up homogeneity, closing the same
        // `Unknown`-cored-sibling launder (`m: Map[str, List[int]] = {"k": empty().reversed(), "j":
        // ["x"]}` was check-clean pre-fix, then faulted at run time). `None` (no concrete expected
        // key/value) keeps the existing accumulate-then-`compatible` homogeneity check verbatim.
        if entries.is_empty() {
            return Ty::map(
                key_expected.unwrap_or(Ty::Unknown),
                val_expected.unwrap_or(Ty::Unknown),
            );
        }
        let mut key = Ty::Unknown;
        let mut value = Ty::Unknown;
        for (((k_expr, v_expr), kt), vt) in entries.iter().zip(&key_tys).zip(&val_tys) {
            let (kt, vt) = (kt.clone(), vt.clone());
            if !kt.is_unknown()
                && let Some(why) = self.key_ty_reject(&kt)
                && self.pending_key_reject.as_deref() != Some(why.as_str())
            {
                self.error(k_expr.span, format!("map key type {why}"));
            }
            match &key_expected {
                Some(ke) => {
                    if !kt.is_unknown() && !self.assignable(ke, &kt) {
                        let [ke_s, kt_s] = Ty::render_distinct([ke, &kt]);
                        self.error(
                            k_expr.span,
                            format!("map key: expected {ke_s}, found {kt_s}"),
                        );
                    }
                }
                None => {
                    if key.is_unknown() {
                        key = kt.clone();
                    } else if !kt.is_unknown() && !self.join_ty(&key, &kt) {
                        let [key_s, kt_s] = Ty::render_distinct([&key, &kt]);
                        self.error(k_expr.span, format!("map keys differ: {key_s} vs {kt_s}"));
                    }
                }
            }
            match &val_expected {
                Some(ve) => {
                    if !vt.is_unknown() && !self.assignable(ve, &vt) {
                        let [ve_s, vt_s] = Ty::render_distinct([ve, &vt]);
                        self.error(
                            v_expr.span,
                            format!(
                                "map value: expected {ve_s}, found {vt_s}{}",
                                float_fix_note(ve, &vt)
                            ),
                        );
                    }
                }
                None => {
                    if value.is_unknown() {
                        value = vt.clone();
                    } else if !vt.is_unknown() && !self.join_ty(&value, &vt) {
                        let [value_s, vt_s] = Ty::render_distinct([&value, &vt]);
                        self.error(
                            v_expr.span,
                            format!(
                                "map values differ: {value_s} vs {vt_s}{}",
                                float_fix_note_join(&value, &vt)
                            ),
                        );
                    }
                }
            }
        }
        Ty::map(key_expected.unwrap_or(key), val_expected.unwrap_or(value))
    }

    /// Infer a comprehension's type. Walks each `for` clause in order (first outermost): binds the
    /// clause's loop variable(s) to the iterand's element type(s) via `for_bindings` (the exact path
    /// a `for` loop uses, so every iterable behaves the same) — inferred in the scope of the earlier
    /// clauses so a later clause can reference an earlier binding — and checks each guard is `Bool`.
    /// Then it infers the element (and key) in the cumulative scope. The result mirrors
    /// `infer_list`/`infer_set`/`infer_map`, including the Hashable check on set elements and map keys.
    pub(super) fn infer_comprehension(
        &mut self,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        clauses: &[CompClause],
    ) -> Ty {
        // TICKET-032 A2 — a comprehension is a hint BARRIER: without taking `expected_hint` here, the
        // outer sink type reaches a clause ITERAND too, and `ys: List[int] = [y for xs in [[1, 2],
        // [3]] for y in xs]` false-rejects (the iterand then types as `List[int]` against a
        // `List[List[int]]` value). Measured on a scratch implementation of this fix: with the take,
        // that program is `ok: no type errors`.
        // TICKET-227: after every clause, the hint's ELEMENT payload (never the whole type,
        // DEC-032) is the element expression's slot, so a plain element wraps into a carrier.
        let outer_hint = self.expected_hint.take().filter(ty_fully_concrete);
        self.push_scope();
        for clause in clauses {
            // `for_bindings` infers the iter IN the current scope, so later clauses see earlier
            // bindings (the whole point of nesting). Compute before declaring this clause's vars.
            let bindings = self.for_bindings(&clause.vars, &clause.iter);
            // A comprehension materializes eagerly, but a `Channel` is a blocking iteration form whose
            // termination depends on `close()`. Draining it into a list/set/map is out of scope; reject
            // it here instead — the `for v in ch:` statement form is the way to drain a channel.
            // Checked per clause so a channel in ANY clause is rejected.
            // `for_bindings` above already handled a range clause SYNTACTICALLY (a comprehension
            // over a range is sanctioned); a range is never a Channel, so skip it here — `infer`
            // would otherwise re-visit it as a VALUE and reject `[i for i in 0..3]`.
            if !matches!(clause.iter.kind, ExprKind::Range { .. })
                && matches!(self.infer(&clause.iter), Ty::Channel(_))
            {
                self.error(
                    clause.iter.span,
                    "a channel cannot be drained in a comprehension; use the `for v in ch:` statement form",
                );
            }
            for (name, ty) in bindings {
                // Intentionally NOT `mark_loop_var`: a comprehension body is an expression, so its
                // binding can't be assigned to — no divergence to guard against. If a statement-bearing
                // comprehension is ever added, mark these too (see `check_assign` / for-loop handling).
                self.declare(&name, ty);
            }
            for g in &clause.guards {
                self.expect_bool(g, "comprehension guard");
            }
        }
        let elem_in = |s: &mut Self, e: &Expr, slot: Option<&Ty>| match slot {
            Some(t) => s.infer_value_in(e, t),
            None => s.infer_value(e),
        };
        let (key_slot, elem_slot) = match (&kind, &outer_hint) {
            (CompKind::List, Some(Ty::List(e))) | (CompKind::Set, Some(Ty::Set(e))) => {
                (None, Some((**e).clone()))
            }
            (CompKind::Map, Some(Ty::Map(k, v))) => (Some((**k).clone()), Some((**v).clone())),
            _ => (None, None),
        };
        let result = match kind {
            CompKind::List => Ty::list(elem_in(self, elem, elem_slot.as_ref())),
            CompKind::Set => {
                let et = elem_in(self, elem, elem_slot.as_ref());
                if !et.is_unknown()
                    && let Some(why) = self.key_ty_reject(&et)
                    && self.pending_key_reject.as_deref() != Some(why.as_str())
                {
                    self.error(elem.span, format!("set element type {why}"));
                }
                Ty::set(et)
            }
            CompKind::Map => {
                let key = key.expect("a map comprehension always carries a key expression");
                let kt = elem_in(self, key, key_slot.as_ref());
                let vt = elem_in(self, elem, elem_slot.as_ref());
                if !kt.is_unknown()
                    && let Some(why) = self.key_ty_reject(&kt)
                    && self.pending_key_reject.as_deref() != Some(why.as_str())
                {
                    self.error(key.span, format!("map key type {why}"));
                }
                Ty::map(kt, vt)
            }
        };
        self.pop_scope();
        result
    }

    pub(super) fn infer_unary(&mut self, node: &Expr, op: UnaryOp, inner: &Expr) -> Ty {
        match op {
            UnaryOp::ErrVal => return self.infer_err_val(node, inner),
            UnaryOp::Wrap => return self.infer_wrap_val(node, inner),
            UnaryOp::Neg | UnaryOp::Not => {}
        }
        let t = self.infer_value(inner);
        match op {
            UnaryOp::Neg => {
                // int/float negate natively; a struct/type-param negates via the `Neg`
                // protocol (method `neg(self) -> Self`) — the unary mirror of how `+` consults `Add`.
                if t.is_numeric() || t.is_unknown() || self.satisfies(&t, "Neg").is_ok() {
                    t
                } else {
                    self.error(inner.span, format!("cannot negate {t}"));
                    Ty::Unknown
                }
            }
            UnaryOp::Not => {
                if t != Ty::Bool && !t.is_unknown() {
                    self.error(inner.span, format!("'not' expects bool, found {t}"));
                }
                Ty::Bool
            }
            UnaryOp::ErrVal | UnaryOp::Wrap => unreachable!("handled above"),
        }
    }

    /// TICKET-227 (D2): prefix `!e` builds an error value. Under an expected `T!E` the operand is
    /// inferred with `E` as a seed hint and must fit `E`; the value is that `T!E`. The operand must
    /// satisfy the `Error` protocol.
    fn infer_err_val(&mut self, node: &Expr, inner: &Expr) -> Ty {
        let hint = self.expected_hint.take();
        let t = match &hint {
            Some(Ty::Result(_, e)) => {
                self.expected_hint = Some((**e).clone());
                let t = self.infer_value(inner);
                self.expected_hint = None;
                t
            }
            _ => self.infer_value(inner),
        };
        if !t.is_unknown() && !self.assignable(&Ty::error_proto(), &t) {
            self.error(inner.span, format!("{t} does not satisfy Error"));
        }
        match hint {
            Some(Ty::Result(ok, e)) => {
                if !t.is_unknown() && !self.assignable(&e, &t) {
                    self.error(inner.span, format!("error value: expected {e}, found {t}"));
                }
                Ty::Result(ok, e)
            }
            // No expected carrier: a fn body pins the success type by a later use in its frame
            // (R5); top level decides at once; the return-inference walk leaves it open.
            None | Some(Ty::Var(_)) if self.in_fn_body && !self.resolving_returns => {
                let v = self.defer_carrier(node, super::tyvar::CarrierKind::Error);
                Ty::Result(Box::new(Ty::Var(v)), Box::new(t))
            }
            None | Some(Ty::Var(_)) if !self.resolving_returns => {
                self.error(node.span, super::tyvar::CANNOT_INFER_SUCCESS.to_string());
                Ty::Result(Box::new(Ty::Unknown), Box::new(t))
            }
            _ => Ty::Result(Box::new(Ty::Unknown), Box::new(t)),
        }
    }

    /// TICKET-227 (D3) — prefix `?x` builds a present/success value, its carrier taken from the
    /// expected type: `T?` -> `Some(x)`, `T!E` -> `Ok(x)`. The operand owns `T` as its slot (so
    /// `?5` at `int??` is `Some(Some(5))`). With no expected carrier the value takes a frame type
    /// variable in a fn body (an unpinned one defaults to `T?`); elsewhere it is `T?` at once.
    fn infer_wrap_val(&mut self, node: &Expr, inner: &Expr) -> Ty {
        let hint = self.expected_hint.take();
        let (payload, w) = match &hint {
            Some(Ty::Option(p)) => ((**p).clone(), crate::checker::Wrap::Some),
            Some(Ty::Result(p, _)) => ((**p).clone(), crate::checker::Wrap::Ok),
            _ => {
                let t = self.infer_value(inner);
                if self.in_fn_body && !self.resolving_returns {
                    let v = self.defer_carrier(node, super::tyvar::CarrierKind::Present(t));
                    return Ty::Var(v);
                }
                self.record_wrap(node.id, crate::checker::Wrap::Some, node.span);
                return Ty::Option(Box::new(t));
            }
        };
        let t = self.infer_value_in(inner, &payload);
        if !t.is_unknown() && !self.assignable(&payload, &t) {
            self.error(
                inner.span,
                format!("'?' value: expected {payload}, found {t}"),
            );
        }
        self.record_wrap(node.id, w, node.span);
        hint.unwrap_or(Ty::Unknown)
    }

    pub(super) fn infer_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Ty {
        use BinaryOp::*;
        // `infer_call` drains the single `expected_hint` slot via `take()` (src/checker/expr.rs:33),
        // so without re-installing it the second-inferred operand loses the hint a generic call
        // there would need to pin its type parameter. Same hop `infer_if_else_chain` makes per
        // branch (src/checker/pattern.rs:1077-1079) and `infer_list` makes per element (:2426-2432).
        // Uniform over every operator, deliberately: `b: bool = pick() == false` already relies on
        // the hint reaching a comparison's LEFT operand, so filtering by operator would narrow an
        // accepted program.
        let hint = self.expected_hint.take();
        self.expected_hint = hint.clone();
        let l = self.infer_value(lhs);
        self.expected_hint = hint;
        let r = self.infer_value(rhs);
        self.expected_hint = None;
        let either_unknown = l.is_unknown() || r.is_unknown();
        match op {
            And | Or => {
                if l != Ty::Bool && !l.is_unknown() {
                    self.error(
                        lhs.span,
                        format!("logical operator expects bool, found {l}"),
                    );
                }
                if r != Ty::Bool && !r.is_unknown() {
                    self.error(
                        rhs.span,
                        format!("logical operator expects bool, found {r}"),
                    );
                }
                Ty::Bool
            }
            Add => {
                if l == Ty::Str && r == Ty::Str {
                    Ty::Str
                } else if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) = self.op_overload_result(&l, &r, "Add") {
                    t
                } else if let (Ty::List(le), Ty::List(re)) = (&l, &r) {
                    // List concat (gap #3): `[1,2] + [3,4]` → `list[T]`, identical to `.concat`.
                    // Element types must be compatible; an empty `[]` side (Unknown elem) is
                    // joined by `merge_unknown` so `[] + [1]` infers `list[int]`.
                    if self.join_ty(le, re) {
                        Ty::List(Box::new(merge_unknown(le, re)))
                    } else {
                        let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                        self.error(
                            lhs.span,
                            format!(
                                "cannot apply + to {l_s} and {r_s}{}",
                                float_fix_note_join(&l, &r)
                            ),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    let note = self.hook_name_note(&l, "Add", "add");
                    self.error(
                        lhs.span,
                        format!(
                            "cannot apply + to {l_s} and {r_s}{}{note}",
                            float_fix_note_join(&l, &r)
                        ),
                    );
                    Ty::Unknown
                }
            }
            // `-`/`*` overload via the `Sub`/`Mul` protocols on same-typed structs; `/`/`%` stay
            // numeric-only (no protocol).
            Sub | Mul => {
                let proto = if op == Sub { "Sub" } else { "Mul" };
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) = self.op_overload_result(&l, &r, proto) {
                    t
                } else if op == Mul && matches!((&l, &r), (Ty::List(_), Ty::Int)) {
                    // List repeat (gap #3): `[0] * 3` → `list[T]`. Result keeps the list's element.
                    l.clone()
                } else if op == Mul && matches!((&l, &r), (Ty::Int, Ty::List(_))) {
                    // Commutative, Python-style: `3 * [0]` → `list[T]`.
                    r.clone()
                } else if op == Sub
                    && let (Ty::Set(le), Ty::Set(re)) = (&l, &r)
                {
                    // Set difference (gap #3): `a - b` → `set[T]`, identical to `.difference`.
                    if self.join_ty(le, re) {
                        Ty::Set(Box::new(merge_unknown(le, re)))
                    } else {
                        let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                        self.error(
                            lhs.span,
                            format!(
                                "cannot apply {} to {l_s} and {r_s}{}",
                                op_sym(op),
                                float_fix_note_join(&l, &r)
                            ),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    let note = self.hook_name_note(&l, proto, &proto.to_lowercase());
                    self.error(
                        lhs.span,
                        format!(
                            "cannot apply {} to {l_s} and {r_s}{}{note}",
                            op_sym(op),
                            float_fix_note_join(&l, &r)
                        ),
                    );
                    Ty::Unknown
                }
            }
            // `/`/`%` overload via the `Div`/`Mod` protocols on same-typed structs/enums/type-params,
            // exactly like `-`/`*` use `Sub`/`Mul` (M22).
            Div | Mod => {
                let proto = if op == Div { "Div" } else { "Mod" };
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) = self.op_overload_result(&l, &r, proto) {
                    t
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    let note = self.hook_name_note(&l, proto, &proto.to_lowercase());
                    self.error(
                        lhs.span,
                        format!(
                            "cannot apply {} to {l_s} and {r_s}{}{note}",
                            op_sym(op),
                            float_fix_note_join(&l, &r)
                        ),
                    );
                    Ty::Unknown
                }
            }
            Lt | LtEq | Gt | GtEq | Eq | NotEq | In => {
                self.compare_pair(op, &l, &r, lhs.span, rhs.span)
            }
            // Bitwise/shift ops are int-only (gap #13), EXCEPT `| & ^` also do set algebra
            // (gap #3): union / intersection / symmetric-difference on two `set[T]`. Shifts
            // (`<< >>`) stay strictly int-only.
            BitAnd | BitOr | BitXor | Shl | Shr => {
                if l == Ty::Int && r == Ty::Int {
                    Ty::Int
                } else if matches!(op, BitAnd | BitOr | BitXor)
                    && let (Ty::Set(le), Ty::Set(re)) = (&l, &r)
                {
                    // Set `|`→union, `&`→intersection, `^`→symmetric-difference → `set[T]`,
                    // identical to the `.union`/`.intersection` methods (`^` has no method form).
                    if self.join_ty(le, re) {
                        Ty::Set(Box::new(merge_unknown(le, re)))
                    } else {
                        let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                        self.error(
                            lhs.span,
                            format!(
                                "bitwise operator {} requires int operands or two sets, found {l_s} and {r_s}",
                                op_sym(op)
                            ),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    self.error(
                        lhs.span,
                        format!(
                            "bitwise operator {} requires int operands or two sets, found {l_s} and {r_s}",
                            op_sym(op)
                        ),
                    );
                    Ty::Unknown
                }
            }
        }
    }

    /// A Python-style chained comparison (`a < b <= c`, TICKET-077): infers each operand exactly
    /// once, then judges every adjacent pair through [`Self::compare_pair`]. Per DEC-034, the
    /// expected-type hint is re-installed before EVERY operand, uniformly, exactly as
    /// `infer_binary` does for a pair.
    pub(super) fn infer_compare_chain(&mut self, operands: &[Expr], ops: &[BinaryOp]) -> Ty {
        let hint = self.expected_hint.take();
        let tys: Vec<Ty> = operands
            .iter()
            .map(|o| {
                self.expected_hint = hint.clone();
                self.infer_value(o)
            })
            .collect();
        self.expected_hint = None;
        for i in 0..ops.len() {
            self.compare_pair(
                ops[i],
                &tys[i],
                &tys[i + 1],
                operands[i].span,
                operands[i + 1].span,
            );
        }
        Ty::Bool
    }

    /// The type rules for the seven comparison operators (`<`/`<=`/`>`/`>=`/`==`/`!=`/`in`),
    /// extracted from `infer_binary` (TICKET-077) so a comparison CHAIN (`a < b == c`) can judge
    /// each adjacent PAIR without re-inferring either operand: `infer_compare_chain` calls
    /// `infer_value` once per operand and then calls this per adjacent pair. Re-inferring an
    /// operand a second time (e.g. via `infer_binary` on synthesized pairs) would double-report its
    /// errors and re-key the span-keyed side tables — `record_proto_eq` treats an aliased key as a
    /// hard error.
    pub(super) fn compare_pair(
        &mut self,
        op: BinaryOp,
        l: &Ty,
        r: &Ty,
        lspan: Span,
        rspan: Span,
    ) -> Ty {
        use BinaryOp::*;
        let either_unknown = l.is_unknown() || r.is_unknown();
        match op {
            Lt | LtEq | Gt | GtEq => {
                let ok = (l.is_numeric() && r.is_numeric())
                    || (*l == Ty::Str && *r == Ty::Str)
                    || self.ordering_allowed(l, r);
                if !ok && !either_unknown {
                    let [l_s, r_s] = Ty::render_distinct([l, r]);
                    let note = self.hook_name_note(l, "Comparable", "compare");
                    self.error(lspan, format!("cannot compare {l_s} and {r_s}{note}"));
                }
                Ty::Bool
            }
            // **B2** (`docs/gaps.md`) — `==`/`!=` yields `bool`, but the operands must be able to be
            // equal. Only a **provably disjoint** pair (`1 == "a"`, `Box[int] == Box[str]`) is
            // rejected: that is always a bug in user code — Python answers `False` at runtime, but
            // Chezzi is statically typed, so — like mypy `--strict-equality`, Go, and Rust — it is a
            // check-time error.
            //
            // The question is CO-INHABITABILITY ("can these two ever be the same value?"), which is
            // [`Checker::may_be_equal`] and NOT `assignable`/`compatible`: those answer the STORAGE
            // question and carry container invariance + a sendability witness that equality, which
            // never writes and never crosses a thread boundary, has no use for. See that fn's doc
            // for the three-way difference and the `Shape == Error` / `List[Error] == List[MyErr]`
            // cases it exists to admit.
            //
            // The runtime's cross-type pairs (`1 == 1.0`, `b"ab" == bytearray(...)`) are arms of
            // `may_be_equal` itself, so they compose through the recursion (`[1.0] == [1]`) instead
            // of being a top-level special case.
            // `either_unknown` keeps a prior error from cascading (and keeps both operands INFERRED,
            // which the range-in-value-position backstop depends on).
            //
            // **W7-41 — co-inhabitance is NOT the only question.** This file used to argue that a
            // user `eq` overload adds nothing to ask here, "since it only ever applies to a same-type
            // pair, which `may_be_equal` already accepts". Conditional conformance — a `where` clause
            // on the method, landed after M23 — falsified exactly that premise: `Box[Tag] == Box[Tag]`
            // IS a same-type pair whose `eq` does not cover it, and it check-cleaned then faulted with
            // *"struct 'Tag' has no 'compare' method"*. So the bound is asked too, below.
            Eq | NotEq => {
                // TICKET-225: an `==` join pins a generic value from its sibling (`inc == g`) before
                // the operands are judged.
                self.join_ty(l, r);
                // A `where T: <scalar>` bound is an EQUALITY constraint (`scalar_bound_ty`), not
                // structural satisfaction: such a `T` is EXACTLY that scalar, at any nesting depth.
                // Substitute the pins away so the pair is judged concretely — without this the
                // blanket "an erased param is never provably disjoint" rule would wave through
                // `fn f[T](a: T, b: int) -> bool where T: str: return a == b`.
                let pins: HashMap<String, Ty> = self
                    .type_params
                    .iter()
                    .filter_map(|(n, bs)| {
                        bs.iter()
                            .find_map(|b| Self::scalar_bound_ty(&b.name))
                            .map(|t| (n.clone(), t))
                    })
                    .collect();
                let (l, r) = if pins.is_empty() {
                    (l.clone(), r.clone())
                } else {
                    (subst(l, &pins), subst(r, &pins))
                };
                // **W7-41.** Does the structural equality walk reach a declared `eq` whose `where`
                // bounds do not hold for this instantiation? The explicit spelling `a.eq(b)` was
                // always rejected here (the instance-method dispatch path runs `enforce_bounds`); the
                // operator was not, so the same program had two answers. Rust owns conditional
                // conformance and agrees — measured, rustc 1.97.0: `error[E0369]: binary operation
                // `==` cannot be applied to type `Boxy<Tag>`` on the `impl<T: Ord> PartialEq` mirror,
                // with `Boxy(1) == Boxy(2)` still compiling.
                //
                // BOTH operands, not one: `may_be_equal` accepts co-inhabitable pairs, not identical
                // ones (the `int`/`float` and `bytes`/`bytearray` cross arms recurse in), so a
                // left-only gate would give `Box(1) == Box(1.0)` and `Box(1.0) == Box(1)` different
                // verdicts. And it belongs HERE rather than inside `may_be_equal`, which is `&self`,
                // non-emitting by contract, and recursive — the predicate already walks elements,
                // payloads and fields itself, so one call per operand covers every nesting.
                //
                // NOT erased (W7-53): a free `T` reached here goes to `eq_bounds_unsatisfied`'s own
                // `Ty::Param` arm, which DOES fail unless `T` carries `Eq` among its declared bounds —
                // `may_be_equal`'s `(Param(_), _) => true` still lets the OPERATOR compile with `T`
                // abstract (co-inhabitance is not the question here), but the bound obligation is the
                // call site's to discharge, matching both owning ancestors: rustc 1.97.0 rejects
                // `fn f<T>(a: T, b: T) -> bool { a == b }` outright (`E0369`, "consider restricting
                // type parameter T with trait PartialEq"), and Go 1.26 rejects the mirror
                // (`invalid operation: a == b (incomparable types in type set)`). `fn f[T](x: Box[T],
                // y: Box[T])` that never compares stays accepted (nothing walks `T`); a CONCRETE part
                // of the same type (`Map[T, Box[Tag]]`) was always judged and still is. This CLOSES
                // W7-41's known ceiling: `fn f[T](a: T, b: T) -> bool: return a == b` now rejects at
                // its OWN definition (`add \`where T: Eq\``) instead of type-checking clean and
                // faulting on a `Box[Tag]` three calls later.
                // `!either_unknown` leads DELIBERATELY: on a cascade the predicate is not run at all,
                // rather than walked and its answer thrown away. Both operands are gated because
                // `n == m` plus co-inhabitable args is NOT identical args — `may_be_equal`'s int/float
                // and bytes/bytearray cross arms recurse in, so a left-only gate would give
                // `Box(1) == Box(1.0)` and its mirror different verdicts off one hook (W7-41 trap 2).
                if !either_unknown
                    && let Some(why) = self
                        .eq_bounds_unsatisfied(&l)
                        .or_else(|| self.eq_bounds_unsatisfied(&r))
                {
                    // Decorated, not replaced: the bare text reads as "you have no equality", and the
                    // user WROTE an `eq`. Same ` — ` separator the `<` operator's note used.
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    self.error(
                        lspan,
                        format!("cannot compare {l_s} and {r_s} for equality — {why}"),
                    );
                    // One diagnostic per site — do not also run the co-inhabitance question.
                    return Ty::Bool;
                }
                let ok = self.may_be_equal(&l, &r);
                if !ok && !either_unknown {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    self.error(
                        lspan,
                        format!("cannot compare {l_s} and {r_s} for equality"),
                    );
                }
                Ty::Bool
            }
            // `x in xs` — membership, type-directed on the RHS container. List/Set test element
            // membership, Map tests KEY membership (Python-style), Str tests substring. Always
            // yields `bool`. A user struct/enum with a `contains(self, item) -> bool` method (the
            // `Contains` protocol, L5) dispatches to that method; anything else rejects. The
            // element/key/item type must be compatible with the LHS.
            In => {
                // (A range RHS needs no special case here: `r = self.infer(rhs)` above already
                // rejected it generically — see `infer_kind`'s `ExprKind::Range` arm — and lands
                // `Unknown`, which `either_unknown` then silences. A guard here would DOUBLE-report.)
                match r {
                    Ty::List(elem) | Ty::Set(elem) => {
                        if !either_unknown && !self.join_ty(elem, l) && !self.assignable(elem, l) {
                            let [l_s, r_s] = Ty::render_distinct([l, r]);
                            self.error(lspan, format!("cannot test membership of {l_s} in {r_s}"));
                        }
                        // **W7-45.** `in` runs `values_equal` per element, exactly as `==` does, but
                        // it is typed by `compatible` — which asks co-inhabitance, not whether the
                        // elements CAN be compared. So `Box(Tag(1)) in [Box(Tag(2))]` check-cleaned
                        // and faulted, while its method spelling `.contains(…)` already rejected:
                        // the same operator-vs-method split W7-41 closed for `==`.
                        //
                        // LIST-ONLY: extending this arm to `Ty::Set` would make the ordinary
                        // `x in Set([...])` report twice, once at the construction site
                        // (`key_ty_reject`) and once here. Two diagnostics for one bug was judged the
                        // worse trade. A generic `fn mk[T: Hashable](x: T) -> Set[T]` that constructs
                        // `Set[T]` inside its own body no longer needs a THIRD site here either (W7-53):
                        // `key_ty_reject`'s own second conjunct (`eq_bounds_unsatisfied`, non-erased
                        // since W7-53) already demands `T: Eq` at that construction site — `Hashable`
                        // does NOT embed `Eq` (measured: embedding it regressed a working ordinary-`eq`-
                        // method escape hatch, `key_ty_reject`'s doc), so `mk` must spell
                        // `[T: Hashable + Eq]` for the construction to type-check at all.
                        else if !either_unknown
                            && matches!(r, Ty::List(_))
                            && let Some(why) = self.eq_bounds_unsatisfied(elem)
                        {
                            let [l_s, r_s] = Ty::render_distinct([l, r]);
                            self.error(
                                lspan,
                                format!("cannot test membership of {l_s} in {r_s} — {why}"),
                            );
                        }
                    }
                    Ty::Map(key, _) => {
                        if !either_unknown && !self.join_ty(key, l) && !self.assignable(key, l) {
                            let [l_s, r_s] = Ty::render_distinct([l, r]);
                            self.error(
                                lspan,
                                format!(
                                    "cannot test membership of {l_s} in {r_s} (map `in` tests keys)"
                                ),
                            );
                        }
                    }
                    Ty::Str => {
                        if *l != Ty::Str && !either_unknown {
                            self.error(
                                lspan,
                                format!("substring `in` requires a str on the left, found {l}"),
                            );
                        }
                    }
                    Ty::Unknown => {}
                    other => {
                        // `Contains` protocol: a struct/enum with `contains(self, item) -> bool`.
                        if let Some(item) = self.contains_item_ty(other) {
                            if !either_unknown && !self.join_ty(&item, l) {
                                let [l_s, r_s] = Ty::render_distinct([l, r]);
                                self.error(
                                    lspan,
                                    format!("cannot test membership of {l_s} in {r_s}"),
                                );
                            }
                        } else {
                            let note = self.hook_name_note(other, "Contains", "contains");
                            self.error(
                                rspan,
                                format!(
                                    "cannot use `in` on {other} (expected a list, set, map, str, or a type with `contains(self, item) -> bool`){note}"
                                ),
                            );
                        }
                    }
                }
                Ty::Bool
            }
            And | Or | Add | Sub | Mul | Div | Mod | BitAnd | BitOr | BitXor | Shl | Shr => {
                unreachable!("compare_pair called with a non-comparison operator")
            }
        }
    }

    /// A "this name is a variant — write it qualified" diagnostic, naming the owning enum(s).
    /// Falls back to "unknown name" if the name isn't a known variant (shouldn't normally happen at
    /// the call sites, which guard on `variant_owners` first).
    pub(super) fn qualify_hint(&self, name: &str) -> String {
        match self.variant_owners.get(name).map(Vec::as_slice) {
            Some([en]) => {
                format!("'{name}' is a variant of enum '{en}'; write it qualified as '{en}.{name}'")
            }
            Some(ens @ [_, _, ..]) => {
                let opts = ens
                    .iter()
                    .map(|e| format!("'{e}.{name}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "'{name}' is a variant of several enums; write it qualified (one of {opts})"
                )
            }
            _ => format!("unknown name '{name}'"),
        }
    }

    /// The enum a scrutinee/slot type belongs to (`Color`, or `Result`/`Option` for the built-ins),
    /// or `None` for a non-enum / un-inferable type. Used to validate a pattern's `Enum.` qualifier
    /// against the value being matched.
    pub(super) fn scrutinee_enum(ty: &Ty) -> Option<&str> {
        ty.as_enum().map(|(k, _)| k)
    }

    /// [`Self::check_pattern_qualifier_inner`], returning whether it reported an error. The caller
    /// then skips its own `'{name}' is not a variant of ...` line: it would be the same fact at the
    /// same position. `Checker::warn` writes to `self.warnings`, so `self.errors` holds errors only
    /// and the length comparison needs no `severity` test.
    pub(super) fn check_pattern_qualifier(
        &mut self,
        module_name: &Option<String>,
        enum_name: &Option<String>,
        name: &str,
        scrut: Option<(&str, &Ty)>,
        span: Span,
    ) -> bool {
        let mark = self.errors.len();
        self.check_pattern_qualifier_inner(module_name, enum_name, name, scrut, span);
        self.errors.len() > mark
    }

    /// Validate the `Enum.` qualifier on a `case Enum.Variant:` pattern. The named variant must (a)
    /// belong to `enum_name`, and (b) — since variant names may now be shared across enums — name the
    /// **scrutinee's** enum (`scrut_enum`): owning the name isn't enough, because a foreign qualifier
    /// resolves to a different `variant_id` (a dead arm that would still be miscounted toward
    /// exhaustiveness → a "checked-OK" match that traps at runtime). When *unqualified*, a user variant
    /// name is an error — variants must be written qualified (built-in Ok/Err/Some/None stay bare).
    fn check_pattern_qualifier_inner(
        &mut self,
        module_name: &Option<String>,
        enum_name: &Option<String>,
        name: &str,
        scrut: Option<(&str, &Ty)>,
        span: Span,
    ) {
        // A leading module binder (`module.Enum.Variant`) is validated here then dropped: the module
        // must be bound and must own the named enum. Resolution mirrors construction
        // (`infer_field`'s module.Enum.Variant path) — `imported_modules` → `ModuleSig` → `enum_defs`.
        // Errors render BARE names only (never the qualified identity key). On success we fall through
        // to the existing `enum_name` validation, which is scrutinee-driven and keeps everything else
        // (variant-exists, scrutinee-agrees, exhaustiveness-by-identity) unchanged.
        // When a module binder is present and resolves, this holds the enum's true IDENTITY KEY
        // (`module::Enum`), used below instead of the bare/scrutinee fallback so variant-lookup and
        // scrutinee-agreement key on the SAME identity as construction.
        let mut module_ekey: Option<String> = None;
        if let Some(m) = module_name {
            let Some(en) = enum_name else {
                // A module binder always comes with an enum name from the parser (3-part form); a
                // None here would be a parser bug. Defensive: nothing to validate.
                return;
            };
            let Some(mid) = self.imported_modules.get(m).cloned() else {
                let msg = self
                    .ambiguous_bind_msg(m)
                    .unwrap_or_else(|| format!("unknown module '{m}'"));
                self.error(span, msg);
                return;
            };
            // An exported alias of an enum (`lib.Tone.Dark(n)`, TICKET-172) keys on its target.
            let alias_ekey = self
                .qualified_alias_ty(m, en)
                .and_then(|t| t.as_enum().map(|(k, _)| k.to_string()));
            match self.module_sigs.get(&mid) {
                Some(sig) if sig.enum_defs.contains_key(en) => {
                    module_ekey = Some(self.type_key(&mid, en));
                }
                _ if alias_ekey.is_some() => module_ekey = alias_ekey,
                _ => {
                    self.error(span, format!("module '{m}' has no enum '{en}'"));
                    return;
                }
            }
        }
        match enum_name {
            Some(en) => {
                // ROOT REDESIGN — the pattern carries the BARE written enum name. Resolve it to its
                // qualified IDENTITY KEY for the layout lookup. A module binder (`module.Enum.Variant`)
                // resolves the key directly (above). Otherwise a bare-visible enum (local / from-import
                // / std) resolves via `bare_types`; a WHOLE-module-imported enum (`Color` from
                // `import geo`) is NOT bare-visible, so fall back to the SCRUTINEE's own enum key when
                // its bare display name equals `en` (the pattern `Color.Red` matching a `geo::Color`
                // value). Error messages keep the bare `en`.
                let ekey = match module_ekey {
                    Some(k) => k,
                    // `en` may be a LOCAL `type` alias of an enum (`type F = E`). Checked BEFORE
                    // `bare_types`: a graph-mode module registers EVERY top-level type decl —
                    // including a `type` alias — into `bare_types` under its OWN dead identity key
                    // (`<module>::F`, a key no value carries — Gotcha 2's compiler-side twin), so a
                    // `bare_types.get(en)` hit for an alias name would resolve to that dead key
                    // instead of the aliased enum's real one. Its type arguments are discarded here —
                    // the scrutinee already supplies those.
                    None => match self.alias_enum_head(en) {
                        Some((k, _)) => k,
                        None => match self.bare_types.get(en) {
                            Some(k) => k.clone(),
                            None => match scrut.map(|(k, _)| k) {
                                Some(s)
                                    if crate::compiler::bare_display(s) == *en
                                        && (self.enum_names.contains(en)
                                            || self.enum_key_imported(s)) =>
                                {
                                    s.to_string()
                                }
                                _ => en.to_string(),
                            },
                        },
                    },
                };
                if !self
                    .variants
                    .contains_key(&(ekey.clone(), name.to_string()))
                {
                    let names = self.variant_names(&ekey);
                    self.error_help(
                        span,
                        format!("enum '{en}' has no variant '{name}'"),
                        suggest::did_you_mean(name, &names),
                    );
                    return;
                }
                // The qualifier must name the scrutinee's own enum. (Skipped when the scrutinee enum
                // is unknown — an int/str/bool or un-inferable scrutinee, handled by the caller.) The
                // scrutinee carries the runtime key, so compare against the resolved `ekey`.
                if let Some((s, ty)) = scrut
                    && ekey != s
                {
                    self.error(
                        span,
                        format!("variant '{en}.{name}' cannot match a value of type {ty}"),
                    );
                }
            }
            None => {
                // A bare variant name in a pattern must be imported (`import V from Enum`, or the
                // prelude's four) or written qualified.
                if !self.imported_variants.contains_key(name)
                    && self.variant_owners.contains_key(name)
                {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                }
            }
        }
    }

    /// The one lookup of a struct field's type at the receiver's type args (`Stack[int].items` is
    /// `List[int]`). `None` when `sname` has no field `field`.
    pub(super) fn struct_field_ty(&self, sname: &str, targs: &[Ty], field: &str) -> Option<Ty> {
        self.struct_shape(sname).and_then(|info| {
            info.fields
                .iter()
                .find(|(f, _)| f == field)
                .map(|(_, ty)| subst(ty, &struct_param_map(info, targs)))
        })
    }

    pub(super) fn infer_field(&mut self, e: &Expr, obj: &Expr, name: &str, name_span: Span) -> Ty {
        // A full module path that did not resolve (TICKET-175): the receiver `obj` is the BARE first
        // segment of an imported dotted module path (`pkg`), never a bound name, and `name` is the
        // NEXT segment. Desugar already folded every full path of an un-aliased import into one
        // dotted name, so what reaches here is an aliased import's path (`import pkg.deep as d` binds
        // only `d`) or a prefix no import binds (`std.concurrency` of `import
        // std.concurrency.collection`). The trailing name isn't visible here, hence `<Name>`.
        if let ExprKind::Ident(head) = &obj.kind
            && self.lookup(head).is_none()
            && !self.imported_modules.contains_key(head)
        {
            let matches: Vec<(String, String, bool)> = self
                .import_paths
                .iter()
                .filter(|(p, _, _)| p[0] == *head && p[1] == name)
                .map(|(p, bound, full)| (p.join("."), bound.clone(), *full))
                .collect();
            // An un-aliased import of exactly `head.name` binds that full path, so the fold only
            // skipped it because a module-level type name shadows the head: neither hint is true.
            let exact_full = self
                .import_paths
                .iter()
                .any(|(p, _, full)| *full && p.len() == 2 && p[0] == *head && p[1] == name);
            if exact_full {
                // fall through to the ordinary field inference
            } else if let Some((dotted, bound, _)) = matches.iter().find(|m| !m.2) {
                self.error(
                    obj.span,
                    format!(
                        "`{dotted}` is imported as `{bound}`, which binds only `{bound}` — write `{bound}.<Name>`, or import it as `import {dotted}` to use the full path"
                    ),
                );
                return Ty::Unknown;
            } else if !matches.is_empty() {
                self.error(
                    obj.span,
                    format!(
                        "module `{head}.{name}` is not imported — add `import {head}.{name}` to use `{head}.{name}.<Name>`"
                    ),
                );
                return Ty::Unknown;
            }
        }
        // What `obj.name` names: the rule table's answer; every arm below reads it.
        let res = self.resolve_path(e, PathPos::Value);
        match &res {
            // MEMBER-as-a-value position through a type parameter: `Col.Red` inside
            // `fn f[Col: Tagged]` is the PARAMETER. Nothing is reachable through an erased type
            // parameter except its bounds' methods: an instance method is a value, a STATIC one
            // only a call (rustc agrees: E0599 "no associated function or constant named `Red`
            // found for type parameter `Col`").
            Some(Resolution::ParamMethodFn { .. } | Resolution::WitnessStatic(_)) => {
                let ExprKind::Ident(tname) = &obj.kind else {
                    return Ty::Unknown;
                };
                if let Some(Resolution::ParamMethodFn { .. }) = res
                    && let Some(pf) = self.param_member_fn(tname, name)
                {
                    return self.path_fn_value_ty(e.id, pf, None, name_span);
                }
                return self.type_param_shadow_error(
                    tname,
                    &format!(
                        "a type parameter has no member '{name}'; through one you reach only the methods its bounds declare: an instance method as a value or a call (`{tname}.<method>(value, ...)`), a STATIC method only as a call (`{tname}.<method>(...)`)"
                    ),
                    obj.span,
                );
            }
            // A type path read as a value (Rust's path-value rule, TICKET-204): `E.V`,
            // `R1[int].L`, `Bx[int].make`, `Pt.getx`, `lib.E.V`, an alias head `A.L`; `None` is a
            // type path's miss or std.json's decode.
            Some(
                Resolution::Variant { .. }
                | Resolution::VariantFn { .. }
                | Resolution::MethodFn { .. },
            )
            | None => {
                if let Some(t) = self.type_member_value(obj, name, name_span, res.as_ref()) {
                    return t;
                }
                if res.is_none()
                    && let Some(t) = self.decode_value(e.id, obj, name, name_span)
                {
                    return t;
                }
            }
            _ => {}
        }
        let obj_ty = self.infer(obj);
        match &obj_ty {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index as a
            // decimal string; out-of-range or non-numeric is an error.
            Ty::Tuple(elems) => match name.parse::<usize>() {
                Ok(i) if i < elems.len() => elems[i].clone(),
                _ => {
                    self.error(
                        name_span,
                        format!("tuple {obj_ty} has no element '.{name}'"),
                    );
                    Ty::Unknown
                }
            },
            Ty::Struct(sname, targs) => {
                if let Some(ty) = self.struct_field_ty(sname, targs, name) {
                    return ty;
                }
                if let Some(info) = self.struct_shape(sname) {
                    // A METHOD is not a field, and a BOUND method is not a value (Rust E0615
                    // "attempted to take value of method"): it would hide its `self` capture, and
                    // the compiler lowers a field-read to a plain field load. The method named
                    // through its TYPE (`Bx[int].get`, receiver first) is the value form.
                    if info.methods.contains_key(name) {
                        let recv = match &obj.kind {
                            ExprKind::Ident(n) => n.as_str(),
                            _ => "x",
                        };
                        self.error(
                            name_span,
                            format!(
                                "type {obj_ty} has no field '{name}' ('{name}' is a method — a bound \
                                 method is not a value: call it (`{recv}.{name}(…)`), wrap it \
                                 (`fn(): {recv}.{name}()`), or name it through its type \
                                 (`{obj_ty}.{name}`, which takes the receiver first))"
                            ),
                        );
                        return Ty::Unknown;
                    }
                }
                let names = self.field_names(sname);
                self.error_help(
                    name_span,
                    format!("type {obj_ty} has no field '{name}'"),
                    suggest::did_you_mean(name, &names),
                );
                Ty::Unknown
            }
            Ty::Module(mname) => {
                // M24 — the fn-as-value wall on the cross-module read (`g := lib.empty`): the member
                // path below hands back a plain `Ty::Func`, which erases the witness exactly like a
                // same-module read does.
                let wparams = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id))
                    .and_then(|sig| sig.certain_fn(name))
                    .map(|f| f.witness_params.clone())
                    .unwrap_or_default();
                if self.reject_witness_fn_value(name, &wparams, obj.span) {
                    return Ty::Unknown;
                }
                // TICKET-187: a generic member read as a value pins or is rejected, exactly like a
                // bare read — its rigid `T` must never reach the caller's scope.
                if let ExprKind::Ident(m) = &obj.kind
                    && let Some((display, sig)) = self.generic_module_fn(m, name)
                    && let Some(ty) = self.generic_fn_value_ty(
                        e.id,
                        &display,
                        &sig,
                        &fn_spelling(&display, &sig.type_params),
                        name_span,
                    )
                {
                    return ty;
                }
                let member = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id))
                    // TICKET-196: the slot's final type (`fn_value_ty` for a `certain_fn`).
                    .map(|sig| sig.value_ty(name).cloned());
                match member {
                    Some(Some(ty)) => ty,
                    _ => {
                        if let ExprKind::Ident(m) = &obj.kind
                            && let Some(th) = self.qualified_type_head(m, name)
                        {
                            self.type_not_value(&th, name_span);
                        } else {
                            let names = self.module_member_names(mname);
                            self.error_help(
                                name_span,
                                format!("module '{mname}' has no member '{name}'"),
                                suggest::did_you_mean(name, &names),
                            );
                        }
                        Ty::Unknown
                    }
                }
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(name_span, format!("type {other} has no field '{name}'"));
                Ty::Unknown
            }
        }
    }

    /// `obj[…]`, one bracket with both readings (TICKET-222). The head picks: a type-applied fn
    /// value, then a type used as a value, then the index reading. A bracket with no index reading
    /// on a value head is a type where a subscript belongs (Go: `more than one index`).
    pub(super) fn infer_index(
        &mut self,
        e: &Expr,
        obj: &Expr,
        index: Option<&Expr>,
        types: &[crate::ast::Type],
    ) -> Ty {
        // A type-applied fn value `idt[int]` (TICKET-197/204). Runs BEFORE `infer_value(obj)` /
        // inferring the index, which would wrongly report `int` as an unknown name and "cannot
        // index into fn".
        if let Some(t) = self.infer_type_applied_fn_value(e) {
            return t;
        }
        // A type applied in value position (`Box[int]`, `Wrap[int]`) is a type, not a value.
        if let Some(app) = crate::ast::type_application(e)
            && let Some(th) = self.type_head(app.head)
        {
            let written: Vec<Ty> = app
                .args
                .iter()
                .map(|t| self.resolve_type(t, e.span))
                .collect();
            if self
                .written_head_args(
                    &th.spelled,
                    self.type_param_count(&th.key),
                    th.pinned.clone(),
                    written,
                    e.span,
                )
                .is_some()
            {
                self.type_not_value(&th, e.span);
            }
            return Ty::Unknown;
        }
        let Some(index) = index else {
            if !self.infer_value(obj).is_unknown() {
                let msg = match types {
                    [t] => format!(
                        "a subscript takes an expression, found the type '{}'",
                        self.resolve_type(t, e.span)
                    ),
                    _ => format!("a subscript takes one index, found {}", types.len()),
                };
                self.error(e.span, msg);
            }
            return Ty::Unknown;
        };
        self.index_value(obj, index, !types.is_empty())
    }

    /// `obj[index]` read as a value index, once the type-application readings declined. When
    /// inferring `obj` reported an error and the bracket also read as a type (`Nope[int]`), the
    /// index is not inferred: that reading would report `int` as an unknown name on top of the
    /// head's error (DEC-158: the mark counts errors only).
    pub(super) fn index_value(&mut self, obj: &Expr, index: &Expr, type_shaped: bool) -> Ty {
        // TICKET-225: the read's expected type is the ELEMENT's slot, never the object's or the
        // subscript's; left in place, `x: int8 = xs[200]` checked the int index `200` against int8.
        self.expected_hint = None;
        let mark = self.errors.len();
        let obj_ty = self.infer_value(obj);
        if self.errors.len() > mark && type_shaped {
            return Ty::Unknown;
        }
        // Map keys are NOT int — infer the object first and check the index against the key type.
        match obj_ty {
            Ty::Map(k, v) => {
                let idx_ty = self.infer_arg(index, Some(&k));
                if !self.join_ty(&k, &idx_ty) && !self.assignable(&k, &idx_ty) {
                    let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                    self.error(index.span, format!("map key must be {k_s}, found {idx_s}"));
                }
                *v
            }
            Ty::List(inner) => {
                self.expect_int(index, "index");
                *inner
            }
            Ty::Str => {
                self.expect_int(index, "index");
                Ty::Str
            }
            Ty::Unknown => {
                self.expect_int(index, "index");
                Ty::Unknown
            }
            // A bounded `[C: Index[K, V]]` type parameter is indexable inside the generic body; its
            // value type is the bound's `V` arg (resolved with sibling params in scope).
            Ty::Param(name) => {
                if let Some((k, v)) = self.param_index_kv(&name) {
                    let idx_ty = self.infer_value(index);
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                        self.error(index.span, format!("index must be {k_s}, found {idx_s}"));
                    }
                    return v;
                }
                self.expect_int(index, "index");
                self.error(obj.span, format!("cannot index into {name}"));
                Ty::Unknown
            }
            other => {
                // A user struct satisfying `Index` (has `index(self, K) -> V`) is indexable by `K`.
                if let Some((k, v)) = self.index_kv(&other) {
                    let idx_ty = self.infer_value(index);
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                        self.error(index.span, format!("index must be {k_s}, found {idx_s}"));
                    }
                    return v;
                }
                self.expect_int(index, "index");
                let note = self.hook_name_note(&other, "Index", "index");
                self.error(obj.span, format!("cannot index into {other}{note}"));
                Ty::Unknown
            }
        }
    }

    /// The `(K, V)` of a bounded type parameter's `Index`/`IndexSet` bound, resolved with the
    /// surrounding params in scope. `None` ⇒ the param has no indexing bound.
    pub(super) fn param_index_kv(&mut self, name: &str) -> Option<(Ty, Ty)> {
        let bound = self
            .type_params
            .get(name)?
            .iter()
            .find(|b| matches!(b.name.as_str(), "Index" | "IndexSet"))
            .cloned()?;
        let k = bound.args.first().cloned().unwrap_or(Ty::Unknown);
        let v = bound.args.get(1).cloned().unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// The `(K, V)` of a bounded type parameter's `IndexSet` bound (write requires `IndexSet`
    /// specifically — a read-only `Index` bound is not assignable). `None` ⇒ no `IndexSet` bound.
    pub(super) fn param_indexset_kv(&mut self, name: &str) -> Option<(Ty, Ty)> {
        let bound = self
            .type_params
            .get(name)?
            .iter()
            .find(|b| b.name == "IndexSet")
            .cloned()?;
        let k = bound.args.first().cloned().unwrap_or(Ty::Unknown);
        let v = bound.args.get(1).cloned().unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// Type `obj[start:end:step]`. Each *present* component must be `int`; the result type follows the
    /// `Slice` protocol — `list[T] → list[T]`, `str → str`, or a struct's
    /// `slice(self, int?, int?, int?) -> R`.
    pub(super) fn infer_slice(
        &mut self,
        obj: &Expr,
        start: Option<&Expr>,
        end: Option<&Expr>,
        step: Option<&Expr>,
        span: Span,
    ) -> Ty {
        // Only the *present* components are constrained to int; an omitted bound/step is `None`.
        for comp in [start, end, step].into_iter().flatten() {
            self.expect_int(comp, "slice bound");
        }
        // A range receiver is a SANCTIONED position: the compiler materializes it and then slices
        // (`CallBuiltin("range", 2)` + `GetSlice`), so `(0..10)[::2]` is a real `List[int]`. Handle
        // it here rather than through `infer_value`, whose `Range` arm rejects every value use.
        let obj_ty = if let ExprKind::Range { start, end } = &obj.kind {
            self.expect_int(start, "range bound");
            self.expect_int(end, "range bound");
            Ty::list(Ty::Int)
        } else {
            self.infer_value(obj)
        };
        if obj_ty.is_unknown() {
            return Ty::Unknown;
        }
        // A bounded `[C: Slice[R]]` type parameter is sliceable inside the generic body; its result
        // type is the bound's `R` arg (resolved with sibling params in scope).
        if let Ty::Param(name) = &obj_ty
            && let Some(bound) = self
                .type_params
                .get(name)
                .and_then(|bs| bs.iter().find(|b| b.name == "Slice").cloned())
        {
            return bound.args.first().cloned().unwrap_or(Ty::Unknown);
        }
        match self.slice_result(&obj_ty) {
            Some(r) => r,
            None => {
                let note = self.hook_name_note(&obj_ty, "Slice", "slice");
                self.error(span, format!("cannot slice {obj_ty}{note}"));
                Ty::Unknown
            }
        }
    }

    /// W7-43 — bind a fresh scratch local to `t` (a carrier operand's ALREADY-inferred type) and hand
    /// back the `Ident` that replaces the operand inside the lowered clone. The caller MUST
    /// [`Self::pop_scope`] once the clone is inferred.
    ///
    /// Why: `lower_carrier_*` lowers a CLONE that still contains the operand, so inferring the clone
    /// inferred the operand a second time. `?.` chains left-nest (`a?.b?.c`'s operand is the previous
    /// carrier), making that `T(n) = 2·T(n-1)` — 22 links took 10s of `chezzi check`, and the checker
    /// runs twice per `run` and once per LSP keystroke. Substituting a pre-typed stand-in makes each
    /// operand infer exactly once, so a chain is linear. It also removes the `errors.truncate(mark)`
    /// rollback the double inference needed: the operand's diagnostics are now emitted exactly once,
    /// by the caller's own `infer_value`, on every arm.
    ///
    /// Three properties this shape depends on:
    /// * **Its own scope.** A fresh scope can't shadow a user name, is removed on every path, and sits
    ///   at or below every `capture_floors` entry — so `is_local_capture` never mistakes the scratch
    ///   for a binding captured by an enclosing `spawn:`/`Executor.submit` and over-fires the
    ///   non-sendable read gate on it.
    /// * **`Span::default()`**, like the `__optN` payload binder `lower_carrier_option` synthesizes.
    ///   No lowered node derives its span from the operand's (both `lower_carrier_*` stamp everything
    ///   from the carrier's own `span`/`name_span`), and a default span can never equal the 1-based
    ///   `hover_probe` position, so the LSP probe keeps landing on the real operand.
    /// * **Side tables are untouched.** `WitnessTable`/`CarrierTable` are keyed by
    ///   source spans and are `HashMap`s, so the operand's entries — recorded by the caller's
    ///   `infer_value` from the ORIGINAL spans — are simply no longer overwritten with themselves.
    fn scratch_operand(&mut self, t: Ty) -> Expr {
        let n = self.next_opt_tmp;
        self.next_opt_tmp += 1;
        let name = format!("__optrecv{n}");
        self.push_scope();
        self.declare(&name, t);
        Expr {
            id: crate::ast::NodeId::SYNTH,
            kind: ExprKind::Ident(name),
            span: Span::default(),
        }
    }

    /// Record one `?.` carrier's lowering, refusing to overwrite a key already bound to a DIFFERENT
    /// mode (W7-49 — see [`crate::checker::record_call_table_entry`]).
    /// W7-49 — record this carrier's chosen lowering, refusing to overwrite a DIFFERENT *settled*
    /// one (see [`crate::checker::record_call_table_entry`] for why that is a hard error).
    ///
    /// [`CarrierMode::Unknown`] is **provisional, not a decision**: the checker types the same
    /// expression more than once by design — `infer_generic_arg_tys`' prepass walks a closure
    /// argument with its params still `Unknown` (`src/checker/expr.rs`), and `infer_fn_ret` walks a
    /// body to infer an unannotated return before the callee it calls is known
    /// (`src/checker/sig.rs`) — and on those early walks the operand types `Unknown`. The settled
    /// walk that follows types it properly. So `Unknown` must never conflict with, nor overwrite, a
    /// settled mode; only `Option`-vs-`Try` is a genuine disagreement. Treating the provisional
    /// value as a decision rejected ordinary two-unannotated-helper and `xs.map(fn(a): a?.len())`
    /// programs — measured, and caught only by adversarial review after a fully green suite.
    fn record_carrier(
        &mut self,
        key: crate::checker::CarrierKey,
        mode: CarrierMode,
        span: Span,
        op: &str,
    ) {
        if mode == CarrierMode::Unknown {
            // Provisional: never displaces a settled decision, and never reports one.
            self.carriers.entry(key).or_insert(CarrierMode::Unknown);
            return;
        }
        if self.carriers.get(&key) == Some(&CarrierMode::Unknown) {
            // A settled decision supersedes the prepass placeholder outright.
            self.carriers.insert(key, mode);
            return;
        }
        crate::checker::record_call_table_entry(
            &mut self.carriers,
            &mut self.table_conflicts,
            key,
            mode,
            &format!("'{op}' lowering"),
            span,
        );
    }

    /// W7-43 — infer a `?.` carrier: type the OPERAND, pick the lowering from it, record the choice
    /// for the compiler, then **clone-and-lower to a real AST shape and infer THAT**.
    ///
    /// Clone-and-lower rather than direct inference because direct inference would re-implement
    /// `infer_call`'s generics + witness recording + keyword resolution against a receiver with no
    /// `Expr` to hang off. It also buys the `Result` mode all of [`Self::infer_try`]'s gates
    /// (`recover_depth`, `in_defer_block`, `in_spawn_block`, the `current_ret`/`in_fn_body`
    /// return-kind gate) with
    /// ZERO new gate code, because the clone literally CONTAINS an `ExprKind::Try` at the right
    /// nesting. Both `lower_carrier_*` stamp every synthesized node from the carrier's own
    /// `span`/`name_span`, so the compiler — calling the same function on the same input — derives
    /// identical spans, and therefore identical `WitnessKey`s.
    ///
    /// ponytail: the reused gates' messages say `'?'`, not `'?.'`. Left verbatim — each message is
    /// TRUE of `?.`, and threading the spelling through would need a saved/restored `self.carrier_op`
    /// field around the nested `infer` below. Upgrade if a report says the wording misleads.
    ///
    /// The clone's OPERAND is swapped for a pre-typed scratch binding first — see
    /// [`Self::scratch_operand`]. Without that swap the operand is inferred twice (once here, once
    /// inside the clone that still contains it) and a chain — which left-nests, so `a?.b?.c`'s
    /// operand IS the previous carrier — costs `T(n) = 2·T(n-1)`.
    pub(super) fn infer_opt_chain(
        &mut self,
        carrier: &Expr,
        obj: &Expr,
        name_span: Span,
        span: Span,
    ) -> Ty {
        let t = self.infer_value(obj);
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        );
        match &t {
            Ty::Result(..) => {
                self.record_carrier(key, CarrierMode::Try, span, "?.");
                let mut c = carrier.clone();
                let scratch = self.scratch_operand(t.clone());
                if let ExprKind::OptChain { obj, .. } = &mut c.kind {
                    **obj = scratch;
                }
                crate::desugar::lower_carrier_try(&mut c);
                let r = self.infer(&c);
                self.pop_scope();
                r
            }
            Ty::Option(..) => {
                self.record_carrier(key, CarrierMode::Option, span, "?.");
                let mut c = carrier.clone();
                let scratch = self.scratch_operand(t.clone());
                if let ExprKind::OptChain { obj, .. } = &mut c.kind {
                    **obj = scratch;
                }
                let tmp = self.next_opt_tmp;
                self.next_opt_tmp += 1;
                crate::desugar::lower_carrier_option(&mut c, tmp);
                let r = self.infer(&c);
                self.pop_scope();
                r
            }
            // The operand already errored (its diagnostic stands, un-truncated) — adding a second
            // one here would be the cascade `Ty::Unknown` exists to suppress.
            Ty::Unknown => {
                self.record_carrier(key, CarrierMode::Unknown, span, "?.");
                self.walk_unknown_carrier(carrier);
                Ty::Unknown
            }
            other => {
                self.record_carrier(key, CarrierMode::Unknown, span, "?.");
                self.error(
                    span,
                    format!("'?.' applies to a `T?` or `T!E` value, found {other}"),
                );
                Ty::Unknown
            }
        }
    }

    /// W7-43 — infer a `??` carrier. `??` accepts BOTH carriers: `Option` unwraps `Some(v)` to `v`,
    /// `Result` unwraps `Ok(v)` to `v` and DISCARDS the `Err` payload (Rust's `unwrap_or`, not `?`).
    /// The decision is recorded under `op_span`, never `Expr::span`: `parse_bp` reuses `lhs.span`
    /// for every infix node and `(e)` grouping keeps the inner span, so in `(a ?? b) ?? c` both
    /// `NullCoalesce` nodes would otherwise share one key.
    /// An `Unknown` operand: the compiler lowers the carrier as an Option (`CarrierMode::Unknown`),
    /// so walk that same lowering to record the names it compiles (TICKET-180). Its diagnostics
    /// are dropped: the operand's own were already reported.
    fn walk_unknown_carrier(&mut self, carrier: &Expr) {
        let mut c = carrier.clone();
        let scratch = self.scratch_operand(Ty::Unknown);
        match &mut c.kind {
            ExprKind::OptChain { obj, .. } => **obj = scratch,
            ExprKind::NullCoalesce { lhs, .. } => **lhs = scratch,
            _ => {}
        }
        let tmp = self.next_opt_tmp;
        self.next_opt_tmp += 1;
        crate::desugar::lower_carrier_option(&mut c, tmp);
        let mark = self.diag_mark();
        self.infer(&c);
        self.diag_rollback(mark);
        self.pop_scope();
    }

    pub(super) fn infer_null_coalesce(&mut self, carrier: &Expr, lhs: &Expr, op_span: Span) -> Ty {
        // Same operand-scratch shape as `infer_opt_chain`, same reason.
        let t = self.infer_value(lhs);
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            op_span,
        );
        match &t {
            Ty::Option(..) => {
                self.record_carrier(key, CarrierMode::Option, op_span, "??");
                let mut c = carrier.clone();
                let scratch = self.scratch_operand(t.clone());
                if let ExprKind::NullCoalesce { lhs, .. } = &mut c.kind {
                    **lhs = scratch;
                }
                let tmp = self.next_opt_tmp;
                self.next_opt_tmp += 1;
                crate::desugar::lower_carrier_option(&mut c, tmp);
                let r = self.infer(&c);
                self.pop_scope();
                // TICKET-064 — `??`'s typed right-hand side is a constraining use of `lhs`, but it
                // never reaches the `drop_empty_site` funnel (only the annotated/argument/return
                // sinks do), so it needs its own carrier-pin record. Must sit AFTER `pop_scope` so
                // `owning_scope` resolves against the real scope stack, not the scratch-operand scope
                // pushed above.
                if let ExprKind::Ident(n) = &lhs.kind
                    && !r.is_unknown()
                    && ty_fully_concrete(&r)
                    && self
                        .lookup(n)
                        .is_some_and(|bt| Self::is_unpinned_carrier(&bt))
                {
                    self.pin_carrier_use(n, &Ty::Option(Box::new(r.clone())));
                }
                r
            }
            Ty::Result(..) => {
                self.record_carrier(key, CarrierMode::ResultCoalesce, op_span, "??");
                let mut c = carrier.clone();
                let scratch = self.scratch_operand(t.clone());
                if let ExprKind::NullCoalesce { lhs, .. } = &mut c.kind {
                    **lhs = scratch;
                }
                let tmp = self.next_opt_tmp;
                self.next_opt_tmp += 1;
                crate::desugar::lower_carrier_result_coalesce(&mut c, tmp);
                let r = self.infer(&c);
                self.pop_scope();
                r
            }
            Ty::Unknown => {
                self.record_carrier(key, CarrierMode::Unknown, op_span, "??");
                self.walk_unknown_carrier(carrier);
                Ty::Unknown
            }
            other => {
                self.record_carrier(key, CarrierMode::Unknown, op_span, "??");
                self.error(
                    op_span,
                    format!("'??' applies to a `T?` or `T!E` value, found {other}"),
                );
                Ty::Unknown
            }
        }
    }

    /// The diagnostic for a `?` whose enclosing function returns neither `Result` nor `Option`.
    ///
    /// W7-51 — inside a synthesized default-argument provider the generic wording would name a
    /// return type the user never wrote (the provider is declared `-> <the parameter's type>`), and
    /// the advice "make the function return Result" is impossible to act on. A default is evaluated
    /// in its DEFINING module, where there is no caller to propagate to, so say that instead.
    fn try_outside_carrier_msg(&self, ret: &Ty) -> String {
        if self.in_default_provider {
            "a default expression cannot propagate with `?` — defaults are evaluated in their \
             defining module, which has no caller to propagate to; use `??` or produce a `T?` value"
                .to_string()
        } else if matches!(ret, Ty::Unknown) {
            "'?' used in a function whose return type is not declared; declare it to return \
             a `T?` or `T!E` value (e.g. `-> int?`)"
                .to_string()
        } else {
            format!("'?' used in a function that returns {ret}, not a `T?` or `T!E` value")
        }
    }

    pub(super) fn infer_try(&mut self, inner: &Expr, span: Span) -> Ty {
        let t = self.infer(inner);
        // Inside a `recover:` block, `?` short-circuits to the boundary (try-block style), not the
        // enclosing function. The boundary's error type is `Error`, and its result is `Result`-typed,
        // so only a `Result` operand fits — `?` on an `Option` is rejected here.
        if self.recover_depth > 0 {
            return match t {
                Ty::Result(ok, err) => {
                    // A `recover:` result's error slot is the built-in `Error` existential (sendable,
                    // like every protocol) — the recover result (`Result[_, Error]`) is itself
                    // sendable, so a propagated error must satisfy Error AND be sendable, else a
                    // non-sendable payload would launder through the erased slot across a task
                    // boundary. Split the diagnostic so a satisfies-but-non-sendable error is not
                    // mislabelled as failing to satisfy Error (it does — it's merely non-sendable).
                    if self.satisfies(&err, "Error").is_err() {
                        self.error(
                            span,
                            format!("'?' inside a recover block propagates error {err}, which must satisfy Error"),
                        );
                    } else if !self.sendable(&err) {
                        self.error(
                            span,
                            format!("'?' inside a recover block propagates error {err}, which satisfies Error but isn't sendable — a recover result's error type is the sendable `Error`; name a sendable error type"),
                        );
                    }
                    *ok
                }
                Ty::Unknown => Ty::Unknown,
                Ty::Option(_) => {
                    self.error(span, "'?' on a `T?` value is not allowed inside a recover block (its result is a `T!E` value); use match instead".to_string());
                    Ty::Unknown
                }
                other => {
                    self.error(
                        span,
                        format!("'?' expects a `T?` or `T!E` value, found {other}"),
                    );
                    Ty::Unknown
                }
            };
        }
        // Inside a `defer:` block (but not a `recover:` nested in it — that's handled above), a `?`
        // is DISCARDED at the block boundary: the block is its own closure with no error-return
        // contract, so a fired Err/None just short-circuits the cleanup and is dropped
        // (`syntax.md`). The enclosing function's return type is therefore irrelevant — accept any
        // Result/Option and yield the success payload; a non-sum operand is rejected as everywhere.
        if self.in_defer_block {
            return match t {
                Ty::Result(ok, _) => *ok,
                Ty::Option(inner) => *inner,
                Ty::Unknown => Ty::Unknown,
                other => {
                    self.error(
                        span,
                        format!("'?' expects a `T?` or `T!E` value, found {other}"),
                    );
                    Ty::Unknown
                }
            };
        }
        // A spawned task is its own frame with NO CALLER: the nursery discards a task's returned
        // `Err` by design (W7-46, Go's contract), so a `?` here propagates to nothing — reject it.
        // The gate order is load-bearing, and is why the spawn arm zeroes `recover_depth` /
        // `in_defer_block`: a `recover:`/`defer:` OUTSIDE the spawn has its state zeroed at the task
        // boundary, so its `?` falls through to here and is rejected; one nested INSIDE the spawn
        // re-arms from zero, so its own gate above fires first and stays legal (its boundary is in
        // the same frame as the `?`).
        // Shaped like the two gates above: a non-carrier operand keeps the `expects Result or
        // Option` diagnostic (the spawn message would HIDE that defect), and `Unknown` stays silent
        // so an already-reported operand does not cascade a second error.
        if self.in_spawn_block {
            return match t {
                Ty::Result(..) | Ty::Option(_) => {
                    self.error(
                        span,
                        "'?' is not allowed inside a spawn block: a spawned task has no caller to propagate to".to_string(),
                    );
                    Ty::Unknown
                }
                Ty::Unknown => Ty::Unknown,
                other => {
                    self.error(
                        span,
                        format!("'?' expects a `T?` or `T!E` value, found {other}"),
                    );
                    Ty::Unknown
                }
            };
        }
        // The enclosing function must be able to early-return the Err/None. The operand's sum-type
        // KIND must match the enclosing return's KIND — a Result-`?` early-returns an `Err`, so the
        // function must itself return `Result`; an Option-`?` early-returns a `None`, so it must
        // return `Option`. `Nil` accepts either ONLY at MODULE TOP-LEVEL (`!in_fn_body` — the runtime
        // unwinds the unhandled Err/None at the program boundary); a nil-returning fn body (named OR
        // nested, `in_fn_body == true`) REJECTS, since the propagated Err/None would be silently
        // swallowed (a fn must return Result/Option to use `?` — no `fn main` exception). Mixing kinds
        // would make the function return the wrong sum-type and fault a downstream exhaustive
        // `match`/`??` at runtime even though `check` passed.
        match t {
            Ty::Result(ok, err) => {
                match self.current_ret.clone() {
                    // Propagating an `Err` early-returns it as the enclosing function's error, so the
                    // inner error type must fit the enclosing one (Rust-like).
                    Ty::Result(_, re) => {
                        if !self.assignable(&re, &err) {
                            self.error(
                                span,
                                format!("'?' propagates error {err}, but the enclosing function's error type is {re}"),
                            );
                        }
                    }
                    // Module top-level ONLY (`!in_fn_body`) — the runtime unwinds the Err at the
                    // program boundary. Inside a nil-returning fn body the flag is true, so this arm
                    // fails its guard and falls through to `other =>`, rejecting the swallow.
                    Ty::Nil if !self.in_fn_body => {}
                    Ty::Option(_) => {
                        self.error(
                            span,
                            "'?' propagates an error, but the enclosing function returns a `T?` value, not a `T!E` value".to_string(),
                        );
                    }
                    other => {
                        self.error(span, self.try_outside_carrier_msg(&other));
                    }
                }
                *ok
            }
            Ty::Option(inner) => {
                match self.current_ret.clone() {
                    Ty::Option(_) => {}
                    // Module top-level ONLY — see the Result arm above; a nil fn body rejects.
                    Ty::Nil if !self.in_fn_body => {}
                    Ty::Result(..) => {
                        self.error(
                            span,
                            "'?' propagates a None, but the enclosing function returns a `T!E` value, not a `T?` value".to_string(),
                        );
                    }
                    other => {
                        self.error(span, self.try_outside_carrier_msg(&other));
                    }
                }
                *inner
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(
                    span,
                    format!("'?' expects a `T?` or `T!E` value, found {other}"),
                );
                Ty::Unknown
            }
        }
    }

    /// `json.decode[T](s)` — the source must be `str`, the target `T` must be decodable. Yields
    /// `Result[T]`. (`obj` is the json-module expression; we infer it only to surface a bad-module
    /// error, but place no constraint on it — any module exposing `parse` works at runtime.)
    pub(super) fn infer_decode(
        &mut self,
        id: crate::ast::NodeId,
        obj: &Expr,
        ty: &Type,
        arg: &Expr,
        span: Span,
    ) -> Ty {
        let _ = self.infer(obj);
        let arg_ty = self.infer_value(arg);
        if !self.join_ty(&Ty::Str, &arg_ty) {
            self.error(span, format!("decode source must be str, found {arg_ty}"));
        }
        let target = self.resolve_type(ty, span);
        if !self.record_decode(id, &target, span) {
            return Ty::Unknown;
        }
        Ty::result(target)
    }

    /// THE one answer to "is `m.name` std.json's decode" (TICKET-187/214). Read by the call arm,
    /// `module_fn` and every decode value record.
    pub(super) fn json_decode_member(&self, m: &str, name: &str) -> bool {
        name == "decode"
            && matches!(self.head_binding(m), HeadBinding::Module)
            && self.json_module.is_some()
            && self.imported_modules.get(m) == self.json_module.as_ref()
    }

    /// A decode value pinned to `ty` (`fn(str) -> Result[X]`): record `X`'s descriptor on `id` and
    /// return `ty`, or `Unknown` when `X` is not decodable.
    pub(super) fn record_decode_value(&mut self, id: crate::ast::NodeId, ty: Ty, span: Span) -> Ty {
        let target = match &ty {
            Ty::Func { ret, .. } => match ret.as_ref() {
                Ty::Result(t, _) => (**t).clone(),
                _ => return ty,
            },
            _ => return ty,
        };
        if self.record_decode(id, &target, span) {
            ty
        } else {
            Ty::Unknown
        }
    }

    /// std.json's decode read as a value (TICKET-214). Its record is its descriptor, so it is
    /// written only at a FINAL verdict, and only `Pinned` records: a read whose `T` nothing pins is
    /// rejected, because the value is compiled per `T`. A read nothing pins at the read defers
    /// (TICKET-225); its frame verdict records or rejects it. `None` when `obj.name` is not that
    /// decode.
    fn decode_value(
        &mut self,
        id: crate::ast::NodeId,
        obj: &Expr,
        name: &str,
        name_span: Span,
    ) -> Option<Ty> {
        let ExprKind::Ident(m) = &obj.kind else {
            return None;
        };
        if !self.json_decode_member(m, name) {
            return None;
        }
        // The compiler loads the module head as a value.
        self.infer(obj);
        let sig = json_decode_sig();
        let display = format!("{m}.{name}");
        let spelling = fn_spelling(&display, &sig.type_params);
        let ty = self.generic_fn_value_ty(id, &display, &sig, &spelling, name_span);
        Some(match ty {
            Some(Ty::Unknown) => Ty::Unknown,
            // TICKET-225: deferred — the frame verdict records or rejects it.
            Some(t) if super::tyvar::has_var(&t) => {
                self.mark_decode_read(id);
                t
            }
            Some(t) => self.record_decode_value(id, t, name_span),
            None => {
                self.reject_undetermined_generic_fn_value(&display, &sig, &spelling, name_span);
                Ty::Unknown
            }
        })
    }

    /// A free closure's param type, inferred from how its body USES the param (sources #2/#3 — only
    /// when there is no expected/slot type). **Shallow + precise** (closes the bare-param structural
    /// trap without over-pinning): source #2 — a `match` whose scrutinee is the BARE param identifier
    /// (`match x:`, not `match x.f:` / `match g(x):`), pinned from its first concrete arm; source #3 —
    /// a member access on the bare param (`x.f` / `x.m()`) whose name is declared by exactly one struct.
    /// Source #2 wins. Does NOT descend into nested closures (an inner param shadowing the name is
    /// unrelated). Read-only; returns `None` when nothing pins the param.
    pub(super) fn scan_free_closure_param(&self, name: &str, body: &Expr) -> Option<Ty> {
        let mut match_pin = None;
        let mut member_pin = None;
        self.scan_expr_for_pin(name, body, &mut match_pin, &mut member_pin);
        match_pin.or(member_pin)
    }

    /// Walk `e` (skipping nested closures) accumulating the source-#2 (`match_pin`) and source-#3
    /// (`member_pin`) candidates for a free closure's param `name`. Stops descending once a source-#2
    /// pin is found (highest priority). See [`Checker::scan_free_closure_param`].
    pub(super) fn scan_expr_for_pin(
        &self,
        name: &str,
        e: &Expr,
        match_pin: &mut Option<Ty>,
        member_pin: &mut Option<Ty>,
    ) {
        if match_pin.is_some() {
            return;
        }
        match &e.kind {
            // Source #2: a match whose scrutinee is the BARE param — pin from the first concrete arm.
            ExprKind::Match { scrutinee, arms } => {
                if let ExprKind::Ident(s) = &scrutinee.kind
                    && s == name
                {
                    for arm in arms {
                        if let Some(t) = self.pin_ty_of_pattern(&arm.pattern) {
                            *match_pin = Some(t);
                            return;
                        }
                    }
                }
                self.scan_expr_for_pin(name, scrutinee, match_pin, member_pin);
                for arm in arms {
                    // Scope-awareness: an arm whose pattern BINDS `name` (a tuple/variant sub-position
                    // or a bare catch-all of the same spelling) shadows the closure param inside the
                    // guard + body — a `match <name>:` there reads that binding, not the param, so it
                    // must NOT pin. Skip the shadowed arm's guard/body.
                    if pattern_binds(&arm.pattern, name) {
                        continue;
                    }
                    if let Some(g) = &arm.guard {
                        self.scan_expr_for_pin(name, g, match_pin, member_pin);
                    }
                    self.scan_expr_for_pin(name, &arm.body, match_pin, member_pin);
                }
            }
            // Source #3: a member access (field or method receiver) on the bare param.
            ExprKind::Field {
                obj, name: member, ..
            } => {
                if member_pin.is_none()
                    && let ExprKind::Ident(r) = &obj.kind
                    && r == name
                    && let Some(t) = self.unique_member_owner(member)
                {
                    *member_pin = Some(t);
                }
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
            }
            // A nested closure is its own scope — never descend (it may shadow `name`).
            ExprKind::Closure { .. } => {}
            // Every other expression: recurse into its child expressions.
            ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
                for c in es {
                    self.scan_expr_for_pin(name, c, match_pin, member_pin);
                }
            }
            ExprKind::Map(pairs) => {
                for (k, v) in pairs {
                    self.scan_expr_for_pin(name, k, match_pin, member_pin);
                    self.scan_expr_for_pin(name, v, match_pin, member_pin);
                }
            }
            ExprKind::Comprehension {
                key, elem, clauses, ..
            } => {
                // Scope-awareness: a clause's `vars` shadow `name` for every LATER clause's
                // iter/guards and for the key/elem. Scan each clause's iter (evaluated before this
                // clause binds), then stop once a clause binds `name` — its own guards and everything
                // downstream read the shadowing binding, not the param.
                let mut shadowed = false;
                for c in clauses {
                    if !shadowed {
                        self.scan_expr_for_pin(name, &c.iter, match_pin, member_pin);
                    }
                    if c.vars.iter().any(|v| v == name) {
                        shadowed = true;
                    }
                    if !shadowed {
                        for g in &c.guards {
                            self.scan_expr_for_pin(name, g, match_pin, member_pin);
                        }
                    }
                }
                if !shadowed {
                    if let Some(k) = key {
                        self.scan_expr_for_pin(name, k, match_pin, member_pin);
                    }
                    self.scan_expr_for_pin(name, elem, match_pin, member_pin);
                }
            }
            ExprKind::Unary { expr, .. } => {
                self.scan_expr_for_pin(name, expr, match_pin, member_pin)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.scan_expr_for_pin(name, lhs, match_pin, member_pin);
                self.scan_expr_for_pin(name, rhs, match_pin, member_pin);
            }
            ExprKind::Compare { operands, .. } => {
                for o in operands {
                    self.scan_expr_for_pin(name, o, match_pin, member_pin);
                }
            }
            ExprKind::Range { start, end } => {
                self.scan_expr_for_pin(name, start, match_pin, member_pin);
                self.scan_expr_for_pin(name, end, match_pin, member_pin);
            }
            ExprKind::Call {
                callee,
                args,
                bracket,
                ..
            } => {
                self.scan_expr_for_pin(name, callee, match_pin, member_pin);
                for a in args.iter().chain(bracket.as_deref()) {
                    self.scan_expr_for_pin(name, a, match_pin, member_pin);
                }
            }
            ExprKind::Index { obj, index, .. } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                if let Some(index) = index {
                    self.scan_expr_for_pin(name, index, match_pin, member_pin);
                }
            }
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                for c in [start, end, step].into_iter().flatten() {
                    self.scan_expr_for_pin(name, c, match_pin, member_pin);
                }
            }
            ExprKind::Try(inner) => self.scan_expr_for_pin(name, inner, match_pin, member_pin),
            ExprKind::IfElse { cond, then, els } => {
                self.scan_expr_for_pin(name, cond, match_pin, member_pin);
                self.scan_expr_for_pin(name, then, match_pin, member_pin);
                self.scan_expr_for_pin(name, els, match_pin, member_pin);
            }
            // String interpolation: the `{…}` fragment expressions are not stored as child `Expr`s —
            // they live inside the raw text and are produced on demand by the shared interpolation
            // parser (the same one `check_interpolation` uses). Parse + scan them so a param pinned
            // ONLY by a member access inside an interpolation (`"{x.f}"`) resolves via source #3. A
            // malformed interpolation is ignored here (it is diagnosed by `check_interpolation`).
            ExprKind::Str(raw) => {
                if let Ok(chunks) = crate::interpolation::parse_interpolation(raw, e.span) {
                    for chunk in &chunks {
                        if let crate::ast::Chunk::Expr(frag, _, fields) = chunk {
                            self.scan_expr_for_pin(name, frag, match_pin, member_pin);
                            for f in fields {
                                self.scan_expr_for_pin(name, f, match_pin, member_pin);
                            }
                        }
                    }
                }
            }
            // The desugared form — the fragments are already parsed children here.
            ExprKind::Interp(chunks) => {
                for chunk in chunks {
                    if let crate::ast::Chunk::Expr(frag, _, fields) = chunk {
                        self.scan_expr_for_pin(name, frag, match_pin, member_pin);
                        for f in fields {
                            self.scan_expr_for_pin(name, f, match_pin, member_pin);
                        }
                    }
                }
            }
            // `?.`/`??` carriers are lowered before checking. `recover:` carries a statement block
            // (which can introduce its own bindings); it is NOT scanned — a param pinnable only from
            // inside a `recover:` body stays un-inferable and requires an annotation (sound: this is
            // the conservative v1 fallback, never a mis-pin). Leaves (`Ident`/literals/`RawStr`)
            // have no child to scan.
            _ => {}
        }
    }

    /// The scrutinee type a top-level match arm pattern implies (source #2 classification). Mirrors
    /// [`Checker::reconstruct_unknown_kind`]'s arm classification: a qualified/unique enum variant or
    /// builtin `Ok`/`Err`/`Some`/`None` → that enum/Result/Option (type args `Unknown`); a tuple → an
    /// all-`Unknown` tuple of that arity; a literal/range → its scalar; a binding/wildcard/ambiguous →
    /// `None` (no pin).
    pub(super) fn pin_ty_of_pattern(&self, p: &Pattern) -> Option<Ty> {
        match p {
            Pattern::Or(alts) => alts.first().and_then(|a| self.pin_ty_of_pattern(a)),
            // `?v` fits a `T?` and a `T!E` alike: no pin.
            Pattern::Carrier { .. } => None,
            Pattern::Tuple(subs) => Some(Ty::Tuple(vec![Ty::Unknown; subs.len()])),
            Pattern::Literal(lit) => Some(lit_pattern_ty(lit)),
            Pattern::Range { .. } => Some(Ty::Int),
            Pattern::Variant {
                name,
                enum_name,
                module_name,
                ..
            } => {
                // A module-qualified variant can't be resolved through the bare-name table — no pin.
                if module_name.is_some() {
                    return None;
                }
                if let Some(en) = enum_name {
                    let key = self.bare_key(en);
                    return self
                        .enums
                        .contains_key(&key)
                        .then(|| self.enum_ty_unknown_args(&key));
                }
                match name.as_str() {
                    other if self.imported_variants.contains_key(other) => {
                        let key = self.imported_variants[other].head.key.clone();
                        Some(self.enum_ty_unknown_args(&key))
                    }
                    other => {
                        // A bare variant uniquely owned by one enum pins it; an ambiguous one, or a
                        // bare binding name (not a known variant), does not.
                        let owners = self.variant_owners.get(other)?;
                        if owners.len() != 1 {
                            return None;
                        }
                        let key = self.bare_key(&owners[0]);
                        self.enums
                            .contains_key(&key)
                            .then(|| self.enum_ty_unknown_args(&key))
                    }
                }
            }
            Pattern::Ident(..) | Pattern::Wildcard => None,
        }
    }

    /// A user enum's `Ty::Enum` with its type arguments filled as `Unknown` (the scrutinee shape an
    /// arm pattern pins — element types are unknown, the enum identity is what matters for call-site
    /// checking).
    pub(super) fn enum_ty_unknown_args(&self, key: &str) -> Ty {
        let n = self
            .enum_type_params
            .get(key)
            .map(|tps| tps.len())
            .unwrap_or(0);
        Ty::enum_ty(key.to_string(), vec![Ty::Unknown; n])
    }

    /// If exactly one struct declares a field OR method `member`, return that struct's type (type args
    /// `Unknown`); else `None` (source #3 only fires for a UNIQUELY-owned member — a name shared by
    /// >1 type, or none, never pins). Read-only.
    pub(super) fn unique_member_owner(&self, member: &str) -> Option<Ty> {
        // A member shared by any PARAMETERIZED collection (`len`/`map`/`get`/`push`/… on
        // `list`/`map`/`set`) is never a unique pin: it is shared across types AND would only pin a
        // weak `list[Unknown]`-style type (design §3 — "methods/fields shared by >1 type … never
        // pin"). Bail before collecting owners so such members fall through to the annotation rule.
        // Phase 5a-containers — the `List`/`Map`/`Set` method tables are harvested from
        // `std/prelude.chz` and re-seeded into `self.structs` by `seed_stdlib_structs`; check them for
        // membership (the retired `list_method_sig`/`map_method_sig`/`set_method_sig` arms' replacement).
        // As of phase 6 the `List` table ALSO contains the closure-driven HOFs (`map`/`filter`/`fold`/
        // `sort_by`/`sort_by_key`, formerly the bespoke `infer_list_hof` arm), so those names now bail
        // here too — correct, since they ARE shared by the parameterized `List` (a name shared across
        // types never uniquely pins), matching the design's collection-method bail.
        if ["List", "Map", "Set"].iter().any(|ty| {
            self.structs
                .get(*ty)
                .is_some_and(|info| info.methods.contains_key(member))
        }) {
            return None;
        }
        // Collect every PINNABLE owner of `member`: user structs (by field or method) plus the
        // concrete scalar builtins `str`/`bytes` (the design's `x.upper()` → `str` case). The pin
        // fires only when exactly one type owns it.
        let mut owners: Vec<Ty> = Vec::new();
        for (key, info) in &self.structs {
            // Source #3 pins only from a USER type. The `Builtin`-origin native structs
            // (Match/Response/ProcResult/…) are seeded into `self.structs` unconditionally at init
            // regardless of imports, so scanning them would mis-pin a param to an unimported,
            // unreferenced builtin (their fields like `end`/`code`/`status`). Skip them.
            if info.origin == StructOrigin::Builtin {
                continue;
            }
            let has =
                info.fields.iter().any(|(n, _)| n == member) || info.methods.contains_key(member);
            if has {
                let n = info.type_params.len();
                owners.push(Ty::Struct(key.clone(), vec![Ty::Unknown; n]));
            }
        }
        // `str`/`bytes` method sets are now the file-backed `native struct` tables seeded into
        // `self.structs` (the retired `str_method_sig`/`bytes_method_sig` replacement); the loop above
        // skips them (Builtin origin), so check them explicitly here to preserve the `x.upper()` → `str`
        // pin case.
        if self
            .structs
            .get("str")
            .is_some_and(|info| info.methods.contains_key(member))
        {
            owners.push(Ty::Str);
        }
        if self
            .structs
            .get("bytes")
            .is_some_and(|info| info.methods.contains_key(member))
        {
            owners.push(Ty::Bytes);
        }
        if owners.len() == 1 {
            owners.pop()
        } else {
            None
        }
    }

    pub(super) fn infer_closure(
        &mut self,
        params: &[Param],
        ret: Option<&Type>,
        body: &Expr,
        expected: Option<&Ty>,
    ) -> Ty {
        self.closure_write_frames
            .push((self.scopes.len(), HashSet::new()));
        // Source #1 — the *expected* type of the slot the closure literal sits in. When it is a
        // `fn(..)` whose arity matches, an UNANNOTATED param binds to the expected param type
        // (checking-mode), and a non-`Unknown` expected return becomes the body's return context.
        // On an arity mismatch the params stay `Unknown` here and the call site's `assignable` check
        // reports the mismatch (single diagnostic).
        let (exp_params, exp_ret): (Option<Vec<Ty>>, Option<Ty>) = match expected {
            Some(Ty::Func {
                params: p, ret: r, ..
            }) if p.len() == params.len() => (Some(p.clone()), Some((**r).clone())),
            _ => (None, None),
        };
        // A `fn`-typed slot whose arity does NOT match: keep unannotated params silently `Unknown`
        // here and let the call site's `assignable` check report the single arity diagnostic — do
        // NOT route them through the free-closure scan (which would emit a spurious, misdirecting
        // "cannot infer type of parameter").
        let expected_arity_mismatch =
            matches!(expected, Some(Ty::Func { params: p, .. }) if p.len() != params.len());
        // A closure body opens a fresh loop context (same rule as `check_fn_body`): a loop around
        // the closure's definition must not make a `break`/`continue` inside it legal.
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        let saved_recover = std::mem::replace(&mut self.recover_depth, 0);
        let saved_in_defer = std::mem::replace(&mut self.in_defer_block, false);
        // `?` inside the body targets THIS closure's return, not the enclosing function's. With no
        // annotation there is no Result/Option context, so `?` is rejected (`Unknown` → `infer_try`
        // errors). Mirrors `check_fn_body`'s `current_ret` handling. An expected (slot) return type
        // supplies that context when the closure is unannotated.
        let declared_ret = ret
            .map(|t| self.resolve_type(t, body.span))
            .or_else(|| exp_ret.clone().filter(|r| !r.is_unknown()))
            .unwrap_or(Ty::Unknown);
        let saved_ret = std::mem::replace(&mut self.current_ret, declared_ret);
        // A closure body is a fn body: a `?` on a `Nil`-returning closure is rejected here (already the
        // pre-existing behavior via `current_ret == Unknown` for an unannotated closure; this keeps the
        // signal exact for an explicitly `-> nil` closure too). Saved/restored beside `current_ret`.
        let saved_in_fn = std::mem::replace(&mut self.in_fn_body, true);
        // …and a closure inside a default-argument provider has its OWN caller (W7-51).
        let saved_in_dflt = std::mem::replace(&mut self.in_default_provider, false);
        // A closure DECLARED inside a `spawn:` block is not itself the task — it has a caller, so a
        // `?` in its body targets the closure's own return (W7-48). Saved/restored beside
        // `current_ret`. W8-3 — the airlock taint is per-frame for the same reason, and
        // `enter_own_frame` moves the pair so neither can be reset without the other (this site was
        // the one that cleared `in_spawn_block` alone: the closure body then reported the enclosing
        // task's pending write AND ate the entry, so the parent's real stale read went silent).
        let saved_frame = self.enter_own_frame();
        // A closure inside a generator is NOT itself a generator: clear the yield context so a stray
        // `yield` in the closure is diagnosed as "outside a generator", not bound to the enclosing
        // one. (Closure bodies are single expressions today, so this is a latent-invariant guard.)
        let saved_yield = self.yield_ty.take();
        // Same for the in-bounds signal: a `yield` inside the closure must be out-of-bounds, and must
        // not seed the enclosing generator's `collected_yields` during inference. (Defensive — mirrors
        // `yield_ty.take()`; closures are single-expression so a closure `yield` is unparseable today.)
        let saved_ig = std::mem::replace(&mut self.in_generator, false);
        let saved_gf = self.gen_frame.take();
        // M24 Task 4: the witness scope CARRIES INTO a closure body. `$w:T` is never a free variable
        // (it is unspellable), so `compile_closure` appends it to the capture entries explicitly —
        // and, since M24-2, only where the body can REACH it, which is a strict superset of what
        // this scope licenses (`compiler::nested_body_needs_witness`). The witness crosses BY VALUE,
        // so a closure that outlives its defining frame still constructs the right type.
        // Mark BEFORE param binding so the free-closure finalize (below) is suppressed if EITHER an
        // un-inferable PARAM (`cannot infer type of parameter`) or the body emits a real error — a
        // residual `Unknown` return is then a cascade, not a genuine un-inferable return.
        // NOT a speculative-rollback site (hence the raw length, not `diag_mark`): the closure body is
        // checked exactly ONCE here, so its diagnostics — errors and any future warning alike — stay.
        // This mark is only read, to answer "did the body error?".
        let closure_mark = self.errors.len();
        self.push_scope();
        let param_tys: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // An annotated param keeps its type; an unannotated param takes the expected (slot)
                // param type (source #1), else is inferred from the body, else `Unknown`.
                let ty = match &p.ty {
                    Some(t) => self.resolve_type(t, body.span),
                    None => {
                        // An unannotated param: prefer the expected (slot) param type (source #1);
                        // else infer it from the body — source #2 (a match whose scrutinee is the
                        // bare param) / source #3 (a uniquely-owned member access).
                        //
                        // An `Unknown` expected param type is NOT a pin: it arises when a generic
                        // slot's type param was unified ONLY from this closure (`store(fn(a): …)` →
                        // `T = fn(Unknown) -> Unknown`), so binding the param to it silently would
                        // leave the call site unchecked → check-passes-then-traps. Filter it out and
                        // fall through to the body scan / annotation requirement (soundness).
                        if let Some(t) = exp_params
                            .as_ref()
                            .and_then(|ps| ps.get(i))
                            .filter(|t| !t.is_unknown())
                            .cloned()
                        {
                            t
                        } else if expected_arity_mismatch {
                            // Arity mismatch against a `fn`-typed slot — stay `Unknown`; the call
                            // site reports the mismatch (single diagnostic).
                            Ty::Unknown
                        } else if self.generic_arg_prepass {
                            // Generic unification prepass: keep the param `Unknown` so the other
                            // args / substituted slot type drive unification; `check_generic_arg`
                            // re-infers it in checking-mode afterwards. Running the free scan here
                            // would corrupt unification (see `generic_arg_prepass` doc).
                            Ty::Unknown
                        } else if let Some(t) = self.scan_free_closure_param(&p.name, body) {
                            t
                        } else {
                            // Genuinely unresolved: no expected/slot type and nothing in the body
                            // pins it. Require an annotation rather than degrade the param to a
                            // runtime `Unknown` value (the one place `Unknown` could reach a value).
                            // Bind `Unknown` after erroring so the body still checks (no cascade).
                            self.error(
                                p.name_span,
                                format!(
                                    "cannot infer type of parameter '{}'; add a type annotation",
                                    p.name
                                ),
                            );
                            Ty::Unknown
                        }
                    }
                };
                // Editor hover: record the closure param's type at its DECL-site name span (no-op
                // off-probe; first-hit-wins, so a body-use span records separately). SKIP during the
                // generic-arg unification prepass: there an unannotated param is forced `Unknown`
                // (see the `generic_arg_prepass` arm above), and first-hit-wins would latch that `?`
                // over the real type the later per-arg check (run with the substituted slot type and
                // `generic_arg_prepass=false`) infers — so `xs.map(fn(a): a + 1)` would hover `?`.
                if !self.generic_arg_prepass {
                    self.hover_record_at(p.name_span, &ty, HoverKind::Param, None);
                }
                self.declare(&p.name, ty.clone());
                ty
            })
            .collect();
        // TICKET-227: the body's slot is the declared return, else a concrete return of the
        // `fn`-typed slot this closure lands in.
        let slot = if ret.is_some() {
            Some(self.current_ret.clone())
        } else {
            match expected {
                Some(Ty::Func { ret: er, .. }) if ty_fully_concrete(er) => Some((**er).clone()),
                _ => None,
            }
        };
        let body_ty = self.infer_in_slot(body, slot);
        self.last_closure_writes = self
            .closure_write_frames
            .pop()
            .map(|(_, writes)| writes)
            .unwrap_or_default();
        if !self.last_closure_writes.is_empty() {
            let mut writes: Vec<String> = self.last_closure_writes.iter().cloned().collect();
            writes.sort();
            self.closure_literal_writes
                .insert((self.graph_module_idx, body.span), writes);
        }
        let closure_had_err = self.errors.len() > closure_mark;
        self.pop_scope();
        self.loop_depth = saved_loop_depth;
        self.recover_depth = saved_recover;
        self.in_defer_block = saved_in_defer;
        self.current_ret = saved_ret;
        self.in_fn_body = saved_in_fn;
        self.in_default_provider = saved_in_dflt;
        self.exit_own_frame(saved_frame);
        self.yield_ty = saved_yield;
        self.in_generator = saved_ig;
        self.gen_frame = saved_gf;
        let ret_ty = match ret {
            Some(t) => {
                let declared = self.resolve_type(t, body.span);
                // The body owned the declared return as its slot, so a wrapped body already has
                // the declared type (TICKET-227).
                if !self.assignable(&declared, &body_ty) {
                    self.error(
                        body.span,
                        format!(
                            "closure body has type {body_ty}, but its return type is {declared}{}",
                            float_fix_note(&declared, &body_ty)
                        ),
                    );
                }
                declared
            }
            // An un-annotated closure's body IS its inferred return — apply the SAME finalize as a
            // free fn/method (Result E-slot default + reject a residual un-inferable `Unknown`), but
            // ONLY for a GENUINELY FREE closure literal. Gated on `expected.is_none()` so a closure
            // sitting in a `fn`-typed slot (source #1) is untouched, and `!generic_arg_prepass` so the
            // proto.rs generic/HOF loop-back contexts (where an `Unknown`/`Param` return is legit and
            // resolved later) are excluded. `!body_had_err` avoids piling onto a real body error.
            None => {
                if expected.is_none() && !self.generic_arg_prepass && !closure_had_err {
                    self.finalize_ret(&body_ty, "<closure>", body.span, false)
                } else {
                    body_ty
                }
            }
        };
        // A closure/lambda value carries its param names as labels, so a keyword call through a
        // closure value (`cb := fn(name: str): …; cb(name="X")`) resolves.
        let labels: Vec<Option<String>> = params.iter().map(|p| Some(p.name.clone())).collect();
        Ty::Func {
            params: param_tys,
            ret: Box::new(ret_ty),
            labels: FnLabels::new(labels),
        }
    }

    // ===== calls =====
}

/// Map a type to the [`crate::fmtspec::ScalarKind`] it renders as for a static format-spec check —
/// but ONLY for CONCRETE scalars. `bool` folds into `Str` (it renders via the runtime `FmtArg::Other`
/// → `render_str` path). Everything else returns `None`: a type that renders as its text form is
/// classified by [`renders_as_text`] (and checked against `ScalarKind::Str` by
/// `Checker::format_spec_kind`), and `Unknown`/`Param(T)`/protocols keep the runtime backstop — the
/// soundness boundary that lets a generic body `"{v:.2f}"` (v: T could be float) pass check.
fn scalar_kind_of(ty: &Ty) -> Option<crate::fmtspec::ScalarKind> {
    use crate::fmtspec::ScalarKind;
    match ty {
        Ty::Int => Some(ScalarKind::Int),
        Ty::Float => Some(ScalarKind::Float),
        Ty::Str | Ty::Bool => Some(ScalarKind::Str),
        _ => None,
    }
}

/// TICKET-142 (W14-19): does a value of this CONCRETE type render through the runtime's text form
/// (`render_str`) when a format spec is present? Measured for every native struct (`FileInfo`,
/// `AtomicInt`, `Atomic`, `Executor`, `RwShared`, `Channel`, `Writer`, `ptr`): none renders as a
/// scalar. An exhaustive `match` with no `_` arm, so a new `Ty` variant does not compile until it is
/// classified — a type whose runtime value IS a scalar must return `false` here.
fn renders_as_text(ty: &Ty) -> bool {
    match ty {
        Ty::Bytes
        | Ty::ByteArray
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Tuple(_)
        | Ty::Option(_)
        | Ty::Result(..)
        | Ty::Func { .. }
        | Ty::BuiltinFn { .. }
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr => true,
        Ty::Int
        | Ty::Width(_)
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Nil
        | Ty::Param(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Var(_)
        | Ty::Unknown => false,
    }
}

/// Written type arguments and the span their diagnostics anchor on.
pub(super) type WrittenTypeArgs = (Vec<Type>, Span);

/// One fn-like path read as a value (TICKET-204): a fn, `m.f`, or a type member (a static method,
/// an instance method through its type, a payload variant). Built by [`Checker::path_fn`].
pub(super) struct PathFn {
    /// The path as written, head args included (`pair`, `lib.pair`, `Bx[int].put`).
    pub(super) display: String,
    /// The type's spelling (`Bx`, `vlib.Bx`) for head-arg diagnostics; empty for a fn.
    pub(super) head_spelled: String,
    /// The value's signature; `type_params` holds the kept head params (the first `head_params`)
    /// then the item's own.
    pub(super) sig: FnSig,
    /// The type's declared params, against which head args are arity-checked and mapped.
    pub(super) head_decl: Vec<TyParam>,
    pub(super) head_params: usize,
    /// The written head args (`Bx[int].make`).
    pub(super) head_args: Option<WrittenTypeArgs>,
    /// An alias head's pinned args (`B.make`); at most one of this and `head_args` is `Some`.
    pub(super) head_pinned: Option<Vec<Ty>>,
    /// `None` for a fn (it records `Resolution::Fn`), else `MethodFn` / `VariantFn`.
    pub(super) res: Option<Resolution>,
    /// The instantiation hint (`pair[<A>, <B>]`, `R1[<T>].L`, `Bx[<T>].put[<U>]`).
    pub(super) spelling: String,
}

impl PathFn {
    fn of_fn(display: String, sig: FnSig) -> PathFn {
        PathFn {
            spelling: fn_spelling(&display, &sig.type_params),
            display,
            head_spelled: String::new(),
            sig,
            head_decl: Vec::new(),
            head_params: 0,
            head_args: None,
            head_pinned: None,
            res: None,
        }
    }
}

/// `decode[T](s: str) -> Result[T]`, the one signature of std.json's decode (TICKET-214). It is no
/// `ModuleSig` member: decode has no runtime slot, and its value is a per-`T` compiled thunk
/// (`Resolution::Decode`).
pub(super) fn json_decode_sig() -> FnSig {
    FnSig {
        type_params: vec![TyParam {
            name: "T".to_string(),
            name_span: Span::default(),
            bounds: Vec::new(),
        }],
        ..FnSig::plain(vec![Ty::Str], Ty::result(Ty::Param("T".to_string())))
    }
}

/// `display[<P>, …]` over `tps`, or `display` alone when there are none.
pub(super) fn fn_spelling(display: &str, tps: &[TyParam]) -> String {
    if tps.is_empty() {
        return display.to_string();
    }
    let holes = tps
        .iter()
        .map(|tp| format!("<{}>", tp.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{display}[{holes}]")
}

/// Does a head with alias-pinned args `pinned` over a type declaring `decl_params` params clash
/// with written type args (DEC-204: an alias that fixes its arguments takes no more)? A pinning
/// alias (`type BI = Bx[int]`) and a non-generic one (`type P2 = P`) clash; an unpinned alias of a
/// generic type (`type BB = Box`, TICKET-180) takes the target's arguments and does not. The one
/// predicate under [`Checker::written_head_args`].
pub(super) fn head_args_clash(pinned: Option<&[Ty]>, decl_params: usize, written: bool) -> bool {
    written && pinned.is_some_and(|p| !p.is_empty() || decl_params == 0)
}

/// A method sig as a value over its type's params `tps` (DEC-197: through `instantiate_method`):
/// the type's params lead `type_params`, and an instance method's receiver slot becomes `recv`,
/// keyword `self`.
fn method_value_sig(msig: &FnSig, tps: &[TyParam], recv: Ty) -> FnSig {
    let recv_map: HashMap<String, Ty> = tps
        .iter()
        .map(|tp| (tp.name.clone(), Ty::Param(tp.name.clone())))
        .collect();
    let mut sig = instantiate_method(msig, &recv_map);
    if !msig.is_static {
        if let Some(p) = sig.params.first_mut() {
            *p = recv;
        }
        if let Some(l) = sig.labels.first_mut() {
            *l = Some("self".to_string());
        }
        if let Some(slots) = &mut sig.slots {
            slots.insert(
                0,
                crate::desugar::SlotSpec {
                    name: Some("self".to_string()),
                    default: None,
                    is_variadic: false,
                },
            );
        }
    }
    sig.type_params = tps.iter().cloned().chain(sig.type_params).collect();
    sig
}
