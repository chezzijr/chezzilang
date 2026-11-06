//! Checker: field and index access, range subscripts, carrier records, `?.`, `??`, `?`,
//! `json.decode`.
//! Split out of `src/checker/pattern.rs`. `super::*` is the parent module.

use super::fn_value::fn_spelling;
use super::infer::{ChildHint, child_hint};
use super::setup::{HeadBinding, TypeRead};
use super::*;

impl Checker {
    /// The one lookup of a struct field's type at the receiver's type args (`Stack[int].items` is
    /// `List[int]`). `None` when `sname` has no field `field`, or when `field` is a std struct's
    /// `_` field read from a user module (TICKET-289: `Task._ch`).
    pub(super) fn struct_field_ty(&self, sname: &str, targs: &[Ty], field: &str) -> Option<Ty> {
        self.struct_shape(sname).and_then(|info| {
            if self.hides_field(info, field) {
                return None;
            }
            info.fields
                .iter()
                .find(|(f, _)| f == field)
                .map(|(_, ty)| subst(ty, &struct_param_map(info, targs)))
        })
    }

    /// True when `field` of the std struct `info` is a std internal and the current module is not
    /// std. The one test `struct_field_ty` and `field_names` read.
    pub(super) fn hides_field(&self, info: &StructInfo, field: &str) -> bool {
        info.origin == StructOrigin::Builtin && self.std_internal_hidden(field)
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
        let res = self.resolve_path(e);
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
                | Resolution::MethodFn { .. }
                | Resolution::NativeStatic(_),
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
        let obj_ty = self.with_child_hint(child_hint(&e.kind), |c| c.infer(obj));
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
                        let recv = match obj.kind.as_ident() {
                            Some(n) if crate::ast::CarrierTemp::of(n).is_none() => n.as_str(),
                            Some(_) | None => "x",
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
                            && let Some(read) = self.qualified_type_read(m, name)
                        {
                            self.type_not_value(&read, name_span);
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
            | other @ Ty::Var(_) => {
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
                self.type_not_value(&TypeRead::Head(th), e.span);
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
        let mark = self.errors.len();
        self.index_value_since(obj, index, type_shaped, mark)
    }

    /// [`Self::index_value`] with its error mark taken at `mark`: a call head `recv.fs[k](..)`
    /// counts the errors of the receiver `bracket_is_index` inferred before this read.
    pub(super) fn index_value_since(
        &mut self,
        obj: &Expr,
        index: &Expr,
        type_shaped: bool,
        mark: usize,
    ) -> Ty {
        #[cfg(any(test, feature = "test-api"))]
        {
            self.index_value_checks += 1;
        }
        // TICKET-225: the read's expected type is the ELEMENT's slot, never the object's or the
        // subscript's; left in place, `x: int8 = xs[200]` checked the int index `200` against int8.
        self.expected_hint = None;
        let obj_ty = self.infer_value(obj);
        if self.errors.len() > mark && type_shaped {
            return Ty::Unknown;
        }
        // TICKET-253 — a subscript whose index is a `Range` is a slice or a key lookup, decided
        // here once (a map keyed by `Range` looks the key up; everything else slices). A map types
        // its index against the key type below. Every other receiver types the index ONCE here and
        // the arms read that type: typing it again per arm costs 2^depth on `xs[xs[xs[i]]]`.
        let idx_ty = if matches!(obj_ty, Ty::Map(..)) {
            Ty::Unknown
        } else {
            let t = self.infer_value(index);
            if t == self.range_ty() {
                return self.range_subscript(&obj_ty, obj, index);
            }
            t
        };
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
                self.expect_int_ty(index, &idx_ty, "index");
                *inner
            }
            Ty::Str => {
                self.expect_int_ty(index, &idx_ty, "index");
                Ty::Str
            }
            Ty::Unknown => {
                self.expect_int_ty(index, &idx_ty, "index");
                Ty::Unknown
            }
            // A bounded `[C: Index[K, V]]` type parameter is indexable inside the generic body; its
            // value type is the bound's `V` arg (resolved with sibling params in scope).
            Ty::Param(name) => {
                if let Some((k, v)) = self.param_index_kv(&name) {
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                        self.error(index.span, format!("index must be {k_s}, found {idx_s}"));
                    }
                    return v;
                }
                self.expect_int_ty(index, &idx_ty, "index");
                self.error(obj.span, format!("cannot index into {name}"));
                Ty::Unknown
            }
            other @ Ty::Int
            | other @ Ty::Float
            | other @ Ty::Bool
            | other @ Ty::Bytes
            | other @ Ty::ByteArray
            | other @ Ty::Nil
            | other @ Ty::Set(_)
            | other @ Ty::Func { .. }
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
            | other @ Ty::Enum(..)
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
                // A user struct satisfying `Index` (has `index(self, K) -> V`) is indexable by `K`.
                if let Some((k, v)) = self.index_kv(&other) {
                    if !idx_ty.is_unknown() && !self.assignable(&k, &idx_ty) {
                        let [k_s, idx_s] = Ty::render_distinct([&k, &idx_ty]);
                        self.error(index.span, format!("index must be {k_s}, found {idx_s}"));
                    }
                    return v;
                }
                self.expect_int_ty(index, &idx_ty, "index");
                let note = self.hook_name_note(&other, Hook::Index);
                self.error(obj.span, format!("cannot index into {other}{note}"));
                Ty::Unknown
            }
        }
    }

    /// `obj[r]` where `r` is a `Range`: the ONE place that decides whether the subscript is a slice
    /// or an index. A type takes a range as a slice or as a key, never both
    /// ([`Self::range_key_conflict`]), so there is no precedence order: the `Slice` bound of a type
    /// parameter, the built-in containers and a type's `slice` hook (`slice_result`) make a slice;
    /// an `index` whose key accepts a `Range` makes an index. The choice rides the hook-call table
    /// to the compiler, which emits the opcode the checker chose; the VM never chooses again by
    /// value.
    fn range_subscript(&mut self, obj_ty: &Ty, obj: &Expr, index: &Expr) -> Ty {
        if obj_ty.is_unknown() {
            return Ty::Unknown;
        }
        let range = self.range_ty();
        if self.range_key_conflict(obj_ty) {
            self.error(obj.span, Self::range_key_conflict_text(&obj_ty.to_string()));
            return Ty::Unknown;
        }
        if let Ty::Param(name) = obj_ty {
            if let Some(bound) = self
                .type_params
                .get(name)
                .and_then(|bs| bs.iter().find(|b| b.name == "Slice").cloned())
            {
                self.record_subscript_slice(index);
                return bound.args.first().cloned().unwrap_or(Ty::Unknown);
            }
            if let Some((k, v)) = self.param_index_kv(name)
                && self.assignable(&k, &range)
            {
                return v;
            }
            self.error(obj.span, format!("cannot slice {name}"));
            return Ty::Unknown;
        }
        if let Some(r) = self.slice_result(obj_ty) {
            self.record_subscript_slice(index);
            if super::fn_writes::builtin_slice(obj_ty) {
                let key = self.subscript_key(index.span);
                self.fresh_subscripts.insert(key);
            }
            return r;
        }
        if let Some((k, v)) = self.index_kv(obj_ty)
            && k == range
        {
            return v;
        }
        let note = self.hook_name_note(obj_ty, Hook::Slice);
        self.error(obj.span, format!("cannot slice {obj_ty}{note}"));
        Ty::Unknown
    }

    /// TICKET-254 — a type that takes a `Range` as a `slice` argument and also as an `index` key is
    /// ambiguous at `x[a..b]`: declaring it or subscripting it is an error. A built-in container has
    /// no user `index(Range)`, and a `Map` has no `slice`, so `m[0..3]` stays a key lookup.
    pub(super) fn range_key_conflict(&mut self, ty: &Ty) -> bool {
        let range = self.range_ty();
        let (slices, key) = match ty {
            Ty::Param(name) => (
                self.type_params
                    .get(name)
                    .is_some_and(|bs| bs.iter().any(|b| b.name == "Slice")),
                self.param_index_kv(name).map(|(k, _)| k),
            ),
            Ty::Struct(..) | Ty::Enum(..) | Ty::Protocol(..) => (
                self.slice_result(ty).is_some(),
                self.index_kv(ty).map(|(k, _)| k),
            ),
            Ty::List(_)
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Nil
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Result(..)
            | Ty::Option(_)
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
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => return false,
        };
        slices && key.is_some_and(|k| self.assignable(&k, &range))
    }

    pub(super) fn range_key_conflict_text(name: &str) -> String {
        format!(
            "'{name}' declares 'slice' and an 'index' that takes a Range: a type takes a range as a slice or as a key, not both"
        )
    }

    /// Record that the subscript whose index expression is `index` reads as a slice.
    fn record_subscript_slice(&mut self, index: &Expr) {
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            index.span,
        );
        crate::checker::record_call_table_entry(
            &mut self.hook_calls,
            &mut self.table_conflicts,
            key,
            crate::base::hooks::Hook::Slice,
            "subscript",
            index.span,
        );
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
    /// * **Its own scope.** A fresh scope can't shadow a user name and is removed on every path.
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
        let name = crate::ast::CarrierTemp::Operand.name(n);
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
    /// argument with its params still `Unknown` (`src/checker/expr.rs`), and `infer_inline_fn_ret` walks a
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
    /// (the block verdict, the `current_ret`/`in_fn_body` return-kind gate) with
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
        let m = self.tyvar_mark();
        let t = self.with_child_hint(child_hint(&carrier.kind), |c| c.infer_value(obj));
        let t = self.close_operand(m, t);
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
                // TICKET-239: a CALL that returns nothing has no value (a field read always has
                // one). Try the void lowering and keep it when it types `Nil`, with whatever
                // errors its arguments reported: the value form would re-infer them and add a
                // "returns no value" error. `infer_value` then rejects it wherever a value is
                // needed. The mode is recorded only after this attempt decides: `carriers` is
                // not rolled back.
                let mut failed = false;
                if matches!(carrier.kind, ExprKind::OptChain { call: Some(_), .. }) {
                    let mut c = carrier.clone();
                    let scratch = self.scratch_operand(t.clone());
                    if let ExprKind::OptChain { obj, .. } = &mut c.kind {
                        **obj = scratch;
                    }
                    let tmp = self.next_opt_tmp;
                    self.next_opt_tmp += 1;
                    crate::desugar::lower_carrier_option_void(&mut c, tmp);
                    let mark = self.diag_mark();
                    // A call that types `Unknown` (its own error: a failed pick) is not void:
                    // the `pass` arm alone makes the match `Nil`. The value form keeps that one
                    // error, with no "returns no value" after it.
                    let armed = self.untyped_call.replace(None);
                    let r = self.infer(&c);
                    let untyped = std::mem::replace(&mut self.untyped_call, armed).flatten();
                    self.pop_scope();
                    failed = untyped == Some(span);
                    if r == Ty::Nil && !failed {
                        self.record_carrier(key, CarrierMode::OptionVoid, span, "?.");
                        return Ty::Nil;
                    }
                    self.diag_rollback(mark);
                }
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
                // A failed call has no value, as under `.`: `Unknown`, not `Option[Unknown]`,
                // so no reader (the discarded-value warning, a binding) cascades on it.
                if failed { Ty::Unknown } else { r }
            }
            // The operand already errored (its diagnostic stands, un-truncated) — adding a second
            // one here would be the cascade `Ty::Unknown` exists to suppress.
            Ty::Unknown => {
                self.record_carrier(key, CarrierMode::Unknown, span, "?.");
                self.walk_unknown_carrier(carrier);
                Ty::Unknown
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
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
            | other @ Ty::Enum(..)
            | other @ Ty::Param(_)
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
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
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
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => {}
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
        let m = self.tyvar_mark();
        let t = self.with_child_hint(child_hint(&carrier.kind), |c| c.infer_value(lhs));
        let t = self.close_operand(m, t);
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
                self.coalesce_join = true;
                let r = self.infer(&c);
                self.pop_scope();
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
                self.coalesce_join = true;
                let r = self.infer(&c);
                self.pop_scope();
                r
            }
            Ty::Unknown => {
                self.record_carrier(key, CarrierMode::Unknown, op_span, "??");
                self.walk_unknown_carrier(carrier);
                Ty::Unknown
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
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
            | other @ Ty::Enum(..)
            | other @ Ty::Param(_)
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

    /// `rule` is the node's [`super::infer::child_hint`]: the operand's hint.
    pub(super) fn infer_try(&mut self, inner: &Expr, span: Span, rule: ChildHint) -> Ty {
        let m = self.tyvar_mark();
        let t = self.with_child_hint(rule, |c| c.infer(inner));
        let t = self.close_operand(m, t);
        // The innermost block around the `?` that stops it decides (`boundary::RULES`). A recover
        // block is its target (try-block style): its error type is `Error` and its result is a
        // `T!E` value, so only a `T!E` operand fits.
        let verdict = self.block_verdict(boundary::Construct::Try);
        if let boundary::Verdict::Target(block, reason) = verdict {
            return match t {
                Ty::Result(ok, err) => {
                    // A `recover:` result's error slot is the built-in `Error` existential, so a
                    // propagated error must satisfy Error.
                    if self.satisfies(&err, "Error").is_err() {
                        self.error(
                            span,
                            format!("'?' inside a recover block propagates error {err}, which must satisfy Error"),
                        );
                    }
                    *ok
                }
                Ty::Unknown => Ty::Unknown,
                Ty::Option(_) => {
                    self.error(
                        span,
                        boundary::message(block, boundary::Construct::Try, reason),
                    );
                    Ty::Unknown
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
                | other @ Ty::Tuple(_)
                | other @ Ty::Struct(..)
                | other @ Ty::Enum(..)
                | other @ Ty::Param(_)
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
                        format!("'?' expects a `T?` or `T!E` value, found {other}"),
                    );
                    Ty::Unknown
                }
            };
        }
        // A defer block or a task has no caller (Rust rejects `?` in `Drop::drop`; the nursery
        // drops a task's `Err`, Go's contract). The user handles the error in the block. A
        // non-carrier operand keeps the `expects` diagnostic, and `Unknown` stays silent so an
        // already-reported operand does not cascade.
        if let boundary::Verdict::Rejected(msg) = verdict {
            return match t {
                Ty::Result(..) | Ty::Option(_) => {
                    self.error(span, msg);
                    Ty::Unknown
                }
                Ty::Unknown => Ty::Unknown,
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
                | other @ Ty::Tuple(_)
                | other @ Ty::Struct(..)
                | other @ Ty::Enum(..)
                | other @ Ty::Param(_)
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
                    | other @ Ty::Tuple(_)
                    | other @ Ty::Struct(..)
                    | other @ Ty::Enum(..)
                    | other @ Ty::Param(_)
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
                    | other @ Ty::Unknown
                    | other @ Ty::Var(_) => {
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
                    | other @ Ty::Tuple(_)
                    | other @ Ty::Struct(..)
                    | other @ Ty::Enum(..)
                    | other @ Ty::Param(_)
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
                    | other @ Ty::Unknown
                    | other @ Ty::Var(_) => {
                        self.error(span, self.try_outside_carrier_msg(&other));
                    }
                }
                *inner
            }
            Ty::Unknown => Ty::Unknown,
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
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
            | other @ Ty::Enum(..)
            | other @ Ty::Param(_)
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
                | Ty::Tuple(_)
                | Ty::Struct(..)
                | Ty::Enum(..)
                | Ty::Param(_)
                | Ty::Option(_)
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
                | Ty::Var(_) => return ty,
            },
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
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
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
            | Ty::Var(_) => return ty,
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
