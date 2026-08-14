// checker::pattern — split out of checker/mod.rs. `super::*` == the `checker` module.
// Pattern / match-arm binding and or-pattern consistency.

use super::*;

/// The one diagnostic for a range used where it has no runtime value. It names every legal position
/// AND the materialization escape hatch — the `range(a, b)` builtin, which really does return a
/// `List[int]` (so `List(0..3)` is rejected and `Set(range(0, 3))` is the way).
pub(super) const RANGE_NOT_A_VALUE: &str = "a range is only valid as the iterable of a `for` loop or comprehension, as a slice receiver, \
     or as a `match` pattern — use `range(a, b)` to materialize a `List[int]`";

impl Checker {
    /// Type-check a *nested* sub-pattern (a variant payload slot or tuple element — gap #15) against
    /// its expected type `ty`, declaring any bindings into the current scope. Returns whether the
    /// sub-pattern is **irrefutable** (matches every value of `ty`): a binding/wildcard is, a
    /// literal/variant is not, a tuple is iff all its elements are.
    pub(super) fn bind_subpattern(&mut self, pattern: &Pattern, ty: &Ty, span: Span) -> bool {
        match pattern {
            Pattern::Wildcard => true,
            Pattern::Ident(name, bind_span) => {
                // A nested bare identifier names a *built-in* nullary variant of the matched type (a
                // refutable variant match — `Some(None)`, `Ok(Err(e))`), or a fresh binding. User
                // variants must be written qualified (handled below), never resolved bare here.
                let is_builtin_variant = crate::checker::is_builtin_variant(name);
                if is_builtin_variant {
                    if let Some(vmap) = self.variants_of(ty)
                        && let Some(payload) = vmap.get(name)
                    {
                        if payload.is_empty() {
                            // A nullary built-in variant of `ty`: a refutable match, binds nothing.
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
                    // on the VM while the interp binds. Reject it so all engines agree.
                    if !ty.is_unknown() {
                        self.error(span, format!("'{name}' is not a variant of {ty}"));
                        return false;
                    }
                }
                // A *user* variant must be written qualified — never resolved bare, never silently a
                // binding (the bare→binding trap). Reject with a hint to the qualified form.
                if self.variant_owners.contains_key(name) {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                    return false;
                }
                // EDITOR HOVER: a pattern binding (`n` in `Col.Val(n)`, `a`/`b` in `(a, b)`) is a
                // NAME, not an `Expr` the probe visits — record its decl-site hover at the binding
                // token's OWN span (`bind_span`, not the arm-level `span`), exactly as the for-loop
                // uses `var_spans`. No-op unless a probe is armed → zero overhead on normal checks.
                self.hover_record_at(*bind_span, ty, HoverKind::Local, None);
                self.declare(name, ty.clone());
                true
            }
            Pattern::Or(alts) => self.bind_or_alternatives(alts, ty, span),
            Pattern::Literal(lit) => {
                let lit_ty = lit_pattern_ty(lit);
                if !ty.is_unknown() && &lit_ty != ty {
                    self.error(
                        span,
                        format!("literal of type {lit_ty} cannot match a value of type {ty}"),
                    );
                }
                false
            }
            Pattern::Range { .. } => {
                // A range sub-pattern is int-only and always refutable.
                if !ty.is_unknown() && ty != &Ty::Int {
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
                    } else if fields.len() != bindings.len() {
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
                    let mut sub_irref = true;
                    for (b, t) in bindings.iter().zip(fields.iter()) {
                        sub_irref &= self.bind_subpattern(b, t, span);
                    }
                    return ctor.is_ok() && fields.len() == bindings.len() && sub_irref;
                }
                self.check_pattern_qualifier(
                    module_name,
                    enum_name,
                    name,
                    Self::scrutinee_enum(ty),
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
                        match vmap.get(name) {
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
                                let mut sub_irref = true;
                                for (b, t) in bindings.iter().zip(payload.iter()) {
                                    sub_irref &= self.bind_subpattern(b, t, span);
                                }
                                single_variant && sub_irref
                            }
                            None => {
                                self.error(span, format!("'{name}' is not a variant of {ty}"));
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
                    && !compatible(lt, rt)
                    && !compatible(rt, lt)
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
        // Mirror that registry here so `(None, None, None)` isn't falsely flagged as a duplicate binding.
        let is_binder = |name: &str| {
            !(self.variant_owners.contains_key(name) || crate::checker::is_builtin_variant(name))
        };
        if let Some(dup) = first_duplicate_binder(pattern, &is_binder) {
            self.error(
                span,
                format!("identifier '{dup}' is bound more than once in this pattern"),
            );
        }
        match kind {
            MatchKind::Skip => {
                // Un-inferable scrutinee with only binding/`_` arms: accept the pattern shape
                // permissively, binding everything as `Unknown`. Still scope so the caller can
                // `pop_scope` uniformly. (A structural arm here was already rejected upstream by §4.1.)
                self.push_scope();
                match pattern {
                    Pattern::Variant {
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
                        let is_known_variant = self.variant_owners.contains_key(name)
                            || crate::checker::is_builtin_variant(name);
                        if enum_name.is_none()
                            && module_name.is_none()
                            && bindings.is_empty()
                            && !is_known_variant
                        {
                            self.declare(name, Ty::Unknown);
                            return true;
                        }
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
                    _ => {}
                }
            }
            MatchKind::Variants { label, variants } => {
                self.push_scope();
                match pattern {
                    Pattern::Variant {
                        name,
                        bindings,
                        enum_name,
                        module_name,
                    } => {
                        self.check_pattern_qualifier(
                            module_name,
                            enum_name,
                            name,
                            Some(label.as_str()),
                            span,
                        );
                        let payload = variants.get(name).cloned();
                        if payload.is_none() {
                            self.error(
                                span,
                                format!(
                                    "'{name}' is not a variant of {}",
                                    crate::compiler::bare_display(label.as_str())
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
                            covered.insert(name.clone());
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
                    Pattern::Ident(..) | Pattern::Wildcard | Pattern::Or(_) => {
                        unreachable!("ident/wildcard/or handled elsewhere")
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
                    // registry, so a colliding name would bind in the interp but trap on the VM; reject
                    // it here so all engines agree. (Rename the binding to fix.)
                    Pattern::Variant {
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
                        if self.variant_owners.contains_key(name)
                            || crate::checker::is_builtin_variant(name)
                        {
                            self.error(
                                span,
                                format!(
                                    "'{name}' is a variant name and cannot bind a scrutinee of type {ty}; rename the binding"
                                ),
                            );
                            return false;
                        }
                        self.declare(name, ty.clone());
                        return true;
                    }
                    Pattern::Variant {
                        bindings,
                        enum_name,
                        name,
                        module_name,
                    } => {
                        self.check_pattern_qualifier(module_name, enum_name, name, None, span);
                        self.error(span, format!("cannot match a variant against {ty}"));
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
                    Pattern::Ident(..) | Pattern::Wildcard | Pattern::Or(_) => {
                        unreachable!("ident/wildcard/or handled elsewhere")
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
                        // the match. A bare name that IS a variant collides and is rejected so all
                        // engines agree (same rule as the `MatchKind::Literal` path).
                        let is_bare = enum_name.is_none() && module_name.is_none();
                        if is_bare && ctor.is_err() && bindings.is_empty() {
                            if self.variant_owners.contains_key(name)
                                || crate::checker::is_builtin_variant(name)
                            {
                                self.error(
                                    span,
                                    format!(
                                        "'{name}' is a variant name and cannot bind a scrutinee of type {shown}; rename the binding"
                                    ),
                                );
                                return false;
                            }
                            self.declare(name, Ty::Struct(label.clone(), targs.clone()));
                            return true;
                        }
                        // A constructor pattern: the name must be the struct's own name, and the
                        // field count must match (a clean checker error, never a runtime panic).
                        let is_ctor = ctor.is_ok();
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
                    Pattern::Ident(..) | Pattern::Wildcard | Pattern::Or(_) => {
                        unreachable!("ident/wildcard/or handled elsewhere")
                    }
                }
            }
        }
        false
    }

    /// Report a non-exhaustive match.
    /// - Variants mode: missing variants, unless a `_` wildcard was seen.
    /// - Literal mode: int/str/bool literal domains are open, so a `_` wildcard is *required*
    ///   (we do NOT special-case `true`+`false` closing the bool domain — keeping one rule).
    /// - Skip mode: un-inferable scrutinee, no exhaustiveness check.
    pub(super) fn check_exhaustive(
        &mut self,
        kind: &MatchKind,
        covered: &std::collections::HashSet<String>,
        has_wildcard: bool,
        span: Span,
    ) {
        if has_wildcard {
            return;
        }
        match kind {
            MatchKind::Skip => {}
            MatchKind::Variants { label, variants } => {
                let mut missing: Vec<String> = variants
                    .keys()
                    .filter(|v| !covered.contains(*v))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    missing.sort();
                    self.error(
                        span,
                        format!(
                            "non-exhaustive match on {}: missing {}",
                            crate::compiler::bare_display(label.as_str()),
                            missing.join(", ")
                        ),
                    );
                }
            }
            MatchKind::Literal(_) => {
                self.error(span, "non-exhaustive match: add a `_` arm".to_string());
            }
            MatchKind::Tuple(_) => {
                // A tuple match is exhaustive only via an irrefutable arm (a `_`, or a tuple of
                // all-binding sub-patterns). `has_wildcard` already captured that.
                self.error(span, "non-exhaustive match: add a `_` arm".to_string());
            }
            MatchKind::Struct { .. } => {
                // A struct has ONE constructor, so a single all-binding `Point(x, y)` arm is
                // irrefutable and closes the match (`has_wildcard` already captured that). Reaching
                // here means every arm was refutable (a literal/nested field like `Point(0, y)`) with
                // no `_` — non-exhaustive.
                self.error(span, "non-exhaustive match: add a `_` arm".to_string());
            }
        }
    }

    /// Resolve a struct pattern's constructor spelling against the scrutinee struct identity `label`
    /// (L2). A BARE `Point` resolves via `bare_key`; a QUALIFIED `mod.Point` resolves the module binder
    /// to the struct's identity key — symmetric with qualified construction (`geo.Point(3, 4)`), and the
    /// only spelling for a whole-module-imported struct (the bare name isn't in scope). `Ok(())` when it
    /// names `label`; `Err(msg)` (already BARE-rendered — never the `::` identity key) otherwise. A
    /// 3-part `a.b.Point` is rejected (structs are two-level). Shared by the top-level `MatchKind::Struct`
    /// arm and the nested-struct sub-pattern arm so both engines agree on what lowers.
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
                if self.bare_key(name) == label {
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
                if self.type_key(mid, name) == label {
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
                        // `=` assigns an existing outer lvalue — reuse the ordinary assignment checks
                        // (assignability, type match, read-only/loop-var gates).
                        WaitTarget::Assign(target) => {
                            self.check_assign(target, AssignOp::Eq, elem, arm.span)
                        }
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
            let mark = self.errors.len();
            let recv_ty = self.infer(obj);
            self.errors.truncate(mark);
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
        for arm in arms {
            // PERSISTENT refine-on-first-use (see `check_block`): a STATEMENT-`match` arm mirrors an
            // if/else statement body — a refine-on-first-use pin of an OUTER empty collection inside
            // one arm PERSISTS across sibling arms and past the match (Option B: a cross-arm element-
            // type conflict is a hard error). No snapshot/restore here, so the pin `repin` wrote to
            // the binding's OWNING scope survives `pop_scope` (which only removes the arm's binders).
            // The EXPRESSION-position matcher `infer_match` keeps its barrier — value-arms stay
            // independent.
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                scrutinee.span,
                &mut covered,
                arm.guard.is_some(),
            );
            // The guard is type-checked with the arm's bindings in scope. A guarded arm is never
            // irrefutable — its guard may fail at runtime — so it can't make the match exhaustive.
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            for stmt in &arm.body {
                self.check_stmt(stmt);
            }
            self.pop_scope();
        }
        self.check_exhaustive(&kind, &covered, has_wildcard, scrutinee.span);
    }

    /// Infer an expression-position `match`: bind each arm, infer its value, and unify the arm
    /// types into one result. Exhaustiveness is still enforced.
    pub(super) fn infer_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[crate::ast::MatchExprArm],
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
        let mut result: Option<Ty> = None;
        // int→float widen an untyped-int-const arm when a float-const sibling arm is present (mirrors
        // the list/map `literal_numeric_mix` peephole the compiler coerces on — see `branch_widen`).
        let mix = crate::compiler::literal_numeric_mix(arms.iter().map(|a| &a.body));
        for arm in arms {
            // Flow-sensitivity barrier (see `check_block`): expression-`match` arms run
            // conditionally too — refinement inside one arm must not leak across arms or past it.
            let snap = self.snapshot_refinable();
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                scrutinee.span,
                &mut covered,
                arm.guard.is_some(),
            );
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
            self.expected_hint = hint.clone();
            let t = self.infer(&arm.body);
            self.pop_scope();
            self.restore_refinable(snap);
            let t = Self::branch_widen(&arm.body, t, mix);
            result = Some(self.unify_branch(result, t, arm.body.span));
        }
        self.expected_hint = None;
        self.check_exhaustive(&kind, &covered, has_wildcard, scrutinee.span);
        let res = result.unwrap_or(Ty::Unknown);
        if had_hint {
            res
        } else {
            self.default_expr_result_e(res)
        }
    }

    /// Infer an expression-position `if c: a else: b`: condition is bool, the two branches unify.
    pub(super) fn infer_if_else(&mut self, cond: &Expr, then: &Expr, els: &Expr) -> Ty {
        self.infer_if_else_chain(cond, then, els, None)
    }

    /// Chain-aware body of `infer_if_else`. `inherited_mix` carries the WHOLE-chain
    /// `if_chain_numeric_mix` down from the head of an `if … elif … else` chain: an `elif` desugars to
    /// a nested `IfElse` in `els`, and the nested `els` sub-chain is inferred by a DIRECT recursive
    /// call here (not generic `infer`) so the head's mix reaches it — otherwise a float constant in an
    /// EARLIER arm would not license widening the int constants in a later all-int suffix, making the
    /// widening order-dependent (unlike a list literal / `match`). `None` = this is the chain head, so
    /// compute the mix over the full chain; `Some(m)` = inherited from the head.
    fn infer_if_else_chain(
        &mut self,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
        inherited_mix: Option<bool>,
    ) -> Ty {
        // Capture + clear the expected-type hint before the condition: the hint is for the branch
        // VALUES (tail position), not the bool condition. Re-install it for EACH branch — both are
        // equally the tail value, and `infer_call` drains the single slot via `take()`, so without
        // re-installing it the second-inferred branch would lose the hint and a generic ctor there
        // would deadlock (acceptance would depend on branch order).
        let hint = self.expected_hint.take();
        let had_hint = hint.is_some();
        // int→float widen an untyped-int-const branch when a float-const sibling is present ANYWHERE
        // in the if/elif chain — the whole-chain mix, computed at the head and threaded down (mirrors
        // the compiler's `compile_if_expr` — see `branch_widen`).
        let mix = inherited_mix.unwrap_or_else(|| crate::compiler::if_chain_numeric_mix(then, els));
        self.expect_bool(cond, "if condition");
        // Flow-sensitivity barrier (see `check_block`): the two branch expressions run
        // conditionally — refinement inside one must not leak into the other or past the `if`.
        let snap = self.snapshot_refinable();
        self.expected_hint = hint.clone();
        let t_then = self.infer(then);
        self.restore_refinable(snap.clone());
        self.expected_hint = hint;
        // A nested-`IfElse` `els` is the `elif` tail — recurse DIRECTLY, threading the head's mix; any
        // other `els` is the final leaf, inferred normally.
        let t_els = if let ExprKind::IfElse {
            cond: c2,
            then: t2,
            els: e2,
        } = &els.kind
        {
            self.infer_if_else_chain(c2, t2, e2, Some(mix))
        } else {
            self.infer(els)
        };
        self.expected_hint = None;
        self.restore_refinable(snap);
        let t_then = Self::branch_widen(then, t_then, mix);
        let t_els = Self::branch_widen(els, t_els, mix);
        let acc = self.unify_branch(None, t_then, then.span);
        let res = self.unify_branch(Some(acc), t_els, els.span);
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

    /// One-way int→float widening for an if/match-EXPRESSION tail branch — the scalar sibling of
    /// [`elem_widen`], under the IDENTICAL soundness rule: a branch widens iff it is an untyped INT
    /// constant AND the compiler is GUARANTEED to emit `Op::CoerceFloat` for it. The guarantee here is
    /// the `literal_numeric_mix` peephole (`mix`) — a float-constant sibling branch is present — the
    /// same predicate the compiler's `compile_if_expr`/`compile_match_expr` key their per-branch
    /// coerce on, so checker and backend cannot drift (both over `crate::ast::const_num` /
    /// `untyped_int_const`). A TYPED int branch (a variable, a call) never widens: the compiler cannot
    /// see its type, so accepting it would leave an `Int` under a static `float` (the V1 hole). This
    /// is what makes `x := if c: 1 else: 2.5` consistent with the accepted list literal `[1, 2.5]`.
    fn branch_widen(body: &Expr, t: Ty, mix: bool) -> Ty {
        if mix && t == Ty::Int && crate::ast::untyped_int_const(body) {
            Ty::Float
        } else {
            t
        }
    }

    /// Fold one branch's type into a match/if expression's running result type. The first concrete
    /// branch sets the type; a later incompatible branch is a real error (and yields `Unknown` to
    /// suppress cascades). `Unknown` branches never override a concrete result.
    pub(super) fn unify_branch(&mut self, acc: Option<Ty>, t: Ty, span: Span) -> Ty {
        match acc {
            None => t,
            Some(prev) => {
                if compatible(&prev, &t) {
                    if prev.is_unknown() { t } else { prev }
                } else {
                    self.error(
                        span,
                        format!("branches have incompatible types: {prev} and {t}"),
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
        // physical positions, so the pair is belt-and-braces (see `KeywordKey`'s doc).
        // Save/restore for nested interpolations. The compiler keeps the identical pair.
        let saved_ctx = self.kw_frag_ctx;
        let saved_ord = self.kw_frag_ord;
        let mut ord = 0usize;
        for chunk in chunks {
            if let crate::ast::Chunk::Expr(e, spec) = chunk {
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
                // Static format-spec/value-type check: when the value is a CONCRETE scalar
                // and the spec is provably wrong for it, reject at COMPILE time (same wording
                // the runtime backstop would emit — single-sourced in `fmtspec`). Only fires
                // for Int/Float/Str/Bool; Unknown, a generic `Param(T)`, protocols, structs,
                // lists, bytes, ... all fall through and keep the runtime backstop.
                if let Some(fs) = spec
                    && let Some(kind) = scalar_kind_of(&ty)
                    && let Err(msg) = crate::fmtspec::spec_valid_for_scalar(fs, kind)
                {
                    self.error(span, msg);
                }
                ord += 1;
            }
        }
        self.kw_frag_ctx = saved_ctx;
        self.kw_frag_ord = saved_ord;
        Ty::Str
    }

    /// Infer an expression that is used in **value position** (assignment RHS, a call/collection
    /// argument, a binary/unary operand, an index/range bound, …). `nil` is a return-only / void
    /// type, never a writable value: a void call's result must not silently propagate into a binding
    /// or another expression. So if the expr is exactly `Ty::Nil`, report it and degrade to `Unknown`
    /// (suppressing the cascade). A bare void call AS A STATEMENT keeps using plain `infer` (legal),
    /// as does a fn/closure RETURN expr (returning nil just makes a void fn — not "using nil").
    pub(super) fn infer_value(&mut self, expr: &Expr) -> Ty {
        let ty = self.infer(expr);
        if ty == Ty::Nil {
            self.error(
                expr.span,
                "expression returns no value (nil) and cannot be used as a value".to_string(),
            );
            return Ty::Unknown;
        }
        ty
    }

    pub(super) fn infer(&mut self, expr: &Expr) -> Ty {
        let ty = self.infer_kind(expr);
        // EDITOR HOVER probe: record this expr's type if its leaf/field anchor is the cursor token.
        // No-op (one `Option` check) unless a probe is armed. Children infer before parents and only
        // LEAF kinds record, so a parent expression never overwrites the smaller symbol's type.
        if self.hover_probe.is_some() {
            self.hover_record_expr(expr, &ty);
        }
        ty
    }

    pub(super) fn infer_kind(&mut self, expr: &Expr) -> Ty {
        // One-way int→float ELEMENT-widening license from an annotated `let` — applies to the
        // IMMEDIATE collection literal only. `take()` it (clearing the field) so a nested element, a
        // call argument, or any other sub-expression does NOT inherit it. Mirrors the compiler's
        // identical `take()` at the top of `compile_expr`.
        let elem_hint = self.float_elem_hint.take();
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
            ExprKind::Ident(name) => self.infer_ident(name, expr.span),
            ExprKind::List(items) => {
                // Consume any expected-type hint (a `List[E]` slot: an annotated `let`, a call
                // arg, a return position — or the synthesized variadic list). `take()` so the
                // hint drives THIS literal's element type and never leaks into a nested element
                // call. `None` keeps the ordinary bottom-up inference.
                let hint = self.expected_hint.take();
                self.infer_list(
                    items,
                    hint.as_ref(),
                    elem_hint == Some(crate::ast::ElemFloatHint::Elem),
                )
            }
            ExprKind::Tuple(items) => {
                Ty::Tuple(items.iter().map(|e| self.infer_value(e)).collect())
            }
            ExprKind::Map(entries) => self.infer_map(
                entries,
                elem_hint == Some(crate::ast::ElemFloatHint::MapValue),
            ),
            ExprKind::Set(elems) => self.infer_set(elems),
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.infer_comprehension(*kind, key.as_deref(), elem, clauses),
            ExprKind::Unary { op, expr: inner } => self.infer_unary(*op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs),
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
            } => self.infer_call(callee, args, named, type_args, expr.span),
            ExprKind::Field { obj, name, .. } => self.infer_field(obj, name),
            ExprKind::Index { obj, index } => self.infer_index(obj, index),
            ExprKind::Try(inner) => self.infer_try(inner, expr.span),
            // W7-43 — optional-chaining `?.` / null-coalescing `??` are CARRIER nodes: the checker
            // types the operand, picks the lowering, then clone-lowers and infers the clone. The
            // choice needs the operand's TYPE, which is why desugar no longer lowers them; the
            // picked mode is recorded in the `CarrierTable` so the type-blind compiler agrees.
            ExprKind::OptChain { obj, name_span, .. } => {
                self.infer_opt_chain(expr, obj, *name_span, expr.span)
            }
            ExprKind::NullCoalesce { lhs, .. } => self.infer_null_coalesce(expr, lhs, expr.span),
            ExprKind::DecodeCall { obj, ty, arg } => self.infer_decode(obj, ty, arg, expr.span),
            ExprKind::Closure { params, ret, body } => {
                // No expected type at the generic `infer` seam — free-closure inference (sources
                // #2/#3) and the ambiguity check happen inside `infer_closure`.
                self.infer_closure(params, ret.as_ref(), body, None)
            }
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms),
            ExprKind::IfElse { cond, then, els } => self.infer_if_else(cond, then, els),
            ExprKind::Recover(block) => self.infer_recover(block),
            // `Type[T1, T2]` is a type-application HEAD — only valid as the receiver of a member
            // access / call (`Result[int, str].Ok(5)`, nullary `Box[int].Empty`). The `infer_call`
            // and `infer_field` paths consume it before it reaches here; a bare one in value
            // position is a use of a type as a value.
            ExprKind::TypeApply { name, .. } => {
                self.error(
                    expr.span,
                    format!(
                        "'{name}' is a type, not a value; access a member (`{name}[…].member`)"
                    ),
                );
                Ty::Unknown
            }
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
    /// Probe-gated; entirely inert off the hover probe → VM/interp parity-neutral.
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
                let owning = (0..self.scopes.len())
                    .rev()
                    .find(|&i| self.scopes[i].contains_key(name))
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

    /// Editor hover for a `from M import T` user type (struct/enum/newtype). Computes the effective
    /// doc — the type's own decl docstring carried across the module boundary, else a `kind (from
    /// module)` fallback — then (1) seeds `name_docs[bind]` so a later bare (`x: T`) / generic-head
    /// (`x: T[..]`) annotation use surfaces the same doc (those arms read `name_docs`), and (2) records
    /// the import-line token hover at `name_span`. Both halves are probe-gated no-ops off the hover
    /// probe (`name_docs` is editor-tooling-only and entry-module-scoped), so this is parity-neutral.
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
                    let at_top_level = (0..self.scopes.len())
                        .rev()
                        .find(|&i| self.scopes[i].contains_key(name))
                        == Some(0);
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
        // A newtype constructor (`UserId(10)`): one arg of the underlying type → the newtype. Mirrors
        // the struct branch (module-keyed `bare_key` lookup); a generic newtype keeps its declared
        // `Ty::Param`s so it Displays "fn(list[T]) -> Stack[T]".
        if self.newtype_names.contains(name) {
            let key = self.bare_key(name);
            if let Some((under, _)) = self.newtype_defs.get(&key) {
                let targs: Vec<Ty> = self
                    .newtype_type_params
                    .get(&key)
                    .map(|tps| tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect())
                    .unwrap_or_default();
                return Some(Ty::Func {
                    params: vec![under.clone()],
                    ret: Box::new(Ty::NewType(name.to_string(), targs)),
                    labels: crate::checker::FnLabels::default(),
                });
            }
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
        // would escape it are rejected (both engines agree). `?` is fine — it propagates normally.
        if let Some((span, kw)) = escaping_flow(block, false) {
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
            // `stmt_terminates` divergence predicate.
            if value_ty == Ty::Nil && Self::stmt_terminates(last) {
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
    fn fold_recover_tail(acc: Option<Ty>, t: Ty) -> Option<Ty> {
        match acc {
            None => Some(t),
            Some(prev) => {
                if compatible(&prev, &t) {
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
        let mut result: Option<Ty> = None;
        let mut uniform = true;
        for arm in arms {
            let irref = self.bind_match_arm(
                &arm.pattern,
                &kind,
                scrutinee.span,
                &mut covered,
                arm.guard.is_some(),
            );
            if let Some(guard) = &arm.guard {
                self.expect_bool(guard, "match guard");
            }
            has_wildcard |= irref && arm.guard.is_none();
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
                match Self::fold_recover_tail(result.take(), t) {
                    Some(u) => result = Some(u),
                    None => uniform = false,
                }
            }
        }
        self.check_exhaustive(&kind, &covered, has_wildcard, scrutinee.span);
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
                match Self::fold_recover_tail(result.take(), t) {
                    Some(u) => result = Some(u),
                    None => uniform = false,
                }
            }
        }
        // `if_tail_is_value` guarantees `else_block.is_some()`.
        if let Some(body) = else_block {
            let (t, _span) = self.infer_recover_tail_block(body);
            if uniform {
                match Self::fold_recover_tail(result.take(), t) {
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

    pub(super) fn infer_ident(&mut self, name: &str, span: Span) -> Ty {
        // BARE-VALUE position, and the same shadowing rule (`Checker::shadowing_type_param`): a type
        // parameter shadows a same-named FUNCTION or module GLOBAL for the whole body, so `g := foo`
        // and `LIM + 1` must not quietly read the outer one while `foo()` / `LIM.m()` resolve to the
        // parameter. FIRST, because `lookup` reaches module globals (scope 0) — an inner LOCAL still
        // wins, which is what `shadowing_type_param` excludes. Go, the one-namespace ancestor, is the
        // reference: reading a type parameter as a value is *"foo (type) is not an expression"*.
        if self.shadowing_type_param(name) {
            return self.type_param_shadow_error(
                name,
                "a type parameter is a type, not a value — it is erased at runtime, so there is nothing to read",
                span,
            );
        }
        if let Some(ty) = self.lookup(name) {
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
        if let Some(sig) = self.functions.get(name) {
            let type_params = sig.type_params.clone();
            let params = sig.params.clone();
            let ret = sig.ret.clone();
            let labels = sig.labels.clone();
            let minp = sig.min_params;
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
            // direct call) whose type params can be PINNED from a concrete expected `fn(..) -> ..`
            // hint (a `let` annotation, a HOF param, or a return position — all delivered via the
            // `expected_hint` slot). Unify the fn's declared signature against the hint to bind its
            // params, enforce its declared bounds against the bindings, then give the value the
            // fully-substituted CONCRETE fn type. Runtime is generic-ERASED — the value is just the
            // underlying function, so an indirect call already works; this is a checker-only pin.
            //
            // SOUNDNESS: `unify` is first-binding-wins + a silent no-op on mismatch, so an
            // unsatisfiable hint (`g: fn(str) -> int = ident`) binds `T=str` (the param position wins)
            // and we return the CONCRETE `fn(str) -> str` — NEVER `expected` — leaving the existing
            // assignability / arg / return check to reject it against `fn(str) -> int`. When the hint
            // fails to pin EVERY param (or is absent / not a matching-arity concrete `Ty::Func`), fall
            // through to the rigid `fn(T) -> T` arm, preserving the out-of-scope bare `g := ident`
            // error (a still-free `Ty::Param` must never leak into a binding).
            //
            // Gated on a SAME-MODULE fn (`local_fn_names`) — the identical same-module restriction the
            // turbofish B-path + the compiler's erase set use, so accept ⟺ runtime stays in lockstep
            // (an imported generic-fn-as-value stays the rigid error, a documented v1 limit).
            if !type_params.is_empty()
                && self.local_fn_names.contains(name)
                && let Some(expected) = self.expected_hint.clone()
                && let Ty::Func { params: ep, .. } = &expected
                && ep.len() == params.len()
                && ty_fully_concrete(&expected)
            {
                let declared = Ty::Func {
                    params: params.clone(),
                    ret: Box::new(ret.clone()),
                    labels: FnLabels::new(labels.clone()),
                };
                let mut map = HashMap::new();
                unify(&declared, &expected, &mut map);
                if type_params.iter().all(|tp| map.contains_key(&tp.name)) {
                    self.enforce_bounds(&type_params, &map, span);
                    return subst(&declared, &map);
                }
            }
            return Ty::Func {
                params,
                ret: Box::new(ret),
                // A user fn's value type carries its param NAMES as labels, so `g := greet` yields a
                // labelled function value and `g(name="Bob")` resolves through it — and its optional
                // arity, so `f := g; f()` may omit the trailing defaults the CALLEE fills.
                labels: FnLabels::new(labels).with_min(minp),
            };
        }
        // A first-class universe builtin fn used in value position (`f := ord`, HOF arg, bare
        // `defer print(...)`). Typed as the dedicated `Ty::BuiltinFn` from `builtin_sig` — a genuine
        // callable that is sendable (crosses the spawn airlock) yet, unlike `Ty::Unknown`, is rejected
        // by `expect_bool` (so `if print:` is a type error, not a VM/interp divergence). Only the four
        // first-class fns; type/ctor names fall through to the "unknown/not first-class" arms below
        // (uniform with `f := Point`).
        //
        // BUT a same-named MODULE-LEVEL global read here is NOT the builtin: `lookup` already resolved
        // a binding that is in scope (the first arm above), so reaching here with a declared global
        // name means it is used BEFORE its definition line — a use-before-def error, exactly like a
        // non-builtin `x := y` before `y := …` (which errors `unknown name 'y'`). Suppress the
        // first-class arm for such a name so it falls through to the same error, keeping the VM (whose
        // `collect_globals` pre-slots every top-level `let` to `nil`) and the interp (source-order
        // env → `Value::Builtin`) from diverging on a program that would otherwise wrongly type-check.
        // `print`'s VALUE form is a FIXED 1-arg function, NOT its variadic call signature: the
        // variadic + `sep=`/`end=` shapes need the specialized `CallPrint`/`CallPrintSep` opcodes,
        // which are unreachable through a bound value (`p := print`). So force the canonical 1-arg
        // `Ty::BuiltinFn` here rather than the harvested variadic sig from `builtin_sig` — this is the
        // design-sanctioned split (the call authority is the variadic prelude decl; the value form is
        // fixed). Suppressed for a use-before-def module global, exactly like the general arm below.
        if name == "print" && !self.module_global_lets.contains(name) {
            return Ty::BuiltinFn {
                params: vec![Ty::Unknown],
                ret: Box::new(Ty::Nil),
            };
        }
        if is_firstclass_builtin_fn(name)
            && !self.module_global_lets.contains(name)
            && let Some(sig) = self.builtin_sig(name)
        {
            return Ty::BuiltinFn {
                params: sig.params,
                ret: Box::new(sig.ret),
            };
        }
        if name == "None" {
            return Ty::option(Ty::Unknown);
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
        if self.types_by_name.contains_key(name) {
            self.error(span, self.unknown_type_msg(name));
            return Ty::Unknown;
        }
        // A multi-level path mistake (`std.concurrency.Shared(0)`): the head `std` is the first
        // segment of a real imported dotted module path, NOT a bound name. Steer to the two-level form
        // instead of the misleading bare "unknown name 'std'". Narrow: fires ONLY for a literal import
        // path head, never a genuine typo (which has no `import_path_heads` entry).
        if let Some((dotted, bound)) = self.import_path_heads.get(name).cloned() {
            self.error(
                span,
                format!(
                    "Chezzi uses two-level paths — write `{bound}.<Name>` (the imported module's \
                     bound name) or alias with `import {dotted} as {bound}` then `{bound}.<Name>`; \
                     multi-level paths like `{name}.….<Name>` are not supported"
                ),
            );
            return Ty::Unknown;
        }
        self.error(span, format!("unknown name '{name}'"));
        Ty::Unknown
    }

    /// One-way int→float ELEMENT widening for a collection literal — the SOUNDNESS gate, applied
    /// IN PLACE to the already-inferred item types BEFORE any expected-type check.
    ///
    /// An item widens iff it is an untyped INT constant (`1`, `-2`, `1 + 1`) AND the compiler is
    /// GUARANTEED to emit `Op::CoerceFloat` for it: either an untyped FLOAT constant sibling is
    /// present (the compiler's `literal_numeric_mix` peephole fires) or the annotated-`let` element
    /// hint is active (`Compiler::float_elem_hint`). Same predicate (`crate::ast::const_num`) over the
    /// same syntax on both sides ⇒ the checker's element type IS what the backend stores, by
    /// construction — in EVERY element context, including a `List[Any]` / variadic `...xs: Any` slot,
    /// where the element type stays `Any` but the stored value is the widened `float`.
    ///
    /// A TYPED int item (a variable, a call result) never widens — the compiler cannot see its type,
    /// so accepting it would leave a runtime `Int` under a static `float` (the V1 hole).
    fn elem_widen<'a>(
        &self,
        items: impl Iterator<Item = &'a Expr> + Clone,
        tys: &mut [Ty],
        hint: bool,
    ) {
        let license = hint
            || items
                .clone()
                .any(|e| crate::ast::const_num(e) == Some(crate::ast::ConstNum::Float));
        if !license {
            return;
        }
        for (e, t) in items.zip(tys) {
            if *t == Ty::Int && crate::ast::untyped_int_const(e) {
                *t = Ty::Float;
            }
        }
    }

    pub(super) fn infer_list(&mut self, items: &[Expr], expected: Option<&Ty>, hint: bool) -> Ty {
        // EXPECTED-TYPE-DIRECTED path: when the slot type is a concrete `List[E]` (an annotated
        // `let xs: List[Any] = …`, a `List[E]` call arg — INCLUDING the synthesized variadic list
        // for `...xs: E` — or a `List[E]` return), drive `E` down onto each element instead of
        // unifying siblings bottom-up. A heterogeneous literal whose every element is assignable to
        // `E` then types as `List[E]`: the element-homogeneity rule is bypassed because the declared
        // element type already sanctions the mix. This is what makes the `Any` top type the honest
        // variadic element type — `fn f(...xs: Any)` called `f(1, "a", true)` (and the equivalent
        // `xs: List[Any] = [1, "a", true]`) collapse to a `List[Any]` and check clean, since every
        // value satisfies the empty `Any` protocol. Falls back to bottom-up inference when `E` is not
        // satisfied-by-all, preserving the existing "list elements differ" diagnostic + numeric
        // (int→float) widening for a genuinely mistyped literal.
        let mut tys: Vec<Ty> = items.iter().map(|it| self.infer_value(it)).collect();
        // Element widening runs FIRST, so the widened element type is what BOTH the expected-type
        // path and the bottom-up path see — the compiler's peephole coerces the same items regardless
        // of the slot, so the checker must not disagree with it in a `List[Any]` / variadic slot.
        self.elem_widen(items.iter(), &mut tys, hint);
        if let Some(Ty::List(e)) = expected
            && !e.is_unknown()
            && !items.is_empty()
            && tys.iter().all(|t| t.is_unknown() || self.assignable(e, t))
        {
            return Ty::list((**e).clone());
        }
        // Bottom-up homogeneity over the (possibly widened) item types. A mixed literal the gate did
        // not license is the ordinary heterogeneity error — `[a, 2.5]` with a TYPED int `a` has no
        // type context to adapt to (Go), so it is rejected rather than silently leaving an `Int` under
        // a static `float`.
        let mut elem = Ty::Unknown;
        for (t, item) in tys.iter().zip(items) {
            if elem.is_unknown() {
                elem = t.clone();
            } else if !t.is_unknown() && !compatible(&elem, t) {
                self.error(item.span, format!("list elements differ: {elem} vs {t}"));
            }
        }
        Ty::list(elem)
    }

    /// Infer the type of a map literal `{k: v, …}`. Keys must share one (hashable) type, values
    /// another; heterogeneity and non-hashable keys are errors. Empty `{}` → `map[?, ?]`.
    pub(super) fn infer_set(&mut self, elems: &[Expr]) -> Ty {
        let mut elem = Ty::Unknown;
        for e in elems {
            let et = self.infer_value(e);
            if !et.is_unknown()
                && let Some(why) = self.key_ty_reject(&et)
            {
                self.error(e.span, format!("set element type {why}"));
            }
            if elem.is_unknown() {
                elem = et;
            } else if !et.is_unknown() && !compatible(&elem, &et) {
                self.error(e.span, format!("set elements differ: {elem} vs {et}"));
            }
        }
        Ty::set(elem)
    }

    pub(super) fn infer_map(&mut self, entries: &[(Expr, Expr)], hint: bool) -> Ty {
        // Infer keys+values in source order first (so the widen gate can see the whole VALUE column),
        // then run the homogeneity checks in that same order — diagnostics are unchanged.
        let mut key_tys: Vec<Ty> = Vec::with_capacity(entries.len());
        let mut val_tys: Vec<Ty> = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            key_tys.push(self.infer_value(k));
            val_tys.push(self.infer_value(v));
        }
        // One-way int→float widening on the VALUE column only (keys are never float — not Hashable).
        self.elem_widen(entries.iter().map(|(_, v)| v), &mut val_tys, hint);
        let mut key = Ty::Unknown;
        let mut value = Ty::Unknown;
        for (((k_expr, v_expr), kt), vt) in entries.iter().zip(&key_tys).zip(&val_tys) {
            let (kt, vt) = (kt.clone(), vt.clone());
            if !kt.is_unknown()
                && let Some(why) = self.key_ty_reject(&kt)
            {
                self.error(k_expr.span, format!("map key type {why}"));
            }
            if key.is_unknown() {
                key = kt;
            } else if !kt.is_unknown() && !compatible(&key, &kt) {
                self.error(k_expr.span, format!("map keys differ: {key} vs {kt}"));
            }
            if value.is_unknown() {
                value = vt;
            } else if !vt.is_unknown() && !compatible(&value, &vt) {
                self.error(v_expr.span, format!("map values differ: {value} vs {vt}"));
            }
        }
        Ty::map(key, value)
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
        self.push_scope();
        for clause in clauses {
            // `for_bindings` infers the iter IN the current scope, so later clauses see earlier
            // bindings (the whole point of nesting). Compute before declaring this clause's vars.
            let bindings = self.for_bindings(&clause.vars, &clause.iter);
            // A comprehension materializes eagerly, but a `Channel` is a blocking iteration form whose
            // termination depends on `close()`. Draining it into a list/set/map is out of scope and would
            // DIVERGE between engines (the VM's `compile_comprehension` reuses the channel-aware
            // `compile_for`, but the interp oracle's comprehension path can't iterate a channel). Reject
            // on both engines instead — the `for v in ch:` statement form is the way to drain a channel.
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
        let result = match kind {
            CompKind::List => Ty::list(self.infer_value(elem)),
            CompKind::Set => {
                let et = self.infer_value(elem);
                if !et.is_unknown()
                    && let Some(why) = self.key_ty_reject(&et)
                {
                    self.error(elem.span, format!("set element type {why}"));
                }
                Ty::set(et)
            }
            CompKind::Map => {
                let key = key.expect("a map comprehension always carries a key expression");
                let kt = self.infer_value(key);
                let vt = self.infer_value(elem);
                if !kt.is_unknown()
                    && let Some(why) = self.key_ty_reject(&kt)
                {
                    self.error(key.span, format!("map key type {why}"));
                }
                Ty::map(kt, vt)
            }
        };
        self.pop_scope();
        result
    }

    pub(super) fn infer_unary(&mut self, op: UnaryOp, inner: &Expr) -> Ty {
        let t = self.infer_value(inner);
        match op {
            UnaryOp::Neg => {
                // int/float negate natively; a struct/newtype/type-param negates via the `Neg`
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
        }
    }

    pub(super) fn infer_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Ty {
        use BinaryOp::*;
        let l = self.infer_value(lhs);
        let r = self.infer_value(rhs);
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
                    if compatible(le, re) {
                        Ty::List(Box::new(merge_unknown(le, re)))
                    } else {
                        self.error(lhs.span, format!("cannot apply + to {l} and {r}"));
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(lhs.span, format!("cannot apply + to {l} and {r}"));
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
                    if compatible(le, re) {
                        Ty::Set(Box::new(merge_unknown(le, re)))
                    } else {
                        self.error(
                            lhs.span,
                            format!("cannot apply {} to {l} and {r}", op_sym(op)),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!("cannot apply {} to {l} and {r}", op_sym(op)),
                    );
                    Ty::Unknown
                }
            }
            // `/`/`%` overload via the `Div`/`Mod` protocols on same-typed structs/enums/type-params,
            // exactly like `-`/`*` use `Sub`/`Mul` (M22). `op_overload_result` also covers the same
            // SCALAR numeric newtype auto-flow (`Meters / Meters`), so no hand-rolled newtype branch.
            Div | Mod => {
                let proto = if op == Div { "Div" } else { "Mod" };
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) = self.op_overload_result(&l, &r, proto) {
                    t
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!("cannot apply {} to {l} and {r}", op_sym(op)),
                    );
                    Ty::Unknown
                }
            }
            Lt | LtEq | Gt | GtEq => {
                let ok = (l.is_numeric() && r.is_numeric())
                    || (l == Ty::Str && r == Ty::Str)
                    || self.ordering_allowed(&l, &r);
                if !ok && !either_unknown {
                    self.error(lhs.span, format!("cannot compare {l} and {r}"));
                }
                Ty::Bool
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
                    if compatible(le, re) {
                        Ty::Set(Box::new(merge_unknown(le, re)))
                    } else {
                        self.error(
                            lhs.span,
                            format!(
                                "bitwise operator {} requires int operands or two sets, found {l} and {r}",
                                op_sym(op)
                            ),
                        );
                        Ty::Unknown
                    }
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    self.error(
                        lhs.span,
                        format!(
                            "bitwise operator {} requires int operands or two sets, found {l} and {r}",
                            op_sym(op)
                        ),
                    );
                    Ty::Unknown
                }
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
                    (l, r)
                } else {
                    (subst(&l, &pins), subst(&r, &pins))
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
                    self.error(
                        lhs.span,
                        format!("cannot compare {l} and {r} for equality — {why}"),
                    );
                    // One diagnostic per site — do not also run the co-inhabitance question.
                    return Ty::Bool;
                }
                let ok = self.may_be_equal(&l, &r);
                if !ok && !either_unknown {
                    self.error(lhs.span, format!("cannot compare {l} and {r} for equality"));
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
                match &r {
                    Ty::List(elem) | Ty::Set(elem) => {
                        if !either_unknown && !compatible(elem, &l) {
                            self.error(lhs.span, format!("cannot test membership of {l} in {r}"));
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
                            self.error(
                                lhs.span,
                                format!("cannot test membership of {l} in {r} — {why}"),
                            );
                        }
                    }
                    Ty::Map(key, _) => {
                        if !either_unknown && !compatible(key, &l) {
                            self.error(
                                lhs.span,
                                format!(
                                    "cannot test membership of {l} in {r} (map `in` tests keys)"
                                ),
                            );
                        }
                    }
                    Ty::Str => {
                        if l != Ty::Str && !either_unknown {
                            self.error(
                                lhs.span,
                                format!("substring `in` requires a str on the left, found {l}"),
                            );
                        }
                    }
                    Ty::Unknown => {}
                    other => {
                        // `Contains` protocol: a struct/enum with `contains(self, item) -> bool`.
                        if let Some(item) = self.contains_item_ty(other) {
                            if !either_unknown && !compatible(&item, &l) {
                                self.error(
                                    lhs.span,
                                    format!("cannot test membership of {l} in {r}"),
                                );
                            }
                        } else {
                            self.error(
                                rhs.span,
                                format!(
                                    "cannot use `in` on {other} (expected a list, set, map, str, or a type with `contains(self, item) -> bool`)"
                                ),
                            );
                        }
                    }
                }
                Ty::Bool
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
        match ty {
            Ty::Enum(name, _) => Some(name),
            Ty::Result(..) => Some("Result"),
            Ty::Option(_) => Some("Option"),
            _ => None,
        }
    }

    /// Validate the `Enum.` qualifier on a `case Enum.Variant:` pattern. The named variant must (a)
    /// belong to `enum_name`, and (b) — since variant names may now be shared across enums — name the
    /// **scrutinee's** enum (`scrut_enum`): owning the name isn't enough, because a foreign qualifier
    /// resolves to a different `variant_id` (a dead arm that would still be miscounted toward
    /// exhaustiveness → a "checked-OK" match that traps at runtime). When *unqualified*, a user variant
    /// name is an error — variants must be written qualified (built-in Ok/Err/Some/None stay bare).
    pub(super) fn check_pattern_qualifier(
        &mut self,
        module_name: &Option<String>,
        enum_name: &Option<String>,
        name: &str,
        scrut_enum: Option<&str>,
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
                self.error(span, format!("unknown module '{m}'"));
                return;
            };
            match self.module_sigs.get(&mid) {
                Some(sig) if sig.enum_defs.contains_key(en) => {
                    module_ekey = Some(self.type_key(&mid, en));
                }
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
                    None => match self.bare_types.get(en) {
                        Some(k) => k.clone(),
                        None => match scrut_enum {
                            Some(s) if crate::compiler::bare_display(s) == *en => s.to_string(),
                            _ => en.to_string(),
                        },
                    },
                };
                // User variants live in `self.variants` keyed by `(enum, variant)`; the built-in
                // Result/Option variants don't, so accept their canonical enums explicitly.
                let builtin_ok = matches!(
                    (en.as_str(), name),
                    ("Result", "Ok") | ("Result", "Err") | ("Option", "Some") | ("Option", "None")
                );
                if !builtin_ok
                    && !self
                        .variants
                        .contains_key(&(ekey.clone(), name.to_string()))
                {
                    self.error(span, format!("enum '{en}' has no variant '{name}'"));
                    return;
                }
                // The qualifier must name the scrutinee's own enum. (Skipped when the scrutinee enum
                // is unknown — an int/str/bool or un-inferable scrutinee, handled by the caller.) The
                // scrutinee carries the runtime key, so compare against the resolved `ekey`.
                if let Some(s) = scrut_enum
                    && ekey != s
                {
                    self.error(
                        span,
                        format!(
                            "variant '{en}.{name}' cannot match a value of enum '{}'",
                            crate::compiler::bare_display(s)
                        ),
                    );
                }
            }
            None => {
                // A bare user-variant name in a pattern must be qualified. (Built-ins are not in
                // `variant_owners`, so they pass through untouched.)
                if self.variant_owners.contains_key(name) {
                    let hint = self.qualify_hint(name);
                    self.error(span, hint);
                }
            }
        }
    }

    pub(super) fn infer_field(&mut self, obj: &Expr, name: &str) -> Ty {
        // A too-deep qualified-path mistake (`std.net.Socket(0)`, `std.concurrency.Shared(0)`,
        // `std.concurrency.collection.Counter(...)`): the receiver `obj` is the BARE first segment of
        // an imported dotted module path (`std`) — never a bound name — and `name` is the NEXT segment.
        // The enclosing call/field already consumed the rest, so we identify the module by its
        // (head, next) prefix and name its EXACT bound name (correct for 2- and 3+-level imports and
        // sibling collisions; the trailing type isn't visible here, hence the `<Name>` placeholder).
        if let ExprKind::Ident(head) = &obj.kind
            && self.lookup(head).is_none()
            && !self.imported_modules.contains_key(head)
            && let Some(slot) = self
                .module_prefix2
                .get(&(head.clone(), name.to_string()))
                .cloned()
        {
            let hint = match slot {
                Some((dotted, bound)) => format!(
                    "Chezzi uses two-level paths — write `{bound}.<Name>` (the imported module's \
                     bound name) or alias with `import {dotted} as {bound}` then `{bound}.<Name>`; \
                     multi-level paths like `{head}.{name}.<Name>` are not supported"
                ),
                None => format!(
                    "Chezzi uses two-level paths — reference an imported module by its bound name \
                     (its last path segment, or an alias); multi-level paths like \
                     `{head}.{name}.<Name>` are not supported"
                ),
            };
            self.error(obj.span, hint);
            return Ty::Unknown;
        }
        // `module.Enum.Variant` used as a value: a bound module dotted with one of its enums dotted
        // with a nullary variant — the qualified analogue of the bare `Enum.Variant` value form.
        if let ExprKind::Field {
            obj: inner_obj,
            name: ename,
            ..
        } = &obj.kind
            && let ExprKind::Ident(mname) = &inner_obj.kind
            && !self.is_local_binding(mname)
            && let Some(mid) = self.imported_modules.get(mname).cloned()
            && let Some(sig) = self.module_sigs.get(&mid).cloned()
            && let Some(edef) = sig.enum_defs.get(ename)
        {
            match edef.variant_names.iter().position(|v| v == name) {
                Some(i) if edef.variants[i].payload.is_empty() => {
                    return Ty::Enum(
                        self.type_key(&mid, ename),
                        vec![Ty::Unknown; edef.type_params.len()],
                    );
                }
                Some(_) => {
                    self.error(
                        obj.span,
                        format!("variant '{name}' of enum '{ename}' carries a payload; construct it as {mname}.{ename}.{name}(…)"),
                    );
                    return Ty::Unknown;
                }
                None => {
                    self.error(obj.span, format!("enum '{ename}' has no variant '{name}'"));
                    return Ty::Unknown;
                }
            }
        }
        // MEMBER-as-a-value position, and the same shadowing rule: `Col.Red` inside
        // `fn f[Col: Tagged]` is the PARAMETER, so the enum/struct arms below must never see the
        // name. Nothing is reachable through an erased type parameter except a STATIC method its
        // bound declares, which is a CALL and is handled in `infer_call` before it ever gets here
        // (rustc agrees: E0599 "no associated function or constant named `Red` found for type
        // parameter `Col`").
        if let ExprKind::Ident(tname) = &obj.kind
            && self.shadowing_type_param(tname)
        {
            return self.type_param_shadow_error(
                tname,
                &format!(
                    "a type parameter has no member '{name}'; the only thing reachable through one is a STATIC method declared by one of its bounds, and only as a call (`{tname}.<method>(...)`)"
                ),
                obj.span,
            );
        }
        // `Enum.Variant` used as a value: a bare *unbound* name that is an enum, dotted with one of
        // its nullary variants — sugar for the bare `Variant`. A real binding (struct/tuple/local
        // named like the enum) wins, so only when `lookup` finds nothing. The bare enum name is gated
        // by `enum_names` (visibility) and resolved to its runtime key for the layout lookup.
        if let ExprKind::Ident(ename) = &obj.kind
            && !self.is_local_binding(ename)
            && self.enum_names.contains(ename)
        {
            let ekey = self.bare_key(ename);
            let resolved = self
                .variants
                .get(&(ekey.clone(), name.to_string()))
                .cloned();
            match resolved {
                Some(v) if v.payload.is_empty() => {
                    let nparams = self.enum_type_params.get(&ekey).map_or(0, |t| t.len());
                    return Ty::Enum(ekey, vec![Ty::Unknown; nparams]);
                }
                Some(_) => {
                    self.error(
                        obj.span,
                        format!("variant '{name}' of enum '{ename}' carries a payload; construct it as {ename}.{name}(…)"),
                    );
                    return Ty::Unknown;
                }
                None => {
                    self.error(obj.span, format!("enum '{ename}' has no variant '{name}'"));
                    return Ty::Unknown;
                }
            }
        }
        // `Type[T…].Variant` used as a VALUE (no call) — the declaration-site turbofish on a nullary
        // variant: `Box[int].Empty`. Mirrors the bare `Enum.Variant` value form above, but returns
        // the EXPLICIT type args (resolved), not `Unknown`. Both carriers (single-arg `Index`,
        // multi-arg `TypeApply`) converge through `type_apply_head`.
        if let Some((tname, ekey, type_exprs)) = self.type_apply_head(obj)
            && self.enum_names.contains(&tname)
        {
            let resolved: Vec<Ty> = type_exprs
                .iter()
                .map(|t| self.resolve_type(t, obj.span))
                .collect();
            // Arity-check the explicit args against the enum's params (reuse `seed_targs`).
            let tps = self
                .enum_type_params
                .get(&ekey)
                .cloned()
                .unwrap_or_default();
            self.seed_targs(&tname, &tps, &resolved, obj.span);
            match self
                .variants
                .get(&(ekey.clone(), name.to_string()))
                .cloned()
            {
                Some(v) if v.payload.is_empty() => {
                    return Ty::Enum(ekey, resolved);
                }
                Some(_) => {
                    self.error(
                        obj.span,
                        format!("variant '{name}' of enum '{tname}' carries a payload; construct it as {tname}[…].{name}(…)"),
                    );
                    return Ty::Unknown;
                }
                None => {
                    self.error(obj.span, format!("enum '{tname}' has no variant '{name}'"));
                    return Ty::Unknown;
                }
            }
        }
        let obj_ty = self.infer(obj);
        match &obj_ty {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index as a
            // decimal string; out-of-range or non-numeric is an error.
            Ty::Tuple(elems) => match name.parse::<usize>() {
                Ok(i) if i < elems.len() => elems[i].clone(),
                _ => {
                    self.error(obj.span, format!("tuple {obj_ty} has no element '.{name}'"));
                    Ty::Unknown
                }
            },
            Ty::Struct(sname, targs) => {
                if let Some(info) = self.struct_shape(sname) {
                    let map = struct_param_map(info, targs);
                    if let Some((_, ty)) = info.fields.iter().find(|(f, _)| f == name) {
                        return subst(ty, &map);
                    }
                    // A METHOD is not a field, and methods are NOT first-class values (a bound
                    // method has no runtime representation — the compiler lowers a field-read to a
                    // plain field load, which the VM would fault on). Reject like every sibling
                    // receiver kind (enum/newtype/protocol) already does, but say WHY: reading a
                    // method used to hand back a `Ty::Func` still carrying the un-bound `self` slot
                    // typed `Ty::Unknown`, which laundered types (the `?` unified with anything).
                    if info.methods.contains_key(name) {
                        self.error(
                            obj.span,
                            format!(
                                "type {obj_ty} has no field '{name}' ('{name}' is a method — methods \
                                 are not values: call it (`x.{name}(…)`) or wrap it (`fn(): x.{name}()`))"
                            ),
                        );
                        return Ty::Unknown;
                    }
                }
                self.error(obj.span, format!("type {obj_ty} has no field '{name}'"));
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
                    .and_then(|sig| sig.functions.get(name))
                    .map(|f| f.witness_params.clone())
                    .unwrap_or_default();
                if self.reject_witness_fn_value(name, &wparams, obj.span) {
                    return Ty::Unknown;
                }
                let member = self
                    .imported_modules
                    .get(mname)
                    .and_then(|id| self.module_sigs.get(id))
                    .map(|sig| {
                        if let Some(fsig) = sig.functions.get(name) {
                            // Expose the FULL parameter list plus the optional arity, so both
                            // `f := request.get; f(url)` and `f(url, 5)` work. This used to TRUNCATE
                            // to `params[..min_params]`, which made supplying the optional tail
                            // through a value a spurious "too many arguments".
                            Some(Ty::Func {
                                params: fsig.params.clone(),
                                ret: Box::new(fsig.ret.clone()),
                                labels: crate::checker::FnLabels::none(fsig.params.len())
                                    .with_min(fsig.min_params),
                            })
                        } else {
                            sig.values.get(name).cloned()
                        }
                    });
                match member {
                    Some(Some(ty)) => ty,
                    _ => {
                        self.error(obj.span, format!("module '{mname}' has no member '{name}'"));
                        Ty::Unknown
                    }
                }
            }
            Ty::Unknown => Ty::Unknown,
            other => {
                self.error(obj.span, format!("type {other} has no field '{name}'"));
                Ty::Unknown
            }
        }
    }

    pub(super) fn infer_index(&mut self, obj: &Expr, index: &Expr) -> Ty {
        // Scope B — turbofish on a generic fn VALUE: `ident[int]` pins the fn's type params from the
        // explicit type arg and yields the CONCRETE `fn(int) -> int` value (`ident[int]` parses as
        // `Index { Ident(ident), int }`, so it lands here). Gated on a SAME-MODULE generic fn
        // (`local_fn_names` — the exact set the compiler erases at codegen, keeping checker-accept ⟺
        // compiler-erase in lockstep) that is NOT shadowed by a local/param binding, indexed by a
        // type-shaped expression. Runs BEFORE `infer_value(obj)`/inferring the index (which would
        // wrongly report `int` as an unknown name and "cannot index into fn").
        if let ExprKind::Ident(name) = &obj.kind {
            let is_local_generic = self.local_fn_names.contains(name)
                && self.lookup(name).is_none()
                && self
                    .functions
                    .get(name)
                    .is_some_and(|s| !s.type_params.is_empty());
            if is_local_generic && let Some(ty_expr) = self.index_as_type(index) {
                let (type_params, params, ret, labels, wparams) = {
                    let s = &self.functions[name];
                    (
                        s.type_params.clone(),
                        s.params.clone(),
                        s.ret.clone(),
                        s.labels.clone(),
                        s.witness_params.clone(),
                    )
                };
                // M24 — the fn-as-value wall again: pinning the type params does NOT recover the
                // witness (the pin is checker-only, the runtime value is the same erased function),
                // so `reset[Counter]` is as unlowerable as a bare `reset`.
                if self.reject_witness_fn_value(name, &wparams, obj.span) {
                    return Ty::Unknown;
                }
                let targ = self.resolve_type(&ty_expr, index.span);
                // `seed_targs` arity-checks the single type arg against the param count and emits the
                // clean "'name' expects N type argument(s), found 1" error on a mismatch.
                let map = self.seed_targs(name, &type_params, &[targ], obj.span);
                if type_params.iter().all(|tp| map.contains_key(&tp.name)) {
                    // Enforce declared bounds against the binding (`addone[str]` where `str: Add`
                    // fails), then yield the CONCRETE substituted fn type. Runtime is generic-ERASED.
                    self.enforce_bounds(&type_params, &map, obj.span);
                    return subst(
                        &Ty::Func {
                            params,
                            ret: Box::new(ret),
                            labels: FnLabels::new(labels),
                        },
                        &map,
                    );
                }
                // Arity mismatch (seed_targs already reported) — degrade to Unknown instead of
                // falling through to the "cannot index into fn" double-report.
                return Ty::Unknown;
            }
        }
        // Map keys are NOT int — infer the object first and check the index against the key type.
        match self.infer_value(obj) {
            Ty::Map(k, v) => {
                let idx_ty = self.infer_value(index);
                if !compatible(&k, &idx_ty) {
                    self.error(index.span, format!("map key must be {k}, found {idx_ty}"));
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
                if let Some((k, v)) = self.param_index_kv(&name, obj.span) {
                    let idx_ty = self.infer_value(index);
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        self.error(index.span, format!("index must be {k}, found {idx_ty}"));
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
                        self.error(index.span, format!("index must be {k}, found {idx_ty}"));
                    }
                    return v;
                }
                self.expect_int(index, "index");
                self.error(obj.span, format!("cannot index into {other}"));
                Ty::Unknown
            }
        }
    }

    /// The `(K, V)` of a bounded type parameter's `Index`/`IndexSet` bound, resolved with the
    /// surrounding params in scope. `None` ⇒ the param has no indexing bound.
    pub(super) fn param_index_kv(&mut self, name: &str, span: Span) -> Option<(Ty, Ty)> {
        let bound = self
            .type_params
            .get(name)?
            .iter()
            .find(|b| matches!(b.name.as_str(), "Index" | "IndexSet"))
            .cloned()?;
        let k = bound
            .args
            .first()
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        let v = bound
            .args
            .get(1)
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        Some((k, v))
    }

    /// The `(K, V)` of a bounded type parameter's `IndexSet` bound (write requires `IndexSet`
    /// specifically — a read-only `Index` bound is not assignable). `None` ⇒ no `IndexSet` bound.
    pub(super) fn param_indexset_kv(&mut self, name: &str, span: Span) -> Option<(Ty, Ty)> {
        let bound = self
            .type_params
            .get(name)?
            .iter()
            .find(|b| b.name == "IndexSet")
            .cloned()?;
        let k = bound
            .args
            .first()
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
        let v = bound
            .args
            .get(1)
            .map(|a| self.resolve_type(a, span))
            .unwrap_or(Ty::Unknown);
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
            return bound
                .args
                .first()
                .map(|a| self.resolve_type(a, span))
                .unwrap_or(Ty::Unknown);
        }
        match self.slice_result(&obj_ty) {
            Some(r) => r,
            None => {
                self.error(span, format!("cannot slice {obj_ty}"));
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
    /// * **Side tables are untouched.** `KeywordTable`/`WitnessTable`/`CarrierTable` are all keyed by
    ///   source spans and are `HashMap`s, so the operand's entries — recorded by the caller's
    ///   `infer_value` from the ORIGINAL spans — are simply no longer overwritten with themselves.
    fn scratch_operand(&mut self, t: Ty) -> Expr {
        let n = self.next_opt_tmp;
        self.next_opt_tmp += 1;
        let name = format!("__optrecv{n}");
        self.push_scope();
        self.declare(&name, t);
        Expr {
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
    fn record_carrier(&mut self, key: crate::checker::CarrierKey, mode: CarrierMode, span: Span) {
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
            "'?.' lowering",
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
    /// identical spans, and therefore identical `KeywordKey`/`WitnessKey`s.
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
                self.record_carrier(key, CarrierMode::Try, span);
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
                self.record_carrier(key, CarrierMode::Option, span);
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
                self.record_carrier(key, CarrierMode::Unknown, span);
                Ty::Unknown
            }
            other => {
                self.record_carrier(key, CarrierMode::Unknown, span);
                self.error(
                    span,
                    format!("'?.' applies to an Option or a Result, found {other}"),
                );
                Ty::Unknown
            }
        }
    }

    /// W7-43 — infer a `??` carrier. Option-ONLY: `??` has no spaced alternative spelling (so there
    /// is no whitespace trap to remove, only a new operator meaning to invent), no ancestor supports
    /// it on a `Result`, and coalescing a `Result` would silently discard its error payload. Because
    /// everything that reaches the compiler is therefore an `Option` (or already-rejected), `??`
    /// needs no [`crate::checker::CarrierTable`] entry and no compiler decision.
    pub(super) fn infer_null_coalesce(&mut self, carrier: &Expr, lhs: &Expr, span: Span) -> Ty {
        // Same operand-scratch shape as `infer_opt_chain`, same reason.
        let t = self.infer_value(lhs);
        match &t {
            Ty::Option(..) => {
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
                r
            }
            Ty::Unknown => Ty::Unknown,
            // No `unwrap_or` suggestion: `Result`/`Option` carry ZERO methods (`std/prelude.chz`).
            // No inline `match` spelling either — expression-`match` is indentation-based here, and
            // every suggested spelling must be one that actually parses.
            Ty::Result(..) => {
                self.error(
                    span,
                    format!(
                        "'??' applies to an Option, found {t} — a Result carries an error that \
                         must be handled: use a match with Ok/Err arms"
                    ),
                );
                Ty::Unknown
            }
            other => {
                self.error(span, format!("'??' applies to an Option, found {other}"));
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
             defining module, which has no caller to propagate to; use `??` or return an Option"
                .to_string()
        } else {
            format!("'?' used in a function that returns {ret}, not Result or Option")
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
                    self.error(span, "'?' on an Option is not allowed inside a recover block (its result is Result-typed); use match instead".to_string());
                    Ty::Unknown
                }
                other => {
                    self.error(span, format!("'?' expects Result or Option, found {other}"));
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
                    self.error(span, format!("'?' expects Result or Option, found {other}"));
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
                    self.error(span, format!("'?' expects Result or Option, found {other}"));
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
                            "'?' propagates a Result error, but the enclosing function returns Option, not Result".to_string(),
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
                            "'?' propagates a None, but the enclosing function returns Result, not Option".to_string(),
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
                self.error(span, format!("'?' expects Result or Option, found {other}"));
                Ty::Unknown
            }
        }
    }

    /// `json.decode[T](s)` — the source must be `str`, the target `T` must be decodable. Yields
    /// `Result[T]`. (`obj` is the json-module expression; we infer it only to surface a bad-module
    /// error, but place no constraint on it — any module exposing `parse` works at runtime.)
    pub(super) fn infer_decode(&mut self, obj: &Expr, ty: &Type, arg: &Expr, span: Span) -> Ty {
        let _ = self.infer(obj);
        let arg_ty = self.infer_value(arg);
        if !compatible(&Ty::Str, &arg_ty) {
            self.error(span, format!("decode source must be str, found {arg_ty}"));
        }
        let target = self.resolve_type(ty, span);
        if let Err(msg) = self.is_decodable(&target, &mut Vec::new()) {
            self.error(span, msg);
            return Ty::Unknown;
        }
        Ty::result(target)
    }

    /// Whether `json.decode` can produce a value of this type. Mirrors `json_decode::from_type`'s
    /// acceptance (kept in sync): scalars, `list`/`map[str,_]`/`Option` of decodables, and
    /// non-generic, non-recursive structs of decodable fields. `visiting` rejects recursive structs.
    pub(super) fn is_decodable(&self, ty: &Ty, visiting: &mut Vec<String>) -> Result<(), String> {
        match ty {
            Ty::Int | Ty::Float | Ty::Str | Ty::Bool => Ok(()),
            Ty::Unknown => Ok(()), // an error was already reported; don't pile on
            Ty::List(t) | Ty::Option(t) => self.is_decodable(t, visiting),
            Ty::Map(k, v) => {
                if !matches!(**k, Ty::Str) {
                    return Err(format!("decode: map keys must be str, found {k}"));
                }
                self.is_decodable(v, visiting)
            }
            Ty::Struct(name, args) => {
                if !args.is_empty() {
                    return Err(format!("decode: cannot decode into generic struct {ty}"));
                }
                if visiting.iter().any(|s| s == name) {
                    return Err(format!(
                        "decode: recursive struct '{name}' is not decodable; use the Json enum instead"
                    ));
                }
                let Some(info) = self.structs.get(name) else {
                    return Err(format!("decode: '{name}' is not a decodable type"));
                };
                visiting.push(name.clone());
                let fields = info.fields.clone();
                for (_, fty) in &fields {
                    self.is_decodable(fty, visiting)?;
                }
                visiting.pop();
                Ok(())
            }
            other => Err(format!("decode: cannot decode into {other}")),
        }
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
            ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
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
            ExprKind::Range { start, end } => {
                self.scan_expr_for_pin(name, start, match_pin, member_pin);
                self.scan_expr_for_pin(name, end, match_pin, member_pin);
            }
            ExprKind::Call { callee, args, .. } => {
                self.scan_expr_for_pin(name, callee, match_pin, member_pin);
                for a in args {
                    self.scan_expr_for_pin(name, a, match_pin, member_pin);
                }
            }
            ExprKind::Index { obj, index } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                self.scan_expr_for_pin(name, index, match_pin, member_pin);
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
            ExprKind::DecodeCall { obj, arg, .. } => {
                self.scan_expr_for_pin(name, obj, match_pin, member_pin);
                self.scan_expr_for_pin(name, arg, match_pin, member_pin);
            }
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
                        if let crate::ast::Chunk::Expr(frag, _) = chunk {
                            self.scan_expr_for_pin(name, frag, match_pin, member_pin);
                        }
                    }
                }
            }
            // The desugared form — the fragments are already parsed children here.
            ExprKind::Interp(chunks) => {
                for chunk in chunks {
                    if let crate::ast::Chunk::Expr(frag, _) = chunk {
                        self.scan_expr_for_pin(name, frag, match_pin, member_pin);
                    }
                }
            }
            // `?.`/`??` carriers are lowered before checking. `recover:` carries a statement block
            // (which can introduce its own bindings); it is NOT scanned — a param pinnable only from
            // inside a `recover:` body stays un-inferable and requires an annotation (sound: this is
            // the conservative v1 fallback, never a mis-pin). Leaves (`Ident`/literals/`TypeApply`/
            // `RawStr`) have no child to scan.
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
                    "Ok" | "Err" => Some(Ty::Result(Box::new(Ty::Unknown), Box::new(Ty::Unknown))),
                    "Some" | "None" => Some(Ty::Option(Box::new(Ty::Unknown))),
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
        Ty::Enum(key.to_string(), vec![Ty::Unknown; n])
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
        // `current_ret`.
        let saved_in_spawn = std::mem::replace(&mut self.in_spawn_block, false);
        // A closure inside a generator is NOT itself a generator: clear the yield context so a stray
        // `yield` in the closure is diagnosed as "outside a generator", not bound to the enclosing
        // one. (Closure bodies are single expressions today, so this is a latent-invariant guard.)
        let saved_yield = self.yield_ty.take();
        // Same for the in-bounds signal: a `yield` inside the closure must be out-of-bounds, and must
        // not seed the enclosing generator's `collected_yields` during inference. (Defensive — mirrors
        // `yield_ty.take()`; closures are single-expression so a closure `yield` is unparseable today.)
        let saved_ig = std::mem::replace(&mut self.in_generator, false);
        // M24 Task 4: the witness scope CARRIES INTO a closure body. `$w:T` is never a free variable
        // (it is unspellable), so `compile_closure` appends it to the capture entries explicitly —
        // and, since M24-2, only where the body can REACH it, which is a strict superset of what
        // this scope licenses (`compiler::nested_body_needs_witness`). The witness crosses BY VALUE,
        // so a closure that outlives its defining frame still constructs the right type.
        // Mark BEFORE param binding so the free-closure finalize (below) is suppressed if EITHER an
        // un-inferable PARAM (`cannot infer type of parameter`) or the body emits a real error — a
        // residual `Unknown` return is then a cascade, not a genuine un-inferable return.
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
        let body_ty = self.infer(body);
        let closure_had_err = self.errors.len() > closure_mark;
        self.pop_scope();
        self.loop_depth = saved_loop_depth;
        self.recover_depth = saved_recover;
        self.in_defer_block = saved_in_defer;
        self.current_ret = saved_ret;
        self.in_fn_body = saved_in_fn;
        self.in_default_provider = saved_in_dflt;
        self.in_spawn_block = saved_in_spawn;
        self.yield_ty = saved_yield;
        self.in_generator = saved_ig;
        let ret_ty = match ret {
            Some(t) => {
                let declared = self.resolve_type(t, body.span);
                if !self.assignable(&declared, &body_ty) {
                    self.error(
                        body.span,
                        format!(
                            "closure body has type {body_ty}, but its return type is {declared}"
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
/// → `render_str` path). Everything else (Unknown, `Param(T)`, protocols, structs, lists, bytes, …)
/// returns `None` so the static check is skipped and the runtime keeps its identical backstop — the
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
