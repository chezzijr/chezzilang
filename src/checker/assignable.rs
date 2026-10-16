//! Checker: hashable keys, `assignable`, `may_be_equal`, builtin `in` verdicts, wrap and return-
//! coerce records.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::*;
use std::collections::HashMap;

impl Checker {
    /// Why `t` may not be a `map` key / `set` element — as the diagnostic's TAIL, so each of the nine
    /// call sites keeps its own prefix (`"Map key type "`, `"map key type "`, `"Set element type "`,
    /// `"set element type "`). `None` = it may. `Unknown` is tolerated (no cascade).
    ///
    /// The two TYPE-position sites (`sig.rs`, `Map[..]` / `Set[..]`) store the bare reason in
    /// `pending_key_reject`; the four literal/comprehension sites (`pattern.rs`) skip their report
    /// when it carries the identical reason, so one bad annotation prints once. The prefixes differ
    /// only in capitalization, so the comparison is on the bare reason, never the message.
    ///
    /// **Two obligations, not one (W7-45).** `Hashable` — the scalars `int`/`str`/`bool`
    /// intrinsically, a tuple whose every element is `Hashable` (TICKET-161), or a struct/enum
    /// defining `hash(self) -> int`; `float` is refused
    /// (NaN/equality footgun). AND `Eq`: a probe compares candidates with `values_equal` on a hash
    /// COLLISION, so a key whose `eq` carries `where` bounds this instantiation does not satisfy
    /// check-cleaned and then faulted — and worse, only *sometimes*, since with distinct hashes no
    /// `eq` runs and the program printed a silent wrong answer at rc=0. `Hashable` does not embed
    /// `Eq` (`embeds: Vec::new()`), so it has to be asked separately.
    ///
    /// **The second conjunct calls [`Self::eq_bounds_unsatisfied`] directly rather than
    /// `self.satisfies(t, "Eq")`, even though the two now agree on every verdict** (a fixed C1,
    /// W7-53 follow-up review: `satisfies(_, "Eq")` used to WRONGLY refuse a type whose only `eq` is
    /// the NON-hook ordinary-method escape hatch — see [`Self::eq_sig_is_hook`] — which made
    /// `key_ty_reject`'s comment below claim this conjunct had "no collateral" from the
    /// `[K: Hashable + Eq]` fallback; that claim was FALSE at the time W7-53 shipped, since
    /// `Counter[Key]`/`ConcurrentMap[Key, V]`/`memoize1` over such a `Key` all newly rejected a
    /// working program, and it is fixed by this same follow-up). Calling `eq_bounds_unsatisfied`
    /// directly (rather than through `satisfies`) is kept anyway, for two reasons that have nothing
    /// to do with that bug: it is the one function all FOUR W7-53 gates (`==`, `in`, the `List`
    /// builtins, this one) already share, so their wording stays consistent; and it is the one that
    /// carries the cycle-guard/budget machinery `Self::enter_eq_obligation` needs for a recursive
    /// generic (`struct D { x: List[C[D]] }`) — `satisfies`'s structural fallback for a HOOK `eq`
    /// checks that method's OWN `where` bounds through a separate, simpler loop
    /// (`satisfies_methods`'s `where_bounds` walk) that was never exercised against that guard.
    ///
    /// **`Hashable` stays `embeds: Vec::new()` — the `[K: Hashable + Eq]` fallback the W7-53 brief
    /// pre-approved, kept over embedding `Eq` into `Hashable` (mirroring `Comparable`'s embed).**
    /// Embedding was re-measured as an ALTERNATIVE to fixing the `satisfies(_, "Eq")` bug above
    /// (rather than fixing it): it does not substitute for that fix — a non-hook-`eq` `Holder`
    /// that needed no `Eq` at all before (`Hashable` alone) newly breaks under the embed, `struct
    /// Holder[T]: v: T; fn hash(self) -> int: return 1; fn eq[U](self, o: U) -> bool: return true`
    /// then `Set([Holder(1)])` — because `Hashable`'s embed makes `self.satisfies(t, "Hashable")`
    /// two lines up transitively demand `Eq`, which the STILL-buggy `satisfies` refuses. `[K:
    /// Hashable + Eq]` is also the more faithful mirror of the ancestor either way (Rust's `Hash`
    /// has no `Eq` supertrait; `HashSet<T>` spells `impl<T: Eq + Hash>`, not one bound).
    ///
    /// (`Channel`/`Func` are NOT the reason — they never reach this conjunct, because the `Hashable`
    /// gate refuses them first, and a struct merely *holding* a `Channel` satisfies `Eq` fine.)
    ///
    /// Every map-key and set-element position that is SPELLED funnels through here (literal,
    /// comprehension, `Set(list)` construction, annotation, `m[k]` read and write,
    /// and thereby every `Map`/`Set`/`RwShared` method).
    ///
    /// **W7-53.** NOT erased any more: a free `Ty::Param` reaching the second conjunct
    /// (`eq_bounds_unsatisfied`, not `_erased`) is judged directly, so `fn mk[T: Hashable](x: T) ->
    /// Set[T]` that builds `Set[T]` in its own body — never spelling the element type — is now
    /// caught at ITS OWN definition instead of handing back a `Set[Cond[Tag]]` no gate ever saw
    /// (measured, was check-clean; W7-53's third instance). The `Eq` conjunct stays second because a
    /// non-`Hashable` type must keep reporting the `Hashable` text.
    pub(super) fn key_ty_reject(&self, t: &Ty) -> Option<String> {
        if self.satisfies(t, "Hashable").is_err() {
            let mut msg = format!(
                "must implement Hashable (int, str, bool, a tuple of Hashable elements, or a struct/enum defining hash(self) -> int), found {t}"
            );
            if let Some(bad) = self.unhashable_tuple_elem(t) {
                msg.push_str(&format!(": element {bad} is not Hashable"));
            }
            msg.push_str(&self.hook_name_note(t, "Hashable", "hash"));
            return Some(msg);
        }
        self.eq_bounds_unsatisfied(t)
    }

    /// TICKET-161 — the first element of tuple type `ty`, depth-first, that is not `Hashable`, so a
    /// rejection can name the offending ELEMENT the way CPython's `unhashable type: 'list'` does. A
    /// nested tuple element is searched rather than reported, so the answer is the innermost
    /// non-tuple type. `None` for a non-tuple, or a tuple whose elements all satisfy `Hashable`.
    pub(super) fn unhashable_tuple_elem(&self, ty: &Ty) -> Option<Ty> {
        let Ty::Tuple(elems) = ty else {
            return None;
        };
        elems.iter().find_map(|e| match e {
            Ty::Tuple(_) => self.unhashable_tuple_elem(e),
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
            | Ty::Var(_) => self.satisfies(e, "Hashable").is_err().then(|| e.clone()),
        })
    }

    /// Assignability with protocol-existential awareness. Like the free [`compatible`], but a
    /// concrete type is assignable to a `Protocol(P)` slot iff it satisfies `P` — which needs the
    /// protocol/struct registry, so it can't live in the context-free `compatible`. Recurses through
    /// compound types so a nested existential (the `E` in `Result[T, Error]`) is checked structurally.
    /// Strict assignability — NO int→float widening (the reverse `float`→`int` is always rejected).
    ///
    /// TICKET-225: a type variable on either side is bound by `solve` first, and the body below
    /// compares the zonked pair; a `false` answer rolls this call's bindings back.
    pub(super) fn assignable(&self, expected: &Ty, actual: &Ty) -> bool {
        if !self.tyvars.borrow().any()
            || !(super::tyvar::has_var(expected) || super::tyvar::has_var(actual))
        {
            return self.assignable_zonked(expected, actual);
        }
        let m = self.tyvars.borrow().mark();
        self.solve(expected, actual);
        let ok = self.assignable_zonked(&self.zonk(expected), &self.zonk(actual));
        if !ok {
            self.tyvars.borrow_mut().rollback(m);
        }
        ok
    }

    fn assignable_zonked(&self, expected: &Ty, actual: &Ty) -> bool {
        use Ty::*;
        match (expected, actual) {
            (Unknown, _) | (_, Unknown) => true,
            // A C width is a tag on its scalar (TICKET-218): assignability ignores it, nested too.
            (Width(_), _) => self.assignable(expected.scalar(), actual),
            (_, Width(_)) => self.assignable(expected, actual.scalar()),
            // A protocol existential slot: the actual type must satisfy the protocol WITH the carried
            // args (empty for a bare existential — reproduces the old `satisfies(a, p)`). This single
            // witness is shared by every value write-site (param/return/field/reassign) since they all
            // route assignment through `assignable`. Task 2 (option a): EVERY protocol widening now
            // requires the concrete witness `a` be sendable (Go `chan interface` parity — all protocol
            // existentials are sendable, not just `Error`). `a` already being a `Protocol(..)` (a prior
            // widening) is always sendable, so this only rejects a genuine non-sendable concrete
            // witness — in practice one reaching `Ty::Module` (near-unconstructible). A witness that
            // carries an FFI/native handle is checker-sendable (its `Ty::Func`/handle type is
            // sendable) and rejected at the RUNTIME airlock instead (`ensure_crossable`), not here.
            // TICKET-053: the erasure hands `a` the unconditional intrinsic `Eq` grant for a
            // protocol receiver (the `INTRINSIC_PROTO_METHODS` row above), so the witness's own
            // conditional `eq` must be discharged HERE — the same
            // place `satisfies_methods` (`:2510`) discharges the `Comparable` sibling's — because
            // this is the last point the concrete witness is visible before it is erased.
            (Protocol(p, pargs), a) => {
                self.satisfies_args(a, p, pargs).is_ok()
                    && self.sendable(a)
                    && self.eq_where_bounds_unsatisfied(a).is_none()
            }
            // `Option`/`Result` are IMMUTABLE carriers — covariant element assignment stays sound
            // (no write-through alias), so they keep recursing via `assignable`. `List`/`Set`/`Map`
            // and user generic `Struct`/`Enum` are MUTABLE, by-reference containers: covariant type
            // args are a soundness hole (a `G[Sub]` bound aliased as `G[Super]` can have a Super value
            // written back into it — see `invariance_rejects_*` tests). Their type ARGUMENTS are
            // therefore compared with the context-free structural-equality primitive `compatible`
            // (= strict INVARIANCE), mirroring the M14 `compatible` Protocol/Struct/Enum arms and
            // `protocol_provides`. Docs: spec.md "strictly invariant"; future.md "no covariance holes".
            (Option(e), Option(a)) => self.assignable(e, a),
            (List(e), List(a)) | (Set(e), Set(a)) => compatible(e, a),
            (Result(et, ee), Result(at, ae)) => self.assignable(et, at) && self.assignable(ee, ae),
            (Map(ek, ev), Map(ak, av)) => compatible(ek, ak) && compatible(ev, av),
            (Struct(n, ea), Struct(m, aa)) | (Enum(n, ea), Enum(m, aa)) => {
                n == m && ea.len() == aa.len() && ea.iter().zip(aa).all(|(x, y)| compatible(x, y))
            }
            (Tuple(e), Tuple(a)) => {
                e.len() == a.len() && e.iter().zip(a).all(|(x, y)| self.assignable(x, y))
            }
            // Parameter NAMES are surface-only, but the OPTIONAL ARITY riding on the same wrapper is
            // directional — see the matching arm in `compatible`. `expected` says how the slot will be
            // called, so a value requiring MORE arguments than the slot promises cannot be stored in
            // it: `h := a; h = b` over `fn a(x: int = 1)` / `fn b(x: int)` was check-clean and then
            // `function 'b' expects 1 argument(s), got 0` at runtime. The reverse
            // (a defaulted fn into a plain `fn(int) -> int`) is strictly more permissive and stays legal.
            //
            // Parameter TYPES, unlike arity, are strictly INVARIANT (TICKET-093), not covariant: a
            // covariant `self.assignable(a, b)` accepted `h: fn(Any) -> Dog = idd` over
            // `fn idd(d: Dog) -> Dog`, so `h(Cat(5))` put a `Cat` value where a `Dog` was promised. The
            // return type stays covariant.
            (
                Func {
                    params: p1,
                    ret: r1,
                    labels: l1,
                    variadic: v1,
                },
                Func {
                    params: p2,
                    ret: r2,
                    labels: l2,
                    variadic: v2,
                },
            ) => {
                p1.len() == p2.len()
                    && v1 == v2
                    && l2.min_or(p2.len()) <= l1.min_or(p1.len())
                    && p1.iter().zip(p2).all(|(a, b)| param_invariant(a, b))
                    && self.assignable(r1, r2)
            }
            _ => compatible(expected, actual),
        }
    }

    /// **CO-INHABITABILITY** — can a value of type `l` and a value of type `r` ever be the *same
    /// value* at runtime? The predicate gap B2 (`==`/`!=`) asks, and the ONLY thing it asks: a pair
    /// that answers `false` here is **provably disjoint**, so comparing it is a bug in user code
    /// (mypy `--strict-equality` / Go / Rust all reject it); everything else is accepted and answered
    /// structurally at runtime, exactly as before.
    ///
    /// It is deliberately NOT [`Checker::assignable`]. Assignability answers a *storage* question —
    /// "can a `B` be written into an `A` slot" — and carries two conjuncts equality has no use for:
    ///
    /// * **Invariance.** `assignable` compares `List`/`Set`/`Map`/generic `Struct`/`Enum` type
    ///   ARGUMENTS with the context-free `compatible`, because a `G[Sub]` aliased as `G[Super]` can be
    ///   *written through*. `==` never writes, so a `List[Error]` and a `List[MyErr]` can genuinely
    ///   hold the same list; here the arguments recurse CO-VARIANTLY.
    /// * **Sendability.** `assignable`'s `Protocol` arm ends in `&& self.sendable(a)` (the spawn-airlock
    ///   witness). Whether two values may be equal has nothing to do with whether either crosses a
    ///   thread boundary.
    ///
    /// And it adds the arm `assignable` cannot have: **two existentials co-inhabit**. `assignable`
    /// only passes `Protocol` vs `Protocol` when one embeds the other (a `Shape` slot really can't
    /// hold an arbitrary `Error`), but ONE concrete type can conform to two unrelated protocols — a
    /// `Sq` that has both `area` and `message` is simultaneously a `Shape` and an `Error` — so the
    /// pair is inhabited and `Shape == Error` must compile.
    ///
    /// An existential against a CONCRETE is still decided by conformance, so this is not a free pass:
    /// `Shape == str` stays an error because no `Shape` witness is ever a `str`.
    pub(super) fn may_be_equal(&self, l: &Ty, r: &Ty) -> bool {
        use Ty::*;
        match (l, r) {
            // A prior error, or a consumed empty literal's element — never cascade off it.
            (Unknown, _) | (_, Unknown) => true,
            // ERASED: a generic body is checked once with `T` abstract, so any concrete pairing is
            // possible at some call site. (A `where T: <scalar>` param is NOT erased — it is an
            // equality bound pinning `T` to that scalar, and `infer_binary`'s `Eq` arm substitutes
            // those pins away BEFORE calling this, so `T: str` vs `int` still rejects.)
            (Param(_), _) | (_, Param(_)) => true,
            // CEILING, deliberate: two protocols with contradictory method signatures (`fn f() -> int`
            // vs `fn f() -> str`) are inhabited by NOTHING, yet this accepts the pair. Tightening it
            // means intersecting two method tables; the rationale for not doing so lives in
            // `docs/contributing/gaps.md` §B2 — read it there before narrowing this arm.
            (Protocol(..), Protocol(..)) => true,
            (Protocol(p, pargs), a) | (a, Protocol(p, pargs)) => {
                // ERASURE, one constructor in: a free `T` in the protocol's args (`Container[T]`) or
                // in the concrete's (`Bag[T]`) is un-decidable here, exactly as a bare `T` is at the
                // arm above — conformance would compare it against a concrete arg and wrong-reject.
                // Erase params to `Ty::Unknown` and reuse `satisfies_args`' existing don't-cascade
                // leniency, so the METHOD SET is still checked (`Container[T] == int` still rejects)
                // while the arguments stop deciding anything.
                let mut names: Vec<String> = Vec::new();
                ty_collect_params(a, None, &mut names);
                for t in pargs {
                    ty_collect_params(t, None, &mut names);
                }
                if names.is_empty() {
                    return self.satisfies_args(a, p, pargs).is_ok();
                }
                let map: HashMap<String, Ty> =
                    names.into_iter().map(|n| (n, Ty::Unknown)).collect();
                let pargs: Vec<Ty> = pargs.iter().map(|t| subst(t, &map)).collect();
                self.satisfies_args(&subst(a, &map), p, &pargs).is_ok()
            }
            // The runtime's own CROSS-TYPE equality arms (`values_equal`), which is what makes these
            // pairs inhabited rather than merely assignable. They live HERE, not as a top-level
            // special case, so they compose through the recursion the same way the runtime does:
            // `[1.0, 2.0] == [1, 2]` and `{"k": 1.0} == {"k": 1}` answer `true`
            // (CPython agrees), so rejecting them would have been a lie about "provably disjoint".
            (Int, Float) | (Float, Int) | (Bytes, ByteArray) | (ByteArray, Bytes) => true,
            // The native generic HANDLES belong here too, not on the `_ => compatible` fall-through:
            // their `==` compares cores (`Obj::identity`, TICKET-177) in `values_equal_guarded`,
            // so `Channel[T] == Channel[int]` is a live, true-capable comparison. `compatible` is
            // neither `Param`-tolerant nor conformance-aware, so leaving them there wrong-rejected
            // working code (`fn cmp[T](a: Channel[T], b: Channel[int])`).
            (List(a), List(b))
            | (Set(a), Set(b))
            | (Option(a), Option(b))
            | (Channel(a), Channel(b))
            | (Shared(a), Shared(b))
            | (RwShared(a), RwShared(b))
            | (Atomic(a), Atomic(b)) => self.may_be_equal(a, b),
            (Map(ak, av), Map(bk, bv)) => self.may_be_equal(ak, bk) && self.may_be_equal(av, bv),
            (Result(at, ae), Result(bt, be)) => {
                self.may_be_equal(at, bt) && self.may_be_equal(ae, be)
            }
            (Tuple(a), Tuple(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| self.may_be_equal(x, y))
            }
            (Struct(n, a), Struct(m, b)) | (Enum(n, a), Enum(m, b)) => {
                n == m
                    && a.len() == b.len()
                    && a.iter().zip(b).all(|(x, y)| self.may_be_equal(x, y))
            }
            (
                Func {
                    params: p1,
                    ret: r1,
                    ..
                },
                Func {
                    params: p2,
                    ret: r2,
                    ..
                },
            ) => {
                p1.len() == p2.len()
                    && p1.iter().zip(p2).all(|(a, b)| self.may_be_equal(a, b))
                    && self.may_be_equal(r1, r2)
            }
            // Everything else (scalars, the concurrency/IO handles, `Module`) is nominal: two values
            // co-inhabit iff the types are structurally the same, which is exactly the runtime's own
            // type-tag guard ("distinct types are never equal"). Unchanged from `assignable`'s `_`.
            _ => self.join_ty(l, r),
        }
    }

    /// TICKET-257 (owner rule U5) — the ONE needle rule for a builtin container's `Contains`: whether
    /// `item in ty` holds. `x in c`, the `Contains[X]` bound and `c.contains(x)` all read it.
    /// `None` = not a builtin container (a user type, a param, a protocol: those go through the
    /// `contains` hook). `Some(Err(Some(why)))` = the needle fits but the element has an unmet `Eq`
    /// where-bound.
    ///
    /// One rule per container kind:
    /// - List compares by `==`: `1 in [1.0]` holds because `1 == 1.0` does (CPython `True`), and a
    ///   `T` needle fits as `x == e` does.
    /// - Set and Map (its keys) use the `==` rule and also require the needle to be `Hashable`: the
    ///   probe hashes it (CPython: `[1] in {1}` raises `TypeError`; `float` is not `Hashable`).
    /// - str, bytes and bytearray take exact needle types: str a substring (a str), bytes and
    ///   bytearray an int byte or a bytes/bytearray subsequence (CPython raises `TypeError` for
    ///   `1.0 in b"a"`).
    pub(super) fn builtin_contains_verdict(
        &self,
        ty: &Ty,
        item: &Ty,
    ) -> Option<Result<(), Option<String>>> {
        let fits = |elem: &Ty| {
            self.join_ty(elem, item) || self.may_be_equal(item, elem) || self.assignable(elem, item)
        };
        match ty {
            Ty::List(e) => Some(if !fits(e) {
                Err(None)
            } else if let Some(why) = self.eq_bounds_unsatisfied(e) {
                Err(Some(why))
            } else {
                Ok(())
            }),
            // A Set or Map probe hashes the needle at run time, so the needle must be `Hashable`
            // (CPython: `[1] in {1}` raises `TypeError: unhashable`). One check, `satisfies`.
            Ty::Set(e) => Some(if fits(e) && self.satisfies(item, "Hashable").is_ok() {
                Ok(())
            } else {
                Err(None)
            }),
            Ty::Map(k, _) => Some(if fits(k) && self.satisfies(item, "Hashable").is_ok() {
                Ok(())
            } else {
                Err(None)
            }),
            // str `in` is a substring test, not `==`: the needle is a str exactly.
            Ty::Str => Some(match item {
                Ty::Str | Ty::Unknown => Ok(()),
                Ty::Int
                | Ty::Float
                | Ty::Bool
                | Ty::Bytes
                | Ty::ByteArray
                | Ty::Nil
                | Ty::List(_)
                | Ty::Map(..)
                | Ty::Set(_)
                | Ty::Func { .. }
                | Ty::BuiltinFn { .. }
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
                | Ty::Var(_) => Err(None),
            }),
            Ty::Bytes | Ty::ByteArray => Some(match item {
                Ty::Int | Ty::Bytes | Ty::ByteArray | Ty::Unknown => Ok(()),
                Ty::Param(_)
                | Ty::Float
                | Ty::Bool
                | Ty::Str
                | Ty::Nil
                | Ty::List(_)
                | Ty::Map(..)
                | Ty::Set(_)
                | Ty::Func { .. }
                | Ty::BuiltinFn { .. }
                | Ty::Tuple(_)
                | Ty::Struct(..)
                | Ty::Enum(..)
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
                | Ty::Var(_) => Err(None),
            }),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Nil
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Option(_)
            | Ty::Result(..)
            | Ty::Func { .. }
            | Ty::BuiltinFn { .. }
            | Ty::Protocol(..)
            | Ty::Param(_)
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

    /// W8-21 / TICKET-227 (D3) — which implicit wrap (if any) a plain value of type `ty` gets at a
    /// typed slot of type `ret`. `None` means "no wrap" — the slot keeps its existing `assignable`
    /// diagnostic unchanged. Called only by [`Self::meet_slot`].
    ///
    /// Order matters: `assignable` first (rule a — an already-legal value is never wrapped), then
    /// "already a carrier" (never re-wrap `Option[Option[T]]`/`Result[Option[T],E]`), then
    /// `ty_fully_concrete(ret)` (rule c — a generic slot `T?` declines). The wrap arms use plain
    /// `assignable` (rule b — no chaining onto int→float: `float?: return 1` must keep erroring).
    pub(super) fn wrap_mode(&self, ret: &Ty, ty: &Ty) -> Option<crate::checker::Wrap> {
        if self.assignable(ret, ty) {
            return None;
        }
        if matches!(ty, Ty::Option(_) | Ty::Result(..)) {
            return None;
        }
        if !crate::checker::ty_fully_concrete(ret) {
            return None;
        }
        match ret.carrier_parts() {
            Some((w, payload, _)) if self.assignable(payload, ty) => Some(w),
            _ => None,
        }
    }

    /// TICKET-227 — record one implicit wrap of the value node `node` into
    /// [`crate::checker::WrapTable`]. Nothing for a synthesized node, the generic-arg prepass or
    /// the return-inference walk (the main walk records).
    pub(super) fn record_wrap(
        &mut self,
        node: crate::ast::NodeId,
        w: crate::checker::Wrap,
        span: Span,
    ) {
        if !self.records_node(node) {
            return;
        }
        crate::checker::record_call_table_entry(
            &mut self.wraps,
            &mut self.table_conflicts,
            (self.graph_module_idx, node.0),
            w,
            "implicit wrap",
            span,
        );
    }

    /// W8-21 — the bare-`return`-at-`Result[nil, E]` case: DEC-017's zero-arg `Ok()`. `None` for every
    /// other declared sink, including `Option[nil]` (there is no zero-arg `Some()`, so a bare `return`
    /// there keeps its existing `expected a return value of type Option[nil]` error).
    pub(super) fn ret_coerce_bare(&self, ret: &Ty) -> Option<crate::checker::RetCoerce> {
        if let Ty::Result(t, _) = ret
            && **t == Ty::Nil
            && crate::checker::ty_fully_concrete(ret)
        {
            return Some(crate::checker::RetCoerce::WrapOkNil);
        }
        None
    }

    /// W8-21 — record one return-sink coercion verdict into [`crate::checker::RetCoerceTable`],
    /// keyed on `span` (the returned value's own span). `mode: None` records `NoWrap`, deliberately
    /// identical to a lookup miss (see [`crate::checker::RetCoerce`]).
    pub(super) fn record_ret_coerce(
        &mut self,
        span: Span,
        mode: Option<crate::checker::RetCoerce>,
    ) {
        let key = crate::checker::ret_coerce_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            span,
        );
        crate::checker::record_call_table_entry(
            &mut self.ret_coerce,
            &mut self.table_conflicts,
            key,
            mode.unwrap_or(crate::checker::RetCoerce::NoWrap),
            "return-coercion",
            span,
        );
    }
}
