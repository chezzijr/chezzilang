//! Checker: builtin arithmetic kinds, operator-overload results, ordering, sum seeds, sendability.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::eq::EqReach;
use super::proto::ty_mentions_error_existential;
use super::*;

impl Checker {
    /// Does builtin type `ty` satisfy the arithmetic operator protocol `protocol`? The one table
    /// behind `+ - * / %` and unary `-` on builtins; `infer_binary` reaches it through
    /// [`Self::op_overload_result`] and a `[T: Add]` bound reaches it through `satisfies_args_d`.
    pub(super) fn builtin_arith_kind(protocol: &str, ty: &Ty) -> bool {
        match ty {
            Ty::Int => matches!(
                protocol,
                "Add"
                    | "Sub"
                    | "Mul"
                    | "Div"
                    | "Mod"
                    | "Neg"
                    | "BitAnd"
                    | "BitOr"
                    | "BitXor"
                    | "Shl"
                    | "Shr"
            ),
            Ty::Float => matches!(protocol, "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg"),
            Ty::Str | Ty::Bytes | Ty::ByteArray | Ty::List(_) => protocol == "Add",
            Ty::Set(_) => matches!(protocol, "Sub" | "BitAnd" | "BitOr" | "BitXor"),
            Ty::Bool
            | Ty::Nil
            | Ty::Map(..)
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
            | Ty::Var(_) => false,
        }
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
            // TICKET-146: a tuple / `List` / `Option` pair orders lexicographically when every
            // element type is Comparable; `compatible` keeps `(int, int) < (int, str)` out.
            (Ty::Tuple(_), Ty::Tuple(_))
            | (Ty::List(_), Ty::List(_))
            | (Ty::Option(_), Ty::Option(_))
                if self.join_ty(l, r) =>
            {
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
    /// Can a value of this type cross a task boundary (`spawn` capture / argument, `Channel.send`)?
    /// Scalars, strings, and containers of sendable elements can; `Channel` (and `Shared`, C3)
    /// handles can. Closures/functions (bound to a heap), modules, and protocol existentials (which
    /// may wrap a closure) cannot. A struct/enum is sendable iff *all its field/payload types* are —
    /// inspected via the registry so a closure smuggled inside a struct field is caught. A generic
    /// type parameter (`Param`) is treated as sendable (the opaque-body case; concrete call sites
    /// resolve to a real type that is checked).
    pub(super) fn sendable(&self, ty: &Ty) -> bool {
        self.sendable_rec(ty, &mut Vec::new())
    }

    /// `sendable` with a cycle guard (`stack` holds the struct/enum names currently being walked,
    /// so a recursive type like `Node { next: Option[Node] }` terminates).
    pub(super) fn sendable_rec(&self, ty: &Ty, stack: &mut Vec<String>) -> bool {
        match ty {
            // `bytearray` crosses by deep copy (a fresh independent buffer on the other side, like
            // `list`) — always sendable (its elements are always `int`).
            Ty::Int
            | Ty::Width(_)
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Unknown
            | Ty::Param(_) => true,
            // A type variable is read through the store: a pending `?x` crosses iff its payload does.
            Ty::Var(_) => match self.zonk(ty) {
                Ty::Var(v) => {
                    let p = self.tyvars.borrow().present_payload(v);
                    p.is_none_or(|p| self.sendable_rec(&p, stack))
                }
                t @ Ty::Int
                | t @ Ty::Float
                | t @ Ty::Bool
                | t @ Ty::Str
                | t @ Ty::Bytes
                | t @ Ty::ByteArray
                | t @ Ty::Nil
                | t @ Ty::List(_)
                | t @ Ty::Map(..)
                | t @ Ty::Set(_)
                | t @ Ty::Func { .. }
                | t @ Ty::BuiltinFn { .. }
                | t @ Ty::Tuple(_)
                | t @ Ty::Struct(..)
                | t @ Ty::Enum(..)
                | t @ Ty::Param(_)
                | t @ Ty::Result(..)
                | t @ Ty::Option(_)
                | t @ Ty::Channel(_)
                | t @ Ty::Shared(_)
                | t @ Ty::Atomic(_)
                | t @ Ty::AtomicInt
                | t @ Ty::RwShared(_)
                | t @ Ty::Executor
                | t @ Ty::Socket
                | t @ Ty::Listener
                | t @ Ty::Writer
                | t @ Ty::Reader
                | t @ Ty::Ptr
                | t @ Ty::Width(_)
                | t @ Ty::Protocol(..)
                | t @ Ty::Module(_)
                | t @ Ty::Unknown => self.sendable_rec(&t, stack),
            },
            // A `Shared[T]` handle always crosses — that's its whole point (one box, many tasks);
            // its element type is *not* a constraint (the value never crosses, only the handle).
            Ty::Shared(_) => true,
            // A `RwShared[T]` handle crosses for the same reason as `Shared` — one box, many tasks;
            // the element type is not a constraint (only the handle crosses).
            Ty::RwShared(_) => true,
            // An `Atomic[T]` handle crosses for the same reason as `Shared` — one box, many tasks;
            // the element type is not a constraint (only the handle crosses).
            Ty::Atomic(_) => true,
            // An `AtomicInt` handle crosses like `Atomic` — one lock-free box, many tasks.
            Ty::AtomicInt => true,
            // An `Executor` handle crosses the airlock like a `Channel`/`Shared` handle (the queue
            // lives outside every heap; tasks reach the one work queue).
            Ty::Executor => true,
            // D6 — a `Socket`/`Listener` handle crosses the airlock like the other core handles (the
            // fd lives in an `Arc`'d core outside every heap), so a `parallel:` accept-loop can
            // `spawn handle(conn)` onto a fiber.
            Ty::Socket | Ty::Listener => true,
            // R2 — a `Writer` handle crosses the airlock like the socket handles (the fd/buffer lives
            // in an `Arc`'d core outside every heap), so a `spawn`ed fiber can write to it.
            // R2b — a `Reader` handle likewise (its `BufReader<File>` lives in an `Arc`'d core).
            Ty::Writer | Ty::Reader => true,
            // An opaque `ptr` is a plain raw address (a `usize`) — it crosses the airlock by value,
            // so it is always sendable (its referent, if any, is the foreign library's concern).
            Ty::Ptr => true,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Channel(t) => {
                self.sendable_rec(t, stack)
            }
            Ty::Map(k, v) => self.sendable_rec(k, stack) && self.sendable_rec(v, stack),
            Ty::Result(t, e) => self.sendable_rec(t, stack) && self.sendable_rec(e, stack),
            Ty::Tuple(elems) => elems.iter().all(|t| self.sendable_rec(t, stack)),
            // B3.3 (Task 2a) — a user `Func` (closure / nested fn / bare fn) crosses the airlock BY
            // VALUE now (the runtime `to_wire`/`to_snap` lowering carries its proto + wired captures),
            // so the bare `fn` type is sendable. The bare type cannot carry its captures, so the
            // per-closure capture-sendability check is done at the airlock SITES (the `spawn:` block
            // read gate + the spawn callee/arg gate in `sig.rs`), NOT here. `Ty::Module` stays
            // non-sendable (a module namespace never crosses).
            Ty::Func { .. } => true,
            Ty::Module(_) => false,
            // Task 2 (option a) — a protocol existential is SENDABLE (Go `chan interface` parity): the
            // erased witness crosses by value like any other type, and the concrete witness is
            // sendable-checked at each widening site (`assignable`). A witness that genuinely can't be
            // serialized (one carrying an FFI/native handle, or a mid-`recover:` generator) is caught
            // at the RUNTIME airlock (`ensure_crossable` over `has_handle`), which is an exhaustive,
            // recoverable safety net — a checker-permissive type here is
            // never UB, at worst a deferred fault.
            Ty::Protocol(..) => true,
            // A first-class builtin fn value is pure code (no captured environment) — always
            // sendable, so a `f := ord` captured into a spawned task crosses the airlock (the
            // `Obj::Builtin`/`SnapValue::Builtin` runtime path), unlike a conservatively-non-sendable
            // user `Func`. All four builtins (`print`/`ord`/`chr`/`panic`) are covered uniformly.
            Ty::BuiltinFn { .. } => true,
            Ty::Struct(name, args) => {
                if !args.iter().all(|a| self.sendable_rec(a, stack)) {
                    return false;
                }
                if stack.contains(name) {
                    return true; // already being walked — the cycle adds no new type
                }
                match self.structs.get(name) {
                    Some(info) => {
                        let fields = info.fields.clone();
                        stack.push(name.clone());
                        let ok = fields.iter().all(|(_, fty)| self.sendable_rec(fty, stack));
                        stack.pop();
                        ok
                    }
                    None => true, // unknown struct: be permissive (any error is reported elsewhere)
                }
            }
            Ty::Enum(name, args) => {
                if !args.iter().all(|a| self.sendable_rec(a, stack)) {
                    return false;
                }
                if stack.contains(name) {
                    return true;
                }
                // Built-in Result/Option are erased here (their payloads are the type args, already
                // checked above); a user enum's variant payloads come from the registry.
                let payloads: Vec<Ty> = self
                    .variants
                    .values()
                    .filter(|v| &v.enum_name == name)
                    .flat_map(|v| v.payload.clone())
                    .collect();
                stack.push(name.clone());
                let ok = payloads.iter().all(|pty| self.sendable_rec(pty, stack));
                stack.pop();
                ok
            }
        }
    }

    /// A targeted hint for the most common non-sendable channel element: the built-in `Error`
    /// existential — what a bare `T!` / `Result[T, Error]` erases to (rendered `Result[int]`). It
    /// isn't sendable because a value satisfying `Error` may carry a non-sendable field; point the
    /// user at a concrete error type. Empty string when the type doesn't mention `Error`.
    pub(super) fn sendable_error_hint(&self, ty: &Ty) -> String {
        if ty_mentions_error_existential(ty) {
            " — the built-in `Error` type can't cross a task boundary (a value satisfying `Error` \
             may hold data that can't be sent between tasks); name a concrete error type, e.g. \
             `Channel[int!str]` or `Channel[int!MyErr]`"
                .to_string()
        } else {
            String::new()
        }
    }

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
    /// of those through the gate (`docs/gaps.md` — M23 review, CRITICAL 2).
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
