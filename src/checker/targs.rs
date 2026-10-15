//! Checker: type-argument seeding and recovery, `enforce_bounds`, type-param shadowing, bound
//! methods.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::setup::HeadBinding;
use super::*;
use std::collections::HashMap;

impl Checker {
    /// A struct/enum's own type params bound to the args of THIS instantiation
    /// (`Box[T]` + `Box[int]` → `{T -> int}`); empty for every other `Ty`. Each arm resolves through
    /// the miss-only owning-module helpers (gap #4), so a named-fn-imported generic instantiation
    /// binds its params identically to a whole-module import.
    pub(super) fn nominal_param_map(&self, ty: &Ty) -> HashMap<String, Ty> {
        match ty {
            Ty::Struct(name, targs) => self
                .struct_shape(name)
                .map(|info| struct_param_map(info, targs))
                .unwrap_or_default(),
            Ty::Enum(name, targs) => self.enum_param_map(name, targs),
            // Built-in containers (TICKET-024, W8-32): binds a harvested native sig's `Ty::Param`
            // (e.g. `List[T]`'s `T`) to this instantiation's element type, from the SAME harvested
            // `StructInfo` `native_handle_method` reads — never a hardcoded "T"/"K"/"V" — so a
            // rename in `std/prelude.chz` can't silently unbind the substitution.
            Ty::List(e) => self.builtin_param_map("List", &[(**e).clone()]),
            Ty::Set(e) => self.builtin_param_map("Set", &[(**e).clone()]),
            Ty::Map(k, v) => self.builtin_param_map("Map", &[(**k).clone(), (**v).clone()]),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Func { .. }
            | Ty::BuiltinFn { .. }
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
            | Ty::Var(_) => HashMap::new(),
        }
    }

    /// Shared by [`Checker::nominal_param_map`]'s built-in-container arms: zips `targs` against
    /// `self.structs[key]`'s own declared type params.
    fn builtin_param_map(&self, key: &str, targs: &[Ty]) -> HashMap<String, Ty> {
        self.structs
            .get(key)
            .map(|info| struct_param_map(info, targs))
            .unwrap_or_default()
    }

    /// Resolve the element type of a value-first concurrency box (`Shared`/`RwShared`/`Atomic`) from
    /// an OPTIONAL turbofish, mirroring the container-ctor turbofish pattern (the `List` arm above).
    /// With no turbofish the value's `inferred` type wins; with one type arg that arg pins the element
    /// type and is checked against `inferred` (a mismatch like `Shared[str](0)` errors); arity > 1 is
    /// rejected and falls back to `inferred`.
    pub(super) fn concurrency_turbofish_elem(
        &mut self,
        name: &str,
        targs: &[Ty],
        inferred: Ty,
        span: Span,
    ) -> Ty {
        match targs {
            [] => inferred,
            [t] => {
                let t = t.clone();
                if !t.is_unknown() && !inferred.is_unknown() && !self.assignable(&t, &inferred) {
                    let note = self.protocol_note(&t, &inferred);
                    let [t_s, inferred_s] = Ty::render_distinct([&t, &inferred]);
                    self.error(
                        span,
                        format!(
                            "{name}[{t_s}]() expected element type {t_s}, found {inferred_s}{note}{}",
                            slot_note(&t, &inferred)
                        ),
                    );
                }
                t
            }
            _ => {
                self.error(span, format!("{name}[T]() takes exactly one type argument"));
                inferred
            }
        }
    }

    pub(super) fn name_is_generic(&self, name: &str) -> bool {
        // The built-in `Channel[T]()` constructor takes its element type as an explicit type arg.
        // The container constructors `List[T]()` / `Set[T]()` / `Map[K, V]()` likewise accept a
        // turbofish that pins the (otherwise un-inferable, for an empty container) element type.
        if matches!(name, "Channel" | "List" | "Set" | "Map") {
            return true;
        }
        // The value-first concurrency boxes accept an OPTIONAL turbofish that pins the element type
        // (`Shared[T](v)`); when present it is checked against the value's inferred type. Unlike
        // `Channel[T]()` the type arg is optional (the value is required and inference still works).
        if matches!(name, "Shared" | "RwShared" | "Atomic") {
            return true;
        }
        // Look the struct up by its module-scoped runtime key (`bare_key`), NOT the bare name: under
        // the real `check_graph` path `structs` is keyed `<module>::Name`, so a bare-name lookup
        // always misses and wrongly reports user generic structs as non-generic — which made the
        // `infer_named_call` gate reject explicit call-site type args (`Pair[int, str](…)`) that the
        // struct-ctor branch fully supports. Mirrors the ctor branch itself.
        if let Some(i) = self.structs.get(&self.bare_key(name)) {
            return !i.type_params.is_empty();
        }
        // Bare-name query (the qualified path resolves genericity directly). A variant name may now
        // belong to several enums; treat it as generic if any owner enum is generic. `variant_owners`
        // stores BARE enum names but `enum_type_params` is module-keyed, so go through `bare_key`
        // (same keying fix as the struct arm above — else a bare generic-enum variant like
        // `Full[int](5)` reports the misleading "takes no type arguments" instead of the
        // "write it qualified as 'Box.Full'" hint under the real `check_graph` path).
        if let Some(owners) = self.variant_owners.get(name) {
            return owners.iter().any(|en| {
                self.enum_type_params
                    .get(&self.bare_key(en))
                    .is_some_and(|t| !t.is_empty())
            });
        }
        if let Some(s) = self.functions.get(name) {
            return !s.type_params.is_empty();
        }
        false
    }

    /// Seed a substitution map from explicit call-site type arguments, validating their count
    /// against the declared type parameters. Empty `targs` (the inference-only case) yields an
    /// empty map. A count mismatch is reported but the overlapping prefix is still seeded so
    /// inference can recover.
    pub(super) fn seed_targs(
        &mut self,
        name: &str,
        tps: &[TyParam],
        targs: &[Ty],
        span: Span,
    ) -> HashMap<String, Ty> {
        let mut sub = HashMap::new();
        if !targs.is_empty() {
            if targs.len() != tps.len() {
                self.error(
                    span,
                    format!(
                        "'{name}' expects {} type argument(s), found {}",
                        tps.len(),
                        targs.len()
                    ),
                );
            }
            for (tp, ta) in tps.iter().zip(targs) {
                sub.insert(tp.name.clone(), ta.clone());
            }
        }
        sub
    }

    /// TICKET-239 — the one pre-argument substitution of a generic call, read by all six generic
    /// paths through [`Checker::infer_generic_arg_tys`]. Returns `(owned, seed)`. `owned` binds
    /// what is written before any argument is read: the explicit type arguments (`targs`, from
    /// [`Self::seed_targs`]) and what the expected type `hint` fixes in the declared result `ret`
    /// (`c: Box[int?] = Box(5)`), one carrier layer looked through; an argument whose slot is
    /// concrete under it owns that slot and may wrap. `seed` holds the one case that guides but
    /// never wraps: a bare-parameter result under a carrier annotation (`r: int? = g(?5)`), where
    /// the value wraps at the binding. Declines unless the hint is concrete up to in-scope type
    /// params (`ty_concrete_but` + `rigid_param`), so it never reads a partially-known annotation
    /// (DEC-054). It changes argument SLOTS only: the caller still solves its substitution from
    /// the arguments first.
    pub(super) fn slot_seed(
        &mut self,
        targs: &HashMap<String, Ty>,
        hint: Option<&Ty>,
        ret: &Ty,
    ) -> (HashMap<String, Ty>, HashMap<String, Ty>) {
        let mut owned = targs.clone();
        let mut seed = HashMap::new();
        if let Some(h) = hint
            && ty_concrete_but(h, &|n| self.rigid_param(n, &[]))
        {
            if let Ty::Param(n) = ret
                && h.carrier_parts().is_some()
            {
                if !owned.contains_key(n) {
                    seed.insert(n.clone(), h.clone());
                }
            } else {
                let h = if ret.carrier_parts().is_some() {
                    h
                } else {
                    h.slot_payload()
                };
                unify(ret, h, &mut owned);
            }
        }
        (owned, seed)
    }

    /// Recover element types from parameterized `Iterator[T]` / `Iterable[T]` bounds: for each type
    /// param already bound to a concrete iterand in `sub`, bind the bound's element arg `T` to the
    /// iterand's element type. Mutates `sub` (collects first to avoid borrowing it while iterating).
    /// Shared by every generic-call site (free fn, struct constructor, enum variant).
    ///
    /// `Iterable` is in scope since W6-3b: `[S: Iterable[T], T]` is what a raw-collection caller
    /// migrates to, and without recovery `T` would stay free and the bound check would then reject the
    /// very iterand it was given. The predicate stays [`iter_elem`](Self::iter_elem) (NOT
    /// `iterable_elem`): recovery is deliberately NOT total for `Iterable` — a struct with only
    /// `iter(self) -> Iterator[E]` still needs a concrete-arg bound (`[S: Iterable[int]]`).
    /// W12-8 (TICKET-106) — bind every still-FREE type param a recovered bound arg mentions, at any
    /// depth (`(A, B)`, `Option[A]`, `List[A]`), then check the SUBSTITUTED arg agrees with what was
    /// recovered. `Some(pinned)` = disagreement, for the caller to word. A bare free param reproduces
    /// the old insert; a bound or concrete arg reproduces the old agreement check.
    fn bind_recovered(
        &self,
        arg_ty: &Ty,
        recovered: &Ty,
        sub: &mut HashMap<String, Ty>,
    ) -> Option<Ty> {
        unify(arg_ty, recovered, sub);
        let pinned = subst(arg_ty, sub);
        if pinned.is_unknown() || recovered.is_unknown() || self.assignable(&pinned, recovered) {
            None
        } else {
            Some(pinned)
        }
    }

    pub(super) fn recover_iter_elems(
        &mut self,
        tps: &[TyParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name).cloned() {
                for b in &tp.bounds {
                    // A type whose two hooks disagree gives `Iterator` no element type; the
                    // bound check reports it.
                    let elem = match b.name.as_str() {
                        "Iterator" => self.iterator_bound_elem(&concrete).ok().flatten(),
                        "Iterable" => self.iterable_elem(&concrete),
                        _ => None,
                    };
                    if let Some(arg) = b.args.first()
                        && let Some(elem) = elem
                    {
                        binds.push((arg.clone(), elem));
                    }
                }
            }
        }
        for (arg_ty, elem) in &binds {
            if let Some(pinned) = self.bind_recovered(arg_ty, elem, sub) {
                self.error(
                    span,
                    format!(
                        "iterator element type {elem} does not match the declared element type {pinned}"
                    ),
                );
            }
        }
    }

    /// Recover the `K`/`V` (`Index`/`IndexSet`) and `R` (`Slice`) type args of parameterized bounds
    /// from each type parameter's inferred binding — the indexing analogue of `recover_iter_elems`,
    /// so `fn first[C: Index[int, V], V](c: C) -> V` recovers `V` from the argument.
    pub(super) fn recover_index_args(
        &mut self,
        tps: &[TyParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            let Some(concrete) = sub.get(&tp.name).cloned() else {
                continue;
            };
            for b in &tp.bounds {
                match b.name.as_str() {
                    "Index" | "IndexSet" => {
                        if let Some((k, v)) = self.index_kv(&concrete) {
                            if let Some(a) = b.args.first() {
                                binds.push((a.clone(), k));
                            }
                            if let Some(a) = b.args.get(1) {
                                binds.push((a.clone(), v));
                            }
                        }
                    }
                    "Slice" => {
                        if let Some(r) = self.slice_result(&concrete)
                            && let Some(a) = b.args.first()
                        {
                            binds.push((a.clone(), r));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (arg_ty, recovered) in &binds {
            if let Some(pinned) = self.bind_recovered(arg_ty, recovered, sub) {
                self.error(
                    span,
                    format!("index type {recovered} does not match the declared type {pinned}"),
                );
            }
        }
    }

    /// W12-9 (TICKET-106) — let an expected type WIDEN a type arg the ARGUMENTS already bound:
    /// `b: Box[Named] = Box(A())` binds `T = A` from the arg, and `seed_from_hint` (which fills only
    /// FREE params) ignored the hint. Declines unless ALL of these hold, so no accepted program's
    /// types move and no aliased mutable value is re-typed:
    ///
    /// 1. no turbofish
    /// 2. the hint is `ty_fully_concrete` (DEC-054)
    /// 3. the result as inferred is NOT already assignable to the hint
    /// 4. every re-bound param's new type accepts its old one
    /// 5. every argument is still assignable to its declared slot under the new binding (this is
    ///    the invariance guard: `Bag(zs)` with `zs: List[A]` fails it)
    /// 6. the declared bounds still hold
    #[allow(clippy::too_many_arguments)]
    pub(super) fn widen_targs_from_hint(
        &mut self,
        hint: Option<&Ty>,
        shape: &Ty,
        tps: &[TyParam],
        decls: &[Ty],
        arg_tys: &[Ty],
        explicit: bool,
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        let Some(h) = hint else { return };
        if explicit || !ty_fully_concrete(h) || self.assignable(h, &subst(shape, sub)) {
            return;
        }
        let mut want: HashMap<String, Ty> = HashMap::new();
        unify(shape, h, &mut want);
        let mut cand = sub.clone();
        let mut changed = false;
        for tp in tps {
            if let (Some(have), Some(w)) = (sub.get(&tp.name), want.get(&tp.name))
                && have != w
                && ty_fully_concrete(w)
                && self.assignable(w, have)
            {
                cand.insert(tp.name.clone(), w.clone());
                changed = true;
            }
        }
        if !changed
            || decls
                .iter()
                .zip(arg_tys)
                .any(|(d, a)| !self.assignable(&subst(d, &cand), a))
        {
            return;
        }
        let mark = self.diag_mark();
        self.enforce_bounds(tps, tps, &cand, span);
        let bounds_fail = self.errors.len() > mark.errors;
        self.diag_rollback(mark);
        if !bounds_fail {
            *sub = cand;
        }
    }

    /// The parameterized bounds whose type args are recovered by a dedicated extractor above
    /// (`recover_iter_elems` / `recover_index_args`). They read the arg straight off the concrete
    /// type (`iter_elem`, `index_kv`, `slice_result`) instead of unifying method signatures, so the
    /// general recovery below must not re-derive them.
    fn has_direct_arg_recovery(name: &str) -> bool {
        matches!(
            name,
            "Iterator" | "Iterable" | "Index" | "IndexSet" | "Slice"
        )
    }

    /// The structural method table a NOMINAL user type witnesses a protocol with — the same three
    /// tables `satisfies_args_d`'s struct/enum arms feed to `satisfies_methods`, including
    /// their miss-only identity-key fallbacks (gap #4). A builtin/native or existential witness
    /// returns `None`: those satisfy through `satisfies_native` / `protocol_method_sig`, whose
    /// signatures are not the user method table this recovery unifies against. It is also the
    /// methods a struct/enum DECLARES (`None` for anything else), for diagnostics that ask "did the
    /// user write this method at all?", which conformance alone can't answer.
    pub(super) fn nominal_method_table(&self, ty: &Ty) -> Option<&HashMap<String, FnSig>> {
        match ty {
            Ty::Struct(k, _) => self.struct_shape(k).map(|i| &i.methods),
            Ty::Enum(k, _) => self.enum_methods_of(k),
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
            | Ty::BuiltinFn { .. }
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

    /// Recover a user protocol's OWN type args from a concrete witnessing type, by unifying each
    /// requirement signature against the actual method that witnesses it. Returns one slot per
    /// protocol type param, in declaration order; `None` where nothing pinned it.
    ///
    /// Unification is one-directional — only the REQUIREMENT side carries `Ty::Param`, so nothing
    /// the user wrote can be rewritten, and a genuinely non-conforming method recovers nothing (an
    /// arity or static-ness mismatch skips the method entirely, exactly as `method_matches` would
    /// reject it). `Self` is bound to the candidate and the receiving type's own params are
    /// substituted into the actual, mirroring `satisfies_methods` so the two agree on what "matches".
    fn recover_bound_args_from(&self, ty: &Ty, protocol: &str) -> Option<Vec<Option<Ty>>> {
        let pinfo = self.protocol_shape(protocol)?;
        if pinfo.type_params.is_empty() {
            return None;
        }
        // A BUILTIN witness (`List`/`Map`/`Set`/`str`/…) has no nominal method table — it conforms
        // through `satisfies_native`, whose table is built inline from `self.structs` with the
        // RECEIVER PREPENDED (`harvest_native_fn_sig` strips the leading bare `self`, while a
        // protocol requirement keeps it as `params[0]`). Rebuild exactly that table here, or the
        // arity test below rejects every native method by one and the recovery silently returns
        // nothing: measured, `take([1, 2, 3])` against `protocol Popper[R]: fn pop(self) -> R` gave
        // *cannot infer type parameter R for 'take'* where the struct-witness twin infers it.
        let native_table;
        let proto_table;
        let methods = match self.nominal_method_table(ty) {
            Some(m) => m,
            // A protocol EXISTENTIAL witnesses out of the protocol's own requirement signatures —
            // the recipe `satisfies_methods`'s `Ty::Protocol` arm already uses. Without this a value
            // annotated with the bound's own protocol recovered nothing: measured,
            // `p: Produces[int] = IntProducer()` then `produce_as(p)` gave *cannot infer type
            // parameter R for 'produce_as'* while the annotated `v: int = produce_as(p)` ran.
            // `protocol_method_sig` walks embeds and re-spells them, so an existential of an
            // EMBEDDING protocol witnesses here too.
            None if matches!(ty, Ty::Protocol(..)) => {
                let Ty::Protocol(pname, _) = ty else {
                    return None;
                };
                proto_table = self
                    .protocol_method_names(pname)
                    .into_iter()
                    .filter_map(|m| Some((m.clone(), self.protocol_method_sig(pname, &m)?)))
                    .collect::<HashMap<String, FnSig>>();
                &proto_table
            }
            None => {
                let key = Self::native_witness_key(ty)?;
                let native = self.structs.get(key).map(|i| &i.methods)?;
                native_table = native
                    .iter()
                    .filter(|(mname, sig)| {
                        // Same two refusals `satisfies_native` makes: a generic native method cannot
                        // witness at all, and one whose dispatch cannot reach this receiver is not a
                        // witness either. Both SKIP here rather than erroring — this is a recovery,
                        // and the conformance check reports for real a few lines later.
                        sig.type_params.is_empty()
                            && self.native_dispatch_residual(ty, mname).is_none()
                    })
                    .map(|(mname, sig)| {
                        let mut params = Vec::with_capacity(sig.params.len() + 1);
                        params.push(ty.clone());
                        params.extend(sig.params.iter().cloned());
                        (
                            mname.clone(),
                            FnSig {
                                params,
                                min_params: sig.min_params + 1,
                                ..sig.clone()
                            },
                        )
                    })
                    .collect::<HashMap<String, FnSig>>();
                &native_table
            }
        };
        // An existential's own params bind to its carried args, and its sigs spell `Self` exactly as
        // the requirement's do — so `Self` goes in the ACTUAL side's map too, mirroring
        // `satisfies_methods`. `nominal_param_map` returns an empty map for `Ty::Protocol`.
        let mut tymap = self.nominal_param_map(ty);
        if let Ty::Protocol(pname, targs) = ty {
            if let Some(pi) = self.protocol_shape(pname) {
                for (n, a) in pi.type_params.iter().zip(targs) {
                    tymap.insert(n.clone(), a.clone());
                }
            }
            tymap.insert("Self".to_string(), ty.clone());
        }
        let selfmap = HashMap::from([("Self".to_string(), ty.clone())]);
        let mut pmap: HashMap<String, Ty> = HashMap::new();
        // Own methods AND those pulled in through an EMBED, transitively. `pinfo.methods` alone
        // never walked `pinfo.embeds`, so a param supplied by an embedded protocol recovered
        // nothing: measured, `protocol Q[R]:` + an embed line `Produces[R]`, used as `[T: Q[R]]`,
        // gave *cannot infer type parameter R for 'use_q'*. `protocol_method_names` /
        // `protocol_method_sig` are the existing pair for this — cycle-guarded, and the sig comes
        // back RE-SPELLED in this protocol's own type-param vocabulary (the embed args resolved at the declaration), so
        // `Q[S]: Produces[S]` binds `S`, not the embedded protocol's `R`.
        for mname in self.protocol_method_names(protocol) {
            let Some(msig) = self.protocol_method_sig(protocol, &mname) else {
                continue;
            };
            let Some(actual) = methods.get(&mname) else {
                continue;
            };
            if msig.params.len() != actual.params.len() || msig.is_static != actual.is_static {
                continue;
            }
            // A GENERIC witness method must recover nothing: its signature is spelled in terms of
            // its OWN type params, which exist in no scope the caller can see, so binding one into
            // the call's substitution is name capture — `fn produce[U](self) -> List[U]` witnessing
            // `Produces[R]` would pin `R = List[U]`, and that `U` then accidentally matches (or
            // fails to match) whatever the CALLER happens to have named `U`. Measured pre-fix:
            // renaming the struct's own `U` to `W` flipped the program from compiling to
            // `expected List[U], found List[W]` — alpha-renaming must never change meaning.
            // `satisfies_native` already refuses a generic method as a witness outright
            // (proto.rs, "native method '…' is generic and cannot witness a protocol requirement");
            // this is the same policy on the recovery side.
            if !actual.type_params.is_empty() {
                continue;
            }
            for (p, a) in msig.params.iter().zip(&actual.params) {
                unify(&subst(p, &selfmap), &subst(a, &tymap), &mut pmap);
            }
            unify(
                &subst(&msig.ret, &selfmap),
                &subst(&actual.ret, &tymap),
                &mut pmap,
            );
        }
        Some(
            pinfo
                .type_params
                .iter()
                .map(|n| pmap.get(n).cloned())
                .collect(),
        )
    }

    /// Recover a USER protocol bound's type args from the type parameter's inferred binding — the
    /// general case of `recover_iter_elems`/`recover_index_args`, which hardcode `Iterator`/`Index`
    /// and read their arg off the concrete type directly.
    ///
    /// `fn produce_as[R, T: Produces[R]](x: T) -> R` binds `T = IntProducer` from the argument, and
    /// `R` then falls out of `IntProducer.produce`'s own return type. Rust infers the identical
    /// program (`impl Produces<i32> for IntProducer`), and gives up only on a genuine multi-impl
    /// ambiguity (`E0283`) that Chezzi cannot have: conformance here is STRUCTURAL, so a type has
    /// exactly one method of a given name and therefore exactly one witnessing signature. Without
    /// this, `enforce_bounds` compared against `Produces[R]` with `R` still free and blamed the
    /// perfectly valid `produce` for having "the wrong signature".
    ///
    /// Only a param still FREE is bound, so turbofish and argument unification keep precedence. A
    /// recovered type that DISAGREES with an existing binding is deliberately dropped rather than
    /// reported: the existing binding is the one the user wrote, and `enforce_bounds` below still
    /// rejects it with its own message.
    pub(super) fn recover_protocol_args(&mut self, tps: &[TyParam], sub: &mut HashMap<String, Ty>) {
        // Speculative: it only decides what to bind. The bound args were resolved silently at the
        // declaration (TICKET-202), so nothing here reports.
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            let Some(concrete) = sub.get(&tp.name).cloned() else {
                continue;
            };
            for b in &tp.bounds {
                if b.args.is_empty() || Self::has_direct_arg_recovery(&b.name) {
                    continue;
                }
                let Some(recovered) = self.recover_bound_args_from(&concrete, &b.name) else {
                    continue;
                };
                for (arg, rec) in b.args.iter().zip(recovered) {
                    let Some(rec) = rec else {
                        continue;
                    };
                    binds.push((arg.clone(), rec));
                }
            }
        }
        for (arg_ty, recovered) in binds {
            // Bind only a still-free param, and never launder `Unknown` into one (a residual Unknown
            // in a type param is a type-check bypass). A pinned param keeps what pinned it.
            if let Ty::Param(n) = arg_ty
                && !recovered.is_unknown()
                && !sub.contains_key(&n)
            {
                sub.insert(n, recovered);
            }
        }
    }

    /// Enforce each type parameter's declared protocol bounds against its inferred binding. A
    /// parameterized bound (`Container[int]`) carries args resolved at the declaration (TICKET-202);
    /// they are substituted here and checked structurally with the protocol's params substituted.
    /// `owner` lists every param the declaring item binds: one the call left unbound becomes
    /// `Unknown`, never a leftover `Ty::Param`, which would equal a same-named caller param.
    pub(super) fn enforce_bounds(
        &mut self,
        tps: &[TyParam],
        owner: &[TyParam],
        sub: &HashMap<String, Ty>,
        span: Span,
    ) {
        // TICKET-225: a binding that still holds a type variable is judged when its frame closes.
        if tps
            .iter()
            .any(|tp| sub.get(&tp.name).is_some_and(super::tyvar::has_var))
        {
            self.defer_bound(super::tyvar::DeferredBound {
                params: tps.to_vec(),
                owner: owner.to_vec(),
                map: sub.clone(),
                span,
            });
            return;
        }
        let mut full = sub.clone();
        for tp in tps.iter().chain(owner) {
            full.entry(tp.name.clone()).or_insert(Ty::Unknown);
        }
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name) {
                // OBJECT SAFETY — a protocol EXISTENTIAL may not witness a type param whose bound
                // requires a `Self`-parameterized method. Two slots of the SAME param could then
                // hold two different witnesses: `sum2[T: Vecish](a: T, b: T)` fed two `Vecish`
                // values type-checks `a.add(b)` against `T`, then hands a `W` to `V::add`.
                //
                // The guard lives HERE, on the bound path, NOT in `satisfies_args_d` — down there it
                // cannot tell a bound from a plain annotation, and plain `fn takes(p: Vecish)` fed a
                // `Vecish` value is sound (nothing pairs two witnesses). Placed there it rejected
                // `expected Vecish, found Vecish`.
                if let Ty::Protocol(p, _) = concrete
                    && let Some(m) = self.protocol_self_param_method(p)
                {
                    self.error(
                        span,
                        format!(
                            "cannot use the protocol value {concrete} as type parameter '{}' — it \
                             requires '{m}', which takes `Self`, and a protocol value erases which \
                             type it holds, so two of them need not be the same type. Pass the \
                             concrete type instead",
                            tp.name
                        ),
                    );
                    continue;
                }
                for bound in &tp.bounds {
                    // Substitute any params recovered into `sub` (e.g. `Index[int, V]` with `V`
                    // recovered to `int`) so the structural/intrinsic check sees concrete args.
                    let bargs: Vec<Ty> = bound.args.iter().map(|a| subst(a, &full)).collect();
                    if let Err(msg) = self.satisfies_args(concrete, &bound.name, &bargs) {
                        self.error(span, msg);
                    }
                }
            }
        }
    }

    /// M24 Task 5 — is `t` a type parameter of the type whose body we are checking (`struct Bx[T]`),
    /// rather than one the MEMBER declares? Such a param can never be witnessed by this mechanism
    /// (see the diagnostic in [`Self::infer_witness_static_call`]). Returns the host type's display
    /// name. `None` outside a method body, or when the enclosing type does not declare `t`.
    pub(super) fn enclosing_type_declaring(&self, t: &str) -> Option<String> {
        let key = match self.current_self_ty.as_ref()? {
            Ty::Struct(k, _) | Ty::Enum(k, _) => k,
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
            | Ty::BuiltinFn { .. }
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
            | Ty::Var(_) => return None,
        };
        let tps = self
            .struct_shape(key)
            .map(|s| &s.type_params)
            .or_else(|| self.enum_type_params_of(key))?;
        tps.iter()
            .any(|tp| tp.name == t)
            .then(|| crate::compiler::bare_display(key))
    }

    /// M24-3 — **THE shadowing rule, stated once: an in-scope generic type parameter shadows a type
    /// of the same name in EVERY type-name position.** `fn f[Item: Tagged](x: Item)` next to a
    /// `struct Item` means the PARAMETER — in the annotation `x: Item`, in `Item.tag()`, in
    /// `Item[int].tag()`, in `Item(99)`, in `Item.Red` — and, since Chezzi has ONE namespace, in a
    /// same-named FUNCTION call `foo()`. **Go** is the reference for the scoping (one namespace too)
    /// and rejects every one of these, measured 2026-08-10: `func fv[foo any](x foo) int { return
    /// foo() }` → *"missing argument in conversion to foo"*, `Item{}` under `[Item any]` → *"invalid
    /// composite literal type Item"*. rustc agrees on the type-namespace shapes (E0109 on
    /// `Item::<i32>::tag()`, E0574 on `P { a: 3 }`, E0599 on `Col::Red`) and differs only where its
    /// separate VALUE namespace keeps `Item(99)` as the tuple-struct constructor.
    ///
    /// Round 3 applied it in ONE spelling, and the same name then meant two different things inside
    /// one expression (`Item.tag() * 100 + Item[int].tag()` answered `107`). Every type-name position
    /// asks THIS predicate; what you may then DO with the parameter is a separate question, answered
    /// by [`Self::infer_witness_static_call`] (a bound's static requirement plus a reachable witness)
    /// and by [`Self::type_param_shadow_error`] everywhere else.
    ///
    /// A real local binding still wins over both — that is the ordinary value/type split, not this
    /// rule.
    pub(super) fn shadowing_type_param(&self, name: &str) -> bool {
        matches!(self.head_binding(name), HeadBinding::TypeParam)
    }

    /// The one answer to "does a `Ty::Param` named `n` stand as a concrete type here": an in-scope
    /// type param ([`Self::shadowing_type_param`]) that is not one of `call_free`, the still-unbound
    /// params of the call being inferred (a HOF callee's free `U` spelled like the caller's `U`).
    pub(super) fn rigid_param(&self, n: &str, call_free: &[String]) -> bool {
        self.shadowing_type_param(n) && !call_free.iter().any(|f| f == n)
    }

    /// The dead end that [`Self::shadowing_type_param`] leads to in every position except the
    /// static-witness call: say that the name resolved to the TYPE PARAMETER and why that is a dead
    /// end here. Never prescribe a bound — a bound licenses a *static method*, not a constructor, a
    /// type argument or a variant.
    ///
    /// The shadowed declaration is not necessarily a type: Chezzi has ONE namespace, so a type
    /// parameter also shadows a same-named FUNCTION for the whole body. Go — the one-namespace
    /// ancestor — rejects exactly the same two shapes (measured 2026-08-10: `func fv[foo any](x foo)
    /// int { return foo() }` → *"missing argument in conversion to foo"*, `func fc[Item any](x Item)
    /// int { y := Item{}; … }` → *"invalid composite literal type Item"*). Rust splits the type and
    /// value namespaces and so keeps `Item(99)` as the tuple-struct constructor, which is why the
    /// wording below claims nothing about which kind of declaration was shadowed.
    pub(super) fn type_param_shadow_error(&mut self, tname: &str, detail: &str, span: Span) -> Ty {
        self.error(
            span,
            format!(
                "'{tname}' resolves to the generic type parameter '{tname}' here, not to the same-named declaration outside it (a type parameter shadows that name for the whole body, in every position) — {detail}. Rename the type parameter if you meant the outer '{tname}'"
            ),
        );
        Ty::Unknown
    }

    /// The first bound of type param `tname` whose protocol declares `method` with a sig `pick`
    /// accepts, that sig, and its map: `Self` ↦ `Ty::Param(tname)` (the body is still checked
    /// abstractly) plus each protocol param ↦ the bound's arg (`Convert[int]` ⇒ `S ↦ int`). The one
    /// bound search, read by the static-witness call, a bound-param receiver's method call and
    /// the path value `T.m`.
    pub(super) fn bound_method(
        &self,
        tname: &str,
        method: &str,
        pick: &dyn Fn(&FnSig) -> bool,
    ) -> Option<(TyBound, FnSig, HashMap<String, Ty>)> {
        let bounds = self.type_params.get(tname)?;
        let (bound, msig) = bounds.iter().find_map(|b| {
            self.protocol_method_sig(&b.name, method)
                .filter(|s| pick(s))
                .map(|s| (b.clone(), s))
        })?;
        let mut map = HashMap::from([("Self".to_string(), Ty::Param(tname.to_string()))]);
        let ptps = self
            .protocol_shape(&bound.name)
            .map(|p| p.type_params.clone())
            .unwrap_or_default();
        for (pn, parg) in ptps.iter().zip(&bound.args) {
            map.insert(pn.clone(), parg.clone());
        }
        Some((bound, msig, map))
    }

    /// The return of a bound's method (sig `msig`, map `map`): the requirement's declared return
    /// with the bound's arguments substituted in.
    pub(super) fn bound_method_ret(&self, msig: &FnSig, map: &HashMap<String, Ty>) -> Ty {
        subst(&msig.ret, map)
    }
}
