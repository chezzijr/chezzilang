// checker::tyvar — TICKET-225 (R5): type variables. A generic fn value read with no pin takes one
// `Ty::Var` per type param; a later use in its frame binds them. `solve` is the one place a var is
// bound, and only `assignable` and `join_ty` call it, so every type compare in the checker sees the
// store. The store is speculative state: `DiagMark` carries a `TyVarMark` (DEC-157).

use super::*;

/// The var store. `slots[id]` is the binding of var `id`, `None` while unbound.
#[derive(Default)]
pub(super) struct TyVars {
    slots: Vec<Option<Ty>>,
    /// Every var bound, in order, so a rollback can unbind the ones bound after its mark.
    log: Vec<u32>,
    /// Every deferred generic fn value read, judged when its frame closes.
    pending: Vec<Pending>,
    /// Bounds a call could not check yet because a type argument still held a var.
    bounds: Vec<DeferredBound>,
    /// Read node -> its `pending` index: one read keeps one set of vars across re-walks.
    reads: HashMap<u32, usize>,
    /// TICKET-227: every `?x` / `!e` built with no expected carrier, judged when its frame closes.
    carriers: Vec<PendingCarrier>,
    /// `?x` / `!e` node -> its `carriers` index: a re-walk of the node reuses its var.
    carrier_reads: HashMap<u32, usize>,
}

/// TICKET-227: a `?x` / `!e` value whose carrier no expected type gave; a later use in its frame
/// pins `var`.
pub(super) struct PendingCarrier {
    node: crate::ast::NodeId,
    span: Span,
    var: u32,
    kind: CarrierKind,
}

pub(super) enum CarrierKind {
    /// `?x` with operand type `t`: `var` is the whole value's type.
    Present(Ty),
    /// `!e`: `var` is the success type of the error value's `Result`.
    Error,
}

/// A generic fn value read that nothing pinned at the read.
pub(super) struct Pending {
    node: crate::ast::NodeId,
    name: String,
    sig: FnSig,
    spelling: String,
    span: Span,
    /// One var per type param of `sig`.
    vars: Vec<(String, u32)>,
    /// A std.json decode read: its record is written at the verdict (DEC-214).
    decode: bool,
}

/// `enforce_bounds` on a call whose bindings held a var; re-run once the vars are bound.
pub(super) struct DeferredBound {
    pub(super) params: Vec<TyParam>,
    pub(super) owner: Vec<TyParam>,
    pub(super) map: HashMap<String, Ty>,
    pub(super) span: Span,
}

/// A point in the store a speculative walk can roll back to.
#[derive(Clone, Copy, Debug)]
pub(super) struct TyVarMark {
    slots: usize,
    log: usize,
    pending: usize,
    bounds: usize,
    carriers: usize,
}

impl TyVars {
    pub(super) fn fresh(&mut self) -> u32 {
        self.slots.push(None);
        (self.slots.len() - 1) as u32
    }

    /// Whether any var was ever created: the cheap gate in front of every walk below.
    pub(super) fn any(&self) -> bool {
        !self.slots.is_empty()
    }

    pub(super) fn bind(&mut self, id: u32, t: Ty) {
        self.slots[id as usize] = Some(t);
        self.log.push(id);
    }

    pub(super) fn binding(&self, id: u32) -> Option<&Ty> {
        self.slots[id as usize].as_ref()
    }

    pub(super) fn mark(&self) -> TyVarMark {
        TyVarMark {
            slots: self.slots.len(),
            log: self.log.len(),
            pending: self.pending.len(),
            bounds: self.bounds.len(),
            carriers: self.carriers.len(),
        }
    }

    /// Unbind every var bound after `m`, and retire every var created after it: it is bound to
    /// `Unknown`, so no type left holding it dangles. Reads and bounds recorded after `m` go too.
    pub(super) fn rollback(&mut self, m: TyVarMark) {
        self.pending.truncate(m.pending);
        self.bounds.truncate(m.bounds);
        self.reads.retain(|_, i| *i < m.pending);
        self.carriers.truncate(m.carriers);
        self.carrier_reads.retain(|_, i| *i < m.carriers);
        for id in self.log.drain(m.log..) {
            if (id as usize) < m.slots {
                self.slots[id as usize] = None;
            }
        }
        for s in &mut self.slots[m.slots..] {
            *s = Some(Ty::Unknown);
        }
    }

    /// `t` with every bound var replaced by its binding, recursively.
    pub(super) fn zonk(&self, t: &Ty) -> Ty {
        if !self.any() || !has_var(t) {
            return t.clone();
        }
        map_ty(t, &mut |x| match x {
            Ty::Var(id) => Some(match self.binding(*id) {
                Some(b) => self.zonk(b),
                None => x.clone(),
            }),
            _ => None,
        })
    }

    /// `t` zonked; a type still holding an unbound var becomes `Unknown` as a whole. For a type that
    /// outlives its walk (an inferred return, a seeded global type).
    pub(super) fn settle(&self, t: &Ty) -> Ty {
        let z = self.zonk(t);
        if has_var(&z) { Ty::Unknown } else { z }
    }
}

/// TICKET-227 — an error value `!e` whose success type nothing pins.
pub(super) const CANNOT_INFER_SUCCESS: &str =
    "cannot infer the success type; annotate the binding, e.g. w: int! = !e";

/// Whether `t` mentions a `Ty::Var` anywhere.
pub(super) fn has_var(t: &Ty) -> bool {
    let mut found = false;
    let _ = map_ty(t, &mut |x| {
        found |= matches!(x, Ty::Var(_));
        None
    });
    found
}

/// Rebuild `t`, replacing each node `f` answers `Some` for (its children are not visited).
pub(super) fn map_ty(t: &Ty, f: &mut dyn FnMut(&Ty) -> Option<Ty>) -> Ty {
    if let Some(r) = f(t) {
        return r;
    }
    let all = |ts: &[Ty], f: &mut dyn FnMut(&Ty) -> Option<Ty>| -> Vec<Ty> {
        ts.iter().map(|x| map_ty(x, f)).collect()
    };
    match t {
        Ty::List(x) => Ty::List(Box::new(map_ty(x, f))),
        Ty::Set(x) => Ty::Set(Box::new(map_ty(x, f))),
        Ty::Option(x) => Ty::Option(Box::new(map_ty(x, f))),
        Ty::Channel(x) => Ty::Channel(Box::new(map_ty(x, f))),
        Ty::Shared(x) => Ty::Shared(Box::new(map_ty(x, f))),
        Ty::Atomic(x) => Ty::Atomic(Box::new(map_ty(x, f))),
        Ty::RwShared(x) => Ty::RwShared(Box::new(map_ty(x, f))),
        Ty::Map(k, v) => Ty::Map(Box::new(map_ty(k, f)), Box::new(map_ty(v, f))),
        Ty::Result(k, v) => Ty::Result(Box::new(map_ty(k, f)), Box::new(map_ty(v, f))),
        Ty::Tuple(ts) => Ty::Tuple(all(ts, f)),
        Ty::Struct(n, a) => Ty::Struct(n.clone(), all(a, f)),
        Ty::Enum(n, a) => Ty::Enum(n.clone(), all(a, f)),
        Ty::Protocol(n, a) => Ty::Protocol(n.clone(), all(a, f)),
        Ty::Func {
            params,
            ret,
            labels,
        } => Ty::Func {
            params: all(params, f),
            ret: Box::new(map_ty(ret, f)),
            labels: labels.clone(),
        },
        Ty::BuiltinFn { params, ret } => Ty::BuiltinFn {
            params: all(params, f),
            ret: Box::new(map_ty(ret, f)),
        },
        other => other.clone(),
    }
}

impl Checker {
    /// Bind the unbound vars of `a` and `b` so the two agree, walking both in parallel. A var meets
    /// a type that holds it (occurs check) or a callee's leaked `Ty::Param` (not a param in scope
    /// here): nothing binds. A TOP-LEVEL `Unknown` binds nothing (it determines nothing); a NESTED
    /// one binds the var to `Unknown` (the empty-collection sentinel `[].map(ident)`). A mismatch
    /// elsewhere binds what it can; the caller compares the zonked pair and rolls back.
    pub(super) fn solve(&self, a: &Ty, b: &Ty) {
        if !self.tyvars.borrow().any() || !(has_var(a) || has_var(b)) {
            return;
        }
        self.solve_at(a, b, true);
    }

    fn solve_at(&self, a: &Ty, b: &Ty, top: bool) {
        let (a, b) = {
            let s = self.tyvars.borrow();
            (s.zonk(a), s.zonk(b))
        };
        match (&a, &b) {
            (Ty::Var(x), Ty::Var(y)) => {
                if x != y {
                    let (hi, lo) = if x > y { (*x, *y) } else { (*y, *x) };
                    self.tyvars.borrow_mut().bind(hi, Ty::Var(lo));
                }
            }
            (Ty::Var(x), t) | (t, Ty::Var(x)) => {
                if t.is_unknown() && top {
                    return;
                }
                let leaked = !ty_all_holes(t, &|h| match h {
                    Ty::Param(n) => self.rigid_param(n, &[]),
                    _ => true,
                });
                if leaked || mentions_var(t, *x) {
                    return;
                }
                self.tyvars.borrow_mut().bind(*x, t.clone());
            }
            (Ty::Width(_), _) => self.solve_at(a.scalar(), &b, top),
            (_, Ty::Width(_)) => self.solve_at(&a, b.scalar(), top),
            (Ty::List(x), Ty::List(y))
            | (Ty::Set(x), Ty::Set(y))
            | (Ty::Option(x), Ty::Option(y))
            | (Ty::Channel(x), Ty::Channel(y))
            | (Ty::Shared(x), Ty::Shared(y))
            | (Ty::Atomic(x), Ty::Atomic(y))
            | (Ty::RwShared(x), Ty::RwShared(y)) => self.solve_at(x, y, false),
            (Ty::Map(k1, v1), Ty::Map(k2, v2)) | (Ty::Result(k1, v1), Ty::Result(k2, v2)) => {
                self.solve_at(k1, k2, false);
                self.solve_at(v1, v2, false);
            }
            (Ty::Struct(n1, a1), Ty::Struct(n2, a2))
            | (Ty::Enum(n1, a1), Ty::Enum(n2, a2))
            | (Ty::Protocol(n1, a1), Ty::Protocol(n2, a2))
                if n1 == n2 && a1.len() == a2.len() =>
            {
                a1.iter()
                    .zip(a2)
                    .for_each(|(x, y)| self.solve_at(x, y, false));
            }
            (Ty::Tuple(t1), Ty::Tuple(t2)) if t1.len() == t2.len() => {
                t1.iter()
                    .zip(t2)
                    .for_each(|(x, y)| self.solve_at(x, y, false));
            }
            _ => {
                if let (Some((p1, r1)), Some((p2, r2))) = (a.fn_parts(), b.fn_parts())
                    && p1.len() == p2.len()
                {
                    p1.iter()
                        .zip(p2)
                        .for_each(|(x, y)| self.solve_at(x, y, false));
                    self.solve_at(r1, r2, false);
                }
            }
        }
    }

    /// A generic fn value read that nothing pins at the read: one fresh var per type param, judged
    /// when the frame closes ([`Self::close_tyvar_frame`]). A re-walk of the same read node gets the
    /// same vars back.
    pub(super) fn defer_generic_fn_value(
        &mut self,
        node: crate::ast::NodeId,
        name: &str,
        sig: &FnSig,
        spelling: &str,
        span: Span,
        decode: bool,
    ) -> Ty {
        let mut s = self.tyvars.borrow_mut();
        let cached = (node.0 != 0)
            .then(|| s.reads.get(&node.0).copied())
            .flatten();
        let vars = match cached {
            Some(i) => s.pending[i].vars.clone(),
            None => {
                let vars: Vec<(String, u32)> = sig
                    .type_params
                    .iter()
                    .map(|tp| (tp.name.clone(), s.fresh()))
                    .collect();
                if node.0 != 0 {
                    let i = s.pending.len();
                    s.reads.insert(node.0, i);
                }
                s.pending.push(Pending {
                    node,
                    name: name.to_string(),
                    sig: sig.clone(),
                    spelling: spelling.to_string(),
                    span,
                    vars: vars.clone(),
                    decode,
                });
                vars
            }
        };
        let map: HashMap<String, Ty> = vars.into_iter().map(|(n, v)| (n, Ty::Var(v))).collect();
        subst(&fn_value_ty(sig), &map)
    }

    /// A generic fn argument in a slot whose params carry the empty-collection sentinel
    /// (`[].map(mk)`'s `fn(?) -> U`): bind its unbound vars to `Unknown`, which the verdict accepts.
    pub(super) fn sentinel_slot_arg(&self, decl: &Ty, arg_ty: &Ty) {
        if !fn_slot_params_have_unknown(decl) {
            return;
        }
        let z = self.zonk(arg_ty);
        let mut s = self.tyvars.borrow_mut();
        let _ = map_ty(&z, &mut |x| {
            if let Ty::Var(v) = x
                && s.binding(*v).is_none()
            {
                s.bind(*v, Ty::Unknown);
            }
            None
        });
    }

    /// The deferred read at `node` is a std.json decode: its verdict writes its record.
    pub(super) fn mark_decode_read(&self, node: crate::ast::NodeId) {
        let mut s = self.tyvars.borrow_mut();
        if let Some(&i) = s.reads.get(&node.0) {
            s.pending[i].decode = true;
        }
    }

    /// Mark the store, for a frame or a speculative walk.
    pub(super) fn tyvar_mark(&self) -> TyVarMark {
        self.tyvars.borrow().mark()
    }

    /// `t` zonked, or `Unknown` if it still holds an unbound var: for a type that outlives its walk.
    pub(super) fn settle(&self, t: &Ty) -> Ty {
        self.tyvars.borrow().settle(t)
    }

    /// `enforce_bounds` for a call whose bindings still hold a var: deferred to the frame verdict.
    pub(super) fn defer_bound(&self, b: DeferredBound) {
        self.tyvars.borrow_mut().bounds.push(b);
    }

    /// TICKET-227 — a `?x` / `!e` with no expected carrier in a fn body: a fresh var (the same one
    /// on a re-walk of `node`), judged at the frame close.
    pub(super) fn defer_carrier(&mut self, node: &Expr, kind: CarrierKind) -> u32 {
        let mut s = self.tyvars.borrow_mut();
        if let Some(&i) = s.carrier_reads.get(&node.id.0) {
            s.carriers[i].kind = kind;
            return s.carriers[i].var;
        }
        let var = s.fresh();
        s.carriers.push(PendingCarrier {
            node: node.id,
            span: node.span,
            var,
            kind,
        });
        let i = s.carriers.len() - 1;
        if node.id.0 != crate::ast::NodeId::SYNTH.0 {
            s.carrier_reads.insert(node.id.0, i);
        }
        var
    }

    /// The frame verdict, over every read and bound recorded since `start`. A read whose vars are
    /// all bound meets its bounds (and a decode read writes its record, DEC-214). A read with a var
    /// still unbound is rejected with today's instantiate hint, and the var is bound to `Unknown`.
    /// A var bound to `Unknown` (the empty-collection sentinel) accepts silently, except for a
    /// decode read, whose value is compiled per `T`.
    pub(super) fn close_tyvar_frame(&mut self, start: TyVarMark) {
        let (pending, bounds, carriers) = {
            let mut s = self.tyvars.borrow_mut();
            if s.pending.len() <= start.pending
                && s.bounds.len() <= start.bounds
                && s.carriers.len() <= start.carriers
            {
                return;
            }
            let p: Vec<Pending> = {
                let n = start.pending.min(s.pending.len());
                s.pending.drain(n..)
            }
            .collect();
            let b: Vec<DeferredBound> = {
                let n = start.bounds.min(s.bounds.len());
                s.bounds.drain(n..)
            }
            .collect();
            s.reads.retain(|_, i| *i < start.pending);
            let c: Vec<PendingCarrier> = {
                let n = start.carriers.min(s.carriers.len());
                s.carriers.drain(n..)
            }
            .collect();
            s.carrier_reads.retain(|_, i| *i < start.carriers);
            (p, b, c)
        };
        for p in pending {
            let map: HashMap<String, Ty> = p
                .vars
                .iter()
                .map(|(n, v)| (n.clone(), self.zonk(&Ty::Var(*v))))
                .collect();
            let unbound = map.values().any(has_var);
            let unknown = map.values().any(|t| !ty_all_holes(t, &|h| !h.is_unknown()));
            if unbound || (p.decode && unknown) {
                self.reject_undetermined_generic_fn_value(&p.name, &p.sig, &p.spelling, p.span);
                let mut s = self.tyvars.borrow_mut();
                for (_, v) in &p.vars {
                    if s.binding(*v).is_none() {
                        s.bind(*v, Ty::Unknown);
                    }
                }
                continue;
            }
            if unknown {
                continue;
            }
            self.enforce_bounds(&p.sig.type_params, &p.sig.type_params, &map, p.span);
            self.enforce_bounds(&p.sig.where_bounds, &p.sig.type_params, &map, p.span);
            if p.decode {
                let refined = subst(&fn_value_ty(&p.sig), &map);
                self.record_decode_value(p.node, refined, p.span);
            }
        }
        for b in bounds {
            let map: HashMap<String, Ty> = b
                .map
                .iter()
                .map(|(n, t)| (n.clone(), self.zonk(t)))
                .collect();
            if !map.values().any(has_var) {
                self.enforce_bounds(&b.params, &b.owner, &map, b.span);
            }
        }
        for c in carriers {
            self.judge_carrier(c);
        }
    }

    /// TICKET-227 — the frame verdict on one `?x` / `!e`. An unpinned `?x` is optional; a `?x`
    /// pinned to `T?` / `T!E` wraps as `Some` / `Ok`. An unpinned `!e` is an error.
    fn judge_carrier(&mut self, c: PendingCarrier) {
        let z = self.zonk(&Ty::Var(c.var));
        match c.kind {
            CarrierKind::Present(t) => match &z {
                Ty::Var(v) => {
                    self.tyvars.borrow_mut().bind(*v, Ty::Option(Box::new(t)));
                    self.record_wrap(c.node, crate::checker::Wrap::Some, c.span);
                }
                Ty::Option(p) if self.assignable(p, &t) => {
                    self.record_wrap(c.node, crate::checker::Wrap::Some, c.span);
                }
                Ty::Result(p, _) if self.assignable(p, &t) => {
                    self.record_wrap(c.node, crate::checker::Wrap::Ok, c.span);
                }
                z if z.is_unknown() => {}
                z => self.error(
                    c.span,
                    format!("'?' builds an optional or success value, found {z}"),
                ),
            },
            CarrierKind::Error => {
                if let Ty::Var(v) = z {
                    self.error(c.span, CANNOT_INFER_SUCCESS.to_string());
                    self.tyvars.borrow_mut().bind(v, Ty::Unknown);
                }
            }
        }
    }

    /// `t` with its bound vars substituted.
    pub(super) fn zonk(&self, t: &Ty) -> Ty {
        self.tyvars.borrow().zonk(t)
    }

    /// The checker's one type compare outside `ty.rs` and `assignable`: `compatible` after `solve`.
    /// A `false` answer rolls back the bindings this call made. A new compare site calls this, never
    /// the free `compatible`, which declines on an unbound var.
    pub(super) fn join_ty(&self, a: &Ty, b: &Ty) -> bool {
        if !self.tyvars.borrow().any() || !(has_var(a) || has_var(b)) {
            return compatible(a, b);
        }
        let m = self.tyvars.borrow().mark();
        self.solve(a, b);
        let ok = compatible(&self.zonk(a), &self.zonk(b));
        if !ok {
            self.tyvars.borrow_mut().rollback(m);
        }
        ok
    }
}

/// Whether `t` mentions var `id` (the occurs check).
fn mentions_var(t: &Ty, id: u32) -> bool {
    let mut found = false;
    let _ = map_ty(t, &mut |x| {
        found |= matches!(x, Ty::Var(v) if *v == id);
        None
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn func(p: Ty, r: Ty) -> Ty {
        Ty::Func {
            params: vec![p],
            ret: Box::new(r),
            labels: FnLabels::default(),
        }
    }

    #[test]
    fn solve_binds_through_func_params() {
        let c = Checker::new();
        let v = c.tyvars.borrow_mut().fresh();
        assert!(c.join_ty(&func(Ty::Var(v), Ty::Var(v)), &func(Ty::Int, Ty::Int)));
        assert_eq!(c.zonk(&Ty::Var(v)), Ty::Int);
        // A mismatch rolls its own bindings back.
        let w = c.tyvars.borrow_mut().fresh();
        assert!(!c.join_ty(&func(Ty::Var(w), Ty::Str), &func(Ty::Int, Ty::Int)));
        assert_eq!(c.zonk(&Ty::Var(w)), Ty::Var(w));
    }

    #[test]
    fn rollback_retires_new_vars_and_unbinds_old() {
        let mut s = TyVars::default();
        let old = s.fresh();
        let m = s.mark();
        s.bind(old, Ty::Int);
        let new = s.fresh();
        s.rollback(m);
        assert_eq!(s.zonk(&Ty::Var(old)), Ty::Var(old));
        assert_eq!(s.zonk(&Ty::Var(new)), Ty::Unknown);
    }

    #[test]
    fn settle_collapses_an_unbound_var_to_unknown() {
        let mut s = TyVars::default();
        let a = s.fresh();
        let b = s.fresh();
        s.bind(a, Ty::Int);
        assert_eq!(s.settle(&Ty::list(Ty::Var(a))), Ty::list(Ty::Int));
        assert_eq!(s.settle(&Ty::list(Ty::Var(b))), Ty::Unknown);
    }
}
