//! Checker: identifiers, list/set/map literals, comprehensions, unary and binary operators,
//! comparison chains.
//! Split out of `src/checker/pattern.rs`. `super::*` is the parent module.

use super::fn_value::fn_spelling;
use super::resolve::PathPos;
use super::setup::TypeRead;
use super::*;
use crate::ast::consteval;

impl Checker {
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
            if self.is_poisoned(name) {
                self.poison_reads += 1;
            }
            // TICKET-183 — a body reads a module global declared below it through the type
            // `seed_module_globals` gave it. An `Unknown` in that type (an un-annotated empty
            // collection, a value of un-inferable type) is pinned by walk-order code this body cannot
            // see, so decline rather than guess: ask for an annotation.
            if self.in_fn_body
                && !self.inferring_ret
                && self.globals.get(name).is_some_and(|g| g.unreached())
                && self.owning_scope(name) == Some(0)
                // A fn value whose return is still being inferred (`g := fn(n: int): g(n)`) is
                // not this gate's: the closure's own return rule reports it.
                && (ty.is_unknown() || (!matches!(ty, Ty::Func { .. }) && ty.has_hole()))
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
        // A bare imported variant: the nullary one is its enum's value. A payload one is a
        // constructor, rejected below as a type read (U10).
        if let Resolution::Variant { enum_key, .. } = &res {
            return self.enum_ty_unknown_args(enum_key);
        }
        // A type name read as a value (`f := Box`, TICKET-204; U10: a payload variant, an alias of
        // any body and a builtin type too).
        let read = if let Resolution::VariantFn { .. } = res {
            Some(TypeRead::Variant(name.to_string()))
        } else {
            self.bare_type_read(name)
        };
        if let Some(read) = read {
            self.type_not_value(&read, span);
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
        if !self.import_hint(name).is_empty() && !self.imported_alias_tys.contains_key(name) {
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
        if crate::ast::is_blank_name(name) {
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

    pub(super) fn infer_list(&mut self, items: &[Expr], expected: Option<&Ty>) -> Ty {
        let sib = match expected {
            Some(Ty::List(e)) if !e.is_unknown() => None,
            _ => self
                .none_sibling_slot(&items.iter().collect::<Vec<_>>())
                .map(Ty::list),
        };
        let expected = sib.as_ref().or(expected);
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
                            slot_note(e, t)
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
            } else if t.is_unknown() {
            } else if let Some(m) = self.join_fill(&elem, t) {
                elem = m;
            } else {
                let [elem_s, t_s] = Ty::render_distinct([&elem, t]);
                self.error(
                    item.span,
                    format!(
                        "list elements differ: {elem_s} vs {t_s}{}{}",
                        float_fix_note_join(&elem, t),
                        crate::checker::carrier_join_note(&elem, t)
                    ),
                );
            }
        }
        Ty::list(elem)
    }

    /// Infer the type of a map literal `{k: v, …}`. Keys must share one (hashable) type, values
    /// another; heterogeneity and non-hashable keys are errors. Empty `{}` → `map[?, ?]`.
    pub(super) fn infer_set(&mut self, elems: &[Expr], expected: Option<&Ty>) -> Ty {
        let sib = match expected {
            Some(Ty::Set(e)) if !e.is_unknown() => None,
            _ => self
                .none_sibling_slot(&elems.iter().collect::<Vec<_>>())
                .map(Ty::set),
        };
        let expected = sib.as_ref().or(expected);
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
                    } else if et.is_unknown() {
                    } else if let Some(m) = self.join_fill(&elem, &et) {
                        elem = m;
                    } else {
                        let [elem_s, et_s] = Ty::render_distinct([&elem, &et]);
                        self.error(
                            e.span,
                            format!(
                                "set elements differ: {elem_s} vs {et_s}{}",
                                crate::checker::carrier_join_note(&elem, &et)
                            ),
                        );
                    }
                }
            }
        }
        Ty::set(elem_expected.unwrap_or(elem))
    }

    pub(super) fn infer_map(&mut self, entries: &[(Expr, Expr)], expected: Option<&Ty>) -> Ty {
        let sib = match expected {
            Some(Ty::Map(_, v)) if !v.is_unknown() => None,
            _ => self
                .none_sibling_slot(&entries.iter().map(|(_, v)| v).collect::<Vec<_>>())
                .map(|s| {
                    let k = match expected {
                        Some(Ty::Map(k, _)) => (**k).clone(),
                        _ => Ty::Unknown,
                    };
                    Ty::map(k, s)
                }),
        };
        let expected = sib.as_ref().or(expected);
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
                    } else if kt.is_unknown() {
                    } else if let Some(m) = self.join_fill(&key, &kt) {
                        key = m;
                    } else {
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
                                slot_note(ve, &vt)
                            ),
                        );
                    }
                }
                None => {
                    if value.is_unknown() {
                        value = vt.clone();
                    } else if vt.is_unknown() {
                    } else if let Some(m) = self.join_fill(&value, &vt) {
                        value = m;
                    } else {
                        let [value_s, vt_s] = Ty::render_distinct([&value, &vt]);
                        self.error(
                            v_expr.span,
                            format!(
                                "map values differ: {value_s} vs {vt_s}{}{}",
                                float_fix_note_join(&value, &vt),
                                crate::checker::carrier_join_note(&value, &vt)
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
        // TICKET-227: after every clause, the hint's ELEMENT payload (never the whole
        // type) is the element expression's slot, so a plain element wraps into a carrier.
        let outer_hint = self.expected_hint.take().filter(ty_fully_concrete);
        self.push_scope();
        for clause in clauses {
            // `for_bindings` infers the iter IN the current scope, so later clauses see earlier
            // bindings (the whole point of nesting). Compute before declaring this clause's vars.
            let bindings = self.for_bindings(&clause.target, &clause.iter);
            // Intentionally NOT `mark_loop_var`: a comprehension body is an expression, so its
            // binding can't be assigned to — no divergence to guard against. If a statement-bearing
            // comprehension is ever added, mark these too (see `check_assign` / for-loop handling).
            self.declare_target_leaves(&bindings, clause.iter.span);
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
                if t.is_numeric()
                    || t.is_unknown()
                    || self.satisfies(&t, Hook::Neg.protocol()).is_ok()
                {
                    t
                } else {
                    let note = self.hook_name_note(&t, Hook::Neg.protocol(), Hook::Neg.method());
                    self.error(inner.span, format!("cannot negate {t}{note}"));
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
        let t = match hint.as_ref().and_then(|h| h.carrier_parts()) {
            Some((_, _, Some(e))) => {
                self.expected_hint = Some(e.clone());
                let t = self.infer_value(inner);
                self.expected_hint = None;
                t
            }
            _ => self.infer_value(inner),
        };
        if !t.is_unknown() && !self.assignable(&Ty::error_proto(), &t) {
            self.error(inner.span, format!("{t} does not satisfy Error"));
        }
        if let Some((_, _, Some(e))) = hint.as_ref().and_then(|h| h.carrier_parts()) {
            if !t.is_unknown() && !self.assignable(e, &t) {
                self.error(inner.span, format!("error value: expected {e}, found {t}"));
            }
            return hint.unwrap_or(Ty::Unknown);
        }
        match hint {
            // No expected carrier: another operand of the same statement binds the success type
            // (TICKET-238: the frame is one statement or one bound operand), at module scope as
            // in a fn body, and in the return-inference walk.
            None | Some(Ty::Var(_)) => {
                let v = self.defer_carrier(node, super::tyvar::CarrierKind::Error);
                Ty::Result(Box::new(Ty::Var(v)), Box::new(t))
            }
            _ => Ty::Result(Box::new(Ty::Unknown), Box::new(t)),
        }
    }

    /// TICKET-227 (D3) — prefix `?x` builds a present/success value, its carrier taken from the
    /// expected type: `T?` -> `Some(x)`, `T!E` -> `Ok(x)`. The operand owns `T` as its slot (so
    /// `?5` at `int??` is `Some(Some(5))`). With no expected carrier the value takes a frame type
    /// variable, at module scope as in a fn body (one still unbound when its operand or statement
    /// closes is `T?`), and in the return-inference walk. Under an expected carrier
    /// whose payload is open the value is the carrier over its operand's type.
    fn infer_wrap_val(&mut self, node: &Expr, inner: &Expr) -> Ty {
        let hint = self.expected_hint.take();
        let (payload, w) = match hint.as_ref().and_then(|h| h.carrier_parts()) {
            Some((w, p, _)) => (p.clone(), w),
            None => {
                let t = self.infer_value(inner);
                let v = self.defer_carrier(node, super::tyvar::CarrierKind::Present(t));
                return Ty::Var(v);
            }
        };
        let t = self.infer_value_in(inner, &payload);
        if !t.is_unknown() && !self.assignable(&payload, &t) {
            self.error(
                inner.span,
                crate::checker::wrap_payload_mismatch(&payload, &t),
            );
        }
        self.record_wrap(node.id, w, node.span);
        if !crate::checker::ty_fully_concrete(&payload) && !t.is_unknown() {
            return match hint.as_ref().and_then(|h| h.carrier_parts()) {
                Some((_, _, Some(e))) => Ty::Result(Box::new(t), Box::new(e.clone())),
                Some(_) => Ty::Option(Box::new(t)),
                None => Ty::Unknown,
            };
        }
        hint.unwrap_or(Ty::Unknown)
    }

    pub(super) fn infer_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Ty {
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
        let r = self.infer_binary_rhs(rhs, hint);
        self.binary_result(op, l, r, lhs.span, rhs.span)
    }

    /// Report the first i64 overflow in the constant expression `expr`, once per span. The root of an
    /// arithmetic tree calls it; its children are covered by that scan (`arith_parent`).
    pub(super) fn scan_const_overflow(&mut self, expr: &Expr) {
        if !self.inferring_ret
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
    }

    /// Infer the right operand of `l OP rhs` under `hint`: the one call both `infer_binary` and a
    /// compound assignment (`x OP= rhs` is `x = x OP rhs`, owner rule U15) make. The operand is an
    /// arithmetic child: it gets `hint` as a seed (never wraps) and meets no width slot of its own.
    pub(super) fn infer_binary_rhs(&mut self, rhs: &Expr, hint: Option<Ty>) -> Ty {
        let covered = std::mem::replace(&mut self.arith_parent, true);
        self.expected_hint = hint;
        let r = self.infer_value(rhs);
        self.expected_hint = None;
        self.arith_parent = covered;
        r
    }

    /// The type of `l OP r` for two inferred operand types; a bad pair reports at `lspan`. The one
    /// owner of the binary-operator typing rules: `infer_binary` calls it, and so does a compound
    /// assignment, because `x OP= v` is `x = x OP v` (owner rule U15).
    pub(super) fn binary_result(
        &mut self,
        op: BinaryOp,
        l: Ty,
        r: Ty,
        lspan: Span,
        rspan: Span,
    ) -> Ty {
        use BinaryOp::*;
        // TICKET-239: an operand that is a same-statement type variable (`?1 < ?2`) is read here.
        // A comparison joins its two operands first (`?2 == f()` with `f() -> int!`); arithmetic
        // does not, so `1 + ?5` stays an error about `int?`.
        let (l, r) = if self.tyvars.borrow().any() {
            if matches!(op, Lt | LtEq | Gt | GtEq | Eq | NotEq) {
                self.join_ty(&l, &r);
            }
            (self.pinning_value_ty(&l), self.pinning_value_ty(&r))
        } else {
            (l, r)
        };
        let either_unknown = l.is_unknown() || r.is_unknown();
        match op {
            And | Or => {
                if l != Ty::Bool && !l.is_unknown() {
                    self.error(lspan, format!("logical operator expects bool, found {l}"));
                }
                if r != Ty::Bool && !r.is_unknown() {
                    self.error(rspan, format!("logical operator expects bool, found {r}"));
                }
                Ty::Bool
            }
            Add => {
                if l.is_numeric() && r.is_numeric() {
                    numeric_result(&l, &r)
                } else if let Some(t) =
                    binary_protocol(op).and_then(|p| self.op_overload_result(&l, &r, p))
                {
                    t
                } else if matches!(
                    (&l, &r),
                    (Ty::Bytes, Ty::ByteArray) | (Ty::ByteArray, Ty::Bytes)
                ) {
                    // Mixed bytes/bytearray `+` has no `Self`-typed protocol form; the result is
                    // the left operand's type (CPython: `b"a" + bytearray(b"b")` is `b"ab"`).
                    l.clone()
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    let note = self.hook_name_note(&l, Hook::Add.protocol(), Hook::Add.method());
                    self.error(
                        lspan,
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
                let proto = binary_protocol(op).expect("an arithmetic operator has a protocol");
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
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    let note = self.hook_name_note(&l, proto, &proto.to_lowercase());
                    self.error(
                        lspan,
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
                let proto = binary_protocol(op).expect("an arithmetic operator has a protocol");
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
                        lspan,
                        format!(
                            "cannot apply {} to {l_s} and {r_s}{}{note}",
                            op_sym(op),
                            float_fix_note_join(&l, &r)
                        ),
                    );
                    Ty::Unknown
                }
            }
            Lt | LtEq | Gt | GtEq | Eq | NotEq | In => self.compare_pair(op, &l, &r, lspan, rspan),
            // Bitwise/shift ops are int-only (gap #13), EXCEPT `| & ^` also do set algebra
            // (gap #3): union / intersection / symmetric-difference on two `set[T]`. Shifts
            // (`<< >>`) stay strictly int-only.
            BitAnd | BitOr | BitXor | Shl | Shr => {
                if l == Ty::Int && r == Ty::Int {
                    Ty::Int
                } else if let Some(t) =
                    binary_protocol(op).and_then(|p| self.op_overload_result(&l, &r, p))
                {
                    t
                } else if either_unknown {
                    Ty::Unknown
                } else {
                    let [l_s, r_s] = Ty::render_distinct([&l, &r]);
                    let note = binary_protocol(op).map_or(String::new(), |p| {
                        self.hook_name_note(&l, p, &p.to_lowercase())
                    });
                    self.error(
                        lspan,
                        format!(
                            "bitwise operator {} requires int operands or two sets, found {l_s} and {r_s}{note}",
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
                let ok = (l.is_numeric() && r.is_numeric()) || self.ordering_allowed(l, r);
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
                // (A range RHS needs no special case here: `r = self.infer(rhs)` above types it as
                // the prelude `Range`, and the `Contains` arm below checks the item against it.)
                if let Some(verdict) = self.builtin_contains_verdict(r, l) {
                    // List, Set, Map, str, bytes, bytearray: one needle rule shared with the
                    // `Contains` bound and `contains()`.
                    if !either_unknown && let Err(why) = verdict {
                        let [l_s, r_s] = Ty::render_distinct([l, r]);
                        let tail = why.map_or(String::new(), |w| format!(" — {w}"));
                        self.error(
                            lspan,
                            format!("cannot test membership of {l_s} in {r_s}{tail}"),
                        );
                    }
                    return Ty::Bool;
                }
                match r {
                    Ty::List(_)
                    | Ty::Set(_)
                    | Ty::Map(..)
                    | Ty::Str
                    | Ty::Bytes
                    | Ty::ByteArray => {
                        unreachable!("builtin_contains_verdict decides these")
                    }
                    Ty::Unknown => {}
                    other @ Ty::Int
                    | other @ Ty::Float
                    | other @ Ty::Bool
                    | other @ Ty::Nil
                    | other @ Ty::Func { .. }
                    | other @ Ty::BuiltinFn { .. }
                    | other @ Ty::Tuple(_)
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
}
