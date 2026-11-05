//! Checker: static `convert` overloads (Rust's several `impl From<S>`): the gate, the one fit relation, overlap, the pick.
//! `super::*` is the parent module.

use super::setup::{TypeHead, TypeHeadKind};
use super::*;

impl Checker {
    /// The gate: the static requirement named `name` of a parameterized std protocol whose
    /// parameter mentions the protocol's own type param, with the protocol spelled `Convert[S]`.
    /// Today that is only `Convert[S]`'s `convert(x: S)` (`std/convert.chz`). Read from the std
    /// protocol table, and only when a method name repeats.
    fn overload_requirement(&self, name: &str) -> Option<(String, FnSig)> {
        super::graph_pass::std_protocol_decls()
            .iter()
            .find_map(|d| {
                let pinfo = self.protocol_shape(&d.name)?;
                if pinfo.type_params.is_empty() {
                    return None;
                }
                let (_, req) = pinfo.methods.iter().find(|(n, s)| {
                    n == name
                        && s.is_static
                        && s.params.iter().any(|p| mentions_any(p, &pinfo.type_params))
                })?;
                let spelled = format!("{}[{}]", d.name, pinfo.type_params.join(", "));
                Some((spelled, req.clone()))
            })
    }

    /// Report each repeated method name of one type body as "already defined", except a name the
    /// gate admits as overloads: every declaration of it is static and it names a requirement
    /// [`Self::overload_requirement`] finds. Returns the admitted names.
    pub(super) fn report_dup_methods(&mut self, methods: &[FnDecl]) -> HashSet<String> {
        let mut overloaded = HashSet::new();
        for m in methods {
            let decls = || methods.iter().filter(|x| x.name == m.name);
            if decls().count() > 1
                && decls().all(|x| !x.params.iter().any(Param::is_self))
                && self.overload_requirement(&m.name).is_some()
            {
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

    /// The declaration rules of one type body's overload sets (`overloads`, built at hoist in
    /// declaration order): each overload has the requirement's shape (one param `x`, a return of
    /// `self_ty` or `self_ty!E`, Rust `E0053`) and a concrete param, and no two overlap. Runs in
    /// pass 2, when every type a param names is hoisted.
    pub(super) fn check_overload_decls(
        &mut self,
        methods: &[FnDecl],
        overloads: &HashMap<String, Vec<FnSig>>,
        own_type_params: &[TypeParam],
        self_ty: &Ty,
    ) {
        for (name, sigs) in overloads {
            let Some((spelled, req)) = self.overload_requirement(name) else {
                continue;
            };
            let protocol = spelled.split('[').next().unwrap_or(&spelled).to_string();
            let mut valid: Vec<(&FnDecl, &FnSig)> = Vec::new();
            for (decl, sig) in methods.iter().filter(|m| &m.name == name).zip(sigs) {
                let rigid: Vec<String> = own_type_params
                    .iter()
                    .map(|tp| tp.name.clone())
                    .chain(sig.type_params.iter().map(|tp| tp.name.clone()))
                    .collect();
                let msg = if decl.ret.is_none() {
                    format!("an overloaded {name} declares its return type, as {spelled} declares")
                } else if sig.ret != *self_ty
                    && !matches!(&sig.ret, Ty::Result(ok, _) if **ok == *self_ty)
                {
                    format!(
                        "an overloaded {name} returns Self or Self!E, found {}",
                        sig.ret
                    )
                } else if sig.params.len() != req.params.len() {
                    let n = req.params.len();
                    let s = if n == 1 { "" } else { "s" };
                    format!("an overloaded {name} takes {n} parameter{s}, as {spelled} declares")
                } else if let Some(mm) = param_name_mismatch(&req, sig) {
                    name_mismatch_text(&protocol, name, mm)
                } else if let Some(p) = sig.params.iter().find(|p| mentions_any(p, &rigid)) {
                    format!("an overloaded {name} takes a concrete type, found {p}")
                } else {
                    valid.push((decl, sig));
                    continue;
                };
                self.error(decl.name_span, msg);
            }
            for (j, (decl, sig)) in valid.iter().enumerate() {
                let b = &sig.params[0];
                let earlier = valid[..j].iter().find_map(|(d, s)| {
                    let a = &s.params[0];
                    if a.is_unknown() || b.is_unknown() {
                        return None;
                    }
                    self.overlap(a, b).map(|what| (d, a, what))
                });
                if let Some((d, a, what)) = earlier {
                    let label = d.params.first().map_or("x", |p| p.name.as_str());
                    let at = d.name_span;
                    self.error(
                        decl.name_span,
                        format!(
                            "conflicting {name} for {what}: {name}({label}: {a}) at {}:{}",
                            at.line, at.col
                        ),
                    );
                }
            }
        }
    }
}

impl Checker {
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
            for (decl, sig) in methods.iter().filter(|m| &m.name == name).zip(sigs) {
                if let Some(s) = sig.params.first() {
                    self.witnesses.overload_names.insert(
                        (self.graph_module_idx, decl.name_span),
                        internal_name(name, s),
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
        let s = want.first().cloned().unwrap_or(Ty::Unknown);
        let fitting = self.fitting_overloads(set, &s);
        Some(match fitting.as_slice() {
            [k] => Ok(&set[*k]),
            _ => Err(overload_misfit(
                set,
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
        let fitting = self.fitting_overloads(set, &t);
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
        self.error(span, overload_misfit(set, &fitting, tname, name, Some(&t)));
        None
    }

    /// The overloaded `name` read as a value (`f: fn(int) -> Rat = Rat.convert`): the index of
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
        let want = match self.expected_hint.clone() {
            Some(Ty::Func { params, .. }) if params.len() == 1 => params.into_iter().next(),
            _ => None,
        };
        let fitting = match &want {
            Some(t) => self.fitting_overloads(set, t),
            None => (0..set.len()).collect(),
        };
        if let [k] = fitting.as_slice() {
            return Some(*k);
        }
        self.error(
            span,
            overload_misfit(set, &fitting, tname, name, want.as_ref()),
        );
        None
    }

    /// Is `e` a read of an overloaded `Type.name` as a value (`Rat.convert` with 2+ overloads)?
    /// A generic slot (`.map`'s `fn(int) -> U`) then seeds it, so [`Self::pick_overload_value`]
    /// picks by the slot's param types while its return is still free.
    pub(super) fn is_overload_value(&self, e: &Expr) -> bool {
        let ExprKind::Field { obj, name, .. } = &e.kind else {
            return false;
        };
        self.peel_type_path(obj)
            .is_some_and(|(th, _)| self.head_overloads(&th, name).is_some())
    }

    /// The indexes of the overloads in `set` whose param an argument of type `t` fits.
    fn fitting_overloads(&self, set: &[FnSig], t: &Ty) -> Vec<usize> {
        (0..set.len())
            .filter(|&i| set[i].params.first().is_some_and(|p| self.fits(p, t)))
            .collect()
    }

    /// Record the overload a direct call picked (`set[k]`) under the call's [`WitnessKey`], for
    /// the compiler's `CallStatic` (the harvest pass only, as every [`WitnessTable`] half).
    pub(super) fn record_overload_call(&mut self, name: &str, s: &Ty, key_span: Span, span: Span) {
        if !self.harvest_keywords || self.generic_arg_prepass || self.resolving_returns {
            return;
        }
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
            "convert overload",
            span,
        );
    }
}

/// THE message for a use of the overloaded `name` that does not pick exactly one of `set`:
/// `fitting` are the overloads a source type `t` fits (every overload when `t` is `None`, a value
/// read with no fn type). None fits: "has no convert for float"; several: "type annotations
/// needed".
fn overload_misfit(
    set: &[FnSig],
    fitting: &[usize],
    tname: &str,
    name: &str,
    t: Option<&Ty>,
) -> String {
    let shown = |ix: &mut dyn Iterator<Item = usize>| {
        ix.filter_map(|i| set[i].params.first().map(Ty::to_string))
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

/// The internal name of the overload of `base` that takes `s`: `convert$int`. The ONE place it is
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
