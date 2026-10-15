// checker::pattern — split out of checker/mod.rs. `super::*` == the `checker` module.
// Pattern / match-arm binding and or-pattern consistency.

use super::*;
use crate::ast::CarrierTag;

/// Where `bind_subpattern` sends the names a pattern binds. `Match` declares them in the current
/// scope under a refutable `match` pattern (a bare name may be a variant or a `const`). `Target`
/// is an irrefutable binding target (`:=`, `for`, a comprehension clause, a wait arm): it collects
/// each leaf (name, name span, type) for the caller's own declare policy, treats every `Ident` as a
/// fresh name, and says nothing over an un-inferable (`Unknown`) type: a wrong shape faults at run
/// time. In `Target` mode the returned bool means "the type proved the shape".
pub(super) enum BindMode<'a> {
    Match,
    Target(&'a mut Vec<(String, Span, Ty)>),
}

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

/// The name prefix of the scratch binding `scratch_operand` declares for a `?.` / `??` operand.
/// No user code can name it, and the hole check does not judge it (TICKET-238).
pub(super) const OPT_RECV_PREFIX: &str = "__optrecv";

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
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::BuiltinFn { .. }
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
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
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
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
            // The carrier's variant by its private spelling: no user declaration captures it.
            Ok((_, variant)) => Some(Pattern::Variant {
                name: crate::ast::private_name(variant),
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
            // A bare head no import binds names a variant only when one of that name is in scope
            // (its use is then the qualify error); a carrier's variants are in no user module's
            // scope, so `Some(v)` never reaches the carrier by its spelling.
            None if bare && !self.variant_owners.contains_key(name) => None,
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
    pub(super) fn bind_subpattern(
        &mut self,
        pattern: &Pattern,
        ty: &Ty,
        span: Span,
        mode: &mut BindMode<'_>,
    ) -> bool {
        if let BindMode::Target(leaves) = mode {
            match pattern {
                Pattern::Ident(name, bind_span, _) => {
                    leaves.push((name.clone(), *bind_span, ty.clone()));
                    return true;
                }
                Pattern::Wildcard | Pattern::Tuple(_) => {}
                Pattern::Variant { .. }
                | Pattern::Literal(_)
                | Pattern::Range { .. }
                | Pattern::Or(_)
                | Pattern::Carrier { .. } => {
                    unreachable!("a binding target holds Ident, Wildcard or Tuple only")
                }
            }
        }
        let target = matches!(mode, BindMode::Target(_));
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
                        self.error(
                            span,
                            format!("'{}' is not a variant of {ty}", crate::ast::unprivate(name)),
                        );
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
                // token's OWN span (`bind_span`, not the arm-level `span`), exactly as a loop
                // target does. No-op unless a probe is armed → zero overhead on normal checks.
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
                    Some(variant) => self.bind_subpattern(&variant, ty, span, &mut BindMode::Match),
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
                        if target {
                            for sub in subs {
                                self.bind_subpattern(sub, &Ty::Unknown, span, &mut *mode);
                            }
                            return false;
                        }
                    }
                    let mut irref = true;
                    for (sub, t) in subs.iter().zip(tys.iter()) {
                        irref &= self.bind_subpattern(sub, t, span, &mut *mode);
                    }
                    irref
                }
                Ty::Unknown => {
                    // §4.1 — a STRUCTURAL sub-pattern (here a tuple) over an un-inferable element/
                    // payload would destructure a value whose shape we can't prove → it traps at
                    // runtime on a wrong shape (a trailing `_` cannot rescue it). Reject; annotate the
                    // enclosing param. Still bind the sub-patterns (as Unknown) so the arm body does
                    // not cascade into spurious "unknown name" errors. A binding target is
                    // irrefutable and says nothing here (it faults at run time on a wrong shape).
                    if !target {
                        self.error(
                            span,
                            "cannot match a tuple pattern on a value of un-inferable type; annotate it"
                                .to_string(),
                        );
                    }
                    for sub in subs {
                        self.bind_subpattern(sub, &Ty::Unknown, span, &mut *mode);
                    }
                    false
                }
                other @ Ty::Int
                | other @ Ty::Float
                | other @ Ty::Bool
                | other @ Ty::Str
                | other @ Ty::Bytes
                | other @ Ty::ByteArray
                | other @ Ty::Nil
                | other @ Ty::List(_)
                | other @ Ty::Map(..)
                | other @ Ty::Set(_)
                | other @ Ty::Func { .. }
                | other @ Ty::BuiltinFn { .. }
                | other @ Ty::Struct(..)
                | other @ Ty::Enum(..)
                | other @ Ty::Param(_)
                | other @ Ty::Result(..)
                | other @ Ty::Option(_)
                | other @ Ty::Channel(_)
                | other @ Ty::Shared(_)
                | other @ Ty::Atomic(_)
                | other @ Ty::AtomicInt
                | other @ Ty::RwShared(_)
                | other @ Ty::Executor
                | other @ Ty::Socket
                | other @ Ty::Listener
                | other @ Ty::Writer
                | other @ Ty::Reader
                | other @ Ty::Ptr
                | other @ Ty::Width(_)
                | other @ Ty::Protocol(..)
                | other @ Ty::Module(_)
                | other @ Ty::Var(_) => {
                    self.error(
                        span,
                        format!("tuple pattern cannot match a value of type {other}"),
                    );
                    for sub in subs {
                        self.bind_subpattern(sub, &Ty::Unknown, span, &mut *mode);
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
                                    crate::base::words::bare_display(sname),
                                    fields.len(),
                                    bindings.len()
                                ),
                            );
                        }
                    }
                    let mut sub_irref = true;
                    for (b, t) in bindings.iter().zip(fields.iter()) {
                        sub_irref &= self.bind_subpattern(b, t, span, &mut BindMode::Match);
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
                                            "variant '{}' binds {} value(s), but {} given",
                                            crate::ast::unprivate(name),
                                            payload.len(),
                                            bindings.len()
                                        ),
                                    );
                                }
                                let mut sub_irref = true;
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    sub_irref &=
                                        self.bind_subpattern(b, t, span, &mut BindMode::Match);
                                }
                                single_variant && sub_irref
                            }
                            None => {
                                if !qualifier_reported {
                                    self.error(
                                        span,
                                        format!(
                                            "'{}' is not a variant of {ty}",
                                            crate::ast::unprivate(name)
                                        ),
                                    );
                                }
                                for b in bindings {
                                    self.bind_subpattern(
                                        b,
                                        &Ty::Unknown,
                                        span,
                                        &mut BindMode::Match,
                                    );
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
                            self.bind_subpattern(b, &Ty::Unknown, span, &mut BindMode::Match);
                        }
                        false
                    }
                    None => {
                        self.error(
                            span,
                            format!("variant pattern '{name}' cannot match a value of type {ty}"),
                        );
                        for b in bindings {
                            self.bind_subpattern(b, &Ty::Unknown, span, &mut BindMode::Match);
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
            let alt_irref = self.bind_subpattern(alt, ty, span, &mut BindMode::Match);
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
    /// `start >= end` matches nothing on any run; `start..=end` (`start <= v <= end`) is empty when
    /// `start > end`. rustc rejects the same shapes (E0579, E0030).
    fn reject_empty_range(&mut self, pattern: &Pattern, span: Span) {
        if let Pattern::Range {
            start,
            end,
            inclusive,
        } = pattern
        {
            if *inclusive && start > end {
                self.error(
                    span,
                    format!(
                        "empty range pattern '{start}..={end}': the lower bound must not exceed the upper bound (a range pattern matches start <= v <= end)"
                    ),
                );
            } else if !*inclusive && start >= end {
                self.error(
                    span,
                    format!(
                        "empty range pattern '{start}..{end}': the lower bound must be less than the upper bound (a range pattern matches start <= v < end)"
                    ),
                );
            }
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
            let scrut = kind.scrut_ty();
            let Some(variant) = self.carrier_as_variant(*tag, inner, *id, &scrut, span) else {
                self.push_scope();
                self.declare_pattern_names_unknown(inner);
                return false;
            };
            let Pattern::Variant { name, .. } = &variant else {
                unreachable!("carrier_as_variant builds a Variant")
            };
            // `covered` holds variant names, never the private spelling the head carries.
            let duplicate = covered.remove(crate::ast::unprivate(name));
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
        // A tuple pattern is one rule over every scrutinee: `bind_subpattern` matches it against the
        // scrutinee's type (a tuple of that arity, or a reported mismatch).
        if matches!(pattern, Pattern::Tuple(_)) && !matches!(kind, MatchKind::Skip) {
            self.push_scope();
            return self.bind_subpattern(pattern, &kind.scrut_ty(), span, &mut BindMode::Match);
        }
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
                            self.bind_subpattern(b, &Ty::Unknown, span, &mut BindMode::Match);
                        }
                    }
                    Pattern::Tuple(subs) => {
                        for s in subs {
                            self.bind_subpattern(s, &Ty::Unknown, span, &mut BindMode::Match);
                        }
                    }
                    Pattern::Carrier { inner, .. } => self.declare_pattern_names_unknown(inner),
                    Pattern::Ident(..)
                    | Pattern::Literal(_)
                    | Pattern::Range { .. }
                    | Pattern::Wildcard
                    | Pattern::Or(_) => {}
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
                            self.error(
                                span,
                                format!(
                                    "'{}' is not a variant of {scrut}",
                                    crate::ast::unprivate(name)
                                ),
                            );
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
                                            "variant '{}' binds {} value(s), but {} given",
                                            crate::ast::unprivate(name),
                                            payload.len(),
                                            bindings.len()
                                        ),
                                    );
                                }
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    payload_irref &=
                                        self.bind_subpattern(b, t, span, &mut BindMode::Match);
                                }
                            }
                            None => {
                                for b in bindings {
                                    payload_irref &= self.bind_subpattern(
                                        b,
                                        &Ty::Unknown,
                                        span,
                                        &mut BindMode::Match,
                                    );
                                }
                            }
                        }
                        // A variant is `covered` ONLY when an arm for it is BOTH unguarded AND has an
                        // all-irrefutable payload (docs/syntax.md §8: a guarded arm is never
                        // irrefutable). Duplicate-arm detection fires only against a PRIOR fully
                        // closing arm, so a guard-then-fallback on the same variant is legal.
                        let variant = crate::ast::unprivate(name);
                        if covered.contains(variant) {
                            self.error(span, format!("duplicate match arm '{variant}'"));
                        } else if !guarded && payload_irref {
                            covered.insert(variant.to_string());
                        }
                    }
                    Pattern::Literal(_) => self.error(
                        span,
                        format!(
                            "cannot match a literal against {}",
                            crate::base::words::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Range { .. } => self.error(
                        span,
                        format!(
                            "cannot match a range against {}",
                            crate::base::words::bare_display(label.as_str())
                        ),
                    ),
                    Pattern::Ident(..)
                    | Pattern::Wildcard
                    | Pattern::Tuple(_)
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("ident/wildcard/tuple/or/carrier handled elsewhere")
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
                            self.bind_subpattern(b, &Ty::Unknown, span, &mut BindMode::Match);
                        }
                    }
                    Pattern::Ident(..)
                    | Pattern::Wildcard
                    | Pattern::Tuple(_)
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("ident/wildcard/tuple/or/carrier handled elsewhere")
                    }
                }
            }
            MatchKind::Tuple(tys) => {
                self.push_scope();
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
                        let shown = crate::base::words::bare_display(label.as_str());
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
                            irref &= self.bind_subpattern(b, t, span, &mut BindMode::Match);
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
                    Pattern::Ident(..)
                    | Pattern::Wildcard
                    | Pattern::Tuple(_)
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("ident/wildcard/tuple/or/carrier handled elsewhere")
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
    pub(super) fn warn_unreachable_arm(&mut self, dead: bool, span: Span) {
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
    pub(super) fn bool_domain_closed(
        kind: &MatchKind,
        covered: &std::collections::HashSet<String>,
    ) -> bool {
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
                        crate::base::words::bare_display(label.as_str())
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
        let shown = crate::base::words::bare_display(label);
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
                                    if crate::base::words::bare_display(s) == *en
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
                if !self.imported_variants.contains_key(name) {
                    if self.variant_owners.contains_key(name) {
                        let hint = self.qualify_hint(name);
                        self.error(span, hint);
                    } else if let Some(msg) = super::suggest::removed_name_message(name) {
                        // `Some(v)` / `Ok(v)` / `Err(e)`: removed spellings of `?v` / `!e`.
                        self.error(span, msg);
                    }
                }
            }
        }
    }
}
