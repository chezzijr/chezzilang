//! Checker: block checking and fresh temporaries: `check_block`, `temporary_write`, `fresh_shape`,
//! generator frames, call crossing.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::*;

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
        let op = fn_writes::RootUse::Op;
        self.use_keeps_root(&op, &l) && self.use_keeps_root(&op, &r)
    }

    /// TICKET-254 — `obj.name(..)` is a native method of a `List`/`Map`/`Set`/`bytearray` whose
    /// declared return type is a container of those four kinds: the result is a new object (no
    /// native method returns its receiver or a view of it;
    /// `native_container_results_are_new_objects` runs every one). Its elements are the receiver's,
    /// so the shape is `Fresh::root()` (DEC-160).
    fn native_method_returns_container(&mut self, obj: &Expr, name: &str) -> bool {
        let recv = self.probe_ty(obj);
        fn_writes::native_receiver(recv.scalar()).is_some_and(|(key, _)| {
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
        let shape = self.fresh_shape(base, 0, true);
        if links == 0 {
            !shape.is_marked()
        } else {
            shape.is_all()
        }
    }

    /// D4 (TICKET-240): what a crossing leaves marked in the value graph `e` builds. The one
    /// decider of "can the parent still reach this object", recursive over the expression. A
    /// constructor-like node is fresh and each child is judged by the same rule: a list, tuple,
    /// set or map literal, a comprehension, a struct or variant constructor, a carrier wrap
    /// (explicit, or the implicit one `WrapTable` records), and a zero-argument `.copy()` on a
    /// List/Map/Set/bytearray (its own level only, DEC-160). A name, a field read, an index and
    /// any other call result stay `Marked` (`id(xs)` returns the parent's own list). A map with a
    /// key that is not fresh is never `All`: a struct with a mutable field is a legal key. Below
    /// `MAX_DEPTH` levels a subtree is `Marked`. `live` is false when `e` is read outside the
    /// scope it was checked in: nothing is inferred then, so a `.copy()` stays `Marked`.
    pub(super) fn fresh_shape(&mut self, e: &Expr, depth: usize, live: bool) -> Fresh {
        if depth >= crate::base::crossing::MAX_DEPTH {
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
            } if live
                && fn_writes::aliases_operand(&e.kind) == fn_writes::OperandAlias::Fresh
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
                Fresh::at(items.iter().map(|x| self.fresh_shape(x, d, live)).collect())
            }
            ExprKind::Set(items) => {
                if items.iter().all(|x| self.fresh_shape(x, d, live).is_all()) {
                    Fresh::All
                } else {
                    Fresh::root()
                }
            }
            ExprKind::Map(pairs) => {
                let keys_fresh = pairs
                    .iter()
                    .all(|(k, _)| self.fresh_shape(k, d, live).is_all());
                let values: Vec<Fresh> = pairs
                    .iter()
                    .map(|(_, v)| self.fresh_shape(v, d, live))
                    .collect();
                if keys_fresh && values.iter().all(Fresh::is_all) {
                    Fresh::All
                } else {
                    Fresh::values(Fresh::meet(&values))
                }
            }
            ExprKind::Comprehension {
                kind, key, elem, ..
            } => {
                let elem = self.fresh_shape(elem, d, live);
                match kind {
                    crate::ast::CompKind::List => Fresh::each(elem),
                    crate::ast::CompKind::Set if elem.is_all() => Fresh::All,
                    crate::ast::CompKind::Set => Fresh::root(),
                    crate::ast::CompKind::Map => {
                        let key_fresh = key
                            .as_deref()
                            .is_some_and(|k| self.fresh_shape(k, d, live).is_all());
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
                    let held = Fresh::at(vec![self.fresh_shape(expr, d, live)]);
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
                    if live && self.native_method_returns_container(obj, name) =>
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
                            Fresh::at(slots.iter().map(|s| self.slot_fresh(s, d, live)).collect())
                        } else {
                            Fresh::root()
                        }
                    }
                    // A payload variant resolves as `VariantFn`; both lower to one `NewEnum`
                    // over the positional args.
                    Some(Resolution::Variant { .. } | Resolution::VariantFn { .. })
                        if named.is_empty() =>
                    {
                        Fresh::at(args.iter().map(|x| self.fresh_shape(x, d, live)).collect())
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
    fn slot_fresh(&mut self, s: &super::fn_writes::SlotSrc, depth: usize, live: bool) -> Fresh {
        match s {
            super::fn_writes::SlotSrc::Arg(e) => self.fresh_shape(e, depth, live),
            super::fn_writes::SlotSrc::Pack(elems) => Fresh::at(
                elems
                    .iter()
                    .map(|e| self.fresh_shape(e, depth + 1, live))
                    .collect(),
            ),
            super::fn_writes::SlotSrc::Default { inline: true } => Fresh::All,
            super::fn_writes::SlotSrc::Default { inline: false } => Fresh::Marked,
        }
    }

    /// TICKET-190, TICKET-240: record a generator decl's frame slot verdicts (Rule G). A slot is
    /// `Root` when every binding of it builds a fresh root and that root cannot escape the body. It
    /// is `All` when, in addition, every binding is all-fresh and [`Self::deep_private`] holds. A
    /// param takes its shape from the creating call's stamp unless an assignment rebinds it. Main
    /// pass only; a second walk of the same decl keeps the lower verdict.
    pub(super) fn record_gen_frame(&mut self, decl: &FnDecl) {
        let Some(acc) = self.gen_frame.take() else {
            return;
        };
        if self.generic_arg_prepass || self.resolving_returns || decl.name_span == Span::default() {
            return;
        }
        let uses = super::fn_writes::frame_uses(decl);
        let is_param = |n: &str| decl.params.iter().any(|p| p.name == n);
        let names: Vec<(String, Fresh)> = decl
            .params
            .iter()
            .map(|p| {
                let shape = acc.fresh.get(&p.name).cloned().unwrap_or(Fresh::All);
                (p.name.clone(), shape)
            })
            .chain(
                acc.fresh
                    .iter()
                    .filter(|(n, _)| !is_param(n))
                    .map(|(n, f)| (n.clone(), f.clone())),
            )
            .collect();
        // The root test first: it only reads `self`.
        let stays: Vec<bool> = {
            let callee = |c: &super::fn_writes::ArgCallee, j: usize| self.arg_escapes(c, j, None);
            let no_tys = Vec::new();
            names
                .iter()
                .map(|(n, shape)| {
                    !shape.is_marked()
                        && !self.root_leaves(&uses, n, acc.tys.get(n).unwrap_or(&no_tys), &callee)
                })
                .collect()
        };
        let mut slots = HashMap::new();
        for ((n, shape), stays) in names.into_iter().zip(stays) {
            if !stays {
                continue;
            }
            if shape.is_all() && self.deep_private(&uses, &acc, &n) {
                slots.insert(n, SlotFresh::All);
            } else if !uses.matched.contains(&n) {
                slots.insert(n, SlotFresh::Root);
            }
        }
        let key = (self.graph_module_idx, decl.name_span);
        match self.gen_crossings.frames.get_mut(&key) {
            Some(prev) => {
                prev.retain(|n, _| slots.contains_key(n));
                for (n, v) in prev.iter_mut() {
                    *v = (*v).min(slots[n]);
                }
            }
            None => {
                self.gen_crossings.frames.insert(key, slots);
            }
        }
    }

    /// TICKET-189: how a call's bound slots cross, for a call that creates a generator: its
    /// creation stamp (TICKET-190).
    pub(super) fn call_crossing(&mut self, slots: &[super::fn_writes::SlotSrc]) -> CallCrossing {
        CallCrossing {
            args: slots.iter().map(|s| self.slot_fresh(s, 0, true)).collect(),
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
