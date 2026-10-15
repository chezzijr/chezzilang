//! Compiler: match and patterns: variant tests, binding names, `emit_pattern`, literal matches.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{Expr, MatchArm, Pattern, Span};
use crate::checker::Resolution;
use crate::vm::op::{NO_IC, Op};

impl Compiler {
    /// True if these arms form a literal/range/wildcard/binding match (no enum or tuple patterns), so
    /// it takes the lighter `compile_match_lit` path (no `EnsureEnum`, no `MatchNoArm`). A bare
    /// top-level identifier is a *binding* unless it names a known enum variant — that distinction is
    /// type-free thanks to the program-global variant registry, mirroring the runtime.
    pub(super) fn arms_are_literal<'a>(&self, patterns: impl Iterator<Item = &'a Pattern>) -> bool {
        patterns.into_iter().all(|p| self.pattern_is_literal(p))
    }

    /// What the checker recorded for a pattern head (`Pattern::Variant` or a nested
    /// `Pattern::Ident`): `Variant`, `PatStruct` or `PatBinding`. The checker records every head
    /// of a pattern it accepts, so a miss is an internal error. `None` for a pattern with no head.
    fn pat_resolution(&self, p: &Pattern, span: Span) -> Option<Result<&Resolution, CompileError>> {
        let id = match p {
            Pattern::Variant { id, .. }
            | Pattern::Ident(_, _, id)
            | Pattern::Carrier { id, .. } => id,
            Pattern::Tuple(_)
            | Pattern::Literal(_)
            | Pattern::Range { .. }
            | Pattern::Wildcard
            | Pattern::Or(_) => return None,
        };
        Some(
            self.resolutions
                .get(&(self.current_module_idx, id.0))
                .ok_or_else(|| CompileError {
                    message: "internal: no name resolution recorded for this pattern -- the \
                              type-checker and the backend disagree"
                        .to_string(),
                    span,
                }),
        )
    }

    /// Whether the checker recorded this pattern head as a variant test.
    fn pat_is_variant(&self, p: &Pattern) -> bool {
        matches!(
            self.pat_resolution(p, Span::RUNTIME),
            Some(Ok(Resolution::Variant { .. }))
        )
    }

    /// Whether an arm pattern is a GENUINE enum-variant pattern (so `compile_match_general` must
    /// emit the `EnsureEnum` scrutinee guard). A struct pattern (`Point(x, y)`, L2) and a bare
    /// whole-value catch-all binding (`rest:`) are not.
    fn pattern_needs_enum(&self, p: &Pattern) -> bool {
        matches!(p, Pattern::Variant { .. } | Pattern::Carrier { .. }) && self.pat_is_variant(p)
    }

    /// M19 lever #2 — the dense `variant_id` of the variant `name` of the enum keyed `enum_key` (the
    /// module-scoped runtime key the checker recorded), baked into `Op::NewEnum`/`Op::MatchArm` so the
    /// VM stamps / compares it without a runtime hash lookup. `VID_NONE` if unregistered.
    /// `Op::NewEnum` for variant `variant` of the enum keyed `enum_key`, over `argc` pushed args.
    pub(super) fn emit_new_enum(
        &self,
        fc: &mut FnComp,
        enum_key: &str,
        variant: &str,
        argc: usize,
        span: Span,
    ) {
        let variant_id = self.variant_id_of_key(enum_key, variant);
        fc.emit(
            Op::NewEnum {
                variant: variant.to_string(),
                variant_id,
                argc,
            },
            span,
        );
    }

    pub(super) fn variant_id_of_key(&self, enum_key: &str, name: &str) -> u32 {
        self.program
            .variants
            .get(&(enum_key.to_string(), name.to_string()))
            .map_or(crate::vm::op::VID_NONE, |v| v.variant_id)
    }

    /// The sorted, de-duplicated *binding* names introduced by an or-pattern's first alternative
    /// (the checker has verified every alternative binds the same set, so the first is canonical).
    /// A nested bare `Ident` naming a nullary variant is a variant match, NOT a binding, so it is
    /// excluded. Recurses into nested patterns; bounded by the finite pattern tree.
    fn or_binding_names(&self, alts: &[Pattern]) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        if let Some(first) = alts.first() {
            self.collect_binding_names(first, &mut names);
        }
        names.into_iter().collect()
    }

    fn collect_binding_names(&self, p: &Pattern, out: &mut std::collections::BTreeSet<String>) {
        match p {
            Pattern::Ident(n, _, _) if !self.pat_is_variant(p) => {
                out.insert(n.clone());
            }
            Pattern::Ident(..) => {}
            Pattern::Variant { bindings, .. }
            | Pattern::Tuple(bindings)
            | Pattern::Or(bindings) => {
                for b in bindings {
                    self.collect_binding_names(b, out);
                }
            }
            Pattern::Carrier { inner, .. } => self.collect_binding_names(inner, out),
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => {}
        }
    }

    /// Whether a single pattern is literal-eligible (literal/range/wildcard/binding-only), recursing
    /// into or-patterns (eligible iff every alternative is). Bounded by the finite pattern tree.
    fn pattern_is_literal(&self, p: &Pattern) -> bool {
        match p {
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => true,
            // empty-binding `Name`: a binding unless the checker recorded a variant. A missing
            // record takes the general path, whose `emit_pattern` reports it.
            Pattern::Variant { bindings, .. } if bindings.is_empty() => matches!(
                self.pat_resolution(p, Span::RUNTIME),
                Some(Ok(r)) if !matches!(r, Resolution::Variant { .. })
            ),
            Pattern::Or(alts) => alts.iter().all(|a| self.pattern_is_literal(a)),
            Pattern::Ident(..)
            | Pattern::Variant { .. }
            | Pattern::Tuple(_)
            | Pattern::Carrier { .. } => false,
        }
    }

    pub(super) fn compile_match(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
    ) -> Result<(), CompileError> {
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, |s, fc, body| {
                s.compile_defer_scoped_arm(fc, body)
            });
        }
        self.compile_match_general(fc, scrutinee, arms, span, |s, fc, body| {
            s.compile_defer_scoped_arm(fc, body)
        })
    }

    /// Lower a `match` whose arms are variant and/or tuple patterns, with arbitrary nesting (gap
    /// #15). Each arm tests its pattern against the scrutinee (via `emit_pattern`); on a mismatch it
    /// jumps to the next arm, on a match it binds and runs the body. Serves both the statement and
    /// expression forms via the `run_body` closure.
    pub(super) fn compile_match_general<A, F>(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[A],
        span: Span,
        mut run_body: F,
    ) -> Result<(), CompileError>
    where
        A: MatchArmLike,
        F: FnMut(&mut Self, &mut FnComp, &A::Body) -> Result<(), CompileError>,
    {
        fc.begin_scope();
        self.compile_expr(fc, scrutinee)?;
        let scrut = fc.add_hidden();
        fc.emit_hidden_set(scrut, span);
        // Variant matches keep the `EnsureEnum` guard so a non-enum scrutinee (possible only when
        // the checker couldn't infer the type) is a clean runtime error, not a panic. Tuple AND
        // struct matches need no such guard — a struct pattern `Point(x, y)` AND a bare whole-value
        // catch-all binding (`rest:`) are both `Pattern::Variant`, so only a GENUINE enum-variant arm
        // arms the guard. Else a struct-only match would emit `Op::EnsureEnum` and fault at runtime
        // (the checker-superset trap, L2).
        if arms.iter().any(|a| self.pattern_needs_enum(a.pattern())) {
            fc.emit(Op::EnsureEnum(scrut), scrutinee.span);
        }
        let mut end_jumps = Vec::new();
        for arm in arms {
            fc.begin_scope();
            let mut fails = Vec::new();
            self.emit_pattern(
                fc,
                arm.pattern(),
                scrut,
                &mut fails,
                scrutinee.span,
                &mut BindInto::Match,
            )?;
            // The guard runs with the pattern's bindings in scope; a false guard joins `fails` and
            // falls through to the next arm.
            self.emit_guard(fc, arm.guard(), &mut fails)?;
            run_body(self, fc, arm.body())?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), span));
            // Any failed test in this arm jumps here — the start of the next arm.
            let next = fc.here();
            for j in fails {
                fc.patch_jump_to(j, next);
            }
            fc.end_scope();
        }
        fc.emit(Op::MatchNoArm(scrut), scrutinee.span); // exhaustive (checker) → a trap
        for j in end_jumps {
            fc.patch_jump(j);
        }
        fc.end_scope();
        Ok(())
    }

    /// Emit an optional arm guard `if <expr>`: compile the bool expr and, on `false`, jump to the
    /// next arm (the jump is pushed onto `fails`, which the caller patches). A `None` guard emits
    /// nothing.
    fn emit_guard(
        &mut self,
        fc: &mut FnComp,
        guard: Option<&Expr>,
        fails: &mut Vec<usize>,
    ) -> Result<(), CompileError> {
        if let Some(g) = guard {
            self.compile_expr(fc, g)?;
            fails.push(fc.emit_jump(Op::JumpIfFalse(0), g.span));
        }
        Ok(())
    }

    /// The variant-headed arm of [`Self::emit_pattern`]: `pattern` is the head the checker
    /// recorded (a struct pattern, a whole-value binding named `name`, or a variant test) and
    /// `bindings` its payload sub-patterns.
    #[allow(clippy::too_many_arguments)]
    fn emit_variant_pattern(
        &mut self,
        fc: &mut FnComp,
        pattern: &Pattern,
        name: &str,
        bindings: &[Pattern],
        enum_name: &Option<String>,
        scrut: usize,
        fails: &mut Vec<usize>,
        span: Span,
    ) -> Result<(), CompileError> {
        let res = self
            .pat_resolution(pattern, span)
            .transpose()?
            .cloned()
            .expect("a Variant pattern has a head");
        // Struct pattern (L2): `Point(x, y)` destructures a struct by DECLARED FIELD NAME.
        // A struct has one constructor, so this is structurally IRREFUTABLE — no `MatchArm`
        // tag test, no fail-jump of its own; refutable SUB-patterns (`Point(0, y)`) push their
        // own fails when they recurse. Mirrors the `Pattern::Tuple` arm but keys `GetField` on
        // the field name instead of a numeric index (the VM resolves both the same way).
        if let Resolution::PatStruct(key) = &res {
            let field_names = self.program.structs[key].fields.clone();
            for (b, fname) in bindings.iter().zip(field_names.iter()) {
                fc.emit_hidden_get(scrut, span);
                fc.emit(
                    Op::GetField {
                        name: fname.clone(),
                        ic: NO_IC,
                    },
                    span,
                );
                let elem = fc.add_hidden();
                fc.emit_hidden_set(elem, span);
                self.emit_pattern(fc, b, elem, fails, span, &mut BindInto::Match)?;
            }
            return Ok(());
        }
        // A bare whole-value binding catch-all (`rest:` after a refutable struct arm) —
        // reachable in a struct, enum, `Option` or `Result` match (TICKET-107). Bind the
        // scrutinee like a plain `Pattern::Ident`.
        let Resolution::Variant { enum_key, variant } = res else {
            fc.emit_hidden_get(scrut, span);
            fc.emit_decl_named(name.to_string(), span);
            return Ok(());
        };
        // One slot per payload element, written positionally by `MatchArm`. An UNBOXED plain
        // `Ident` binding names its slot directly (so `Some(c)` binds `c` with no copy); a
        // BOXED plain ident (captured by a closure in the arm) needs a cell, but `MatchArm`
        // writes a raw value — so it gets a hidden RAW slot here and the user cell is bound
        // from it after the arm. A nested nullary-variant `Ident` (e.g. the `None` in
        // `Some(None)`) and any other sub-pattern also get a hidden slot to test/destructure.
        let bind_start = fc.next_slot();
        for b in bindings {
            match b {
                Pattern::Ident(n, _, _) if !self.pat_is_variant(b) && !fc.is_boxed_name(n) => {
                    fc.add_local(n.clone());
                }
                Pattern::Ident(..)
                | Pattern::Variant { .. }
                | Pattern::Tuple(_)
                | Pattern::Literal(_)
                | Pattern::Range { .. }
                | Pattern::Wildcard
                | Pattern::Or(_)
                | Pattern::Carrier { .. } => {
                    fc.add_hidden();
                }
            }
        }
        let variant_id = self.variant_id_of_key(&enum_key, &variant);
        let arm_op = fc.emit_jump(
            Op::MatchArm {
                scrut,
                variant,
                variant_id,
                // SCRUTINEE-DRIVEN fallback — carry the BARE written enum qualifier so the
                // VM can resolve an id-compare MISS against the scrutinee's own enum key
                // (two whole-imported same-named enums). `None` for a built-in (`Ok`/`Err`).
                enum_name: enum_name.clone(),
                nbind: bindings.len(),
                bind_start,
                next: 0,
            },
            span,
        );
        fails.push(arm_op);
        for (i, b) in bindings.iter().enumerate() {
            match b {
                // Unboxed plain binding — the VM wrote it straight into its user slot.
                Pattern::Ident(n, _, _) if !self.pat_is_variant(b) && !fc.is_boxed_name(n) => {}
                // Boxed plain binding — bind the user cell from the raw slot the VM wrote.
                Pattern::Ident(n, _, _) if !self.pat_is_variant(b) => {
                    fc.emit_hidden_get(bind_start + i, span);
                    fc.emit_decl_named(n.clone(), span);
                }
                // Nested / nullary-variant sub-pattern: test/destructure the raw slot.
                Pattern::Ident(..)
                | Pattern::Variant { .. }
                | Pattern::Tuple(_)
                | Pattern::Literal(_)
                | Pattern::Range { .. }
                | Pattern::Wildcard
                | Pattern::Or(_)
                | Pattern::Carrier { .. } => {
                    self.emit_pattern(fc, b, bind_start + i, fails, span, &mut BindInto::Match)?
                }
            }
        }
        Ok(())
    }

    /// Emit code testing the value in local `scrut` against `pattern`. Failed tests push their jump
    /// onto `fails` (the caller patches them to the next arm); successful matches bind every name in
    /// the pattern to a fresh local in the current scope. Recurses for nested tuple/variant
    /// patterns. No new opcodes — reuses `MatchArm` (variant), `GetField` (tuple element), and
    /// `Eq`+`JumpIfFalse` (literal).
    ///
    /// `into` says where a name lands (`BindInto`). A binding target (`Decl`, `Loop`) holds only
    /// `Ident`, `Wildcard` and `Tuple`, so it never pushes onto `fails`.
    pub(super) fn emit_pattern(
        &mut self,
        fc: &mut FnComp,
        pattern: &Pattern,
        scrut: usize,
        fails: &mut Vec<usize>,
        span: Span,
        into: &mut BindInto<'_>,
    ) -> Result<(), CompileError> {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Ident(name, _, _) => match into {
                // A `:=` name: a module global at global scope, else a fresh local.
                BindInto::Decl => {
                    fc.emit_hidden_get(scrut, span);
                    if fc.is_global_scope() {
                        fc.emit(Op::DefineGlobalSlot(self.global_slot(name)), span);
                    } else {
                        fc.emit_decl_named(name.clone(), span);
                    }
                }
                // A loop name: the loop's raw slot, in binder order (the loop refreshes a boxed
                // cell from it each iteration).
                BindInto::Loop(raws) => {
                    let slot = *raws.next().expect("one raw slot per binder");
                    fc.emit_hidden_get(scrut, span);
                    fc.emit_set_local_raw(slot, span);
                }
                BindInto::Match => {
                    // A nested bare identifier the checker recorded as a NULLARY variant is a
                    // refutable variant match (`Some(None)`); it binds nothing and is tested like a
                    // top-level nullary variant. Otherwise it is a binding capturing the whole
                    // sub-value.
                    let res = self.pat_resolution(pattern, span).transpose()?.cloned();
                    if let Some(Resolution::Variant { enum_key, variant }) = res {
                        let bind_start = fc.next_slot();
                        let variant_id = self.variant_id_of_key(&enum_key, &variant);
                        let arm_op = fc.emit_jump(
                            Op::MatchArm {
                                scrut,
                                variant: name.clone(),
                                variant_id,
                                // A nested bare nullary variant is always a BUILT-IN (`None`); user
                                // variants are qualified. The id compare is sufficient — no fallback key.
                                enum_name: None,
                                nbind: 0,
                                bind_start,
                                next: 0,
                            },
                            span,
                        );
                        fails.push(arm_op);
                    } else {
                        // Bind the whole scrutinee to `name` (a fresh cell when captured).
                        fc.emit_hidden_get(scrut, span);
                        fc.emit_decl_named(name.clone(), span);
                    }
                }
            },
            Pattern::Literal(lit) => {
                fc.emit_hidden_get(scrut, span);
                emit_lit_const(fc, lit, span);
                fc.emit(Op::Eq, span);
                fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
            }
            Pattern::Range {
                start,
                end,
                inclusive,
            } => {
                emit_range_test(fc, scrut, (*start, *end, *inclusive), fails, span);
            }
            Pattern::Tuple(subs) => {
                for (i, sub) in subs.iter().enumerate() {
                    // A binding target skips a `_` slot: nothing to read.
                    if !matches!(into, BindInto::Match) && matches!(sub, Pattern::Wildcard) {
                        continue;
                    }
                    fc.emit_hidden_get(scrut, span);
                    fc.emit(
                        Op::GetField {
                            name: i.to_string(),
                            ic: NO_IC,
                        },
                        span,
                    ); // tuple element `.i`
                    let elem = fc.add_hidden();
                    fc.emit_hidden_set(elem, span);
                    self.emit_pattern(fc, sub, elem, fails, span, &mut *into)?;
                }
            }
            Pattern::Variant {
                name,
                bindings,
                enum_name,
                ..
            } => self
                .emit_variant_pattern(fc, pattern, name, bindings, enum_name, scrut, fails, span)?,
            // The checker recorded which variant the `?` / `!` head means on the pattern's id.
            Pattern::Carrier { inner, .. } => self.emit_variant_pattern(
                fc,
                pattern,
                "",
                std::slice::from_ref(&**inner),
                &None,
                scrut,
                fails,
                span,
            )?,
            Pattern::Or(alts) => {
                // Pre-allocate ONE canonical slot per agreed binding name (the checker has verified
                // every alternative binds the same set). Each alternative binds into fresh scratch
                // slots, then copies its values into the canonical slots before jumping to a shared
                // matched-label; the body reads the canonical slots regardless of which alt matched.
                let names = self.or_binding_names(alts);
                let canon: Vec<(String, usize)> = names
                    .iter()
                    .map(|n| {
                        let slot = fc.add_local(n.clone());
                        // A BOXED canonical binding needs its cell allocated up front — each
                        // alternative `CellStore`s its value into this one shared cell.
                        if fc.is_boxed_slot(slot) {
                            fc.emit(Op::Nil, span);
                            fc.emit(Op::NewCell, span);
                            fc.emit_set_local_raw(slot, span);
                        }
                        (n.clone(), slot)
                    })
                    .collect();
                let mut matched_jumps = Vec::new();
                for (idx, alt) in alts.iter().enumerate() {
                    // Scope each alternative's scratch slots so they don't leak between alternatives.
                    fc.begin_scope();
                    let mut alt_fails = Vec::new();
                    self.emit_pattern(fc, alt, scrut, &mut alt_fails, span, &mut BindInto::Match)?;
                    // Copy this alternative's bindings into the canonical slots, then jump to the
                    // shared matched-label (fall-through past the remaining alternatives). Reading via
                    // `emit_get_named` (CellLoad if the alt's binding is boxed) and writing via
                    // `emit_set_named` (CellStore into the canonical cell) keeps the unboxed case
                    // byte-identical to a plain `GetLocal`/`SetLocal`.
                    for (name, slot) in &canon {
                        let src = fc.resolve_local(name).expect("alt binds the agreed name");
                        if src != *slot {
                            fc.emit_get_named(src, span);
                            fc.emit_set_named(*slot, span);
                        }
                    }
                    matched_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    // A miss in this alternative falls through to the next alternative (or, for the
                    // last alternative, joins the arm `fails` → next arm).
                    let next = fc.here();
                    if idx + 1 < alts.len() {
                        for j in alt_fails {
                            fc.patch_jump_to(j, next);
                        }
                    } else {
                        fails.extend(alt_fails);
                    }
                    fc.end_scope();
                }
                // All matched-jumps land here: bindings are in the canonical slots, run guard+body.
                for j in matched_jumps {
                    fc.patch_jump(j);
                }
            }
        }
        Ok(())
    }

    /// Lower a literal/wildcard `match` (no `EnsureEnum` — a literal `match` on an int must not
    /// raise "cannot match on int"). Each literal arm does `scrut == literal`, jumping to the next
    /// arm on a miss; the (checker-guaranteed) `_` wildcard is the unconditional fallback. The
    /// `run_body` closure compiles an arm body — a statement block or a value-expression — so this
    /// serves both the statement and expression forms.
    pub(super) fn compile_match_lit<A, F>(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[A],
        span: Span,
        mut run_body: F,
    ) -> Result<(), CompileError>
    where
        A: MatchArmLike,
        F: FnMut(&mut Self, &mut FnComp, &A::Body) -> Result<(), CompileError>,
    {
        fc.begin_scope();
        self.compile_expr(fc, scrutinee)?;
        let scrut = fc.add_hidden();
        fc.emit_hidden_set(scrut, span);
        let mut end_jumps = Vec::new();
        for arm in arms {
            match arm.pattern() {
                Pattern::Literal(lit) => {
                    fc.begin_scope();
                    fc.emit_hidden_get(scrut, scrutinee.span);
                    emit_lit_const(fc, lit, span);
                    fc.emit(Op::Eq, span);
                    let mut fails = vec![fc.emit_jump(Op::JumpIfFalse(0), span)];
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Range {
                    start,
                    end,
                    inclusive,
                } => {
                    fc.begin_scope();
                    let mut fails = Vec::new();
                    emit_range_test(fc, scrut, (*start, *end, *inclusive), &mut fails, span);
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Wildcard => {
                    // A bare `_` is the unconditional fallback (the checker guarantees exactly one
                    // covers every reachable path). A *guarded* `_ if g:` is refutable: its guard may
                    // fail, so it tests the guard and falls through to the next arm on a miss.
                    fc.begin_scope();
                    let mut fails = Vec::new();
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                // A bare top-level identifier that isn't a known variant is a binding catch-all
                // capturing the whole scrutinee (irrefutable, like `_` but named).
                Pattern::Variant { name, bindings, .. } if bindings.is_empty() => {
                    fc.begin_scope();
                    fc.emit_hidden_get(scrut, span);
                    fc.emit_decl_named(name.clone(), span);
                    let mut fails = Vec::new();
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Or(alts) => {
                    // All-literal or-pattern = OR-of-equality chain: any alternative hit runs the
                    // body; only when EVERY alternative misses do we fall through to the next arm.
                    // Literal/range alternatives bind nothing; a bare-binding/wildcard alternative is
                    // an unconditional hit (an irrefutable catch-all inside the or).
                    fc.begin_scope();
                    let mut hit_jumps = Vec::new();
                    let mut fails = Vec::new();
                    for (idx, alt) in alts.iter().enumerate() {
                        let last = idx + 1 == alts.len();
                        match alt {
                            Pattern::Literal(lit) => {
                                fc.emit_hidden_get(scrut, scrutinee.span);
                                emit_lit_const(fc, lit, span);
                                fc.emit(Op::Eq, span);
                                if last {
                                    // Last alternative: a miss joins `fails` → next arm.
                                    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
                                } else {
                                    // Earlier alternative: a miss falls through to the next
                                    // alternative's test; a hit jumps to the body.
                                    let miss = fc.emit_jump(Op::JumpIfFalse(0), span);
                                    hit_jumps.push(fc.emit_jump(Op::Jump(0), span));
                                    fc.patch_jump(miss);
                                }
                            }
                            Pattern::Range {
                                start,
                                end,
                                inclusive,
                            } => {
                                let mut alt_fails = Vec::new();
                                emit_range_test(
                                    fc,
                                    scrut,
                                    (*start, *end, *inclusive),
                                    &mut alt_fails,
                                    span,
                                );
                                if last {
                                    fails.extend(alt_fails);
                                } else {
                                    // On a hit (no fail), jump to the body; patch the range's fails
                                    // to the next alternative.
                                    hit_jumps.push(fc.emit_jump(Op::Jump(0), span));
                                    let next = fc.here();
                                    for j in alt_fails {
                                        fc.patch_jump_to(j, next);
                                    }
                                }
                            }
                            Pattern::Wildcard | Pattern::Variant { .. } => {
                                // An unconditional catch-all alternative — always hits.
                                hit_jumps.push(fc.emit_jump(Op::Jump(0), span));
                            }
                            Pattern::Ident(..)
                            | Pattern::Tuple(_)
                            | Pattern::Or(_)
                            | Pattern::Carrier { .. } => unreachable!(
                                "literal or-pattern has only literal/range/wildcard/binding alternatives"
                            ),
                        }
                    }
                    // All hit_jumps land here (the body); a last-alt miss in `fails` skips it.
                    for j in hit_jumps {
                        fc.patch_jump(j);
                    }
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Variant { .. }
                | Pattern::Tuple(_)
                | Pattern::Ident(..)
                | Pattern::Carrier { .. } => {
                    unreachable!(
                        "literal match has only literal/range/wildcard/binding arms (arms_are_literal)"
                    )
                }
            }
        }
        for j in end_jumps {
            fc.patch_jump(j);
        }
        fc.end_scope();
        Ok(())
    }

    // ----- expressions -----

    /// W7-43 — the next `__optN` temp index for the Option carrier lowering.
    pub(super) fn next_opt_tmp(&mut self) -> usize {
        let n = self.next_opt_tmp;
        self.next_opt_tmp += 1;
        n
    }
}
