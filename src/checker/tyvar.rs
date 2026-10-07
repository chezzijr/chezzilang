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
}

/// A point in the store a speculative walk can roll back to.
#[derive(Clone, Copy, Debug)]
pub(super) struct TyVarMark {
    slots: usize,
    log: usize,
}

impl TyVars {
    #[allow(dead_code)] // TICKET-225 step 5 creates vars
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
        }
    }

    /// Unbind every var bound after `m`, and retire every var created after it: it is bound to
    /// `Unknown`, so no type left holding it dangles.
    pub(super) fn rollback(&mut self, m: TyVarMark) {
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
    #[allow(dead_code)] // TICKET-225 step 5 settles escaping types
    pub(super) fn settle(&self, t: &Ty) -> Ty {
        let z = self.zonk(t);
        if has_var(&z) { Ty::Unknown } else { z }
    }
}

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
