//! Checker: block checking and fresh temporaries: `check_block`, `temporary_write`, `fresh_shape`
//! and its `Fresh` shape.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::*;

/// The deepest shape level `Checker::fresh_shape` describes; below it a subtree is [`Fresh::Marked`].
const MAX_DEPTH: usize = 16;
/// The most positional children one [`Kids::At`] holds; a wider node falls back to [`Kids::Each`].
const MAX_WIDTH: usize = 64;

/// TICKET-254 — which objects of the value graph an expression builds are new: no name holds them
/// (TICKET-240 drew the shape for the deleted airlock; the temporary-write warning reads it).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Fresh {
    /// A name may still hold it: do not descend.
    Marked,
    /// Built by the expression; [`Kids`] says which of its children are.
    Node(Kids),
    /// Every object reachable from it was built by the expression.
    All,
}

/// The children of a [`Fresh::Node`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kids {
    /// Every child stays marked (`copy()` is shallow, DEC-160).
    Marked,
    /// Every child takes this one shape. For a map these are its VALUES; a key stays marked.
    Each(Box<Fresh>),
    /// Child `i` takes shape `i`.
    At(Box<[Fresh]>),
}

impl Fresh {
    pub(super) fn is_all(&self) -> bool {
        *self == Fresh::All
    }

    pub(super) fn is_marked(&self) -> bool {
        *self == Fresh::Marked
    }

    /// A fresh object whose children all stay marked.
    pub(super) fn root() -> Fresh {
        Fresh::Node(Kids::Marked)
    }

    /// A fresh object whose children each take shape `k`.
    pub(super) fn each(k: Fresh) -> Fresh {
        match k {
            Fresh::All => Fresh::All,
            Fresh::Marked => Fresh::root(),
            k => Fresh::Node(Kids::Each(Box::new(k))),
        }
    }

    /// A fresh object with positional children. No child, or every child `All`, is `All`. Past
    /// [`MAX_WIDTH`] children it is `each` of their [`meet`](Fresh::meet).
    pub(super) fn at(kids: Vec<Fresh>) -> Fresh {
        if kids.iter().all(Fresh::is_all) {
            Fresh::All
        } else if kids.len() > MAX_WIDTH {
            Fresh::each(Fresh::meet(&kids))
        } else {
            Fresh::Node(Kids::At(kids.into_boxed_slice()))
        }
    }

    /// A fresh map whose keys are not all fresh: its values each take shape `k`. It NEVER gives
    /// `All`, because a name may still hold a key.
    pub(super) fn values(k: Fresh) -> Fresh {
        match k {
            Fresh::Marked => Fresh::root(),
            k @ Fresh::Node(_) | k @ Fresh::All => Fresh::Node(Kids::Each(Box::new(k))),
        }
    }

    /// One shape that is sound for every one of `kids`: the shared shape when all are equal,
    /// root-only when none is `Marked`, else `Marked`.
    pub(super) fn meet(kids: &[Fresh]) -> Fresh {
        match kids {
            [] => Fresh::All,
            [first, rest @ ..] if rest.iter().all(|k| k == first) => first.clone(),
            _ if kids.iter().any(Fresh::is_marked) => Fresh::Marked,
            _ => Fresh::root(),
        }
    }
}

impl Checker {
    pub(super) fn check_block(&mut self, block: &Block) {
        self.push_scope();
        for stmt in block {
            self.check_stmt(stmt);
        }
        self.pop_scope();
    }

    /// The type of `e`, inferred under a diagnostic window that is rolled back: a probe, no report.
    pub(super) fn probe_ty(&mut self, e: &Expr) -> Ty {
        let mark = self.diag_mark();
        let ty = self.infer(e);
        self.diag_rollback(mark);
        ty
    }

    /// TICKET-247 — both operands of a node `aliases_operand` calls `Fresh` have a type with no
    /// user operator, so the result is a new value that holds no operand.
    fn operands_keep_root(&mut self, left: &Expr, right: &Expr) -> bool {
        let (l, r) = (self.probe_ty(left), self.probe_ty(right));
        self.op_keeps_root(&l) && self.op_keeps_root(&r)
    }

    /// TICKET-254 — `obj.name(..)` is a native method of a `List`/`Map`/`Set`/`bytearray` whose
    /// declared return type is a container of those four kinds: the result is a new object (no
    /// native method returns its receiver or a view of it;
    /// `native_container_results_are_new_objects` runs every one). Its elements are the receiver's,
    /// so the shape is `Fresh::root()` (DEC-160).
    fn native_method_returns_container(&mut self, obj: &Expr, name: &str) -> bool {
        let recv = self.probe_ty(obj);
        fn_writes::native_receiver(recv.scalar()).is_some_and(|key| {
            self.structs
                .get(key)
                .and_then(|info| info.methods.get(name))
                .is_some_and(|sig| fn_writes::native_container(&sig.ret))
        })
    }

    /// TICKET-254 — the store `obj.f = v` or `obj[i] = v` is made by the engine itself, not by user
    /// code: a field store always is; a subscript store is when `obj` is a native container. A user
    /// `set_index` hook may do I/O, so a store through one is no write the warning can call lost
    /// (the ceiling of DEC-089 item 2, the same one the call site keeps with `mutates_receiver`).
    pub(super) fn store_is_native(&mut self, obj: &Expr, is_index: bool) -> bool {
        !is_index || fn_writes::native_receiver(self.probe_ty(obj).scalar()).is_some()
    }

    /// TICKET-254 — the one decider of "a write to `written` changes a value no name holds": the
    /// warning on a mutating call and on a store read it.
    /// `written` is the object the mutating call or the store lands on. Peel its place links
    /// ([`fn_writes::place_base`]): with none, the object itself is the base and any fresh shape
    /// loses the write; with one or more, the write lands on a part of the base, so only a base
    /// that is fresh at every level (`Fresh::All`) loses it (`xs.copy()[0].push(1)` writes an
    /// element `xs` shares).
    pub(super) fn temporary_write(&mut self, written: &Expr) -> bool {
        let (base, links) = fn_writes::place_base(written, &|span| self.is_fresh_subscript(span));
        let shape = self.fresh_shape(base, 0);
        if links == 0 {
            !shape.is_marked()
        } else {
            shape.is_all()
        }
    }

    /// TICKET-240 / TICKET-254: which objects of the value graph `e` builds are new. The one
    /// decider of "can a name still reach this object", recursive over the expression. A
    /// constructor-like node is fresh and each child is judged by the same rule: a list, tuple,
    /// set or map literal, a comprehension, a struct or variant constructor, a carrier wrap
    /// (explicit, or the implicit one `WrapTable` records), and a zero-argument `.copy()` on a
    /// List/Map/Set/bytearray (its own level only, DEC-160). A name, a field read, an index and
    /// any other call result stay `Marked` (`id(xs)` returns the caller's own list). A map with a
    /// key that is not fresh is never `All`: a struct with a mutable field is a legal key. Below
    /// `MAX_DEPTH` levels a subtree is `Marked`.
    pub(super) fn fresh_shape(&mut self, e: &Expr, depth: usize) -> Fresh {
        if depth >= MAX_DEPTH {
            return Fresh::Marked;
        }
        let d = depth + 1;
        let shape = match &e.kind {
            ExprKind::Index { index: Some(i), .. } if self.is_fresh_subscript(i.span) => {
                Fresh::root()
            }
            ExprKind::Binary {
                lhs: left,
                rhs: right,
                ..
            } if fn_writes::aliases_operand(&e.kind) == fn_writes::OperandAlias::Fresh
                && self.operands_keep_root(left, right) =>
            {
                Fresh::root()
            }
            ExprKind::Int(..)
            | ExprKind::Float(..)
            | ExprKind::Str(..)
            | ExprKind::Bytes(..)
            | ExprKind::RawStr(..)
            | ExprKind::Bool(..)
            | ExprKind::NoneLit => Fresh::All,
            ExprKind::List(items, _) | ExprKind::Tuple(items) => {
                Fresh::at(items.iter().map(|x| self.fresh_shape(x, d)).collect())
            }
            ExprKind::Set(items) => {
                if items.iter().all(|x| self.fresh_shape(x, d).is_all()) {
                    Fresh::All
                } else {
                    Fresh::root()
                }
            }
            ExprKind::Map(pairs) => {
                let keys_fresh = pairs.iter().all(|(k, _)| self.fresh_shape(k, d).is_all());
                let values: Vec<Fresh> =
                    pairs.iter().map(|(_, v)| self.fresh_shape(v, d)).collect();
                if keys_fresh && values.iter().all(Fresh::is_all) {
                    Fresh::All
                } else {
                    Fresh::values(Fresh::meet(&values))
                }
            }
            ExprKind::Comprehension {
                kind, key, elem, ..
            } => {
                let elem = self.fresh_shape(elem, d);
                match kind {
                    crate::ast::CompKind::List => Fresh::each(elem),
                    crate::ast::CompKind::Set if elem.is_all() => Fresh::All,
                    crate::ast::CompKind::Set => Fresh::root(),
                    crate::ast::CompKind::Map => {
                        let key_fresh = key
                            .as_deref()
                            .is_some_and(|k| self.fresh_shape(k, d).is_all());
                        if key_fresh && elem.is_all() {
                            Fresh::All
                        } else {
                            Fresh::values(elem)
                        }
                    }
                }
            }
            // An explicit `?x` takes its one level from the node: its carrier record can arrive
            // at frame end, after this runs. The compiler finds no record for a synthesized node.
            ExprKind::Unary { op, expr } => match super::fn_writes::aliases_operand(&e.kind) {
                super::fn_writes::OperandAlias::Holds => {
                    let held = Fresh::at(vec![self.fresh_shape(expr, d)]);
                    if *op == crate::ast::UnaryOp::Wrap {
                        return if e.id.0 == crate::ast::NodeId::SYNTH.0 {
                            Fresh::Marked
                        } else {
                            held
                        };
                    }
                    held
                }
                super::fn_writes::OperandAlias::Fresh | super::fn_writes::OperandAlias::Child => {
                    Fresh::Marked
                }
            },
            ExprKind::Call {
                callee,
                args,
                named,
                ..
            } => match &callee.kind {
                ExprKind::Field { obj, name, .. }
                    if self.native_method_returns_container(obj, name) =>
                {
                    Fresh::root()
                }
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
                | ExprKind::OptChain { .. }
                | ExprKind::NullCoalesce { .. }
                | ExprKind::Closure { .. }
                | ExprKind::Match { .. }
                | ExprKind::IfElse { .. }
                | ExprKind::Recover(..)
                | ExprKind::ElseGuard { .. } => match self
                    .resolutions
                    .get(&(self.graph_module_idx, callee.id.0))
                    .cloned()
                {
                    Some(Resolution::StructCtor(key)) => {
                        let slots = self.bound_slots(e.id, args, named);
                        if self
                            .struct_shape(&key)
                            .is_some_and(|s| s.fields.len() == slots.len())
                        {
                            Fresh::at(slots.iter().map(|s| self.slot_fresh(s, d)).collect())
                        } else {
                            Fresh::root()
                        }
                    }
                    // A payload variant resolves as `VariantFn`; both lower to one `NewEnum`
                    // over the positional args.
                    Some(Resolution::Variant { .. } | Resolution::VariantFn { .. })
                        if named.is_empty() =>
                    {
                        Fresh::at(args.iter().map(|x| self.fresh_shape(x, d)).collect())
                    }
                    Some(Resolution::Variant { .. } | Resolution::VariantFn { .. }) => {
                        Fresh::root()
                    }
                    _ => Fresh::Marked,
                },
            },
            ExprKind::Interp(_)
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => Fresh::Marked,
        };
        // The implicit wrap at a typed slot is one more object around the value. The compiler
        // emits it from this same record.
        if self.wraps.contains_key(&(self.graph_module_idx, e.id.0)) {
            Fresh::at(vec![shape])
        } else {
            shape
        }
    }

    /// The shape of one bound slot of a call. A variadic pack is a list built at the call site; an
    /// inline default is a self-contained literal; a provider default is a call result, which may
    /// return a global (a known false-fault ceiling).
    fn slot_fresh(&mut self, s: &super::fn_writes::SlotSrc, depth: usize) -> Fresh {
        match s {
            super::fn_writes::SlotSrc::Arg(e) => self.fresh_shape(e, depth),
            super::fn_writes::SlotSrc::Pack(elems) => Fresh::at(
                elems
                    .iter()
                    .map(|e| self.fresh_shape(e, depth + 1))
                    .collect(),
            ),
            super::fn_writes::SlotSrc::Default { inline: true } => Fresh::All,
            super::fn_writes::SlotSrc::Default { inline: false } => Fresh::Marked,
        }
    }

    /// TICKET-238 -- run `f` as one statement: `stmt_mark` records where its errors and poisoned
    /// reads begin. A nested statement's reads do not count for the statement around it, at any
    /// depth. `diag_rollback` restores `stmt_mark.poison` with `poison_reads`.
    pub(super) fn with_stmt_mark<R>(&mut self, span: Span, f: impl FnOnce(&mut Self) -> R) -> R {
        let start = self.poison_reads;
        let outer = std::mem::replace(
            &mut self.stmt_mark,
            super::StmtMark {
                errors: self.errors.len(),
                poison: self.poison_reads,
                span,
            },
        );
        let r = f(self);
        // Every read made inside `f`, at any depth: counted from `start`, not from the mark,
        // which the statements nested in `f` have already raised.
        let inner = self.poison_reads.saturating_sub(start);
        self.stmt_mark = outer;
        self.stmt_mark.poison += inner;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_layout_round_trips() {
        use Fresh::{All, Marked, Node};
        assert_eq!(Fresh::root(), Node(Kids::Marked));
        assert_eq!(Fresh::each(All), All);
        assert_eq!(Fresh::each(Marked), Fresh::root());
        assert_eq!(
            Fresh::each(Fresh::root()),
            Node(Kids::Each(Box::new(Fresh::root())))
        );
        // `at`: no kid and all-`All` kids are `All`; anything else keeps its positions.
        assert_eq!(Fresh::at(vec![]), All);
        assert_eq!(Fresh::at(vec![All, All]), All);
        assert_eq!(
            Fresh::at(vec![All, Marked]),
            Node(Kids::At(vec![All, Marked].into_boxed_slice()))
        );
        // The value each side of MAX_WIDTH.
        let mut wide = vec![Fresh::root(); MAX_WIDTH];
        assert!(matches!(Fresh::at(wide.clone()), Node(Kids::At(k)) if k.len() == MAX_WIDTH));
        wide.push(Fresh::root());
        assert_eq!(Fresh::at(wide.clone()), Fresh::each(Fresh::root()));
        wide.push(Marked);
        assert_eq!(Fresh::at(wide), Fresh::root());
        // `meet`.
        assert_eq!(Fresh::meet(&[Fresh::root(), Fresh::root()]), Fresh::root());
        assert_eq!(Fresh::meet(&[All, Fresh::root()]), Fresh::root());
        assert_eq!(Fresh::meet(&[Marked, All]), Marked);
        assert_eq!(Fresh::meet(&[All, All]), All);
        // A map with a key that is not fresh never becomes `All`, at any nesting.
        let v = Fresh::values(All);
        assert_eq!(v, Node(Kids::Each(Box::new(All))));
        assert!(!v.is_all());
        assert!(!Fresh::at(vec![v.clone()]).is_all());
        assert!(!Fresh::each(v).is_all());
        assert_eq!(Fresh::values(Marked), Fresh::root());
    }
}
