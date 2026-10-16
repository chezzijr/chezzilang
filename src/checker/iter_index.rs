//! Checker: iteration element types, index and slice typing, ranges, `for_bindings`, match kinds,
//! enum and struct members.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::pattern::BindMode;
use super::proto::HookMiss;
use super::setup::{TypeHead, TypeHeadKind};
use super::sig::bare_head_name;
use super::sig::qualified_head_names;
use super::*;

impl Checker {
    /// The element type produced by iterating `iter` in a `for` loop.
    /// The per-iteration bindings of a `for` loop: one name for the common form, or two
    /// (`for k, v in m:`) to destructure a map's entries. A range/list/str binds a single value; a
    /// map binds its key (1 name) or key+value (2 names). Any other arity/iterand combination is an
    /// error (a dummy `Unknown` binding is returned per name so checking continues).
    /// The element type the `next` hook of declared type `ty` yields, or `None` when `ty` has no
    /// `next` hook ([`Checker::hook_method`] decides).
    pub(super) fn struct_iter_elem(&self, ty: &Ty) -> Option<Ty> {
        Self::next_ret_elem(self.hook_method(ty, "Iterator", "next")?.0.ret)
    }

    /// The element type in the return type of a `next` hook, `Option[E]`.
    pub(super) fn next_ret_elem(ret: Ty) -> Option<Ty> {
        match ret {
            Ty::Option(inner) => Some(*inner),
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
            | Ty::Result(..)
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

    /// The element type in the return type of an `iter` hook, `Iterator[E]`.
    pub(super) fn iter_ret_elem(ret: Ty) -> Option<Ty> {
        match ret {
            Ty::Struct(name, mut args) if name == "Iterator" && args.len() == 1 => args.pop(),
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

    /// The element type the `iter` hook of declared type `ty` produces, or `None` when `ty` has
    /// no `iter` hook ([`Checker::hook_method`] decides).
    pub(super) fn struct_iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        Self::iter_ret_elem(self.hook_method(ty, "Iterable", "iter")?.0.ret)
    }

    /// The element type of ANY `Iterable` value — the single source of truth for `Iterable`
    /// conformance and the `Iterable`-driven `for`. A declared type is iterated through its `iter`
    /// hook, and through its `next` hook only when `iter` is not the hook (CPython: `for` calls
    /// `__iter__`). Everything else flows through [`iter_elem`](Self::iter_elem).
    /// `None` ⇒ not iterable.
    pub(super) fn iterable_elem(&self, ty: &Ty) -> Option<Ty> {
        // A type parameter bounded `S: Iterator[T]` or `S: Iterable[T]` yields the bound's
        // element argument (resolved at the declaration, TICKET-202).
        if let Ty::Param(name) = ty {
            return self.type_params.get(name).and_then(|bs| {
                bs.iter()
                    .find(|b| b.name == "Iterator" || b.name == "Iterable")
                    .and_then(|b| b.args.first().cloned())
            });
        }
        // The run time calls `iter` wherever the declaration has that hook, so its verdict
        // decides which hook types the elements.
        match self.hook_verdict(ty, "Iterable", "iter") {
            Ok(_) => self.struct_iterable_elem(ty),
            // `iter` runs and this instantiation fails its bound: no hook can type the elements.
            Err(HookMiss::Bound(_)) => None,
            Err(HookMiss::Absent | HookMiss::Static | HookMiss::Shape(_)) => self.iter_elem(ty),
        }
    }

    /// What a BUILTIN receiver yields per step: list/set → element, str → str, map → (key, value),
    /// bytes → int, and an `Iterator[T]` cursor its type argument. No declared type and no
    /// protocol existential is answered here.
    pub(super) fn builtin_iter_elem(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            Ty::List(e) | Ty::Set(e) => Some((**e).clone()),
            Ty::Str => Some(Ty::Str),
            // `bytes`/`bytearray` iterate to `int` (0–255), like Python.
            Ty::Bytes | Ty::ByteArray => Some(Ty::Int),
            // A Map iterates its `(key, value)` pairs (owner U9); `m.keys()` gives the keys.
            Ty::Map(k, v) => Some(Ty::Tuple(vec![(**k).clone(), (**v).clone()])),
            // A channel yields each received value and ends when it is closed and drained (Go's
            // `for v := range ch`, Rust's `rx.iter()`).
            Ty::Channel(e) => Some((**e).clone()),
            // `Iterator[T]` value (a generator result): element type is its single type argument.
            Ty::Struct(name, args) if name == "Iterator" && args.len() == 1 => {
                Some(args[0].clone())
            }
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Nil
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
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

    /// What iterating `ty` yields per step — the `Iterator` element type: a builtin receiver
    /// ([`builtin_iter_elem`](Self::builtin_iter_elem)), an `Iterator[T]`/`Iterable[T]`
    /// existential (its single type argument), or a declared type's `next` hook.
    /// `None` ⇒ not iterable.
    pub(super) fn iter_elem(&self, ty: &Ty) -> Option<Ty> {
        if let Some(elem) = self.builtin_iter_elem(ty) {
            return Some(elem);
        }
        match ty {
            // The SAME existential written in TYPE position. Representation asymmetry: `resolve_type`
            // intercepts the reserved name `Iterator[T]` into `Ty::Struct` (see its `("Iterator",
            // [elem])` arm), while every other protocol name — `Iterable[T]` included — falls to the
            // generic-protocol arm and becomes `Ty::Protocol`. Both spell "an existential I can
            // iterate", so both recover their element from the single type argument. The arity guard
            // is load-bearing: a BARE `Iterable` is `Ty::Protocol("Iterable", [])`, an existential
            // with unbound params, and stays non-iterable.
            Ty::Protocol(name, args)
                if (name == "Iterable" || name == "Iterator") && args.len() == 1 =>
            {
                Some(args[0].clone())
            }
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
            | Ty::Var(_) => self.struct_iter_elem(ty),
        }
    }

    /// The `(key, value)` types of `obj[k]` — the `Index` protocol's args. Built-in collections
    /// intrinsically (list/str index by int, map by its key); a user struct via its structural
    /// `index(self, K) -> V`. Single source of truth for `Index` conformance, `infer_index`, and the
    /// `Index[K,V]` arg-recovery in generic calls. `None` ⇒ not indexable.
    pub(super) fn index_kv(&self, ty: &Ty) -> Option<(Ty, Ty)> {
        match ty {
            Ty::List(e) => Some((Ty::Int, (**e).clone())),
            Ty::Str => Some((Ty::Int, Ty::Str)),
            // `bytes[i]`/`bytearray[i]` yield an `int` (0–255).
            Ty::Bytes | Ty::ByteArray => Some((Ty::Int, Ty::Int)),
            Ty::Map(k, v) => Some(((**k).clone(), (**v).clone())),
            Ty::Struct(..) | Ty::Enum(..) => {
                let (sig, _) = self.hook_method(ty, "Index", "index")?;
                Some((sig.params.get(1)?.clone(), sig.ret))
            }
            // A protocol existential that IS or embeds `Index[K, V]` (M22).
            Ty::Protocol(..) => {
                let (sig, map) = self.protocol_op_sig(ty, "index")?;
                if sig.params.len() != 2 {
                    return None;
                }
                Some((subst(&sig.params[1], &map), subst(&sig.ret, &map)))
            }
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Nil
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
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// M22 — `(sig, param-map)` for `method` on a protocol EXISTENTIAL: the signature resolved
    /// through the protocol's own methods or its embeds, paired with the map binding the protocol's
    /// type params to the value's carried args. The single seam every operator recovery below uses
    /// to read `K`/`V`/`R`/item off a protocol-typed receiver, mirroring `struct_param_map` for a
    /// struct one. The runtime receiver is always the concrete witness, dispatched by name, so
    /// every caller of this is a checker-only widening.
    fn protocol_op_sig(&self, ty: &Ty, method: &str) -> Option<(FnSig, HashMap<String, Ty>)> {
        let Ty::Protocol(p, pargs) = ty else {
            return None;
        };
        let sig = self.protocol_method_sig(p, method)?;
        let mut map: HashMap<String, Ty> = self
            .protocol_shape(p)?
            .type_params
            .iter()
            .cloned()
            .zip(pargs.iter().cloned())
            .collect();
        // `Self` binds to the receiver, exactly as the method-call arm does — otherwise `o[0]` on a
        // `fn index(self, k: int) -> Self` protocol leaks the raw `Ty::Param("Self")` out while the
        // hand-written `o.index(0)` yields the existential. An operator and its method spelling must
        // not disagree.
        map.insert("Self".to_string(), ty.clone());
        Some((sig, map))
    }

    /// The `item` type of `x in obj` when `obj` is a user type satisfying the `Contains` protocol —
    /// a struct/enum with `contains(self, item) -> bool`. Single source of truth for the `in`
    /// operator's LHS↔item compatibility check (`pattern.rs`). `None` ⇒ no valid `Contains` impl
    /// (unknown type, missing/wrong-arity/non-`bool` `contains`), so the `in` arm falls through to its
    /// reject-with-hint. The item type is generic-substituted (`Box[int]`'s `T` → `int`), mirroring
    /// `index_kv`.
    pub(super) fn contains_item_ty(&self, ty: &Ty) -> Option<Ty> {
        // A `Contains`-bounded type parameter (`fn f[C: Contains[int]](...)`): recover the item type
        // from the bound, mirroring `ordering_allowed`'s `Comparable`-bound arm — `in` resolves
        // through a bound just as `<` does. At runtime the value is always a concrete monomorphized
        // struct/enum, which `op_contains` already dispatches, so this is a checker-only widening.
        if let Ty::Param(pname) = ty {
            for b in self.type_params.get(pname)? {
                let Some(pinfo) = self.protocol_shape(&b.name) else {
                    continue;
                };
                // Own `contains` OR one an embed requires (`protocol Bag: Contains[int]`).
                let Some(sig) = self.protocol_method_sig(&b.name, "contains") else {
                    continue;
                };
                if sig.params.len() != 2 || sig.ret != Ty::Bool {
                    continue;
                }
                let map: HashMap<String, Ty> = pinfo
                    .type_params
                    .iter()
                    .cloned()
                    .zip(b.args.iter().cloned())
                    .collect();
                return Some(subst(&sig.params[1], &map));
            }
            return None;
        }
        // A protocol EXISTENTIAL (`b: Bag[int]`) — same recovery, with the value's own carried args
        // standing in for the bound's.
        if let Ty::Protocol(..) = ty {
            let (sig, map) = self.protocol_op_sig(ty, "contains")?;
            if sig.params.len() != 2 || sig.ret != Ty::Bool {
                return None;
            }
            return Some(subst(&sig.params[1], &map));
        }
        let (sig, _) = self.hook_method(ty, "Contains", "contains")?;
        sig.params.get(1).cloned()
    }

    /// TICKET-227 — the type a plain `=` writes into `target`: its read type, except a struct
    /// `obj[k]` target, whose write slot is `set_index`'s value ([`Self::index_set_kv`]; a plain `=`
    /// never reads through `index`). A tuple target is per element. A probe: the caller rolls back
    /// its diagnostics.
    pub(super) fn assign_slot_ty(&mut self, target: &Expr) -> Ty {
        match &target.kind {
            ExprKind::Tuple(items) => {
                Ty::Tuple(items.iter().map(|t| self.assign_slot_ty(t)).collect())
            }
            ExprKind::Index { obj, .. } => {
                let obj_ty = self.infer(obj);
                match self.index_set_kv(&obj_ty) {
                    Some((_, v)) => v,
                    None => self.infer(target),
                }
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
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Field { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => self.infer(target),
        }
    }

    /// The `(key, value)` types of a mutable `obj[k] = v` — the `IndexSet` protocol's args. Built-in
    /// `list`/`map` are mutable intrinsically (handled directly in `check_assign`); this resolves the
    /// struct case via `set_index(self, K, V)`. `IndexSet` *requires* `index` too (Rust `IndexMut: Index`):
    /// a plain `=` only calls `set_index`, but a compound `b[k] += v` reads via `index` first, so a
    /// struct missing `index` would type-check then crash. `None` ⇒ not index-assignable.
    ///
    /// The pair is `set_index`-derived: it is the WRITE slot. A plain `=` never reads through
    /// `index`, so the two may legitimately disagree (a `index -> V?` safe-read container, a widening
    /// writer). The COMPOUND form is the one that reads — `check_assign`'s struct arm types its LHS
    /// from `index_kv` (the read side) and then requires the result to fit this write slot.
    pub(super) fn index_set_kv(&self, ty: &Ty) -> Option<(Ty, Ty)> {
        // A protocol existential that IS or embeds `IndexSet[K, V]` (M22). Same read-too rule as a
        // struct: compound index-assign needs `index` as well as `set_index`.
        if let Ty::Protocol(..) = ty {
            let (sig, map) = self.protocol_op_sig(ty, "set_index")?;
            let read = self.protocol_op_sig(ty, "index")?.0;
            if sig.params.len() != 3 || read.params.len() != 2 {
                return None;
            }
            return Some((subst(&sig.params[1], &map), subst(&sig.params[2], &map)));
        }
        let (sig, _) = self.hook_method(ty, "IndexSet", "set_index")?;
        // Must also be readable — the `index` hook — or compound index-assign would crash.
        self.hook_method(ty, "Index", "index")?;
        Some((sig.params.get(1)?.clone(), sig.params.get(2)?.clone()))
    }

    /// The type of a `Range` value: the prelude struct every module sees (TICKET-253).
    pub(super) fn range_ty(&self) -> Ty {
        // The prelude's `Range`, whatever this module declares: `a..b` is syntax, so it means the
        // struct the compiler builds. A module's own `struct Range` shadows only the NAME.
        Ty::Struct(self.range_key.clone(), Vec::new())
    }

    /// The result type of `obj[a..b]` — the `Slice` protocol's arg. `list[T] → list[T]`, `str → str`;
    /// a user struct via `slice(self, int, int) -> R`. `None` ⇒ not sliceable.
    pub(super) fn slice_result(&self, ty: &Ty) -> Option<Ty> {
        match ty {
            // `bytes[a:b:c]` yields a new `bytes`; `bytearray` slices to a new `bytearray`;
            // `list`/`str` slice to themselves.
            Ty::List(_) | Ty::Str | Ty::Bytes | Ty::ByteArray => Some(ty.clone()),
            Ty::Struct(..) | Ty::Enum(..) => Some(self.hook_method(ty, "Slice", "slice")?.0.ret),
            // A protocol existential that IS or embeds `Slice[R]` (M22) — same signature rule.
            Ty::Protocol(..) => {
                let (sig, map) = self.protocol_op_sig(ty, "slice")?;
                if sig.params.len() != 2 || sig.params[1] != self.range_ty() {
                    return None;
                }
                Some(subst(&sig.ret, &map))
            }
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Nil
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
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// TICKET-202 — the ONE producer of a resolved bound: the protocol name keyed by
    /// `protocol_key`, every type arg resolved in the CURRENT scope. Callers own the scope (the
    /// declaration's); a use site never calls this on a stored bound.
    pub(super) fn resolve_bound(&mut self, b: &Bound, span: Span) -> TyBound {
        TyBound {
            name: self.user_protocol_key(&b.name),
            args: b.args.iter().map(|a| self.resolve_type(a, span)).collect(),
        }
    }

    /// Resolve every bound of `tps` in the declaring scope, with `tps`' own names in scope (so a
    /// sibling `[S, B: Conv[S]]` resolves to `Ty::Param("S")`). Silent: `check_bounds` is the one
    /// reporter of a bound's diagnostics, and a declaration's params are entered several times.
    pub(super) fn resolve_bounds(&mut self, tps: &[TypeParam], span: Span) -> Vec<TyParam> {
        let has_args = tps
            .iter()
            .any(|tp| tp.bounds.iter().any(|b| !b.args.is_empty()));
        let mark = has_args.then(|| self.diag_mark());
        let saved = self.type_params.clone();
        for tp in tps {
            self.type_params.insert(tp.name.clone(), Vec::new());
        }
        let out = tps
            .iter()
            .map(|tp| TyParam {
                name: tp.name.clone(),
                name_span: tp.name_span,
                bounds: tp
                    .bounds
                    .iter()
                    .map(|b| self.resolve_bound(b, span))
                    .collect(),
            })
            .collect();
        self.type_params = saved;
        if let Some(m) = mark {
            self.diag_rollback(m);
        }
        out
    }

    /// TICKET-253 — the two faults of `Range`'s own methods, reported early when the checker sees the
    /// range LITERAL as the direct iterand or `.len()` receiver: iterating a range with no start, and
    /// `len` of a range with a missing bound. A range VALUE reaches the same rules at run time, from
    /// `Range.iter` and `Range.len` in `std/prelude.chz` (the one source of the two messages).
    pub(super) fn range_literal_fault(&mut self, e: &Expr, needs_end: bool) {
        let ExprKind::Range { start, end, .. } = &e.kind else {
            return;
        };
        if needs_end {
            if start.is_none() || end.is_none() {
                self.error(e.span, "len() needs a range with a start and an end");
            }
        } else if start.is_none() {
            self.error(e.span, "a range with no start cannot be iterated");
        }
    }

    /// The names a loop target binds over `iter`, each with its name span and type. The element
    /// type has one source, `iterable_elem`; the one binder (`bind_subpattern`, target mode)
    /// matches the target against it, so `for a, (b, c) in xs` unpacks a tuple element of any
    /// iterable.
    pub(super) fn for_bindings(
        &mut self,
        target: &Pattern,
        iter: &Expr,
    ) -> Vec<(String, Span, Ty)> {
        self.range_literal_fault(iter, false);
        let m = self.tyvar_mark();
        let it = self.infer(iter);
        let it = self.close_operand(m, it);
        let elem = if it.is_unknown() {
            Ty::Unknown
        } else if let Some(elem) = self.iterable_elem(&it) {
            elem
        } else {
            let note = self.iter_hook_note(&it);
            self.error(iter.span, format!("cannot iterate over {it}{note}"));
            Ty::Unknown
        };
        let mut leaves = Vec::new();
        self.bind_subpattern(target, &elem, iter.span, &mut BindMode::Target(&mut leaves));
        leaves
    }

    /// How a `match` is being checked, derived from the scrutinee's type.
    ///
    /// `patterns` are the arms' top-level patterns — used ONLY to recover exhaustiveness when the
    /// scrutinee is un-inferable (`Ty::Unknown`, e.g. an unannotated closure param): a `Skip` there
    /// would bypass coverage entirely (soundness hole — `match x: E.A: ..` over `enum E{A,B}` checked
    /// ok then trapped at runtime). See `reconstruct_unknown_kind`.
    pub(super) fn match_kind(&mut self, scrutinee: &Expr, patterns: &[&Pattern]) -> MatchKind {
        let m = self.tyvar_mark();
        let sty = self.infer(scrutinee);
        let sty = self.close_operand(m, sty);
        // Every enum type, carrier or user, reads its variants from the one table reader.
        if let Some((name, _)) = sty.as_enum() {
            return MatchKind::Variants {
                label: name.to_string(),
                variants: self.variants_of(&sty).unwrap_or_default(),
                scrut: sty.clone(),
            };
        }
        match &sty {
            // int/str/bool scrutinees match against literal patterns (+ a `_` wildcard).
            Ty::Int => MatchKind::Literal(Ty::Int),
            Ty::Str => MatchKind::Literal(Ty::Str),
            Ty::Bool => MatchKind::Literal(Ty::Bool),
            Ty::Tuple(tys) => MatchKind::Tuple(tys.clone()),
            // A USER struct scrutinee (L2) matches positional field patterns (`Point(x, y)`). Gated
            // to user-declared structs via `struct_fields_of`: a native/reserved handle
            // (Socket/Iterator — a `Ty::Struct` with a `struct_shape` but `StructOrigin::Builtin`)
            // is NOT bare-destructurable by the compiler, so it stays on the `other =>` reject below —
            // never a pattern the checker accepts but the compiler can't lower (the checker-superset
            // trap). Fields are already instantiated (generic `Box[int]` → field `int`, not `T`).
            Ty::Struct(name, targs) if self.struct_fields_of(&sty).is_some() => MatchKind::Struct {
                label: name.clone(),
                fields: self.struct_fields_of(&sty).unwrap_or_default(),
                targs: targs.clone(),
            },
            // Un-inferable scrutinee: rather than skip exhaustiveness outright (a soundness hole),
            // reconstruct a concrete kind from the arm patterns when they unambiguously name a single
            // known enum or are literals — so the normal coverage check applies. Genuinely unknowable
            // cases still fall back to `Skip`.
            Ty::Unknown => self.reconstruct_unknown_kind(patterns, scrutinee.span),
            other @ Ty::Float
            | other @ Ty::Bytes
            | other @ Ty::ByteArray
            | other @ Ty::Nil
            | other @ Ty::List(_)
            | other @ Ty::Map(..)
            | other @ Ty::Set(_)
            | other @ Ty::Func { .. }
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
                self.error(
                    scrutinee.span,
                    format!("cannot match on non-enum type {other}"),
                );
                MatchKind::Skip
            }
        }
    }

    /// Classify a match over a residual un-inferable (`Ty::Unknown`) scrutinee — one that §3 closure
    /// inference did NOT pin (e.g. a tuple-element binding `a` from `(a, b)`, or an `Unknown`-typed
    /// expression). The top-level scrutinee arm goes through this path (not `bind_subpattern`), so it
    /// is the OTHER half of the §4.1 structural-over-`Unknown` reject (the nested half is in
    /// `bind_subpattern`):
    /// - **Any STRUCTURAL arm** (a tuple, or a real variant — qualified, builtin `Ok`/`Err`/`Some`/
    ///   `None`, an owned bare variant, or a `Name(..)` with a payload) tests a shape/tag against a
    ///   value whose type we can't prove → it traps at runtime on a wrong shape (a trailing `_` cannot
    ///   rescue it). Reject; annotate the enclosing param. Returns `Skip` afterwards so the arm bodies
    ///   still bind permissively (no cascade).
    /// - **Only literal/range arms** → `Literal(first-arm scalar)`: the existing literal-domain rule
    ///   then makes a non-`_` match non-exhaustive AND a heterogeneous arm (`1` + `"b"`) a literal
    ///   mismatch — restoring the pre-`OpenScrutinee` behaviour.
    /// - **Only bindings / `_`** → `Skip` (a bare-ident catch-all closes it; nothing to enforce).
    ///
    /// Top-level `Or` alternatives are flattened first. `&mut self` (it may emit the §4.1 reject).
    pub(super) fn reconstruct_unknown_kind(
        &mut self,
        patterns: &[&Pattern],
        span: Span,
    ) -> MatchKind {
        // Flatten top-level or-alternatives into a flat head list.
        let mut heads: Vec<&Pattern> = Vec::new();
        for p in patterns {
            match p {
                Pattern::Or(alts) => heads.extend(alts.iter()),
                other @ Pattern::Ident(..)
                | other @ Pattern::Variant { .. }
                | other @ Pattern::Tuple(_)
                | other @ Pattern::Literal(_)
                | other @ Pattern::Range { .. }
                | other @ Pattern::Wildcard
                | other @ Pattern::Carrier { .. } => heads.push(other),
            }
        }
        let mut lit_ty: Option<Ty> = None;
        // The kind word of the FIRST structural arm seen (`"tuple"` / `"variant"`), if any.
        let mut structural: Option<&'static str> = None;
        for h in &heads {
            match h {
                Pattern::Tuple(_) => {
                    structural.get_or_insert("tuple");
                }
                Pattern::Variant {
                    id: _,
                    name,
                    enum_name,
                    module_name,
                    bindings,
                } => {
                    // A real variant pattern (qualified, builtin, owned, or carrying a payload) is
                    // structural; a bare name that is NOT a known variant and binds nothing is a
                    // catch-all binding → not structural.
                    let is_variant = module_name.is_some()
                        || enum_name.is_some()
                        || self.bare_pattern_name(name).is_variant()
                        || !bindings.is_empty();
                    if is_variant {
                        structural.get_or_insert("variant");
                    }
                }
                Pattern::Literal(lit) => {
                    if lit_ty.is_none() {
                        lit_ty = Some(lit_pattern_ty(lit));
                    }
                }
                Pattern::Range { .. } if lit_ty.is_none() => {
                    lit_ty = Some(Ty::Int);
                }
                Pattern::Carrier { .. } => {
                    structural.get_or_insert("variant");
                }
                // Ident/Wildcard/(nested Or) — no structural or literal signal.
                Pattern::Ident(..) | Pattern::Range { .. } | Pattern::Wildcard | Pattern::Or(_) => {
                }
            }
        }
        if let Some(kind) = structural {
            self.error(
                span,
                format!(
                    "cannot match a {kind} pattern on a value of un-inferable type; annotate it"
                ),
            );
            return MatchKind::Skip;
        }
        if let Some(t) = lit_ty {
            return MatchKind::Literal(t);
        }
        MatchKind::Skip
    }

    /// The substitution from a generic enum's type parameters to a concrete instantiation's type
    /// arguments (`Tree[int]` ⇒ `{T: int}`). Empty for a non-generic enum.
    pub(super) fn enum_param_map(&self, name: &str, targs: &[Ty]) -> HashMap<String, Ty> {
        // `enum_type_params_of` adds the miss-only owning-module fallback (gap #4) so a named-fn-
        // imported enum value binds its params identically to a whole-module import.
        self.enum_type_params_of(name)
            .map(|tps| {
                tps.iter()
                    .map(|tp| tp.name.clone())
                    .zip(targs.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The one type-parameter default an enum has (a named interim, TICKET-228 D2): `Result[T]` in
    /// TYPE position means `Result[T, Error]`. Every other enum, and every other arity, is unchanged.
    pub(super) fn carrier_default_args(&self, key: &str, mut args: Vec<Ty>) -> Vec<Ty> {
        if key == "Result" && args.len() == 1 {
            args.push(Ty::error_proto());
        }
        args
    }

    /// The variant→payload map for an enum type (carrier or user, read through `Ty::as_enum`),
    /// else `None`. The one reader of the variant tables: `match_kind` and the nested-pattern
    /// checker (gap #15) both call it.
    pub(super) fn variants_of(&self, ty: &Ty) -> Option<HashMap<String, Vec<Ty>>> {
        let (name, targs) = ty.as_enum()?;
        let map = self.enum_param_map(name, &targs);
        let vs = self.enums.get(name)?;
        Some(
            vs.iter()
                .map(|v| {
                    let payload = self.variants[&(name.to_string(), v.clone())]
                        .payload
                        .iter()
                        .map(|p| subst(p, &map))
                        .collect();
                    (v.clone(), payload)
                })
                .collect(),
        )
    }

    /// The INSTANTIATED positional field types of a USER struct (`Ty::Struct`) — the shape a struct
    /// pattern `Point(x, y)` binds against (L2). Returns `None` for anything that is not a
    /// user-declared struct: non-struct types, and — crucially — native/reserved struct handles
    /// (`StructOrigin::Builtin`: Socket/Iterator/Match/…), whose fields the compiler cannot bare-
    /// destructure. Gating here (not just in the compiler) keeps the checker from accepting a struct
    /// pattern the compiler can't lower. Generic params are substituted (`Box[int]` → field `int`).
    pub(super) fn struct_fields_of(&self, ty: &Ty) -> Option<Vec<Ty>> {
        let Ty::Struct(name, targs) = ty else {
            return None;
        };
        let info = self.struct_shape(name)?;
        if info.origin != StructOrigin::User {
            return None;
        }
        let map = struct_param_map(info, targs);
        Some(info.fields.iter().map(|(_, t)| subst(t, &map)).collect())
    }

    /// M24-5b — is this `defer`/`spawn` target's dotted callee a CONSTRUCTOR rather than a call?
    /// `defer Color.Val(3)` builds a value and throws it away, exactly like the bare `defer P(3)`
    /// the same rule already rejects — a variant constructor IS a constructor — so it earns that
    /// rule's message and, crucially, its PHASE: the compiler has no receiver value for it, and a
    /// refusal that lives there is a program `chezzi check` calls clean and only `chezzi run`
    /// refuses. A dotted STATIC METHOD (`H.build(3)`, `B[int].make(3)`, `lib.Holder.build(3)`) is an
    /// ordinary call and is NOT rejected — it lowers through the eager-args wrapper, and Go accepts
    /// its analogue (`defer pkg.F(x)`).
    ///
    /// The spellings mirror `infer_call`'s constructor arms — ALL of them, so one concept gets one
    /// verdict: bare `Enum.Variant` and its two turbofish carriers, qualified `module.Enum.Variant`
    /// (turbofished too), the qualified struct constructor `module.Point(…)`, and the
    /// qualified NATIVE constructor `concurrency.Shared(…)` / `time.timer(…)`.
    pub(super) fn dotted_ctor_target(&self, callee: &Expr) -> bool {
        let ExprKind::Field { obj, name, .. } = &callee.kind else {
            return false;
        };
        // `Enum.Variant(…)` / `Enum[T].Variant(…)`
        if let Some(ename) = bare_head_name(&obj.kind)
            && let Some(TypeHead {
                kind: TypeHeadKind::Enum,
                key,
                pinned: None,
                ..
            }) = self.bare_type_head(ename)
            && self.variants.contains_key(&(key, name.clone()))
        {
            return true;
        }
        // `module.Point(…)` — the head is the MODULE and the member the type.
        // A reserved native handle (`net.Socket`) has a `struct_defs` entry for its method table but
        // no constructor; `infer_call` skips it the same way, so its own diagnostic stays single.
        if let ExprKind::Ident(mname) = &obj.kind
            && !self.module_owns_builtin(mname, name)
            && self
                .qualified_type_head(mname, name)
                .is_some_and(|th| th.pinned.is_none() && matches!(th.kind, TypeHeadKind::Struct))
        {
            return true;
        }
        // `concurrency.Shared(…)` — a qualified opcode constructor, the same concept as the bare
        // `Shared(0)` the `Ident` arm already refuses. `BuiltinType::has_opcode_ctor` is the one set
        // both this and `qualified_ctor` read: the head is the MODULE and the member a builtin type
        // its owning std module lists in `sig.builtins`. Without this the compiler lowers it to a module-MEMBER load — a
        // reserved ctor is not a module member — so `chezzi check` passed a program only
        // `chezzi run` refused ("module 'std.concurrency' has no member 'Shared'"). Type-only
        // native names (`net.Socket`) are excluded: `infer_call` gives them their own, better
        // message ("has no constructor"), and doubling it would say less.
        if let ExprKind::Ident(mname) = &obj.kind
            && !self.head_is_value(mname)
            && BuiltinType::from_name(name).is_some_and(BuiltinType::has_opcode_ctor)
            && let Some(mid) = self.imported_modules.get(mname)
            && let Some(sig) = self.module_sigs.get(mid)
            && sig.builtins.contains(name)
        {
            return true;
        }
        // `module.Enum.Variant(…)` / `module.Enum[T].Variant(…)`
        if let Some((mname, ename)) = qualified_head_names(&obj.kind)
            && let Some(TypeHead {
                kind: TypeHeadKind::Enum,
                key,
                pinned: None,
                ..
            }) = self.qualified_type_head(mname, ename)
            && self.variants.contains_key(&(key, name.clone()))
        {
            return true;
        }
        false
    }
}
