//! Checker: protocol satisfaction: `satisfies`, `protocol_provides`, `satisfies_args`, scalar and
//! container bounds.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

use super::proto::Grant;
use super::proto::HookMiss;
use super::proto::ReqMiss;
use super::*;
use std::collections::{HashMap, HashSet};

impl Checker {
    pub(super) fn satisfies(&self, ty: &Ty, protocol: &str) -> Result<(), String> {
        self.satisfies_args(ty, protocol, &[])
    }

    /// The value-slot twin of the generic-bound conformance sentence (`satisfies_args`'s `Err`
    /// string) — TICKET-008 / `docs/contributing/gaps.md` W8-16. Called only from an already-failing
    /// `assignable` branch, so it cannot change what the checker accepts: it only appends WHY a
    /// value slot rejected a type, when that type was rejected because it fails a protocol.
    /// Empty when `expected` is not a protocol, and empty when `actual` DOES satisfy the
    /// protocol — a witness that satisfies but fails another `assignable` conjunct still reaches
    /// the error path, and claiming a missing method there would be false.
    pub(super) fn protocol_note(&self, expected: &Ty, actual: &Ty) -> String {
        // A tuple misfits through its first misfitting element (a `Map[K, V]([(k, v)])` element).
        if let (Ty::Tuple(es), Ty::Tuple(xs)) = (expected, actual)
            && es.len() == xs.len()
        {
            return es
                .iter()
                .zip(xs)
                .map(|(e, x)| self.protocol_note(e, x))
                .find(|n| !n.is_empty())
                .unwrap_or_default();
        }
        let Ty::Protocol(p, pargs) = expected else {
            return String::new();
        };
        match self.satisfies_args(actual, p, pargs) {
            Ok(()) => match self.eq_where_bounds_unsatisfied(actual) {
                // TICKET-053: the third rejection ground `assignable` now checks — conformance and
                // sendability both hold, but the witness's own `eq` carries an unmet `where` bound.
                // Same producer (`eq_where_unsatisfied`) the direct-comparison path already uses, so
                // this never fabricates a reason DEC-008 didn't already prove true.
                Some(why) => format!(" \u{2014} {why}"),
                None => String::new(),
            },
            Err(why) => format!(" \u{2014} {why}"),
        }
    }

    /// M22 — does protocol `p`, with its own type params bound to `pargs`, BE or transitively EMBED
    /// `protocol` with args matching `required`? The ONE yes/no for a declared bound and an
    /// existential alike (TICKET-202): a type param equals only itself, `Unknown` is the only
    /// wildcard. This is what makes `a + b` legal inside a `[T: Arithmetic]` body and lets a
    /// `[T: Arithmetic]` value forward into a `[U: Div]` call. Each embed's args (resolved at the
    /// declaration) are re-spelled through `p`'s own bindings, so `protocol P[T]: Container[T]` at
    /// `P[int]` provides `Container[int]` and not `Container[T]`.
    pub(super) fn protocol_provides(
        &self,
        p: &str,
        pargs: &[Ty],
        protocol: &str,
        required: &[Ty],
    ) -> bool {
        self.protocol_provides_d(p, pargs, protocol, required, &mut HashSet::new())
    }

    /// `seen` is keyed on the protocol AND its args, because the same protocol reached along two
    /// paths with different args is a different question (`P[int]` vs `P[str]`). Same reason as
    /// `protocol_method_sig_d`: a depth cap does not terminate a branching walk.
    fn protocol_provides_d(
        &self,
        p: &str,
        pargs: &[Ty],
        protocol: &str,
        required: &[Ty],
        seen: &mut HashSet<String>,
    ) -> bool {
        let key = format!(
            "{p}[{}]",
            pargs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        if !seen.insert(key) {
            return false;
        }
        let args_match = pargs.len() == required.len()
            && pargs.iter().zip(required).all(|(x, y)| self.join_ty(x, y));
        if self.protocol_key(p) == self.protocol_key(protocol) && args_match {
            return true;
        }
        // Subsumption: every `Iterator[T]` IS `Iterable[T]` (its `iter()` returns self).
        if protocol == "Iterable" && self.protocol_key(p) == "Iterator" && args_match {
            return true;
        }
        let Some(pinfo) = self.protocol_shape(p) else {
            return false;
        };
        let map: HashMap<String, Ty> = pinfo
            .type_params
            .iter()
            .cloned()
            .zip(pargs.iter().cloned())
            .collect();
        pinfo.embeds.iter().any(|e| {
            let eargs: Vec<Ty> = e.args.iter().map(|t| subst(t, &map)).collect();
            self.protocol_provides_d(&e.name, &eargs, protocol, required, seen)
        })
    }

    /// Does concrete `ty` satisfy `protocol` instantiated with `args` (the bound's type arguments,
    /// e.g. `[int]` for `Container[int]`)? `args` is empty for a bare protocol. For a parameterized
    /// protocol the structural check substitutes the protocol's type params with `args` before
    /// matching method signatures.
    pub(super) fn satisfies_args(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
    ) -> Result<(), String> {
        self.satisfies_args_d(ty, protocol, args, &mut HashSet::new())
            .map(|_| ())
    }

    /// A `where T: <scalar>` bound names a concrete scalar type rather than a protocol, making it an
    /// EQUALITY constraint (`T` must be exactly this type) instead of structural satisfaction. Scoped
    /// to scalars — the concrete-equality case the surface needs (`Channel[T].trip()`'s `where T: bool`)
    /// without opening generic-struct equality. Returns the scalar `Ty`, or `None` if `name` is not a
    /// scalar type (so the caller falls back to the protocol path / an `unknown protocol` error).
    /// This is the one scalar type-name table: `resolve_type` and `resolve_ty_ro_d` read it.
    pub(super) fn scalar_bound_ty(name: &str) -> Option<Ty> {
        BuiltinType::from_name(name)?.scalar_ty()
    }

    /// A `where T: List/Map/Set` bound names a container CONSTRUCTOR rather than a protocol, making it
    /// a HEAD-CONSTRUCTOR equality constraint (`T`'s head must be exactly this container, its element/
    /// key/value types free). The constructor-kind generalization of [`scalar_bound_ty`] — closed set
    /// in ONE place (add a container = add an arm). Returns `true` iff `ty`'s head matches `name`
    /// (`Ty::Unknown` doesn't cascade — handled by the caller). Tuple is EXCLUDED (heterogeneous).
    /// This is the surface form of the `RwShared` read-view gate (`expr.rs`); no element binder, so no
    /// harvest-scoping change. Returns `None` if `name` is not a recognized container.
    pub(super) fn container_bound_matches(name: &str, ty: &Ty) -> Option<bool> {
        Some(match name {
            "List" => matches!(ty, Ty::List(_)),
            "Map" => matches!(ty, Ty::Map(_, _)),
            "Set" => matches!(ty, Ty::Set(_)),
            _ => return None,
        })
    }

    /// Visited-set core of [`satisfies_args`]. `seen` guards the embed-flattening recursion (M22):
    /// cycles are rejected at declare time, but a malformed cyclic program still runs the rest of the
    /// checker, so a hard cap (mirroring `resolve_ty_ro_d`) breaks the recursion with a plain failure
    /// instead of overflowing the stack.
    pub(super) fn satisfies_args_d(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        seen: &mut HashSet<String>,
    ) -> Result<Grant, String> {
        // TICKET-218: a C width satisfies exactly what its scalar does.
        let ty = ty.scalar();
        // TICKET-027: `protocol` may carry a module-qualified KEY (e.g. from a re-keyed stored
        // bound); every user-facing message below renders the BARE name, matching `Ty::Protocol`'s
        // own `Display`.
        let protocol_display = crate::base::words::bare_display(protocol);
        let Some(pinfo) = self.protocol_shape(protocol) else {
            // A `where T: <scalar>` EQUALITY bound: the name is a concrete scalar type, not a
            // protocol, so it constrains `ty` to be EXACTLY that type (e.g. `trip()`'s `where T: bool`).
            if let Some(expected) = Self::scalar_bound_ty(protocol) {
                return match ty {
                    // Don't cascade off an unresolved operand (mirrors the `Ty::Unknown` arm below).
                    Ty::Unknown => Ok(Grant::no_intrinsic_method()),
                    _ if *ty == expected => Ok(Grant::no_intrinsic_method()),
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
                    | Ty::Var(_) => Err(format!("expected {expected}, found {ty}")),
                };
            }
            // A `where T: List/Map/Set` constructor-kind bound: `ty`'s HEAD must equal the container.
            if let Some(ok) = Self::container_bound_matches(protocol, ty) {
                return match ty {
                    Ty::Unknown => Ok(Grant::no_intrinsic_method()),
                    _ if ok => Ok(Grant::no_intrinsic_method()),
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
                    | Ty::Var(_) => Err(format!("expected {protocol_display}[...], found {ty}")),
                };
            }
            return Err(format!("unknown protocol '{protocol_display}'"));
        };
        if let Ty::Unknown = ty {
            return Ok(Grant::no_intrinsic_method()); // don't cascade
        }
        // TICKET-214: `Num` is SEALED to int and float. Its embeds are only what a `T: Num` body's
        // operators read; a struct or enum with all eight methods satisfies that shape and would reach
        // a native declared `[T: Num]` (`math.abs`), which takes a raw number. A bound `T` falls through
        // to the `Ty::Param` arm, which matches bounds by name (`protocol_provides`), so
        // `[T: Arithmetic + Mod + Neg + Comparable]` is not `Num`. A protocol value passes only if its
        // protocol embeds `Num`, so erasure cannot bypass the seal.
        // TICKET-250: `StructuralEq` is SEALED too. Its body is empty, so without this arm every
        // type would satisfy it through the empty-protocol rule below.
        if protocol == "StructuralEq" {
            return self
                .structural_eq(ty)
                .map(|()| Grant::no_intrinsic_method());
        }
        if protocol == "Num" {
            let admitted = match ty {
                Ty::Int | Ty::Float | Ty::Param(_) => true,
                Ty::Protocol(p, pargs) => self.protocol_provides(p, pargs, protocol, args),
                Ty::Bool
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
                | Ty::Var(_) => false,
            };
            if !admitted {
                return Err(format!(
                    "type {ty} does not satisfy Num (only int and float do)"
                ));
            }
        }
        // An EMPTY structural protocol (zero embeds AND zero methods — e.g. the `Any` top type) is
        // satisfied by EVERY type, scalars included. Without this short-circuit a zero-method/zero-embed
        // protocol would fall past every intrinsic arm to the `_ => Err` at the bottom for Int/Float/
        // Bool/Str/Nil (structs pass via the vacuous `satisfies_methods` over zero methods, but scalars
        // have no structural arm). This makes any empty protocol a genuine top type for every `Ty`.
        if pinfo.embeds.is_empty() && pinfo.methods.is_empty() {
            return Ok(Grant::no_intrinsic_method());
        }
        // M22 — embedded (super-)protocols: a type satisfies `protocol` iff it satisfies every embed
        // (transitively) AND has every OWN method below. A PURE bundle (`Arithmetic` = embeds only,
        // no own methods) short-circuits once its embeds pass — this is what lets int/float/struct
        // satisfy `Arithmetic` (each embed recurses into the intrinsic/structural arms). A `Ty::Param`
        // is NOT flattened here — it forwards through its declared bounds in the `Ty::Param` arm below
        // (which knows, via `protocol_provides`, that an `Arithmetic`-bound param provides Add/Sub/…).
        if !pinfo.embeds.is_empty() && !matches!(ty, Ty::Param(_)) {
            // A VISITED SET, not a depth cap: `ty` is fixed across the whole walk, so revisiting
            // one (protocol, args) pair can only re-derive the same answer, while a depth bound of
            // 64 over a branching graph is 2^64 visits — a 42-protocol DAG hung `check` (and the
            // LSP) past 25 s. Same class the sibling walkers close; this one is the third.
            {
                // The embed args were resolved at the declaration (TICKET-202, not by a use-site
                // `resolve_ty_ro`, which read the CALLER scope) and then bound to the args actually
                // being required. Without both,
                // `protocol Bag[T]: Contains[T]` witnessed conformance against an `Unknown` element
                // and `PBag[str] = B` (a `contains(self, int)`) passed — the CONFORMANCE half of the
                // same bug the read side had, and the thing that makes the read side's substitution
                // sound to trust.
                let omap: HashMap<String, Ty> = pinfo
                    .type_params
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                for emb in &pinfo.embeds {
                    let eargs: Vec<Ty> = emb.args.iter().map(|t| subst(t, &omap)).collect();
                    let key = format!(
                        "{}[{}]",
                        emb.name,
                        eargs
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    if !seen.insert(key) {
                        continue; // already answered on this walk — an embed diamond, not new work
                    }
                    let _: Grant = self
                        .satisfies_args_d(ty, &emb.name, &eargs, seen)
                        .map_err(|why| embed_miss_text(why, ty, &emb.name, &protocol_display))?;
                }
            }
            if pinfo.methods.is_empty() {
                // Pure bundle — all embeds satisfied, no own methods to check. Each embed already
                // registered its own intrinsic grant (or needed none), so this is not itself a grant.
                return Ok(Grant::no_intrinsic_method());
            }
        }
        if protocol == "Contains"
            && let Some(verdict) =
                self.builtin_contains_verdict(ty, args.first().unwrap_or(&Ty::Unknown))
        {
            return match verdict {
                Ok(()) => self.grant_intrinsic(protocol, ty),
                Err(None) => Err(format!("type {ty} does not satisfy Contains")),
                Err(Some(why)) => Err(format!("type {ty} does not satisfy Contains ({why})")),
            };
        }
        if protocol == "Comparable" {
            // TICKET-146: a tuple / `List` is Comparable when every element type is; `T?` never is
            // (owner decision 4 of TICKET-265). Each element is asked through `satisfies` with a
            // FRESH `seen` set: the shared one is keyed by embed name without the subject type, so
            // reusing it would skip the element's own `Eq` embed.
            let elems: Option<Vec<&Ty>> = match ty {
                Ty::Tuple(es) => Some(es.iter().collect()),
                Ty::List(e) => Some(vec![&**e]),
                Ty::Option(_)
                | Ty::Int
                | Ty::Float
                | Ty::Bool
                | Ty::Str
                | Ty::Bytes
                | Ty::ByteArray
                | Ty::Nil
                | Ty::Map(..)
                | Ty::Set(_)
                | Ty::Func { .. }
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
            };
            if let Some(elems) = elems {
                // A protocol VALUE whose protocol takes `Self` (object safety) is not orderable:
                // the `Ty::Protocol` arm below deliberately skips that guard, but ordering pairs two
                // values, which may hold two different witnesses.
                let unorderable = |e: &Ty| match e {
                    Ty::Protocol(p, _) => self.protocol_self_param_method(p).is_some(),
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
                    | Ty::Module(_)
                    | Ty::Unknown
                    | Ty::Var(_) => false,
                };
                if elems
                    .iter()
                    .any(|e| unorderable(e) || self.satisfies(e, "Comparable").is_err())
                {
                    return Err(format!("type {ty} does not satisfy Comparable"));
                }
                return self.grant_intrinsic(protocol, ty);
            }
        }
        if protocol == "Comparable"
            && matches!(
                ty,
                Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::ByteArray
            )
        {
            return self.grant_intrinsic(protocol, ty);
        }
        // **D1 — `Eq` satisfaction IS what `==` accepts.** Chezzi's structural `==` is an automatic
        // derive (`Vm::values_equal`) that never told the protocol system: `[1] == [1]`,
        // `(1,2) == (1,2)`, `Some(1) == Some(1)`, `P(1) == P(2)` for a plain struct — every one of
        // them works, while `where T: Eq` used to reject all of them because the grant was the four
        // scalars and nothing else. That is what made `where T: Eq` unwritable and got the first
        // W7-41 fix reverted (`docs/contributing/gaps.md` W7-41). **Rust owns this** — `Vec<i32>`, `(i32,i32)`,
        // `Option<i32>`, `Vec<u8>` and `#[derive(PartialEq, Eq)] struct P` all satisfy `Eq` and all
        // compile under `struct Boxy<T: Eq>` (measured, rustc 1.97.0); only a type with no
        // `PartialEq` at all is `E0277`.
        //
        // **MEMBERSHIP IS THE RECEIVER KIND, not `may_be_equal`.** The rule is stated above in terms
        // of `==`, but the gate below is a KIND test, and that is deliberate rather than an
        // approximation: `may_be_equal(ty, ty)` is reflexively `true` for every kind that reaches
        // here, so calling it would decide nothing (it was here, dead, and is gone). What actually
        // decides membership is `intrinsic_recv_kind(ty) != "?"` — "a shape the W6-3 ratchet can key
        // a row on", which is exactly the precondition for an INTRINSIC grant: no row means no probe
        // means no proof the method is callable at runtime.
        //
        // Four gates:
        // * kind ≠ `"?"`. That covers every handle (`Channel`/`Shared`/`Executor`/`Socket`/…),
        //   `Module` and `Ty::Param` (`Func` graduated into the `"func"` kind in W7-54,
        //   `Ty::Protocol` into the `"protocol"` kind in W7-52, so neither lands here any more). The
        //   handles compare by identity but have no constructible probe receiver, so kind `"?"` is
        //   the right refusal for them; `Ty::Param` MUST fall through too — `may_be_equal` treats a
        //   `Param` as ERASED, so admitting it would make every UNBOUNDED `T` satisfy `Eq` (a
        //   soundness hole). `Ty::Protocol` no longer falls through: at runtime a protocol-typed value
        //   IS its concrete witness, so its `Eq`-ness is exactly as decidable as any other receiver's
        //   — undecidable here, deferred to the witness the same way `Ty::Result`'s error slot already
        //   defers (`eq_bounds_unsatisfied_rec`'s `Ty::Protocol` catch-all, below), matching Go's
        //   interface `==` (defers to the dynamic type, panics if it's uncomparable — W7-52).
        // * kind ≠ `"nil"`: a nil-typed expression cannot be used as a value at all, so `nil == nil`
        //   is not a writable program and the grant would have no probeable receiver.
        // * not the built-in cursor ([`Self::is_cursor_ty`]) — a `Ty::Struct` whose runtime arms
        //   expose only `next`/`iter`, so the `"struct"` row does not speak for it.
        // * the type does not declare the `eq` HOOK. A NON-hook `eq` (the ordinary-method escape
        //   hatch, `fn eq(self, x: T)` with a generic operand) is dispatched by name only from an
        //   explicit `.eq(b)` call — `==` never sees it and stays structural — so such a type is
        //   graded the same as one with no `eq` at all (C1, W7-53 follow-up: before this, `Key` below
        //   had `==` structural and working while `[T: Eq]`/`.eq()` refused it — the exact
        //   two-spellings-disagree shape Tasks 1/2 closed for `Ty::Func`/
        //   `Ty::Protocol`):
        //   ```text
        //   struct Key: fn hash(self) -> int: return 1
        //               fn eq[U](self, o: U) -> bool: return true   # non-hook: generic operand
        //   ```
        //   A type that DOES declare the hook is decided structurally below: an erased `[T: Eq]`
        //   body's `a.eq(b)` dispatches by NAME to it, handing it an operand it never declared.
        //   [`Self::eq_sig_is_hook`] is the single question — same predicate `validate_eq_shape`
        //   (`sig.rs`) enforces at the declaration, so the two can never disagree about which shape a
        //   program's `eq` is.
        //
        // …and then the real question: [`Self::eq_bounds_unsatisfied`] — nothing the structural
        // equality walk REACHES is a declared `eq` whose `where` bounds fail for this instantiation
        // (W7-41's actual defect). Its `Some` is the refusal, and **its `None` is this grant**, which
        // is why every "cannot tell" inside it answers `Some`.
        // TICKET-257: the handles (`Channel`/`Shared`/`RwShared`/`Atomic`) and the built-in cursor are
        // `Eq` by IDENTITY, exactly as `==` already accepts them; each has a `RecvKind` and a row.
        if protocol == "Eq"
            && Self::intrinsic_recv_kind(ty).is_some()
            && self.hook_method(ty, Hook::Eq).is_none()
        {
            return match self.eq_bounds_unsatisfied(ty) {
                Some(why) => Err(format!("type {ty} does not satisfy Eq ({why})")),
                None => self.grant_intrinsic(protocol, ty),
            };
        }
        // `Stringable` (sole method `str(self) -> str`) is satisfied intrinsically by every scalar —
        // all four stringify (int/float/bool/str), so a `[T: Stringable]` generic accepts them (the
        // erased body's `v.str()` is dispatched by the scalar `str` branch in `Vm::do_method_call`).
        // Note the membership is all FOUR scalars, as for Comparable (Hashable has no Float).
        // TICKET-257 (owner rule U5): `print`, `str()` and `"{x}"` accept every value, so every kind a
        // `RecvKind` names is `Stringable`. A method that is not the hook is invisible to `print`, so
        // a type whose `str` has the wrong shape is graded like one with no `str` (the hook grid pins
        // this). Only the hook itself (`Ok`) and a hook failing its own `where` bound (`Bound`) fall
        // through to the structural `satisfies_methods` below.
        if protocol == "Stringable"
            && Self::intrinsic_recv_kind(ty).is_some()
            && matches!(
                self.hook_verdict(ty, Hook::Str),
                Err(HookMiss::Absent | HookMiss::Static | HookMiss::Shape(_))
            )
        {
            return self.grant_intrinsic(protocol, ty);
        }
        // TICKET-161: a tuple is `Hashable` exactly when every element type is (CPython's rule),
        // asked through `satisfies` per element like the `Comparable` arm above. The `Err` names the
        // innermost failing element, the way CPython's `unhashable type: 'list'` does.
        if protocol == "Hashable" && matches!(ty, Ty::Tuple(_)) {
            if let Some(bad) = self.unhashable_tuple_elem(ty) {
                return Err(format!(
                    "type {ty} does not satisfy Hashable: element {bad} is not Hashable"
                ));
            }
            return self.grant_intrinsic(protocol, ty);
        }
        // `Hashable` is satisfied intrinsically by the scalar key types (mirrors the map/set key
        // restriction; float is excluded — its equality is a hazard). A struct or enum needs both
        // hooks (below).
        if protocol == "Hashable" && matches!(ty, Ty::Int | Ty::Str | Ty::Bytes | Ty::Bool) {
            return self.grant_intrinsic(protocol, ty);
        }
        // TICKET-280 (owner decision U15): a struct or enum is `Hashable` only when it declares both
        // hooks, `hash` and `eq`, as Rust needs `Hash` and `Eq`. No declared type is a key on its
        // own: a zero-field struct needs both hooks, and structural `==` does not count as the `Eq`
        // half. A type without the `hash` hook falls through to the structural check below, which
        // names `hash`. `declares_hook` is false for every type that is not a struct or an enum.
        if protocol == "Hashable"
            && self.declares_hook(ty, Hook::Hash)
            && !self.declares_hook(ty, Hook::Eq)
        {
            return Err(format!(
                "type {ty} does not satisfy Hashable (missing method '{}')",
                Hook::Eq.method()
            ));
        }
        // `str` conforms to `Error` intrinsically (Go-style: its message is itself).
        if protocol == "Error" && matches!(ty, Ty::Str) {
            return self.grant_intrinsic(protocol, ty);
        }
        // W7-8 — `PathLike` is satisfied intrinsically by the three byte-ish spellings of a path: a
        // `str` (UTF-8-encoded), a `bytes` (itself), a `bytearray` (copied). This is a VALUE-level
        // early-out keyed on the concrete scalar `Ty` alone — it can never widen a CONTAINER (a
        // `List[str]` is compared element-wise by `compatible`, which does not route here), so
        // `List[int] -> List[Any]` and friends stay rejected exactly as before. `path.Path` is a
        // struct and falls through to the structural check against its own `as_path(self) -> bytes`.
        if protocol == "PathLike" && matches!(ty, Ty::Str | Ty::Bytes | Ty::ByteArray) {
            return self.grant_intrinsic(protocol, ty);
        }
        // `Iterator` conformance is "HOLDS a cursor position" — a real cursor/generator
        // (`Ty::Struct("Iterator", [E])`, minted by `.iter()` or returned by a generator) or a user
        // struct with a structural `next(self) -> Option[E]`. NOT a raw collection: `next` is stateful
        // and a bare list/set/map/str/bytes holds no position, so the old can-be-iterated
        // predicate check-OK'd `c.next()` and then faulted at runtime (W6-3b). A raw
        // collection satisfies `Iterable` instead — `[S: Iterable[T], T]` is the migration form, and it
        // recovers `T` the same way (see `recover_iter_elems`). A `Ty::Param` falls through to the
        // declared-bounds check below (so a `[S: Iterator[T]]` value forwards into another
        // iterator-generic call), since neither predicate can see through a bare param.
        if protocol == "Iterator" && !matches!(ty, Ty::Param(_) | Ty::Protocol(..)) {
            // `for` in a `[S: Iterator[T]]` body iterates the value as it iterates any other, so
            // the bound also needs `iterable_elem`, the one reader of what that yields.
            let elem = match self.iterator_bound_elem(ty) {
                Ok(elem) => elem,
                Err(why) => return Err(format!("type {ty} does not satisfy Iterator {why}")),
            };
            let has_next = self.struct_iter_elem(ty).is_some();
            return if has_next && elem.is_some() {
                // A declared type's own `next` hook: a real method, not an intrinsic grant.
                Ok(Grant::no_intrinsic_method())
            } else if Self::is_cursor_ty(ty) {
                self.grant_intrinsic(protocol, ty)
            } else if !has_next && elem.is_some() {
                // ITERABLE but position-less (a raw collection): every remedy below actually applies
                // to it, so spell them out — this is the W6-3b migration path.
                Err(format!(
                    "type {ty} does not satisfy Iterator — `next` needs a cursor that holds a \
                     position. Iterate it with `for`, take a cursor with `.iter()`, or bound the \
                     parameter `[S: Iterable[T], T]`{}",
                    self.bound_hint("Iterable")
                ))
            } else {
                // NOT iterable at all (`int`, a plain struct): `for`/`.iter()`/`Iterable` are all dead
                // ends for it, so appending that advice would misdirect. Keep the bare pre-W6-3b text.
                let note = self.iter_hook_note(ty);
                Err(format!("type {ty} does not satisfy Iterator{note}"))
            };
        }
        // `Iterable` conformance is "can produce a fresh cursor". Built-in collections satisfy it
        // intrinsically; ANY `Iterator[T]`-satisfying type satisfies it too (every Iterator IS
        // Iterable — `iter()` returns self), so `iterable_elem`, which covers both and the `iter` hook,
        // is the predicate. The bound's `[T]` arg, if supplied and concrete,
        // must match the element type (mirrors the parameterized-`Index` arg check). A `Ty::Param`
        // falls through to the declared-bounds check below (so `[S: Iterable[T]]` forwards), and so
        // does a `Ty::Protocol` existential (an `Iterable[T]`-ANNOTATED value): its runtime receiver
        // is whatever concrete thing witnesses it, whose own intrinsic row already exists, so it is
        // decided by the protocol-existential arm below — `Iterable[int]` satisfies `Iterable[int]`
        // and nothing wider (that arm is where the strict arg invariance lives).
        if protocol == "Iterable" && !matches!(ty, Ty::Param(_) | Ty::Protocol(..)) {
            let Some(elem) = self.iterable_elem(ty) else {
                let note = self.iter_hook_note(ty);
                return Err(format!("type {ty} does not satisfy Iterable{note}"));
            };
            if let Some(want) = args.first()
                && !want.is_unknown()
                && !elem.is_unknown()
                && !self.join_ty(want, &elem)
            {
                return Err(format!("type {ty} does not satisfy Iterable"));
            }
            // A declared type iterates through its own hook, not through an intrinsic grant.
            return if self.nominal_method_table(ty).is_some() {
                Ok(Grant::no_intrinsic_method())
            } else {
                self.grant_intrinsic(protocol, ty)
            };
        }
        // `Index`/`IndexSet`/`Slice` — built-in `list`/`map`/`str` conform intrinsically (a struct
        // conforms structurally, falling through to the matcher below; a `Ty::Param` forwards to its
        // declared bounds). `str` is immutable, so it satisfies `Index`/`Slice` but NOT `IndexSet`.
        if matches!(protocol, "Index" | "IndexSet" | "Slice")
            && !matches!(
                ty,
                Ty::Param(_) | Ty::Struct(..) | Ty::Enum(..) | Ty::Protocol(..)
            )
        {
            let provided: Vec<Ty> = match protocol {
                "Slice" => match self.slice_result(ty) {
                    Some(r) => vec![r],
                    None => return Err(format!("type {ty} does not satisfy Slice")),
                },
                _ => {
                    if protocol == "IndexSet"
                        && !matches!(ty, Ty::List(_) | Ty::Map(_, _) | Ty::ByteArray)
                    {
                        return Err(format!("type {ty} does not satisfy IndexSet"));
                    }
                    match self.index_kv(ty) {
                        Some((k, v)) => vec![k, v],
                        None => {
                            return Err(format!("type {ty} does not satisfy {protocol_display}"));
                        }
                    }
                }
            };
            // Any args the bound supplied must match what the built-in actually provides.
            for (want, got) in args.iter().zip(&provided) {
                if !want.is_unknown() && !got.is_unknown() && !self.join_ty(want, got) {
                    return Err(format!("type {ty} does not satisfy {protocol_display}"));
                }
            }
            return self.grant_intrinsic(protocol, ty);
        }
        // A protocol existential value satisfies a protocol iff it IS that protocol, or EMBEDS it
        // (M22, transitively — a `Person` value is accepted where `Named` is wanted, matching Go's
        // interface-to-interface assignment), or STRUCTURALLY has its methods (so a `Vecish` that
        // declares `add(self, o: Self) -> Self` witnesses the builtin `Add`, which is what makes
        // `a + b` work on two `Vecish` values). Arg matching stays STRICT throughout — invariance
        // when a Protocol VALUE is the subject: a bare `Container` value does NOT satisfy
        // `Container[int]` (0 args vs 1) and vice-versa, and `Container[str]` ≠ `Container[int]`.
        if let Ty::Protocol(p, pargs) = ty {
            // OBJECT SAFETY is deliberately NOT enforced here: this arm answers plain assignability
            // too, and `fn takes(p: Vecish)` fed a `Vecish` value is sound — nothing pairs two
            // witnesses. Placed here it rejected `expected Vecish, found Vecish`. The pairing sites
            // each carry their own guard instead: `enforce_bounds` (a generic type param, whose two
            // slots could hold two different witnesses), the existential method-call arm in
            // `expr.rs`, and `op_overload_result`/`ordering_allowed` (which simply have no
            // `Ty::Protocol` arm at all). See `self_in_param_position`.
            if self.protocol_provides(p, pargs, protocol, args) {
                return Ok(Grant::no_intrinsic_method());
            }
            // Structural: does `p` (own + flattened embeds) supply every method `protocol` requires?
            // Only meaningful when `protocol` has own methods — a pure bundle already returned above
            // once its embeds passed.
            let provided: HashMap<String, FnSig> = pinfo
                .methods
                .iter()
                .filter_map(|(n, _)| self.protocol_method_sig(p, n).map(|s| (n.clone(), s)))
                .collect();
            return if !pinfo.methods.is_empty()
                && self
                    .satisfies_methods(ty, protocol, args, pinfo, &provided)
                    .is_ok()
            {
                Ok(Grant::no_intrinsic_method())
            } else {
                Err(format!("type {ty} does not satisfy {protocol_display}"))
            };
        }
        // The operator protocols a BUILTIN type satisfies (`builtin_arith_kind`): int/float natively,
        // plus `+` on str, bytes, bytearray and List and `-` on Set (TICKET-257, owner rule U5).
        if Self::builtin_arith_kind(protocol, ty) {
            return self.grant_intrinsic(protocol, ty);
        }
        // A bound type parameter satisfies a protocol if that protocol is among its declared bounds —
        // this is what lets a generic forward its `T: P` value into another `[U: P]` call. For a
        // parameterized protocol the bound's type args must also match the required ones, so a
        // `Container[str]` value is NOT accepted where `Container[int]` is required (forwarding hole),
        // and an abstract arg matches only by identity (TICKET-202).
        if let Ty::Param(name) = ty {
            let matched = self.type_params.get(name).is_some_and(|bs| {
                bs.iter()
                    .any(|b| self.protocol_provides(&b.name, &b.args, protocol, args))
            });
            return if matched {
                Ok(Grant::no_intrinsic_method())
            } else if args.is_empty() {
                Err(format!("type {ty} does not satisfy {protocol_display}"))
            } else {
                let shown: Vec<String> = args.iter().map(|a| a.to_string()).collect();
                Err(format!(
                    "type {ty} does not satisfy {protocol_display}[{}]",
                    shown.join(", ")
                ))
            };
        }
        // A built-in witnesses a USER protocol out of its own harvested `native struct` method
        // table (TICKET-024, W8-32) -- reserved protocols are excluded because each already has a
        // hand-written intrinsic arm above whose grant is a W6-3 promise about a method with no
        // user code behind it; routing them through the native table too would silently re-decide
        // conformance for every built-in (a builtin whose signature matched a protocol method would otherwise witness it).
        if !is_std_protocol(protocol)
            && let Some(key) = Self::native_witness_key(ty)
        {
            return self
                .satisfies_native(ty, protocol, args, pinfo, key)
                .map(|()| Grant::no_intrinsic_method());
        }
        match ty {
            Ty::Struct(sname, _) => {
                // MISS-ONLY identity-key fallback (gap #4): a named-fn-imported factory result carries
                // its owning module's `Ty::Struct` key but injects nothing into the local `self.structs`
                // table, so resolve the shape from the owning `ModuleSig` on a local miss — otherwise a
                // structurally-conforming value is spuriously rejected at a protocol bound (the same
                // three-import-forms inconsistency the member-access fix already closed).
                let Some(info) = self.struct_shape(sname) else {
                    return Err(format!("type {ty} does not satisfy {protocol_display}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, &info.methods)
                    .map(|()| Grant::no_intrinsic_method())
            }
            // Enum conformance is structural exactly like a struct's: the enum satisfies `protocol`
            // iff its `methods` map carries every protocol method with a matching signature. This
            // unlocks Stringable/Hashable/Add/Sub/Mul/Comparable for enums and protocol-bound generics.
            // A carrier that no intrinsic arm above granted reads its (empty) method table here too.
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
            | Ty::Var(_) => match ty.as_enum() {
                Some((ename, _)) => {
                    // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported enum
                    // value's method table from the owning `ModuleSig` on a local-table miss (see
                    // the struct arm).
                    let Some(methods) = self.enum_methods_of(ename) else {
                        return Err(format!("type {ty} does not satisfy {protocol_display}"));
                    };
                    self.satisfies_methods(ty, protocol, args, pinfo, methods)
                        .map(|()| Grant::no_intrinsic_method())
                }
                None => Err(format!("type {ty} does not satisfy {protocol_display}")),
            },
        }
    }

    /// A native method's DISPATCH-TIME residual that a `FnSig` alone can't express (TICKET-024,
    /// W8-32) — mirrors the two gates `src/checker/expr.rs` applies at `Ty::List`'s direct-call arm
    /// (`:2981`'s numeric-`sum` gate, `:3001`'s `eq_bounds_unsatisfied` gate for
    /// `contains`/`index_of`/`dedup`/`unique`), reused rather than re-derived so a protocol-erased
    /// call can't type-check where a direct call would fault. No other covered built-in
    /// (`Map`/`Set`/`str`/`bytes`/`bytearray`) has one. `Some(reason)` refuses; `None` defers to the
    /// ordinary signature match.
    pub(super) fn native_dispatch_residual(&self, ty: &Ty, method: &str) -> Option<String> {
        let Ty::List(elem) = ty else { return None };
        if method == "sum" {
            return (!elem.is_numeric() && !elem.is_unknown())
                .then(|| "native method 'sum' needs a numeric element type".to_string());
        }
        if matches!(method, "contains" | "index_of" | "dedup" | "unique") {
            let why = self.eq_bounds_unsatisfied(elem)?;
            return Some(format!(
                "native method '{method}' compares List elements for equality — {why}"
            ));
        }
        None
    }

    /// Structural conformance check shared by the struct and enum arms of [`satisfies_args`]: a type
    /// satisfies `protocol` iff `methods` carries every protocol method with a matching signature
    /// (the protocol's own type params substituted from the bound's `args`; `Self` handled inside
    /// `method_matches`).
    pub(super) fn satisfies_methods(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        methods: &HashMap<String, FnSig>,
    ) -> Result<(), String> {
        for (mname, _) in &pinfo.methods {
            self.requirement_matches(ty, protocol, args, pinfo, mname, methods)
                .map_err(ReqMiss::text)?;
        }
        Ok(())
    }
}

/// An embed's rejection, reported under the protocol the user asked for: `type T does not satisfy
/// <embed> (<reason>)` reads `type T does not satisfy <protocol> (<reason>)`, so the reason names
/// the first missing method across the protocol's embeds and its own methods. Any other text (a
/// sealed protocol, a bad type argument) is kept as it is.
fn embed_miss_text(why: String, ty: &Ty, embed: &str, protocol: &str) -> String {
    let prefix = format!(
        "type {ty} does not satisfy {}",
        crate::base::words::bare_display(embed)
    );
    match why.strip_prefix(&prefix) {
        Some(rest) if rest.is_empty() || rest.starts_with(' ') => {
            format!("type {ty} does not satisfy {protocol}{rest}")
        }
        Some(_) | None => why,
    }
}
