//! Checker: protocol-slot overloads (Rust's several `impl From<S>` / `impl Parse<S>`): the slot of
//! a method decided once at its declaration, the one fit relation, overlap and the pick.
//! `super::*` is the parent module.

use super::setup::{TypeHead, TypeHeadKind};
use super::*;
use std::sync::Arc;

/// The slot of a protocol requirement that keys its overloads: the parameter that mentions the
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

    /// The type a payload hint (the operand of `?`, `??` or `else:`, which names a carrier's
    /// `T`) reads at this slot: a target's carrier payload, the `int` of `-> int?` as of
    /// `-> int!` (whose [`Self::key`] is that same `int`). `None` for a source or a plain target.
    fn payload_key(self, sig: &FnSig) -> Option<&Ty> {
        match self {
            OverloadSlot::Param(_) => None,
            OverloadSlot::Ret => sig.ret.carrier_payload(),
        }
    }
}

/// A requirement of a parameterized protocol that keys overloads: its name, its protocol's key and
/// the slot. Its sig and spelling are read off the protocol only where a declaration is checked
/// ([`Checker::req_shape`]), so deciding a slot copies no signature.
#[derive(Clone)]
pub(super) struct OverloadReq {
    pub(super) name: String,
    pub(super) protocol: String,
    pub(super) slot: OverloadSlot,
}

/// One type body's protocol-slot methods, decided at its declaration and carried with the type
/// (an import copies it): `slots` names each method a visible protocol keys, a lone one included;
/// the sets (read through `Deref`) are each slotted name declared 2+ times, its sigs in
/// declaration order.
#[derive(Clone, Default)]
pub(super) struct Overloads {
    pub(super) slots: HashMap<String, Arc<OverloadReq>>,
    sets: HashMap<String, Vec<FnSig>>,
}

impl std::ops::Deref for Overloads {
    type Target = HashMap<String, Vec<FnSig>>;
    fn deref(&self) -> &Self::Target {
        &self.sets
    }
}

impl Checker {
    /// Every requirement with an [`OverloadSlot`] named in `only` (`None`: any name), of the
    /// parameterized protocols this module sees by a name it declares or imports (`import From
    /// from std.convert`, `import p` for `p.Parse`); a std module sees every std protocol
    /// ([`Self::std_slot_table`]). A protocol with no wanted method is skipped before any key or sig
    /// is built (this runs per type body). Sorted by protocol key, so a message naming two reads
    /// the same every run.
    fn visible_slot_reqs(&self, only: Option<&[&str]>) -> Vec<Arc<OverloadReq>> {
        if self.current_module_is_stdlib {
            let table = self.std_slot_table();
            return table
                .iter()
                .filter(|r| only.is_none_or(|w| w.contains(&r.name.as_str())))
                .cloned()
                .collect();
        }
        let wanted = &|n: &str| only.is_none_or(|w| w.contains(&n));
        let keep = |p: &ProtocolInfo| {
            !p.type_params.is_empty() && p.methods.iter().any(|(n, _)| wanted(n))
        };
        let mut found: Vec<(String, &ProtocolInfo)> = Vec::new();
        // A declared or from-imported protocol is in `protocols` under its key (hoist, `bind_import`).
        for (bind, key) in &self.bare_types {
            let other_type = self.struct_names.contains(bind)
                || self.enum_names.contains(bind)
                || self.aliases.contains_key(bind);
            if !other_type && let Some(p) = self.protocols.get(key).filter(|p| keep(p)) {
                found.push((key.clone(), p));
            }
        }
        for mid in self.imported_modules.values() {
            if let Some(sig) = self.module_sigs.get(mid) {
                for (member, def) in &sig.protocol_defs {
                    if keep(&def.info) {
                        found.push((self.type_key(mid, member), &def.info));
                    }
                }
            }
        }
        slot_reqs(found, wanted).into_iter().map(Arc::new).collect()
    }

    /// The slotted requirements of every parameterized std protocol, sorted by protocol key. The
    /// std shapes are the same for every checker of a process (`seed_std_protocols`), so the table
    /// is derived once and shared; a checker missing a std shape derives its own.
    fn std_slot_table(&self) -> std::borrow::Cow<'static, [Arc<OverloadReq>]> {
        static TABLE: std::sync::OnceLock<Vec<Arc<OverloadReq>>> = std::sync::OnceLock::new();
        if let Some(t) = TABLE.get() {
            return std::borrow::Cow::Borrowed(t);
        }
        // Only a parameterized protocol keys a slot: its decl says so before any shape is read.
        let decls: Vec<&str> = super::graph_pass::std_protocol_decls()
            .iter()
            .filter(|d| {
                matches!(&d.stmt.kind, StmtKind::Protocol { type_params, .. } if !type_params.is_empty())
            })
            .map(|d| d.name.as_str())
            .collect();
        let found: Vec<(String, &ProtocolInfo)> = decls
            .iter()
            .filter_map(|k| Some((k.to_string(), self.protocols.get(*k)?)))
            .collect();
        let complete = found.len() == decls.len();
        let table: Vec<Arc<OverloadReq>> = slot_reqs(found, &|_| true)
            .into_iter()
            .map(Arc::new)
            .collect();
        if complete {
            std::borrow::Cow::Borrowed(TABLE.get_or_init(|| table))
        } else {
            std::borrow::Cow::Owned(table)
        }
    }

    /// THE owner of "is this method a protocol slot", decided once per type body at hoist: for each
    /// method name, the requirement of a visible protocol ([`Self::visible_slot_reqs`]) that keys
    /// it, when every declaration of the name has that slot's receiver (a `Param` slot keys a
    /// static, a `Ret` slot an instance method). Any other method is plain. Two visible protocols
    /// whose requirements can both key one declared name (same receiver) and key it differently
    /// are an error naming both.
    pub(super) fn decl_slots(&mut self, methods: &[FnDecl]) -> HashMap<String, Arc<OverloadReq>> {
        let mut slots = HashMap::new();
        if methods.is_empty() {
            return slots;
        }
        let mut names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let reqs = self.visible_slot_reqs(Some(&names));
        if reqs.is_empty() {
            return slots;
        }
        for (i, m) in methods.iter().enumerate() {
            if methods[..i].iter().any(|x| x.name == m.name) {
                continue;
            }
            // The receiver decides which requirements can key this name, before any conflict: a
            // `Ret` slot keys only instance methods, a `Param` slot only statics.
            let decls = || methods.iter().filter(|x| x.name == m.name);
            let mut named = reqs.iter().filter(|r| {
                let instance = r.slot == OverloadSlot::Ret;
                r.name == m.name && decls().all(|x| x.takes_self() == instance)
            });
            let Some(first) = named.next() else {
                continue;
            };
            if let Some(other) = named.find(|r| r.slot != first.slot) {
                let spelled =
                    |r: &OverloadReq| self.req_shape(r).map(|(s, _)| s).unwrap_or_default();
                let (a, b, name) = (spelled(first), spelled(other), &m.name);
                self.error(
                    m.name_span,
                    format!(
                        "protocols {a} and {b} both declare '{name}' with different overload keys"
                    ),
                );
                continue;
            }
            slots.insert(m.name.clone(), first.clone());
        }
        slots
    }

    /// Report each repeated method name of one type body as "already defined", except a name the
    /// body's [`Self::decl_slots`] key: its 2+ declarations are an overload set. Returns the
    /// slots, for [`overload_sets`].
    pub(super) fn report_dup_methods(
        &mut self,
        methods: &[FnDecl],
    ) -> HashMap<String, Arc<OverloadReq>> {
        let slots = self.decl_slots(methods);
        self.report_dup_names(
            methods
                .iter()
                .filter(|m| !slots.contains_key(&m.name))
                .map(|m| (m.name.as_str(), m.name_span)),
            "method",
        );
        slots
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
        overloads: &Overloads,
        own_type_params: &[TypeParam],
        self_ty: &Ty,
    ) {
        for (name, sigs) in overloads.iter() {
            let Some(r) = overloads.slots.get(name) else {
                continue;
            };
            let Some((spelled, req)) = self.req_shape(r) else {
                continue;
            };
            let (spelled, req, slot) = (&spelled, &req, r.slot);
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
                } else if let Some(msg) = self.ret_misfit(r, req, &sig.ret, self_ty) {
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
    fn ret_misfit(&self, r: &OverloadReq, req: &FnSig, ret: &Ty, self_ty: &Ty) -> Option<String> {
        let name = &r.name;
        let shape = ret_ok(&req.ret).to_string();
        let ok_fits = |ok: &Ty| match r.slot {
            OverloadSlot::Param(_) => *ok == *self_ty,
            OverloadSlot::Ret => true,
        };
        let is_result = matches!(ret, Ty::Result(..));
        if matches!(req.ret, Ty::Result(..)) {
            return (!(is_result && ok_fits(ret_ok(ret))))
                .then(|| format!("an overloaded {name} returns {shape}!E, found {ret}"));
        }
        if !is_result && ok_fits(ret) {
            return None;
        }
        let twin = (is_result && ok_fits(ret_ok(ret)))
            .then(|| {
                self.visible_slot_reqs(None).into_iter().find(|t| {
                    self.req_shape(t).is_some_and(
                        |(_, s)| matches!(&s.ret, Ty::Result(ok, _) if **ok == req.ret),
                    )
                })
            })
            .flatten();
        let hint = twin
            .map(|t| format!("; a fallible conversion is {}", t.name))
            .unwrap_or_default();
        Some(format!(
            "an overloaded {name} returns {shape}, found {ret}{hint}"
        ))
    }

    /// The protocol of `r` spelled with its type params (`From[S]`) and the requirement's sig.
    fn req_shape(&self, r: &OverloadReq) -> Option<(String, FnSig)> {
        let p = self
            .protocols
            .get(&r.protocol)
            .or_else(|| self.owning_protocol_def(&r.protocol))?;
        let (_, req) = p.methods.iter().find(|(n, _)| *n == r.name)?;
        let bare = crate::base::words::bare_display(&r.protocol);
        Some((format!("{bare}[{}]", p.type_params.join(", ")), req.clone()))
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
    pub(super) fn record_overload_names(&mut self, methods: &[FnDecl], overloads: &Overloads) {
        if !self.harvest_keywords {
            return;
        }
        for (name, sigs) in overloads.iter() {
            let Some(slot) = overloads.slots.get(name).map(|r| r.slot) else {
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

    /// The protocol-slot methods of the type `th` names: a struct's, an enum's, or a builtin's
    /// native struct's (`int.from`).
    fn head_table(&self, th: &TypeHead) -> Option<&Overloads> {
        self.overloads_at(th.kind, &th.key, th.native_handle)
    }

    /// THE overload-table lookup, by the type's kind and key (`native`: a builtin's native struct,
    /// keyed by the builtin's name). A type head ([`Self::head_table`]) and a type
    /// ([`Self::nominal_overloads`]) both read it.
    fn overloads_at(&self, kind: TypeHeadKind, key: &str, native: bool) -> Option<&Overloads> {
        match kind {
            TypeHeadKind::Enum => self.enum_overloads_of(key),
            TypeHeadKind::Struct if native => self.container_seeds.get(key).map(|i| &i.overloads),
            TypeHeadKind::Struct => self.struct_shape(key).map(|i| &i.overloads),
            TypeHeadKind::Protocol => None,
        }
    }

    /// The overload set of `name` on the type `th` names and its stored slot, when `name` is
    /// overloaded there.
    pub(super) fn head_overloads(
        &self,
        th: &TypeHead,
        name: &str,
    ) -> Option<(Vec<FnSig>, OverloadSlot)> {
        let table = self.head_table(th)?;
        Some((table.get(name)?.clone(), table.slots.get(name)?.slot))
    }

    /// The stored slot of the method `name` of the type `th` names, a lone method included.
    pub(super) fn head_slot(&self, th: &TypeHead, name: &str) -> Option<OverloadSlot> {
        Some(self.head_table(th)?.slots.get(name)?.slot)
    }

    /// The slot of the requirement `name` of the protocol `key`, off its shape: a call through a
    /// bound type param (`T.from(x)`) has no receiver type that stores one.
    pub(super) fn requirement_slot(&self, key: &str, name: &str) -> Option<OverloadSlot> {
        let p = self.protocol_shape(key)?;
        let (_, req) = p.methods.iter().find(|(n, _)| n == name)?;
        overload_slot(req, &p.type_params)
    }

    /// The stored slot of the method `name` of the type `ty`, a lone method included.
    pub(super) fn slot_of(&self, ty: &Ty, name: &str) -> Option<&OverloadReq> {
        self.nominal_overloads(ty)?.slots.get(name).map(|r| &**r)
    }

    /// The protocol-slot methods of the struct or enum `ty`, or of a builtin's native struct.
    pub(super) fn nominal_overloads(&self, ty: &Ty) -> Option<&Overloads> {
        if let Ty::Struct(k, _) = ty {
            return self.overloads_at(TypeHeadKind::Struct, k, false);
        }
        if let Ty::Enum(k, _) = ty {
            return self.overloads_at(TypeHeadKind::Enum, k, false);
        }
        self.overloads_at(TypeHeadKind::Struct, BuiltinType::of(ty)?.name(), true)
    }

    /// THE witness pick, read by conformance and by the witness record alike: the overload of
    /// `name` on `ty` that witnesses a requirement with param types `want` (its `S` already
    /// substituted), keyed at the slot `ty` stores. It is the direct call's pick
    /// ([`Self::fitting_overloads`]): the one overload the source fits, so a source with holes (`List[?]` from `[]`, `None`) still picks, and the
    /// declaration order never matters. `None` when `name` is not overloaded on `ty`; `Some(Err)`,
    /// the call's message, when no overload or more than one fits.
    pub(super) fn witness_overload(
        &self,
        ty: &Ty,
        name: &str,
        want: &[Ty],
    ) -> Option<Result<&FnSig, String>> {
        let table = self.nominal_overloads(ty)?;
        let set = table.get(name)?;
        let i = match table.slots.get(name)?.slot {
            OverloadSlot::Param(i) => i,
            OverloadSlot::Ret => {
                return Some(Err(format!(
                    "its {name} is picked by the target type: write `.{name}[T]()`"
                )));
            }
        };
        let slot = OverloadSlot::Param(i);
        let s = want.get(i).cloned().unwrap_or(Ty::Unknown);
        let fitting = self.fitting_overloads(set, slot, &s, false);
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

    /// The direct call `Type.name(args)` on an overloaded `name`: the index of the one overload in
    /// `set` that takes the argument for the param at `slot` (the type's stored slot, `Param(i)`),
    /// bound as the call binds it ([`Self::bind_sig`] against the first overload: every overload
    /// has the requirement's params). The call passes every param of the requirement: a binding
    /// that leaves the slot's param out is *an overloaded mk is picked by argument 'x': it is
    /// missing*, a wrong count *an overloaded from takes 1 argument, found 0*. No overload fits (no
    /// widening, Rust `E0277`), or more than one fits (Rust `E0283`), is an error that lists the
    /// overloads; `None` then, and the arguments are inferred for their own errors.
    fn pick_overload(
        &mut self,
        set: &[FnSig],
        slot: OverloadSlot,
        tname: &str,
        name: &str,
        args: &[Expr],
        span: Span,
    ) -> Option<usize> {
        let i = match slot {
            OverloadSlot::Param(i) => i,
            OverloadSlot::Ret => 0,
        };
        let mut sig = set.first()?.clone();
        if sig.slots.is_none() && sig.native.is_none() {
            let n = sig.params.len();
            let slots = super::expr::callable_slots(&sig.labels, sig.min_params, n, sig.variadic);
            sig.slots = Some(slots);
        }
        let label = |j: usize| sig.slots.as_ref()?.get(j)?.name.clone();
        let labels: Vec<String> = (0..sig.params.len()).filter_map(label).collect();
        let at = label(i).unwrap_or_else(|| "x".to_string());
        let named: Vec<String> = self
            .call_ctx
            .as_ref()
            .map(|c| c.named.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default();
        let (arity, n) = (sig.params.len().max(i + 1), args.len() + named.len());
        let mark = self.diag_mark();
        let bound = self.bind_sig(&sig, name, args, 0, span);
        let arg = bound.filter(|_| n == arity).and_then(|b| b.get(i).cloned());
        let Some(arg) = arg else {
            // An unknown keyword keeps the binding's own error; any other miss is the pick's.
            if named.iter().all(|k| labels.contains(k)) {
                self.diag_rollback(mark);
                self.infer_all(args);
                self.consume_named();
                let msg = if n == arity {
                    format!("an overloaded {name} is picked by argument '{at}': it is missing")
                } else {
                    let s = if arity == 1 { "" } else { "s" };
                    format!("an overloaded {name} takes {arity} argument{s}, found {n}")
                };
                self.error(span, msg);
            }
            return None;
        };
        let msg = match self.pick_input(set, slot, tname, name, PickInput::Arg(&arg)) {
            Ok(k) => return Some(k),
            Err(msg) => msg,
        };
        let before = self.errors.len();
        self.infer_all(args);
        self.consume_named();
        if self.errors.len() > before && self.probe_ty(&arg).is_unknown() {
            // The argument's own error already explains it.
            return None;
        }
        self.error(span, msg);
        None
    }

    /// The [`PickInput::Arg`] reading: the overloads whose key at `slot` takes the argument `arg`
    /// checked under that key, as the call binds it (a bare literal takes its type from the slot:
    /// `[1]` is a `List[int?]` under `from(x: List[int?])`, as one decl checks it), and the
    /// argument's own type, for the misfit message. Each candidate's check is a trial: its
    /// diagnostics and bindings are rolled back. An argument with no type of its own (its own
    /// error) fits every overload, and the caller reports that error alone.
    fn arg_fitting(&mut self, set: &[FnSig], slot: OverloadSlot, arg: &Expr) -> (Vec<usize>, Ty) {
        let saved = std::mem::replace(&mut self.generic_arg_prepass, true);
        let t = self.probe_ty(arg);
        let fitting = if t.is_unknown() {
            self.fitting_overloads(set, slot, &t, false)
        } else {
            (0..set.len())
                .filter(|&k| {
                    let Some(key) = slot.key(&set[k]).cloned() else {
                        return false;
                    };
                    let mark = self.diag_mark();
                    let under = self.infer_arg(arg, Some(&key));
                    let clean = self.errors.len() == mark.errors;
                    let fits = clean && self.fits(&key, &under);
                    self.diag_rollback(mark);
                    fits
                })
                .collect()
        };
        self.generic_arg_prepass = saved;
        (fitting, t)
    }

    /// The overloaded `name` read as a value (`f: fn(int) -> Rat = Rat.from`), keyed at the type's
    /// stored `slot`: the one overload whose fn type is assignable to the position's fn type (the
    /// `expected_hint`), which takes the requirement's arity ([`PickInput::FnType`]). An open type
    /// var there (`.map`'s `fn(int) -> U`) names nothing and takes every overload. No fn type there,
    /// or one that does not pick exactly one, is an error that lists the overloads (Rust `E0283`:
    /// `let g = Rat::from;` needs an annotation).
    pub(super) fn pick_overload_value(
        &mut self,
        set: &[FnSig],
        slot: OverloadSlot,
        tname: &str,
        name: &str,
        span: Span,
    ) -> Option<usize> {
        let want = self.expected_hint.as_ref().map(|h| h.ty.clone());
        let picked = self.pick_input(set, slot, tname, name, PickInput::FnType(want));
        self.report_pick(picked, span)
    }

    /// Report a failed [`Self::pick_input`] at `span`: the index of the pick, else `None`.
    fn report_pick(&mut self, picked: Result<usize, String>, span: Span) -> Option<usize> {
        match picked {
            Ok(k) => Some(k),
            Err(msg) => {
                self.error(span, msg);
                None
            }
        }
    }

    /// THE pick of a protocol-slot overload: the index of the one overload in `set` that takes
    /// `input` at `slot`, else the misfit message ([`overload_misfit`]). Every reader of a pick (a
    /// direct call, `[X]`, an annotation target, a value's fn type, `T.from[X]` read as a value)
    /// comes here; a bound's witness reads the same relation ([`Self::witness_overload`]).
    pub(super) fn pick_input(
        &mut self,
        set: &[FnSig],
        slot: OverloadSlot,
        tname: &str,
        name: &str,
        input: PickInput,
    ) -> Result<usize, String> {
        let (fitting, shown) = match input {
            PickInput::Target(t, payload) => {
                (self.fitting_overloads(set, slot, &t, payload), Some(t))
            }
            PickInput::Arg(arg) => {
                let (fitting, t) = self.arg_fitting(set, slot, arg);
                (fitting, Some(t))
            }
            PickInput::FnType(want) => self.fn_type_fitting(set, slot, want),
        };
        match fitting.as_slice() {
            [k] => Ok(*k),
            _ => Err(overload_misfit(
                set,
                slot,
                &fitting,
                tname,
                name,
                shown.as_ref(),
            )),
        }
    }

    /// The [`PickInput::FnType`] reading: the overloads whose fn type the position's fn type
    /// `want` takes by assignability (a free type param or var of `want` is open: it takes any
    /// type), and the type at `want`'s slot for the misfit message (`None` when it is open, or
    /// there is no fn type of the requirement's arity: every overload then fits).
    fn fn_type_fitting(
        &self,
        set: &[FnSig],
        slot: OverloadSlot,
        want: Option<Ty>,
    ) -> (Vec<usize>, Option<Ty>) {
        // The value's fn type takes the requirement's params (a `Ret` slot's method only `self`).
        let arity = match slot {
            OverloadSlot::Param(_) => set.first().map_or(1, |s| s.params.len()),
            OverloadSlot::Ret => 1,
        };
        let Some(want) =
            want.filter(|w| matches!(w, Ty::Func { params, .. } if params.len() == arity))
        else {
            return ((0..set.len()).collect(), None);
        };
        // A free type param of the position (not one of the enclosing fn's) is open.
        let open = std::cell::RefCell::new(HashMap::new());
        ty_all_holes(&want, &|h| {
            if let Ty::Param(n) = h
                && !self.rigid_param(n, &[])
            {
                open.borrow_mut().insert(n.clone(), Ty::Unknown);
            }
            true
        });
        let want = subst(&want, &open.into_inner());
        let Ty::Func {
            params,
            ret,
            labels,
            variadic,
        } = &want
        else {
            return ((0..set.len()).collect(), None);
        };
        let at = match slot {
            OverloadSlot::Param(i) => params.get(i).cloned(),
            OverloadSlot::Ret => Some((**ret).clone()),
        }
        .filter(|t| self.names_target(t));
        let fitting = (0..set.len())
            .filter(|&k| {
                // A `Ret` slot's method takes only the receiver: the position's own param.
                let cand = match slot {
                    OverloadSlot::Param(_) => fn_value_ty(&set[k]),
                    OverloadSlot::Ret => Ty::Func {
                        params: params.clone(),
                        ret: Box::new(set[k].ret.clone()),
                        labels: labels.clone(),
                        variadic: *variadic,
                    },
                };
                let m = self.tyvars.borrow().mark();
                let ok = self.assignable(&want, &cand);
                self.tyvars.borrow_mut().rollback(m);
                ok
            })
            .collect();
        (fitting, at)
    }

    /// `Type.name[X]` read as a value, where `name` is a protocol slot of the type (Rust
    /// `<Rat as From<i32>>::from`): the `[X]` picks the overload ([`Self::pick_slot`], on a set or
    /// on one decl), and the value is that overload's fn. `None` when `head` is no such member, or
    /// the method declares type params of its own (the `[X]` is then theirs).
    pub(super) fn slot_applied_value(
        &mut self,
        head: &Expr,
        args: &[crate::ast::Type],
        args_span: Span,
    ) -> Option<Ty> {
        let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &head.kind
        else {
            return None;
        };
        let (th, head_args) = self.peel_type_path(obj)?;
        let lone_slot = self.head_slot(&th, name)?;
        let one = self.type_member_fn_at(&th, head_args.clone(), name, None)?;
        if one.sig.type_params.len() > one.head_params {
            return None;
        }
        let x: Vec<Ty> = args
            .iter()
            .map(|t| self.resolve_type(t, args_span))
            .collect();
        let tname = th.spelled.clone();
        let (set, slot, overloaded) = match self.head_overloads(&th, name) {
            Some((set, slot)) => (set, slot, true),
            None => (vec![one.sig], lone_slot, false),
        };
        let k = match self.pick_slot(&set, slot, &tname, name, &x, None, &[], *name_span) {
            Some(k) => k,
            None => return Some(Ty::Unknown),
        };
        let pick = overloaded.then(|| {
            self.record_overload_call(name, slot, &set[k], *name_span, *name_span);
            k
        });
        let pf = self.type_member_fn_at(&th, head_args, name, pick)?;
        Some(self.path_fn_value_ty(obj.id, pf, None, *name_span))
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
    /// of type `t` [`Self::fits`], or the target `t` itself (Rust's `Into<T>` is exact). Read
    /// under a payload hint (`payload`), a target's key is its [`OverloadSlot::payload_key`].
    fn fitting_overloads(
        &self,
        set: &[FnSig],
        slot: OverloadSlot,
        t: &Ty,
        payload: bool,
    ) -> Vec<usize> {
        (0..set.len())
            .filter(|&i| {
                let key = if payload {
                    slot.payload_key(&set[i])
                } else {
                    slot.key(&set[i])
                };
                key.is_some_and(|k| match slot {
                    OverloadSlot::Param(_) => self.fits(k, t),
                    OverloadSlot::Ret => k == t,
                })
            })
            .collect()
    }

    /// Record the overload a direct call picked (`sig`, keyed at the type's stored `slot`) under the
    /// call's [`WitnessKey`], for the compiler's `CallStatic` (the harvest pass only, as every
    /// [`WitnessTable`] half).
    pub(super) fn record_overload_call(
        &mut self,
        name: &str,
        slot: OverloadSlot,
        sig: &FnSig,
        key_span: Span,
        span: Span,
    ) {
        if !self.harvest_keywords || self.generic_arg_prepass || self.resolving_returns {
            return;
        }
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
    /// `x.into()` on a receiver of type `obj_ty`, when the receiver's type stores `method` as a
    /// slot keyed by its target (`Into[T]`'s `into`, a user `Emit[T]`'s `emit(self) -> T`): the
    /// overload [`Self::pick_slot`] picks (by `[T]`, else by the annotation `hint`), and the call
    /// is that overload's return. A lone decl is a plain method: a `[T]` on it is checked by the
    /// same pick against its return (the receiver's type args substituted), then `type_args` is
    /// cleared for the ordinary call. `None` when the ordinary method call runs.
    #[allow(clippy::too_many_arguments)] // receiver type + method + args + targs + hint + spans
    pub(super) fn infer_target_call(
        &mut self,
        obj_ty: &Ty,
        method: &str,
        args: &[Expr],
        type_args: &mut &[Ty],
        hint: Option<&Ty>,
        name_span: Span,
        span: Span,
    ) -> Option<Ty> {
        let slot = self.slot_of(obj_ty, method)?.slot;
        if slot != OverloadSlot::Ret {
            return None;
        }
        let set = self
            .nominal_overloads(obj_ty)
            .and_then(|o| o.get(method))
            .cloned();
        let tname = obj_ty.to_string();
        let Some(set) = set else {
            if type_args.is_empty() || self.member_own_type_params(obj_ty, method) == Some(true) {
                return None;
            }
            let one = self.conversion_sigs(obj_ty, method);
            if let [sig] = one.as_slice()
                && !sig.is_static
            {
                let targs = *type_args;
                if self
                    .pick_slot(&one, slot, &tname, method, targs, None, args, name_span)
                    .is_none()
                {
                    return Some(Ty::Unknown);
                }
                *type_args = &[];
            }
            return None;
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
        let k = self.pick_slot(&set, slot, &tname, method, type_args, hint, args, name_span);
        let Some(k) = k else {
            return Some(Ty::Unknown);
        };
        self.record_overload_call(method, slot, &set[k], name_span, span);
        Some(set[k].ret.clone())
    }

    /// THE pick of a protocol-slot call on `set` (one decl or several, static or instance), keyed
    /// at the type's stored `slot`: `[X]` (`targs`) names the type at the slot (`Rat.from[int](3)`:
    /// the source; `r.into[str]()`: the target) and wins; with no `[X]`, a `Param` slot picks by
    /// its argument ([`Self::pick_overload`]) and a `Ret` slot by the annotation `hint`
    /// ([`Self::hint_target`]). The index of the one overload that takes `X`; else an error that
    /// names no receiver (a `?.` receiver is an internal name), `None`, and the arguments inferred
    /// for their own errors.
    #[allow(clippy::too_many_arguments)] // set + slot + names + targs + hint + args + span
    pub(super) fn pick_slot(
        &mut self,
        set: &[FnSig],
        slot: OverloadSlot,
        tname: &str,
        name: &str,
        targs: &[Ty],
        hint: Option<&Ty>,
        args: &[Expr],
        span: Span,
    ) -> Option<usize> {
        let t = match (targs, slot) {
            ([t], _) => Ok((t.clone(), false)),
            ([], OverloadSlot::Param(_)) => {
                return self.pick_overload(set, slot, tname, name, args, span);
            }
            ([], OverloadSlot::Ret) => self.hint_target(set, hint).ok_or_else(|| {
                format!("{name} on {tname} needs a target type: write `.{name}[T]()`")
            }),
            ([..], _) => {
                let n = targs.len();
                let what = match slot {
                    OverloadSlot::Param(_) => "the source",
                    OverloadSlot::Ret => "the target",
                };
                Err(format!("{name} takes 1 type argument ({what}), found {n}"))
            }
        };
        let msg = match t {
            Ok((t, payload)) => {
                match self.pick_input(set, slot, tname, name, PickInput::Target(t, payload)) {
                    Ok(k) => return Some(k),
                    Err(msg) => msg,
                }
            }
            Err(msg) => msg,
        };
        self.infer_all(args);
        self.consume_named();
        self.error(span, msg);
        None
    }

    /// The target an annotation `hint` names for a call of the target-keyed `set`, and whether
    /// it is read as a payload ([`Self::fitting_overloads`]): a payload hint (the call is the
    /// operand of `?`, `??` or `else:`) when an overload's carrier payload is it (`k: int =
    /// r.into() ?? 7` picks `into(self) -> int?`); else the hint itself when an overload returns
    /// it, else a carrier hint's payload (`n: int! = k.try_into()` picks `try_into(self) -> int!`;
    /// a plain `int` wraps into `int!`), else the hint, for the misfit message. `None` with no
    /// hint or one that names no target ([`Self::names_target`]: `print`'s `Any`, an open `T`).
    fn hint_target(&self, set: &[FnSig], hint: Option<&Ty>) -> Option<(Ty, bool)> {
        let h = hint.filter(|h| self.names_target(h))?;
        let slot = OverloadSlot::Ret;
        if self.call_ctx.as_ref().is_some_and(|c| c.payload_hint)
            && !self.fitting_overloads(set, slot, h, true).is_empty()
        {
            return Some((h.clone(), true));
        }
        if self.fitting_overloads(set, slot, h, false).is_empty()
            && let Some((_, payload, _)) = h.carrier_parts()
            && !self.fitting_overloads(set, slot, payload, false).is_empty()
        {
            return Some((payload.clone(), false));
        }
        Some((h.clone(), false))
    }

    /// Does an expected type name a conversion target? [`hint_fixes`] (not `Unknown`, not a
    /// protocol such as `print`'s `Any`), and no open type var: a callee's free `U` or an inference
    /// var takes any target, so it names none (Rust `E0283`). The enclosing fn's own `T` is a type.
    fn names_target(&self, t: &Ty) -> bool {
        hint_fixes(t)
            && !matches!(t, Ty::Var(_))
            && !matches!(t, Ty::Param(n) if !self.rigid_param(n, &[]))
    }

    /// The one method `name` of `ty` (a struct, an enum or a builtin scalar's native struct), as a
    /// set of one, with the receiver's type args substituted (`Box[int]`'s `into(self) -> T`
    /// returns `int`).
    fn conversion_sigs(&self, ty: &Ty, name: &str) -> Vec<FnSig> {
        let (table, map) = if let Ty::Enum(k, targs) = ty {
            (self.enum_methods_of(k), self.enum_param_map(k, targs))
        } else if let Ty::Struct(k, targs) = ty {
            let info = self.struct_shape(k);
            let map = info.map(|i| struct_param_map(i, targs)).unwrap_or_default();
            (info.map(|i| &i.methods), map)
        } else {
            let info = Self::native_witness_key(ty).and_then(|k| self.struct_shape(k));
            (info.map(|i| &i.methods), HashMap::new())
        };
        table
            .and_then(|t| t.get(name))
            .map(|sig| instantiate_method(sig, &map))
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
    // The prelude types a shape names (`T!` is `$Result`), as a checked prelude leaves them.
    let mut c = super::entry::prelude_checker();
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

/// The requirements with an [`OverloadSlot`] whose name `wanted` takes, of the parameterized
/// protocols `found` (key, shape), sorted and deduplicated by key.
fn slot_reqs(
    mut found: Vec<(String, &ProtocolInfo)>,
    wanted: &dyn Fn(&str) -> bool,
) -> Vec<OverloadReq> {
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let mut out = Vec::new();
    for (key, pinfo) in found {
        if pinfo.type_params.is_empty() {
            continue;
        }
        for (n, req) in pinfo.methods.iter() {
            if !wanted(n) {
                continue;
            }
            if let Some(slot) = overload_slot(req, &pinfo.type_params) {
                out.push(OverloadReq {
                    name: n.clone(),
                    protocol: key.clone(),
                    slot,
                });
            }
        }
    }
    out
}

/// What a protocol-slot pick ([`Checker::pick_input`]) reads at the slot.
pub(super) enum PickInput<'a> {
    /// A type at the slot, `[X]` or an annotation's target; `true` reads a target's carrier
    /// payload ([`OverloadSlot::payload_key`]).
    Target(Ty, bool),
    /// The argument for the slot's param, checked under each overload's key.
    Arg(&'a Expr),
    /// The position's fn type, for a read as a value (`None`: no expected type there).
    FnType(Option<Ty>),
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

/// One type body's [`Overloads`]: its `slots` ([`Checker::report_dup_methods`]), and each slotted
/// name declared 2+ times with its sigs in declaration order.
pub(super) fn overload_sets(
    slots: HashMap<String, Arc<OverloadReq>>,
    sigs: &[(String, FnSig)],
) -> Overloads {
    let mut sets: HashMap<String, Vec<FnSig>> = HashMap::new();
    for (name, sig) in sigs {
        if slots.contains_key(name) {
            sets.entry(name.clone()).or_default().push(sig.clone());
        }
    }
    sets.retain(|_, set| set.len() > 1);
    Overloads { slots, sets }
}

/// The signature the body of `methods[i]` is checked against: its own overload's sig when its name
/// is overloaded, else the method table's. `None` for a repeat the gate did not admit: its
/// "already defined" error stands alone, not a mismatch against the surviving sig.
pub(super) fn method_decl_sig(
    methods: &[FnDecl],
    i: usize,
    table: &HashMap<String, FnSig>,
    overloads: Option<&Overloads>,
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
    // The common source `x: S` answers without the round-trip's copy.
    if let Ty::Param(n) = ty {
        return names.contains(n);
    }
    let probe: HashMap<String, Ty> = names.iter().map(|n| (n.clone(), Ty::Unknown)).collect();
    subst(ty, &probe) != *ty
}
