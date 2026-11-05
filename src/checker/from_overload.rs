//! Checker: conversion overloads (Rust's several `impl From<S>` / `impl Into<T>`): the gate, the
//! one fit relation, overlap, the pick, and one owner per conversion pair.
//! `super::*` is the parent module.

use super::setup::{TypeHead, TypeHeadKind};
use super::*;

/// The slot of a conversion requirement that keys its overloads: the parameter that mentions the
/// protocol's type param (`from(x: S)`, the source), or the return of a receiver-only method
/// that IS the type param or its `!` (`into(self) -> T`, the target).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OverloadSlot {
    Param(usize),
    Ret,
}

impl OverloadSlot {
    /// The type at this slot of `sig`: the source of a `from`, the target of an `into` (the ok
    /// type of a `try_into`'s `T!E`).
    pub(super) fn key(self, sig: &FnSig) -> Option<&Ty> {
        match self {
            OverloadSlot::Param(i) => sig.params.get(i),
            OverloadSlot::Ret => Some(ret_ok(&sig.ret)),
        }
    }
}

/// A conversion requirement the gate admits overloads of: its name, its protocol spelled
/// (`From[S]`), its sig and the slot that keys the overloads.
pub(super) struct OverloadReq {
    pub(super) name: String,
    pub(super) spelled: String,
    pub(super) req: FnSig,
    pub(super) slot: OverloadSlot,
}

impl Checker {
    /// The gate: every requirement of a parameterized std protocol with an [`OverloadSlot`]:
    /// `From[S]`'s `from(x: S)`, `TryFrom[S]`'s `try_from(x: S)`, `Into[T]`'s `into(self) -> T`
    /// and `TryInto[T]`'s `try_into(self) -> T!` (`std/convert.chz`). Read from the std protocol
    /// table; `only` limits it to one name, checked on the declaration before any shape is read.
    fn overload_reqs(&self, only: Option<&str>) -> Vec<OverloadReq> {
        let mut out = Vec::new();
        for d in super::graph_pass::std_protocol_decls() {
            if let StmtKind::Protocol {
                methods,
                type_params,
                ..
            } = &d.stmt.kind
                && (type_params.is_empty()
                    || only.is_some_and(|n| !methods.iter().any(|m| m.name == n)))
            {
                continue;
            }
            let Some(pinfo) = self.protocol_shape(&d.name) else {
                continue;
            };
            if pinfo.type_params.is_empty() {
                continue;
            }
            let spelled = format!("{}[{}]", d.name, pinfo.type_params.join(", "));
            for (n, req) in pinfo.methods.iter() {
                if only.is_some_and(|o| o != n) {
                    continue;
                }
                if let Some(slot) = overload_slot(req, &pinfo.type_params) {
                    out.push(OverloadReq {
                        name: n.clone(),
                        spelled: spelled.clone(),
                        req: req.clone(),
                        slot,
                    });
                }
            }
        }
        out
    }

    /// The requirement [`Self::overload_reqs`] names `name`.
    pub(super) fn overload_requirement(&self, name: &str) -> Option<OverloadReq> {
        self.overload_reqs(Some(name)).into_iter().next()
    }

    /// Report each repeated method name of one type body as "already defined", except a name the
    /// gate admits as overloads: it names a requirement [`Self::overload_requirement`] finds and
    /// every declaration of it has the requirement's receiver (none for `from`, `self` for
    /// `into`). An overload set is two or more declarations; a lone `from` or `into` is a plain
    /// method. Returns the admitted names.
    pub(super) fn report_dup_methods(&mut self, methods: &[FnDecl]) -> HashSet<String> {
        let mut overloaded = HashSet::new();
        for m in methods {
            if overloaded.contains(&m.name) {
                continue;
            }
            let decls = || methods.iter().filter(|x| x.name == m.name);
            if decls().count() == 1 {
                continue;
            }
            let Some(r) = self.overload_requirement(&m.name) else {
                continue;
            };
            let instance = r.slot == OverloadSlot::Ret;
            if decls().all(|x| x.takes_self() == instance) {
                overloaded.insert(m.name.clone());
            }
        }
        self.report_dup_names(
            methods
                .iter()
                .filter(|m| !overloaded.contains(&m.name))
                .map(|m| (m.name.as_str(), m.name_span)),
            "method",
        );
        overloaded
    }

    /// Does a value of type `arg` fit a parameter of type `param`? Assignable, or a plain value the
    /// slot implicitly wraps (`3` into `int?`, as `meet_slot` decides). The one relation the
    /// overlap check and the overload pick share.
    pub(super) fn fits(&self, param: &Ty, arg: &Ty) -> bool {
        self.assignable(param, arg) || self.wrap_mode(param, arg).is_some()
    }

    /// Do two overload params overlap (one value fits both)? `Some(what)` names that value. The
    /// candidates: each param's own type (one fits the other; an alias is the same type), each
    /// param's carrier payload (a plain value the wrap rule puts into both: `3` into `int!str` and
    /// `int!int`), and `None`; else both are protocols (a type may satisfy both).
    fn overlap(&self, a: &Ty, b: &Ty) -> Option<String> {
        let payload = |t: &Ty| t.carrier_parts().map(|(_, p, _)| p.clone());
        let none = Ty::Option(Box::new(Ty::Unknown));
        let candidates = [Some(b.clone()), Some(a.clone()), payload(a), payload(b)];
        if let Some(v) = candidates
            .into_iter()
            .flatten()
            .chain([none])
            .find(|v| self.fits(a, v) && self.fits(b, v))
        {
            return Some(v.to_string());
        }
        match (a, b) {
            (Ty::Protocol(..), Ty::Protocol(..)) => {
                Some(format!("a type that satisfies both {a} and {b}"))
            }
            _ => None,
        }
    }

    /// Do two overload keys at `slot` overlap? A source: [`Self::overlap`]. A target: the pick is
    /// exact, so two equal targets.
    fn key_overlap(&self, slot: OverloadSlot, a: &Ty, b: &Ty) -> Option<String> {
        match slot {
            OverloadSlot::Param(_) => self.overlap(a, b),
            OverloadSlot::Ret => (a == b).then(|| a.to_string()),
        }
    }

    /// The declaration rules of one type body's overload sets (`overloads`, built at hoist in
    /// declaration order): each overload has the requirement's shape (Rust `E0053`: a `from` takes
    /// one param `x` and returns `self_ty`, an `into` takes only `self`, a `try_` twin returns
    /// `!`) and a concrete key, and no two keys overlap. Runs in pass 2, when every type a sig
    /// names is hoisted.
    pub(super) fn check_overload_decls(
        &mut self,
        methods: &[FnDecl],
        overloads: &HashMap<String, Vec<FnSig>>,
        own_type_params: &[TypeParam],
        self_ty: &Ty,
    ) {
        for (name, sigs) in overloads {
            let Some(r) = self.overload_requirement(name) else {
                continue;
            };
            let (spelled, req, slot) = (&r.spelled, &r.req, r.slot);
            let protocol = spelled.split('[').next().unwrap_or(spelled).to_string();
            let mut valid: Vec<(&FnDecl, &FnSig, &Ty)> = Vec::new();
            for (decl, sig) in methods.iter().filter(|m| &m.name == name).zip(sigs) {
                let rigid: Vec<String> = own_type_params
                    .iter()
                    .map(|tp| tp.name.clone())
                    .chain(sig.type_params.iter().map(|tp| tp.name.clone()))
                    .collect();
                let key = slot.key(sig);
                let msg = if decl.ret.is_none() {
                    format!("an overloaded {name} declares its return type, as {spelled} declares")
                } else if let Some(msg) = self.ret_misfit(&r, &sig.ret, self_ty) {
                    msg
                } else if sig.params.len() != req.params.len() && req.is_static {
                    let n = req.params.len();
                    let s = if n == 1 { "" } else { "s" };
                    format!("an overloaded {name} takes {n} parameter{s}, as {spelled} declares")
                } else if sig.params.len() != req.params.len() {
                    format!(
                        "an overloaded {name} takes no parameter but self, as {spelled} declares"
                    )
                } else if let Some(mm) = param_name_mismatch(req, sig) {
                    name_mismatch_text(&protocol, name, mm)
                } else if let Some(k) = key.filter(|k| mentions_any(k, &rigid)) {
                    match slot {
                        OverloadSlot::Param(_) => {
                            format!("an overloaded {name} takes a concrete type, found {k}")
                        }
                        OverloadSlot::Ret => {
                            format!("an overloaded {name} returns a concrete type, found {k}")
                        }
                    }
                } else {
                    if let Some(k) = key {
                        valid.push((decl, sig, k));
                    }
                    continue;
                };
                self.error(decl.name_span, msg);
            }
            for (j, (decl, _, b)) in valid.iter().enumerate() {
                let earlier = valid[..j].iter().find_map(|(d, s, a)| {
                    if a.is_unknown() || b.is_unknown() {
                        return None;
                    }
                    self.key_overlap(slot, a, b).map(|what| (d, s, a, what))
                });
                if let Some((d, s, a, what)) = earlier {
                    let shown = match slot {
                        OverloadSlot::Param(_) => {
                            let label = d.params.first().map_or("x", |p| p.name.as_str());
                            format!("{name}({label}: {a})")
                        }
                        OverloadSlot::Ret => format!("{name}(self) -> {}", s.ret),
                    };
                    let at = d.name_span;
                    self.error(
                        decl.name_span,
                        format!(
                            "conflicting {name} for {what}: {shown} at {}:{}",
                            at.line, at.col
                        ),
                    );
                }
            }
        }
    }
}

impl Checker {
    /// The message for an overload whose return `ret` is not what its requirement `r` declares
    /// (Rust `E0053`): `Self` (`self_ty`) for `from`, `Self!E` for `try_from`, a target `T` for
    /// `into`, `T!E` for `try_into`. A `!` return where a plain one is due names the fallible twin:
    /// the requirement that returns `r`'s return with `!` (`try_from`, `try_into`).
    fn ret_misfit(&self, r: &OverloadReq, ret: &Ty, self_ty: &Ty) -> Option<String> {
        let name = &r.name;
        let shape = ret_ok(&r.req.ret).to_string();
        let ok_fits = |ok: &Ty| match r.slot {
            OverloadSlot::Param(_) => *ok == *self_ty,
            OverloadSlot::Ret => true,
        };
        let is_result = matches!(ret, Ty::Result(..));
        if matches!(r.req.ret, Ty::Result(..)) {
            return (!(is_result && ok_fits(ret_ok(ret))))
                .then(|| format!("an overloaded {name} returns {shape}!E, found {ret}"));
        }
        if !is_result && ok_fits(ret) {
            return None;
        }
        let twin = (is_result && ok_fits(ret_ok(ret)))
            .then(|| {
                self.overload_reqs(None)
                    .into_iter()
                    .find(|t| matches!(&t.req.ret, Ty::Result(ok, _) if **ok == r.req.ret))
            })
            .flatten();
        let hint = twin
            .map(|t| format!("; a fallible conversion is {}", t.name))
            .unwrap_or_default();
        Some(format!(
            "an overloaded {name} returns {shape}, found {ret}{hint}"
        ))
    }

    /// One owner per conversion pair (Rust `E0119`): of `B.from(A)`, `B.try_from(A)`,
    /// `A.into -> B` and `A.try_into -> B!`, at most one converts an `A` to a `B`. Each conversion
    /// of the module's type bodies is a (source, target) pair: a `from`'s param to its type, its
    /// type to an `into`'s target. Two of different names with one target and an overlapping
    /// source conflict; two of one name are [`Self::check_overload_decls`]'s rule. A target
    /// declared in another module is checked from the `into` side, the module that names both
    /// types: its static conversions ([`Self::static_owner`]) are visible there, while the target's
    /// module may not name `A` at all (`B.from(x: P)` for a protocol `P` that `A` satisfies).
    pub(super) fn check_conversion_owners(&mut self, stmts: &[Stmt]) {
        let mut convs: Vec<(&FnDecl, Ty, Ty)> = Vec::new();
        for s in stmts {
            let (name, methods, is_enum) = if let StmtKind::Struct { name, methods, .. } = &s.kind {
                (name, methods, false)
            } else if let StmtKind::Enum { name, methods, .. } = &s.kind {
                (name, methods, true)
            } else {
                continue;
            };
            let reqs: Vec<(usize, OverloadReq)> = (0..methods.len())
                .filter_map(|i| Some((i, self.overload_requirement(&methods[i].name)?)))
                .filter(|(i, r)| methods[*i].takes_self() == (r.slot == OverloadSlot::Ret))
                .collect();
            if reqs.is_empty() {
                continue;
            }
            let key = self.bare_key(name);
            let (self_ty, table, overloads) = if is_enum {
                let table = self.enum_methods_of(&key).cloned().unwrap_or_default();
                let ov = self.enum_overloads_of(&key).cloned();
                (self.enum_self_ty(name), table, ov)
            } else {
                let Some(info) = self.structs.get(&key) else {
                    continue;
                };
                let (table, ov) = (info.methods.clone(), Some(info.overloads.clone()));
                (self.struct_self_ty(name), table, ov)
            };
            for (i, r) in reqs {
                let Some(sig) = method_decl_sig(methods, i, &table, overloads.as_ref()) else {
                    continue;
                };
                let Some(k) = r.slot.key(&sig).filter(|k| !k.is_unknown()).cloned() else {
                    continue;
                };
                let pair = match r.slot {
                    OverloadSlot::Param(_) => (k, self_ty.clone()),
                    OverloadSlot::Ret => (self_ty.clone(), k),
                };
                convs.push((&methods[i], pair.0, pair.1));
            }
        }
        for (decl, a, b) in &convs {
            let local = |d: &FnDecl, t: &Ty| !d.takes_self() && t == b;
            if !decl.takes_self() || convs.iter().any(|(d, _, t)| local(d, t)) {
                continue;
            }
            if let Some(owner) = self.static_owner(b, a) {
                let here = decl.name_span;
                self.error(
                    here,
                    format!(
                        "conflicting conversion {a} -> {b}: {} at {}:{} and {b}.{owner} of an imported module",
                        decl.name, here.line, here.col
                    ),
                );
            }
        }
        for (j, (decl, a2, b2)) in convs.iter().enumerate() {
            let earlier = convs[..j]
                .iter()
                .filter(|(d, _, b)| d.name != decl.name && b == b2)
                .find_map(|(d, a, _)| self.overlap(a, a2).map(|what| (d, what)));
            if let Some((d, what)) = earlier {
                let (at, here) = (d.name_span, decl.name_span);
                self.error(
                    here,
                    format!(
                        "conflicting conversion {what} -> {b2}: {} at {}:{} and {} at {}:{}",
                        d.name, at.line, at.col, decl.name, here.line, here.col
                    ),
                );
            }
        }
    }

    /// The sig of `methods[i]` at hoist: [`Self::fn_sig`], with the call-binding slots named for
    /// its place among same-named methods (each overload's default providers are its own).
    pub(super) fn method_sig_at(&mut self, methods: &[FnDecl], i: usize, self_ty: &Ty) -> FnSig {
        let m = &methods[i];
        let mut sig = self.fn_sig(m, m.name_span, Some(self_ty));
        let nth = methods[..i].iter().filter(|x| x.name == m.name).count();
        if nth > 0 {
            sig.slots = Some(self.decl_param_slots(m, m.name_span.file, nth));
        }
        sig
    }

    /// Record each overload's internal name under its decl name span, for the compiler (the
    /// harvest pass only, as every [`WitnessTable`] half).
    pub(super) fn record_overload_names(
        &mut self,
        methods: &[FnDecl],
        overloads: &HashMap<String, Vec<FnSig>>,
    ) {
        if !self.harvest_keywords {
            return;
        }
        for (name, sigs) in overloads {
            let Some(slot) = self.overload_requirement(name).map(|r| r.slot) else {
                continue;
            };
            for (decl, sig) in methods.iter().filter(|m| &m.name == name).zip(sigs) {
                if let Some(k) = slot.key(sig) {
                    self.witnesses.overload_names.insert(
                        (self.graph_module_idx, decl.name_span),
                        internal_name(name, k),
                    );
                }
            }
        }
    }

    /// The overload set of `name` on the type `th` names, when `name` is overloaded there.
    pub(super) fn head_overloads(&self, th: &TypeHead, name: &str) -> Option<Vec<FnSig>> {
        let sets = match th.kind {
            TypeHeadKind::Enum => self.enum_overloads_of(&th.key)?,
            TypeHeadKind::Struct if th.native_handle => return None,
            TypeHeadKind::Struct => &self.struct_shape(&th.key)?.overloads,
            TypeHeadKind::Protocol => return None,
        };
        sets.get(name).cloned()
    }

    /// The overload sets of the struct or enum `ty`, if it has any.
    pub(super) fn nominal_overloads(&self, ty: &Ty) -> Option<&HashMap<String, Vec<FnSig>>> {
        match ty {
            Ty::Struct(k, _) => self.struct_shape(k).map(|i| &i.overloads),
            Ty::Enum(k, _) => self.enum_overloads_of(k),
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
            | Ty::Var(_) => None,
        }
    }

    /// THE witness pick, read by conformance and by the witness record alike: the overload of
    /// `name` on `ty` that witnesses a requirement with param types `want` (its `S` already
    /// substituted). It is the direct call's pick ([`Self::fitting_overloads`]): the one overload
    /// the source fits, so a source with holes (`List[?]` from `[]`, `None`) still picks, and the
    /// declaration order never matters. `None` when `name` is not overloaded on `ty`; `Some(Err)`,
    /// the call's message, when no overload or more than one fits.
    pub(super) fn witness_overload(
        &self,
        ty: &Ty,
        name: &str,
        want: &[Ty],
    ) -> Option<Result<&FnSig, String>> {
        let set = self.nominal_overloads(ty)?.get(name)?;
        let slot = self.overload_requirement(name)?.slot;
        if slot == OverloadSlot::Ret {
            return Some(Err(format!(
                "its {name} is picked by the target type, as x.{name}[T]()"
            )));
        }
        let s = want.first().cloned().unwrap_or(Ty::Unknown);
        let fitting = self.fitting_overloads(set, slot, &s);
        Some(match fitting.as_slice() {
            [k] => Ok(&set[*k]),
            _ => Err(overload_misfit(
                set,
                slot,
                &fitting,
                &ty.to_string(),
                name,
                Some(&s),
            )),
        })
    }

    /// The direct call `Type.name(arg)` on an overloaded `name`: the index of the one overload in
    /// `set` whose param the argument [`Self::fits`]. The argument is the one positional argument
    /// or the keyword argument named as the param. No overload fits (no widening, Rust `E0277`),
    /// or more than one fits (Rust `E0283`), is an error that lists the overloads; `None` then, and
    /// the arguments are inferred for their own errors.
    pub(super) fn pick_overload(
        &mut self,
        set: &[FnSig],
        tname: &str,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<usize> {
        let named: Vec<(String, Expr)> = self
            .call_ctx
            .as_ref()
            .map(|c| c.named.clone())
            .unwrap_or_default();
        let label = set
            .first()
            .and_then(|s| s.slots.as_ref())
            .and_then(|sl| sl.first())
            .and_then(|sl| sl.name.clone())
            .unwrap_or_else(|| "x".to_string());
        let arg = match (args, named.as_slice()) {
            ([a], []) => Some(a.clone()),
            ([], [(n, a)]) if *n == label => Some(a.clone()),
            _ => None,
        };
        let Some(arg) = arg else {
            self.infer_all(args);
            self.consume_named();
            let msg = match named.iter().find(|(n, _)| *n != label) {
                Some((k, _)) => super::expr::unknown_named_arg(k, &[label.as_str()]),
                None => {
                    let n = args.len() + named.len();
                    format!("an overloaded {name} takes 1 argument, found {n}")
                }
            };
            self.error(span, msg);
            return None;
        };
        let saved = std::mem::replace(&mut self.generic_arg_prepass, true);
        let t = self.probe_ty(&arg);
        self.generic_arg_prepass = saved;
        let slot = OverloadSlot::Param(0);
        let fitting = self.fitting_overloads(set, slot, &t);
        if let [k] = fitting.as_slice() {
            return Some(*k);
        }
        let before = self.errors.len();
        self.infer_all(args);
        self.consume_named();
        if !fitting.is_empty() && t.is_unknown() && self.errors.len() > before {
            // The argument's own error already explains it.
            return None;
        }
        self.error(
            span,
            overload_misfit(set, slot, &fitting, tname, name, Some(&t)),
        );
        None
    }

    /// The overloaded `name` read as a value (`f: fn(int) -> Rat = Rat.from`): the index of
    /// the one overload whose param [`Self::fits`] the single param of the position's fn type
    /// (the `expected_hint`). No fn type there, or one that does not pick exactly one, is an
    /// error that lists the overloads (Rust `E0283`: `let g = Rat::from;` needs an annotation).
    pub(super) fn pick_overload_value(
        &mut self,
        set: &[FnSig],
        tname: &str,
        name: &str,
        span: Span,
    ) -> Option<usize> {
        let slot = self
            .overload_requirement(name)
            .map_or(OverloadSlot::Param(0), |r| r.slot);
        let want = match self.expected_hint.clone() {
            Some(Ty::Func { params, ret, .. }) if params.len() == 1 => match slot {
                OverloadSlot::Param(_) => params.into_iter().next(),
                OverloadSlot::Ret => Some(*ret),
            },
            _ => None,
        };
        let fitting = match &want {
            Some(t) => self.fitting_overloads(set, slot, t),
            None => (0..set.len()).collect(),
        };
        if let [k] = fitting.as_slice() {
            return Some(*k);
        }
        self.error(
            span,
            overload_misfit(set, slot, &fitting, tname, name, want.as_ref()),
        );
        None
    }

    /// Is `e` a read of an overloaded `Type.name` as a value (`Rat.from` with 2+ overloads)?
    /// A generic slot (`.map`'s `fn(int) -> U`) then seeds it, so [`Self::pick_overload_value`]
    /// picks by the slot's param types while its return is still free.
    pub(super) fn is_overload_value(&self, e: &Expr) -> bool {
        let ExprKind::Field { obj, name, .. } = &e.kind else {
            return false;
        };
        self.peel_type_path(obj)
            .is_some_and(|(th, _)| self.head_overloads(&th, name).is_some())
    }

    /// The indexes of the overloads in `set` whose key at `slot` takes `t`: a source an argument
    /// of type `t` [`Self::fits`], or the target `t` itself (Rust's `Into<T>` is exact).
    fn fitting_overloads(&self, set: &[FnSig], slot: OverloadSlot, t: &Ty) -> Vec<usize> {
        (0..set.len())
            .filter(|&i| {
                slot.key(&set[i]).is_some_and(|k| match slot {
                    OverloadSlot::Param(_) => self.fits(k, t),
                    OverloadSlot::Ret => k == t,
                })
            })
            .collect()
    }

    /// Record the overload a direct call picked (`set[k]`) under the call's [`WitnessKey`], for
    /// the compiler's `CallStatic` (the harvest pass only, as every [`WitnessTable`] half).
    pub(super) fn record_overload_call(
        &mut self,
        name: &str,
        sig: &FnSig,
        key_span: Span,
        span: Span,
    ) {
        if !self.harvest_keywords || self.generic_arg_prepass || self.resolving_returns {
            return;
        }
        let slot = self
            .overload_requirement(name)
            .map_or(OverloadSlot::Param(0), |r| r.slot);
        let Some(s) = slot.key(sig) else {
            return;
        };
        crate::checker::record_call_table_entry(
            &mut self.witnesses.overload_calls,
            &mut self.table_conflicts,
            crate::checker::witness_key(
                self.graph_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                key_span,
            ),
            internal_name(name, s),
            "from overload",
            span,
        );
    }
}

impl Checker {
    /// `x.into[T]()` on a receiver of type `obj_ty`, when `method` is a conversion keyed by its
    /// target (`Into[T]`'s `into`, `TryInto[T]`'s `try_into`): the `[T]` picks the one overload of
    /// the receiver whose target is `T`, and the call is that overload's return. A lone `into` is
    /// a plain method: the same pick checks a `[T]` on it against its return, then clears
    /// `type_args` for the ordinary call. With no `into` and a `T` that converts the receiver's
    /// type itself (`B.from(A)`), the error names that conversion: there is no blanket `into` from
    /// a `from`. `None` when the ordinary method call runs.
    #[allow(clippy::too_many_arguments)] // receiver + its type + method + args + targs + spans
    pub(super) fn infer_target_call(
        &mut self,
        obj: &Expr,
        obj_ty: &Ty,
        method: &str,
        args: &[Expr],
        type_args: &mut &[Ty],
        name_span: Span,
        span: Span,
    ) -> Option<Ty> {
        let set = self
            .nominal_overloads(obj_ty)
            .and_then(|o| o.get(method))
            .cloned();
        if set.is_none() && type_args.is_empty() {
            return None;
        }
        if self.overload_requirement(method)?.slot != OverloadSlot::Ret {
            return None;
        }
        let recv = if let ExprKind::Ident(n) = &obj.kind {
            n.clone()
        } else {
            "x".to_string()
        };
        let tname = obj_ty.to_string();
        let Some(set) = set else {
            if self.member_own_type_params(obj_ty, method) == Some(true) {
                return None;
            }
            let one = self.conversion_sigs(obj_ty, method);
            if let [sig] = one.as_slice()
                && !sig.is_static
            {
                let targs = *type_args;
                if self
                    .pick_target(&one, &tname, method, targs, &recv, Some(obj_ty), name_span)
                    .is_none()
                {
                    self.infer_all(args);
                    self.consume_named();
                    return Some(Ty::Unknown);
                }
                *type_args = &[];
                return None;
            }
            let [t] = *type_args else {
                return None;
            };
            let hint = self.owner_hint(t, obj_ty, &recv)?;
            self.infer_all(args);
            self.consume_named();
            self.error(
                name_span,
                format!("type {obj_ty} has no {method} for {t}{hint}"),
            );
            return Some(Ty::Unknown);
        };
        let named = self.call_ctx.as_ref().map_or(0, |c| c.named.len());
        if !args.is_empty() || named > 0 {
            self.infer_all(args);
            self.consume_named();
            let n = args.len() + named;
            self.error(
                span,
                format!("an overloaded {method} takes no argument, found {n}"),
            );
            return Some(Ty::Unknown);
        }
        let Some(k) = self.pick_target(
            &set,
            &tname,
            method,
            type_args,
            &recv,
            Some(obj_ty),
            name_span,
        ) else {
            return Some(Ty::Unknown);
        };
        self.record_overload_call(method, &set[k], name_span, span);
        Some(set[k].ret.clone())
    }

    /// Is `name` a conversion keyed by its target (`into`, `try_into`)?
    pub(super) fn target_keyed(&self, name: &str) -> bool {
        self.overload_requirement(name)
            .is_some_and(|r| r.slot == OverloadSlot::Ret)
    }

    /// The call `x.name[T]()` (or `A.name[T](x)`) on `set`, an overload set keyed by its target,
    /// with `targs` its type arguments and `src` the receiver's type when known: the index of the
    /// one overload whose target is `T`. No `[T]` ("needs a target type"), or no overload for `T`
    /// (with the owner of the pair when `T` converts `src` itself), is an error.
    #[allow(clippy::too_many_arguments)] // the set + its type + name + targs + receiver + span
    pub(super) fn pick_target(
        &mut self,
        set: &[FnSig],
        tname: &str,
        name: &str,
        targs: &[Ty],
        recv: &str,
        src: Option<&Ty>,
        span: Span,
    ) -> Option<usize> {
        let t = match targs {
            [t] => t,
            [] => {
                self.error(
                    span,
                    format!("{name} on {tname} needs a target type: {recv}.{name}[T]()"),
                );
                return None;
            }
            [..] => {
                let n = targs.len();
                self.error(
                    span,
                    format!("{name} takes 1 type argument (the target), found {n}"),
                );
                return None;
            }
        };
        let slot = OverloadSlot::Ret;
        let fitting = self.fitting_overloads(set, slot, t);
        if let [k] = fitting.as_slice() {
            return Some(*k);
        }
        let hint = src
            .and_then(|s| self.owner_hint(t, s, recv))
            .unwrap_or_default();
        let msg = overload_misfit(set, slot, &fitting, tname, name, Some(t));
        self.error(span, format!("{msg}{hint}"));
        None
    }

    /// When the target `t` converts `src` itself (a static conversion of `t` whose source `src`
    /// fits: `Big.from(x: Rat)`), the note that names it: "; Big defines it: use Big.from(r)".
    fn owner_hint(&self, t: &Ty, src: &Ty, recv: &str) -> Option<String> {
        let owner = self.static_owner(t, src)?;
        Some(format!("; {t} defines it: use {t}.{owner}({recv})"))
    }

    /// The static conversion of the target `t` that converts `src` (`from` for `Big.from(x: Rat)`
    /// and a `Rat`, or a type that satisfies a protocol-typed source), if any.
    fn static_owner(&self, t: &Ty, src: &Ty) -> Option<String> {
        let owner = self.overload_reqs(None).into_iter().find(|r| {
            let OverloadSlot::Param(i) = r.slot else {
                return false;
            };
            self.conversion_sigs(t, &r.name).iter().any(|sig| {
                sig.is_static
                    && sig
                        .params
                        .get(i)
                        .is_some_and(|p| !p.is_unknown() && self.fits(p, src))
            })
        })?;
        Some(owner.name)
    }

    /// The sigs of `ty`'s method `name`: its overload set, else its one method (a struct, an
    /// enum or a builtin scalar's native struct).
    fn conversion_sigs(&self, ty: &Ty, name: &str) -> Vec<FnSig> {
        if let Some(set) = self.nominal_overloads(ty).and_then(|o| o.get(name)) {
            return set.clone();
        }
        let table = if let Ty::Enum(k, _) = ty {
            self.enum_methods_of(k)
        } else {
            let key = if let Ty::Struct(k, _) = ty {
                Some(k.as_str())
            } else {
                Self::native_witness_key(ty)
            };
            key.and_then(|k| self.struct_shape(k)).map(|i| &i.methods)
        };
        table
            .and_then(|t| t.get(name))
            .cloned()
            .into_iter()
            .collect()
    }
}

/// The std protocols whose every requirement is a conversion keyed by its target (`Into[T]`,
/// `TryInto[T]`): an instance method no hook or operator calls, reached only as `x.into[T]()`.
/// Read with the gate's own [`overload_slot`] off the harvested std shapes. Test surface: the
/// protocol grid (`tests/protocol_method_grid.rs`) exempts these from its hook-owner rule.
#[cfg(any(test, feature = "test-api"))]
pub fn std_target_keyed_protocols() -> Vec<String> {
    let mut c = Checker::new();
    // The prelude types a shape names (`T!` is `$Result`), as the single-module check seeds them.
    c.seed_native_prelude_sigs();
    c.seed_std_protocols();
    super::graph_pass::std_protocol_decls()
        .iter()
        .filter(|d| {
            c.protocol_shape(&d.name).is_some_and(|p| {
                !p.methods.is_empty()
                    && p.methods
                        .iter()
                        .all(|(_, r)| overload_slot(r, &p.type_params) == Some(OverloadSlot::Ret))
            })
        })
        .map(|d| d.name.clone())
        .collect()
}

/// The [`OverloadSlot`] of the requirement `req` of a protocol with the type params `tps`: a
/// static's first param that mentions one, or the return of a receiver-only method that IS one
/// (or its `!`). `None` for every other requirement (`Iterator[T]`'s `next(self) -> T?`).
fn overload_slot(req: &FnSig, tps: &[String]) -> Option<OverloadSlot> {
    if req.is_static {
        return req
            .params
            .iter()
            .position(|p| mentions_any(p, tps))
            .map(OverloadSlot::Param);
    }
    let is_tp = matches!(ret_ok(&req.ret), Ty::Param(n) if tps.contains(n));
    (req.params.len() == 1 && is_tp).then_some(OverloadSlot::Ret)
}

/// The ok type of a `T!E` return, else the return itself.
fn ret_ok(ret: &Ty) -> &Ty {
    if let Ty::Result(ok, _) = ret { ok } else { ret }
}

/// THE message for a use of the overloaded `name` that does not pick exactly one of `set`:
/// `fitting` are the overloads whose key at `slot` takes `t` (every overload when `t` is `None`, a
/// value read with no fn type). None fits: "has no from for float"; several: "type annotations
/// needed".
fn overload_misfit(
    set: &[FnSig],
    slot: OverloadSlot,
    fitting: &[usize],
    tname: &str,
    name: &str,
    t: Option<&Ty>,
) -> String {
    let shown = |ix: &mut dyn Iterator<Item = usize>| {
        ix.filter_map(|i| slot.key(&set[i]).map(Ty::to_string))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match t {
        Some(t) if fitting.is_empty() => {
            let all = shown(&mut (0..set.len()));
            format!("type {tname} has no {name} for {t} (it has: {all})")
        }
        _ => {
            let n = fitting.len();
            let which = shown(&mut fitting.iter().copied());
            let take = t.map(|t| format!(" that take {t}")).unwrap_or_default();
            format!("type annotations needed: {tname}.{name} has {n} overloads{take} ({which})")
        }
    }
}

/// The internal name of the overload of `base` that takes `s`: `from$int`. The ONE place it is
/// decided. Nominal types are spelled by their identity key, so two distinct types never share a
/// name. Users never see it: `provider_name::display_fn_name` cuts it at the separator.
pub(super) fn internal_name(base: &str, s: &Ty) -> String {
    format!(
        "{base}{}{}",
        crate::base::provider_name::OVERLOAD_SEP,
        s.render_keyed()
    )
}

/// The overload sets of one type body: each admitted name's sigs, in declaration order.
pub(super) fn overload_sets(
    overloaded: &HashSet<String>,
    sigs: &[(String, FnSig)],
) -> HashMap<String, Vec<FnSig>> {
    let mut sets: HashMap<String, Vec<FnSig>> = HashMap::new();
    for (name, sig) in sigs {
        if overloaded.contains(name) {
            sets.entry(name.clone()).or_default().push(sig.clone());
        }
    }
    sets
}

/// The signature the body of `methods[i]` is checked against: its own overload's sig when its name
/// is overloaded, else the method table's. `None` for a repeat the gate did not admit: its
/// "already defined" error stands alone, not a mismatch against the surviving sig.
pub(super) fn method_decl_sig(
    methods: &[FnDecl],
    i: usize,
    table: &HashMap<String, FnSig>,
    overloads: Option<&HashMap<String, Vec<FnSig>>>,
) -> Option<FnSig> {
    let name = &methods[i].name;
    if let Some(set) = overloads.and_then(|o| o.get(name)) {
        let nth = methods[..i].iter().filter(|m| &m.name == name).count();
        return set.get(nth).cloned();
    }
    if methods.iter().filter(|m| &m.name == name).count() > 1 {
        return None;
    }
    table.get(name).cloned()
}

/// Does `ty` mention any of the type params `names`? A `subst` round-trip, as `mentions_self`.
fn mentions_any(ty: &Ty, names: &[String]) -> bool {
    let probe: HashMap<String, Ty> = names.iter().map(|n| (n.clone(), Ty::Unknown)).collect();
    subst(ty, &probe) != *ty
}
