//! Checker: builtin arithmetic kinds, operator-overload results, ordering, sum seeds, `Atomic`
//! payload equality.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::eq::EqReach;
use super::*;

impl Checker {
    /// Does builtin type `ty` satisfy the arithmetic operator protocol `protocol`? `infer_binary`
    /// reaches it through [`Self::op_overload_result`] and a `[T: Add]` bound through
    /// `satisfies_args_d`. Derived: an arithmetic hook (`Hook::is_arithmetic`) whose
    /// `(protocol, receiver kind)` row is in `INTRINSIC_PROTO_METHODS`, the one builtin grant table.
    /// A width type reads as its scalar here, as everywhere a width value is read (TICKET-262).
    pub(super) fn builtin_arith_kind(protocol: &str, ty: &Ty) -> bool {
        let arithmetic = HOOKS
            .iter()
            .any(|row| row.protocol == protocol && row.hook.is_arithmetic());
        let Some(kind) = Self::intrinsic_recv_kind(ty) else {
            return false;
        };
        arithmetic
            && super::proto::INTRINSIC_PROTO_METHODS
                .iter()
                .any(|(p, _, k)| *p == protocol && *k == kind)
    }

    /// Result type of an overloaded arithmetic operator (`+`/`-`/`*`) on two operands of the *same*
    /// struct or type-parameter that satisfies `protocol` (`Add`/`Sub`/`Mul`). The runtime dispatches
    /// to the `add`/`sub`/`mul` method; the result type is that same type. `None` ⇒ not overloadable.
    pub(super) fn op_overload_result(&self, l: &Ty, r: &Ty, protocol: &str) -> Option<Ty> {
        // SAME generic type REQUIRES matching type ARGS, not just the same name. The user's operator
        // method `add(self, o: Box[T]) -> Box[T]` on a `Box[int]` receiver needs `o: Box[int]`, so a
        // heterogeneous pair like `Box[int] + Box[str]` must NOT overload — admitting it would infer
        // the result `Box[int]` for a value carrying a `Box[str]` (the returned type the checker can't
        // honor → runtime type confusion). `compatible` checks name + pairwise-compatible targs; an
        // `Unknown` targ on a partially-inferred side still unifies (no false rejection there).
        let joined: Option<Ty> = match (l, r) {
            (Ty::Struct(..), Ty::Struct(..)) | (Ty::Enum(..), Ty::Enum(..)) => {
                self.join_ty(l, r).then(|| l.clone())
            }
            (Ty::Param(a), Ty::Param(b)) => (a == b).then(|| l.clone()),
            // Builtin containers: two operands of one kind are joined (an empty `[]` side takes the
            // other's element, `[] + [1]` is `List[int]`) and the joined type is asked the protocol.
            (Ty::Str, Ty::Str) | (Ty::Bytes, Ty::Bytes) | (Ty::ByteArray, Ty::ByteArray) => {
                Some(l.clone())
            }
            (Ty::List(le), Ty::List(re)) => self.join_fill(le, re).map(|m| Ty::List(Box::new(m))),
            (Ty::Set(le), Ty::Set(re)) => self.join_fill(le, re).map(|m| Ty::Set(Box::new(m))),
            // NOT `(Ty::Protocol, Ty::Protocol)`: every operator protocol's method is
            // `(self, Self) -> Self`, and two values of one protocol need not hold the same witness
            // (`Vecish + Vecish` over a `V` and a `W`), so the pair is un-dispatchable by object
            // safety — see `self_in_param_position`. Bind the operands together with a generic
            // parameter (`[T: Vecish](a: T, b: T)`) and this works, soundly.
            _ => None,
        };
        joined.filter(|t| self.satisfies(t, protocol).is_ok())
    }

    /// Are `l < r` etc. allowed? True for same-named comparable type params, or same-named structs
    /// that satisfy `Comparable` (operator overloading dispatches to their `compare` at runtime),
    /// or a tuple / `List` / `Option` pair whose element types are all Comparable (TICKET-146).
    pub(super) fn ordering_allowed(&self, l: &Ty, r: &Ty) -> bool {
        self.cmp_overload_allowed(l, r, "Comparable", "compare")
    }

    /// Shared body of [`Self::ordering_allowed`]: do `l` and `r` name the
    /// SAME type param / struct / enum, such that the comparison operator dispatches to
    /// `protocol`'s `method`?
    ///
    /// Still protocol/method parameterized (the `op_overload_result` precedent) even with one caller:
    /// `==`'s twin predicate was deleted in M23 Task 3 because the `Eq` overload is decided at RUNTIME
    /// off the operand's heap tag (`Vm::user_eq_method`) and never asked here — `infer_binary`'s `Eq`
    /// arm only needs the legality question (`may_be_equal`), which already accepts every pair this
    /// would have.
    fn cmp_overload_allowed(&self, l: &Ty, r: &Ty, protocol: &str, method: &str) -> bool {
        match (l, r) {
            (Ty::Param(a), Ty::Param(b)) if a == b => self.type_params.get(a).is_some_and(|bs| {
                bs.iter()
                    .any(|proto| self.protocol_has_method(&proto.name, method))
            }),
            // Same generic struct/enum REQUIRES matching type ARGS (`compatible` = name + targs), not
            // just the same name — `Box[int] < Box[str]` must not overload `compare` (same
            // heterogeneous laundering as `+`; see `op_overload_result`).
            (Ty::Struct(..), Ty::Struct(..)) | (Ty::Enum(..), Ty::Enum(..))
                if self.join_ty(l, r) =>
            {
                self.satisfies(l, protocol).is_ok()
            }
            // TICKET-257: two str / bytes / bytearray operands ask `Comparable` like any pair of one
            // kind. The mixed `bytes < bytearray` pair has no `Self`-typed protocol form and stays an
            // operator form (CPython orders it; so does `compare_op`).
            (Ty::Str, Ty::Str) | (Ty::Bytes, Ty::Bytes) | (Ty::ByteArray, Ty::ByteArray) => {
                self.satisfies(l, protocol).is_ok()
            }
            (Ty::Bytes, Ty::ByteArray) | (Ty::ByteArray, Ty::Bytes) => true,
            // TICKET-146: a tuple / `List` pair orders lexicographically when every element type is
            // Comparable; `compatible` keeps `(int, int) < (int, str)` out.
            (Ty::Tuple(_), Ty::Tuple(_)) | (Ty::List(_), Ty::List(_)) if self.join_ty(l, r) => {
                self.satisfies(l, protocol).is_ok()
            }
            // No `(Ty::Protocol, Ty::Protocol)` arm — `Comparable.compare(self, o: Self)` (and
            // `Eq.eq(self, o: Self)`) is `Self`-parameterized, so two values of one protocol are
            // un-orderable/un-dispatchable for the same object-safety reason `+` is (see
            // `op_overload_result`).
            _ => false,
        }
    }

    /// The seed a `List[T].sum()` needs: `Some(SumSeed::Float)` for a `List[float]`, `None` for a
    /// plain `List[int]`.
    pub(super) fn sum_seed(&self, elem: &Ty) -> Option<SumSeed> {
        matches!(elem.scalar(), Ty::Float).then_some(SumSeed::Float)
    }

    /// Record one `.sum()` site's seed for the backend, under the same key derivation
    /// [`Self::record_proto_eq`] uses (the method-NAME token — see [`crate::checker::CarrierKey`]).
    pub(super) fn record_sum_seed(&mut self, name_span: Span, seed: Option<SumSeed>, span: Span) {
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        );
        crate::checker::record_call_table_entry(
            &mut self.sum_seeds,
            &mut self.table_conflicts,
            key,
            seed,
            "'.sum()' element",
            span,
        );
    }

    /// Own methods OR anything an embed requires (M22) — so `protocol Ord2: Comparable` makes
    /// `a < b` legal on an `Ord2`-bounded param, exactly as declaring `compare` directly does.
    pub(super) fn protocol_has_method(&self, protocol: &str, method: &str) -> bool {
        self.protocol_method_sig(protocol, method).is_some()
    }

    /// Whether `name` is a *generic* user fn / struct / enum-variant constructor (i.e. one that can
    /// accept explicit call-site type arguments). Non-generic decls and builtins return `false`.
    /// **M23 decision 5** — an `Atomic[T]` payload may not define its own `eq`. `Atomic.cas(expected,
    /// new)` compares the stored value with the runtime's STRUCTURAL equality, and it can never route
    /// through a user `eq` (the compare happens under the box's lock, off any user frame), so an
    /// `Atomic[P]` over a type with custom equality would silently answer a DIFFERENT question than
    /// `P == P` does on the same two values. Rejected at both spellings of the type — the ctor
    /// (`Atomic(P(1))`) and the annotation (`a: Atomic[P]`). `Shared[T]` has no `cas`, so it is the
    /// escape hatch and stays unrestricted.
    ///
    /// M23 Task 3 made the claim literally true for the struct/enum arms: `P == P` now DOES route
    /// through the user `eq` while `cas` stays structural.
    ///
    /// Keyed on every type the payload REACHES, not just its own: structural equality recurses into
    /// elements, entries, tuple slots, struct fields and enum payloads, so `Atomic[List[P]]`,
    /// `Atomic[Option[P]]`, `Atomic[(int, P)]` and `Atomic[Wrapper]{p: P}` all reach `P`'s `eq` on
    /// the same compare that a bare `Atomic[P]` does. Keying on the payload's own `eq` let every one
    /// of those through the gate (`docs/contributing/gaps.md` — M23 review, CRITICAL 2).
    pub(super) fn reject_eq_atomic_payload(&mut self, elem: &Ty, span: Span) {
        if let Some(reach) = self.reaches_user_eq(elem, &mut Vec::new()) {
            let msg = match reach {
                EqReach::Param(p) => format!(
                    "Atomic[{elem}] payload '{p}' must carry the bound StructuralEq (import it from std.concurrency)"
                ),
                EqReach::Hook(inner) => {
                    let owner = if inner == elem.to_string() {
                        "payload defines its own 'eq'".to_string()
                    } else {
                        format!("payload reaches '{inner}', which defines its own 'eq'")
                    };
                    format!(
                        "Atomic[{elem}] {owner} — Atomic.cas compares stored values structurally, never through that 'eq', so cas and the payload's own equality would disagree; use Shared[{elem}] (no cas) instead"
                    )
                }
            };
            // ONCE per (payload type, SITE). `a: Atomic[P]` resolves its annotation TWICE, which
            // would report the identical message at the identical span; the span keys that pair
            // down to one. It deliberately does NOT key on the type alone: a second, genuinely
            // different `Atomic[P]` site (another statement, another module) is its own bug and
            // must be reported, or the user fixes one and only then learns about the next.
            // (`ends_with`, not `==`: `Checker::error` may prefix an `in module '<label>': `.)
            if self
                .errors
                .iter()
                .any(|e| e.span == span && e.message.ends_with(&msg))
            {
                return;
            }
            self.error(span, msg);
        }
    }
}
