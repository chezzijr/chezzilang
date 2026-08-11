// checker::proto — split out of checker/mod.rs. `super::*` == the `checker` module.
// Protocol hoisting/embedding, satisfies, receiver refinement, hashability.

use super::*;
use std::collections::HashSet;

thread_local! {
    /// **THE cycle guard for the `Eq` walk — one stack, one key, both levels.** Holds the types
    /// [`Checker::eq_bounds_unsatisfied`] is currently proving, innermost last.
    ///
    /// It has to be ONE stack because the recursion has two levels that look different and are the
    /// same question: the walk descending into a struct's fields, and the outward hop through
    /// `satisfies` when a reached `eq` carries a `where` bound. They were two guards keyed
    /// differently — an instantiation-keyed one out here and a bare-NAME-keyed one down in the walk
    /// — and each one's blind spot was a soundness hole: the name-keyed one assumed a DIFFERENT
    /// instantiation of a name already in progress was sound (`R[T]` with a field `Option[R[Tag]]`),
    /// and a `Display`-keyed one collides two same-named structs from different modules.
    ///
    /// The key is the `Ty` itself. `Ty: PartialEq` compares `Ty::Struct`'s name field, which is the
    /// module-scoped IDENTITY key (`a::H`), not the bare display name (`H`) — so neither blind spot
    /// survives. Per-THREAD, so parallel test threads never share it; popped by an RAII guard on
    /// every exit path.
    static EQ_BOUNDS_IN_PROGRESS: std::cell::RefCell<Vec<Ty>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// Types already PROVEN sound during the CURRENT outermost query — the memo that makes the walk
    /// linear instead of exponential. The guard above is PATH-based: it stops a type being re-entered
    /// while it is on the stack, but says nothing about one already finished and popped. So a struct
    /// with two fields of the same nested type re-walks that subtree twice per level — 2^N, measured
    /// at 37s for N=26 against 0.003s pre-D1, on the same path the LSP runs on every keystroke.
    /// `EQ_BOUNDS_MAX_IN_PROGRESS` cannot catch it: that bounds DEPTH, and this shape stays shallow.
    ///
    /// **Only assumption-free results may be cached** — see [`EQ_BOUNDS_ASSUMED`]. A `None` derived
    /// while a coinductive assumption was in scope is valid only INSIDE that assumption; caching one
    /// would let it escape to a sibling branch that never made the assumption, which is C4/C5 (a type
    /// assumed sound without being proven) wearing a third disguise.
    ///
    /// Scoped to one outermost query — reset by [`Checker::eq_bounds_unsatisfied`] whenever the
    /// in-progress stack is empty on entry. That is what keeps it honest across the two things a
    /// longer-lived cache would get wrong: the checker's tables still being filled in (a type's `eq`
    /// may not be hoisted yet), and one thread checking several PROGRAMS in sequence, where
    /// `main::P` means something different each time.
    static EQ_BOUNDS_PROVEN: std::cell::RefCell<Vec<Ty>> =
        const { std::cell::RefCell::new(Vec::new()) };

    /// Set when the subtree just walked CONSUMED a coinductive assumption (the guard answered "this
    /// type is already being proven"). Such a result is conditional on that assumption, so it must
    /// not enter [`EQ_BOUNDS_PROVEN`]. Propagated outward: if a child used an assumption, the parent's
    /// result rests on it too.
    ///
    /// **DO NOT DELETE THIS BECAUSE YOU CANNOT WRITE A TEST THAT FAILS WITHOUT IT.** You cannot, and
    /// that is a property of today's traversal, not of the invariant. [`Checker::walk_eq_members`]
    /// short-circuits on the FIRST `Some`, and within one outermost query there is a single root, so
    /// any genuine `Some` under that root propagates to it — a poisoned entry consulted *in the same
    /// query* can never flip that query's verdict from reject to accept. Every distinguishing shape
    /// therefore has to span two queries, which [`EQ_BOUNDS_PROVEN`]'s per-query reset already clears.
    /// The two defences overlap completely **today** and each alone still rejects (the 2×2 in
    /// `eq_walk_memo_never_caches_an_assumed_result` measures exactly that).
    ///
    /// The overlap ends the moment `walk_eq_members` stops short-circuiting — e.g. the plausible UX
    /// change "report EVERY unsound field, not just the first". Then a sibling branch can be walked
    /// after the poisoned one without a `Some` riding along to invalidate it, the same-query hazard
    /// goes live, and this flag becomes the only thing standing between a cached assumption and a
    /// silent grant. That is C4/C5 in a fourth disguise.
    static EQ_BOUNDS_ASSUMED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Backstop on the in-progress stack. Once the guard keys on the instantiated type it no longer
/// terminates by itself: POLYMORPHIC RECURSION never repeats an instantiation (`N[T]` with a field
/// `Option[N[List[T]]]` expands `N[int]`, `N[List[int]]`, …), so without this `check` would hang, and
/// the LSP with it. Hitting it REFUSES, like every other decline here.
///
/// **Rust agrees that this shape is unprovable** — measured, rustc 1.97.0
/// (`scratchpad/polyrec.rs`): `#[derive(PartialEq, Eq)] struct N<T: Eq> { v: T, next:
/// Option<Box<N<Vec<T>>>> }` is `error[E0320]: overflow while adding drop-check rules for N<i32>`.
/// So refusing is not a Chezzi quirk; it is the same answer the owning ancestor gives.
///
/// **The ceiling is STACK SAFETY, not ambition, and it is measured.** Each level costs a full
/// `satisfies` → `satisfies_args_d` → walk → `eq_where_unsatisfied` → `satisfies` round trip, so the
/// bound has to fit the SMALLEST stack the checker runs on — the repo's `recursion-guard: size for
/// the smallest stack the path runs on` rule.
///
/// Do NOT re-derive that floor as "everything goes through [`crate::on_frontend_stack`]'s 1 GiB
/// thread". An earlier version of this comment said exactly that and it was WRONG: `editor::hover`
/// called `checker::hover_type` directly, so hover ran the whole checker on a ~2 MiB `chezzi-lsp`
/// tokio worker (`#[tokio::main]`, `rt-multi-thread`, no `stack_size`). That hole is closed —
/// `editor::hover` now wraps the same way `editor::diagnostics` does — but the bound is deliberately
/// NOT sized on the assumption that it stays closed.
///
/// Measured floor: the Rust test harness calls `check_graph` directly on its own thread, and a DEBUG
/// build (frames 3-5x release) survives a 200-link chain and aborts with `stack overflow` by 260.
/// 128 keeps ~40% headroom under that while being double the 64 that was refusing sound 63-link
/// chains. A stack overflow is an abort — strictly worse than any wrong answer — so this is the one
/// place the safe direction is "smaller", not "more permissive".
const EQ_BOUNDS_MAX_IN_PROGRESS: usize = 128;

/// The marker every budget refusal carries, so [`Checker::eq_where_unsatisfied`] can tell "the bound
/// genuinely failed" from "I ran out of budget proving it" and not reword the second into the first.
const EQ_BUDGET_MARKER: &str = "nests too deeply";

/// One live entry on [`EQ_BOUNDS_IN_PROGRESS`], popped on every exit path (including `?` and panic).
/// Minted only by [`Checker::enter_eq_obligation`], so an entry cannot be pushed without its pop.
struct EqObligation;

impl Drop for EqObligation {
    fn drop(&mut self) {
        EQ_BOUNDS_IN_PROGRESS.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// **The intrinsic-grant ↔ runtime-arm pairing table (W6-3's structural ratchet).**
///
/// One row per `(protocol, method, receiver-kind)` a built-in is granted conformance to
/// *intrinsically* — i.e. with NO user method behind it — by the [`Checker::grant_intrinsic`]
/// early-outs in [`Checker::satisfies_args_d`]. A row is a PROMISE that an erased generic body (or a
/// protocol-typed value) may CALL that method on that receiver kind, so each row MUST be **callable
/// at runtime** or the program type-checks and then faults `has no method` (bug-hunt wave-6 W6-3, the
/// check-OK-then-run-fault class).
///
/// **Keyed on the receiver KIND, not just the protocol** — that is the axis W6-3 actually failed on:
/// `compare`/`str` *were* paired, but their `Vm::do_method_call` interceptions were type-gated
/// narrower than the checker's grant set, so numeric newtypes and boxed scalars fell through. A
/// protocol-keyed table cannot express, and so cannot enforce, that obligation. The kind string is
/// [`Checker::intrinsic_recv_kind`]'s classification of the granted `Ty` (`"?"` for an unclassified
/// one, which matches no row and therefore trips the assert).
///
/// Which runtime site SERVES a row is deliberately not recorded — the three that do are the
/// pre-dispatch interceptions at the top of `Vm::do_method_call` (`compare`/`str` on a scalar, `iter`
/// on a collection, `next` on a struct-with-next), `Vm::core_method` (`Error.message`), and the
/// miss-only `Vm::intrinsic_proto_method` (everything else). The ratchet asserts CALLABILITY, which is
/// the invariant; pinning the site would just be a second thing to keep in sync.
///
/// The pairing is enforced three ways, so a NEW grant cannot silently skip its runtime arm:
/// * a bare `return Ok(())` in `satisfies_args_d` does not COMPILE — the success type is [`Grant`],
///   whose only constructors are [`Checker::grant_intrinsic`] (which consults this table) and
///   [`Grant::no_intrinsic_method`] (documented for the arms that grant no callable method);
/// * `grant_intrinsic` `debug_assert`s that the `(protocol, kind)` it is granting has a row here (or
///   in [`INTRINSIC_UNPAIRED`]) — so widening a grant to one more TYPE fails the suite;
/// * `vm::tests::intrinsic_grants_all_have_vm_arms` type-checks AND runs a generated probe per row on
///   both engines, so a row must be genuinely granted *and* genuinely callable.
///
/// Many-to-many on purpose: `IndexSet` contributes two methods, and `Index`/`IndexSet` share `index`.
/// `"struct"` is a coarsening — the `Hashable` grant is only for a ZERO-FIELD struct without its own
/// `hash`, and the `Iterable`/`Iterator` grants only for a struct with `iter`/`next` — so the probe
/// receiver for that kind is one struct that satisfies all three at once.
pub const INTRINSIC_PROTO_METHODS: &[(&str, &str, &str)] = &[
    // Comparable — int/float/str scalars + a numeric newtype (its `<` unwraps to the underlying).
    ("Comparable", "compare", "int"),
    ("Comparable", "compare", "float"),
    ("Comparable", "compare", "str"),
    ("Comparable", "compare", "newtype"),
    // Eq — D1: EVERY receiver kind whose `==` is the structural derive, which is every kind this
    // table can key a row on except `nil` (not spellable as a value). The four scalars are all here
    // because `==` is defined on `bool` too, unlike `Comparable`; a newtype's `==` unwraps to the
    // underlying's native equality, exactly as its `<` unwraps to the ordering. `option`/`result`
    // land on `Obj::Enum` at runtime, same as `enum`.
    ("Eq", "eq", "int"),
    ("Eq", "eq", "float"),
    ("Eq", "eq", "str"),
    ("Eq", "eq", "bool"),
    ("Eq", "eq", "bytes"),
    ("Eq", "eq", "bytearray"),
    ("Eq", "eq", "list"),
    ("Eq", "eq", "set"),
    ("Eq", "eq", "map"),
    ("Eq", "eq", "tuple"),
    ("Eq", "eq", "struct"),
    ("Eq", "eq", "enum"),
    ("Eq", "eq", "option"),
    ("Eq", "eq", "result"),
    ("Eq", "eq", "newtype"),
    // Stringable — all four scalars.
    ("Stringable", "str", "int"),
    ("Stringable", "str", "float"),
    ("Stringable", "str", "bool"),
    ("Stringable", "str", "str"),
    // Hashable — the scalar key types + a zero-field struct with no own `hash`.
    ("Hashable", "hash", "int"),
    ("Hashable", "hash", "str"),
    ("Hashable", "hash", "bytes"),
    ("Hashable", "hash", "bool"),
    ("Hashable", "hash", "struct"),
    // Error — `str`'s message is itself (Go model).
    ("Error", "message", "str"),
    // PathLike (W7-8) — the three byte-ish scalars a path can be spelled as. None of them HAS an
    // `as_path` method, so the grant is the only seam; `path.Path` conforms structurally instead.
    ("PathLike", "as_path", "str"),
    ("PathLike", "as_path", "bytes"),
    ("PathLike", "as_path", "bytearray"),
    // Iterable — everything `iterable_elem` accepts.
    ("Iterable", "iter", "list"),
    ("Iterable", "iter", "set"),
    ("Iterable", "iter", "map"),
    ("Iterable", "iter", "str"),
    ("Iterable", "iter", "bytes"),
    ("Iterable", "iter", "bytearray"),
    ("Iterable", "iter", "struct"),
    // Iterator — only a struct/cursor holds the position `next` needs (W6-3b: a raw collection does
    // NOT satisfy `Iterator`, only `Iterable`).
    ("Iterator", "next", "struct"),
    // Index / IndexSet / Slice — the built-in containers `index_kv`/`slice_result` accept
    // (`IndexSet` excludes the immutable `str`/`bytes`; `Slice` excludes `map`).
    ("Index", "index", "list"),
    ("Index", "index", "map"),
    ("Index", "index", "str"),
    ("Index", "index", "bytes"),
    ("Index", "index", "bytearray"),
    ("IndexSet", "index", "list"),
    ("IndexSet", "index", "map"),
    ("IndexSet", "index", "bytearray"),
    ("IndexSet", "set_index", "list"),
    ("IndexSet", "set_index", "map"),
    ("IndexSet", "set_index", "bytearray"),
    ("Slice", "slice", "list"),
    ("Slice", "slice", "str"),
    ("Slice", "slice", "bytes"),
    ("Slice", "slice", "bytearray"),
    // The operator protocols — int/float natively, plus a numeric newtype's unwrap→op→rewrap
    // auto-flow (`Neg` has no newtype path, so it is int/float only).
    ("Add", "add", "int"),
    ("Add", "add", "float"),
    ("Add", "add", "newtype"),
    ("Sub", "sub", "int"),
    ("Sub", "sub", "float"),
    ("Sub", "sub", "newtype"),
    ("Mul", "mul", "int"),
    ("Mul", "mul", "float"),
    ("Mul", "mul", "newtype"),
    ("Div", "div", "int"),
    ("Div", "div", "float"),
    ("Div", "div", "newtype"),
    ("Mod", "mod", "int"),
    ("Mod", "mod", "float"),
    ("Mod", "mod", "newtype"),
    ("Neg", "neg", "int"),
    ("Neg", "neg", "float"),
];

/// Intrinsic grants that have NO runtime arm and CANNOT get one — a KNOWN check-OK/run-fault, kept
/// here (rather than silently absent) so the carve-out is asserted instead of rotting.
///
/// **Currently none.** W6-3b retired the only entry: `Iterator[E]`'s `next` used to be granted to every
/// RAW collection (the grant was keyed on `iter_elem`, i.e. "can be iterated"), but `next` is stateful
/// and a raw collection holds no cursor position, so it check-OK'd and then faulted. The fix was the
/// coherent one — narrow the checker grant to real cursors/generators/`next`-structs (see
/// `satisfies_args_d`'s `Iterator` arm); a raw collection satisfies only `Iterable`.
///
/// The const and both `vm::tests` loops over it stay so the ratchet RE-ARMS the moment a new unpairable
/// grant is added: registering a row here (instead of in [`INTRINSIC_PROTO_METHODS`]) is the only way to
/// ship a grant with no runtime arm, and `vm::tests::intrinsic_grants_all_have_vm_arms` then asserts the
/// row is still granted and STILL faults.
pub const INTRINSIC_UNPAIRED: &[(&str, &str, &str)] = &[];

/// Proof that a conformance decision went through one of [`Checker::satisfies_args_d`]'s documented
/// grant paths — the compile-time half of the W6-3 ratchet. The field is private to this module, so a
/// new early-out CANNOT be written as a bare `return Ok(())`: the author must pick
/// [`Checker::grant_intrinsic`] (which registers the grant against [`INTRINSIC_PROTO_METHODS`]) or
/// [`Grant::no_intrinsic_method`], and reading either doc surfaces the pairing obligation. Carries no
/// data — conformance itself is still the `Ok`/`Err` discriminant.
#[must_use]
pub struct Grant(());

impl Grant {
    /// This conformance grants NO intrinsically-callable method, so it needs no runtime arm and no
    /// [`INTRINSIC_PROTO_METHODS`] row. Valid ONLY when the method the caller will go on to invoke is
    /// a real user method or nothing at all: the `Ty::Unknown` don't-cascade guard, an empty
    /// (top-type) protocol, a pure-embed bundle (each embed grants its own methods and is registered
    /// separately), a `Ty::Param` forwarding to its declared bounds, a protocol existential matching
    /// itself, and structural satisfaction via a user method table.
    ///
    /// If the arm you are writing makes a BUILT-IN satisfy a protocol with no user method behind it,
    /// this is the WRONG constructor — use [`Checker::grant_intrinsic`] and add the row.
    fn no_intrinsic_method() -> Self {
        Grant(())
    }
}

impl Checker {
    /// The receiver-KIND key [`INTRINSIC_PROTO_METHODS`] is indexed by — the granted `Ty`'s built-in
    /// shape, at the granularity the runtime dispatches on. `"?"` for anything unclassified, which
    /// matches no row and therefore trips `grant_intrinsic`'s assert (classify it and add its rows
    /// rather than widening this catch-all).
    fn intrinsic_recv_kind(ty: &Ty) -> &'static str {
        match ty {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Bool => "bool",
            Ty::Str => "str",
            Ty::Bytes => "bytes",
            Ty::ByteArray => "bytearray",
            Ty::Nil => "nil",
            Ty::List(_) => "list",
            Ty::Map(_, _) => "map",
            Ty::Set(_) => "set",
            Ty::Tuple(_) => "tuple",
            Ty::Struct(..) => "struct",
            Ty::Enum(..) => "enum",
            // `Option`/`Result` are their OWN `Ty`s but ONE runtime shape with `enum` (`Obj::Enum`);
            // they are kinds of their own here because the ratchet keys the CHECKER's grant set, and
            // the checker distinguishes them.
            Ty::Option(_) => "option",
            Ty::Result(..) => "result",
            Ty::NewType(..) => "newtype",
            _ => "?",
        }
    }

    /// The BUILT-IN cursor shape — `Ty::Struct("Iterator", [E])`, what `.iter()` mints and a
    /// generator returns. It is a `Ty::Struct` but NOT a struct at runtime (`Obj::Iterator` /
    /// `Obj::Generator`), and its dispatch arms deliberately expose only `next`/`iter`, so a
    /// protocol grant keyed on `"struct"` would be check-OK-then-`has no method` for it.
    fn is_cursor_ty(ty: &Ty) -> bool {
        matches!(ty, Ty::Struct(n, a) if n == "Iterator" && a.len() == 1)
    }

    /// Grant `ty` conformance to `protocol` INTRINSICALLY (no user method) — the single funnel every
    /// intrinsic early-out in [`Checker::satisfies_args_d`] returns through. It exists for the
    /// [`INTRINSIC_PROTO_METHODS`] `debug_assert`: granting a `(protocol, receiver-kind)` pair whose
    /// method is not callable at runtime trips here in the test suite instead of shipping a
    /// check-OK-then-run-fault. Behaviorally it is `Ok(_)` (the assert is debug-only), and it does NOT
    /// decide conformance — every caller already did.
    fn grant_intrinsic(&self, protocol: &str, ty: &Ty) -> Result<Grant, String> {
        let kind = Self::intrinsic_recv_kind(ty);
        debug_assert!(
            INTRINSIC_PROTO_METHODS
                .iter()
                .chain(INTRINSIC_UNPAIRED)
                .any(|(p, _, k)| *p == protocol && *k == kind),
            "intrinsic conformance granted for ({protocol}, {kind}) with no row in \
             INTRINSIC_PROTO_METHODS — add the (protocol, method, kind) row AND make the method \
             callable at runtime (`Vm::intrinsic_proto_method`), or register the row in \
             INTRINSIC_UNPAIRED (W6-3)"
        );
        Ok(Grant::no_intrinsic_method())
    }

    /// Register a `protocol` declaration's method signatures. `Self` resolves to `Ty::Param("Self")`.
    pub(super) fn hoist_protocol(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_reserved_protocol(name) {
            // Phase 5c-protocols — a reserved protocol's SHAPE is now ALSO declared in `std/prelude.chz`
            // as a plain `protocol` decl (a drift-guarded ADDITIVE mirror; see
            // `assert_native_protocol_shape_matches`). In a stdlib module that decl is VALIDATE-AND-NO-OP:
            // do NOT insert (the live source stays `prebuilt_protocols`, seeded at `Checker::new`) and do
            // NOT error — mirroring the `native struct`/`native enum` stdlib arms. In a USER module the
            // reserved name stays rejected (a user can't redeclare a builtin protocol).
            if !self.current_module_is_stdlib {
                self.error(span, format!("protocol '{name}' is reserved (builtin)"));
            }
            return;
        }
        // A protocol may not shadow a reserved builtin TYPE name either (`protocol List` / `protocol
        // int`). Sibling struct/enum/newtype/type-alias decl guards all reject `is_reserved_type`;
        // protocol was the sole decl path that omitted it. Ordered AFTER the reserved-protocol arm so
        // `Iterator` (both a reserved protocol AND type) is caught once, above, keeping its protocol
        // wording. The stdlib carve-out mirrors the reserved-protocol arm (no native protocol is named
        // after a reserved type, so it never fires there, but preserves symmetry).
        if is_reserved_type(name) {
            if !self.current_module_is_stdlib {
                self.error(span, format!("type '{name}' is reserved (builtin)"));
            }
            return;
        }
        if self.protocols.contains_key(name) {
            self.error(span, format!("protocol '{name}' is already defined"));
        }
        // A protocol's own type param may not be named after a reserved builtin type (`protocol
        // P[int]`) — same rule as struct/enum/newtype/fn params.
        self.reject_reserved_type_params(type_params);
        let mut saved = self.type_params.clone();
        std::mem::swap(&mut self.type_params, &mut saved); // start clean, with only Self visible
        self.type_params.insert("Self".to_string(), Vec::new());
        // The protocol's own type params are in scope while resolving its method signatures, so
        // `fn get(self, i: int) -> T` resolves `T` to `Ty::Param("T")`.
        for tp in type_params {
            if tp.name == "Self" {
                self.error(
                    span,
                    "protocol type parameter cannot be named 'Self'".to_string(),
                );
            }
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        for tp in type_params {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
        let sigs = methods
            .iter()
            .map(|m| {
                let params = m
                    .params
                    .iter()
                    .map(|p| match &p.ty {
                        Some(t) => self.resolve_type(t, span),
                        None if p.name == "self" => Ty::Unknown,
                        None => {
                            self.error(
                                span,
                                format!("protocol method parameter '{}' needs a type", p.name),
                            );
                            Ty::Unknown
                        }
                    })
                    .collect();
                let ret = m
                    .ret
                    .as_ref()
                    .map(|t| self.resolve_type(t, span))
                    .unwrap_or(Ty::Nil);
                // A protocol method whose first param is NOT `self` (or which has no params) is a STATIC
                // (associated) requirement — mirrors `Checker::fn_sig`'s rule for concrete methods. This
                // is load-bearing for `Convert`-style static-ctor protocols: it drives both the
                // static-slot witnessing check (`method_matches`) and the bound-only value-position gate
                // (`protocol_has_static_method`). Ordinary `fn get(self, …)` requirements are instance
                // methods (`is_static == false`), unchanged.
                let mut sig = FnSig::plain(params, ret);
                sig.is_static = m.params.first().is_none_or(|p| p.name != "self");
                (m.name.clone(), sig)
            })
            .collect();
        self.type_params = saved; // restore
        self.protocols.insert(
            name.to_string(),
            ProtocolInfo {
                type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                methods: sigs,
                embeds: embeds.to_vec(),
            },
        );
    }

    /// M22 — validate a protocol's embeds AFTER every protocol is hoisted (so forward/cyclic refs
    /// resolve). Three LOCKED rules: (1) an OWN `fn` whose name collides with any transitively-embedded
    /// required method → error, regardless of order or sig; (2) two embeds pulling the same method name
    /// with DIFFERING signatures → error (identical sigs dedup silently — the legal diamond); (3) a
    /// cyclic embed (A embeds B, B embeds A) → error. Built-in protocols (registered via
    /// [`prebuilt_protocols`], not here) are trusted and skipped.
    pub(super) fn validate_protocol_embeds(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_reserved_protocol(name) {
            return; // builtin (or already errored as a redeclaration / reserved name)
        }
        // Each embed must name a real protocol.
        for emb in embeds {
            if !self.protocols.contains_key(&emb.name) {
                self.error(
                    span,
                    format!("unknown protocol '{}' embedded in '{name}'", emb.name),
                );
            }
        }
        // An embed arg may mention one of the owner's type params only as the WHOLE arg
        // (`Contains[T]`), never nested inside a constructor (`Contains[List[T]]`). Re-spelling the
        // pulled-in signature happens through `embed_arg_tys`, which resolves an arg with `&self` —
        // and `resolve_ty_ro` reads the AMBIENT scope, where the owner's params are long gone. A
        // nested `T` therefore resolves to `Unknown`, and an `Unknown` element type accepts every
        // argument: `protocol Bag[T]: Contains[List[T]]` let `["x"] in b` pass on a `Bag[int]` and
        // then fault at runtime. DECLINE rather than answer wrongly — a rejected declaration is
        // recoverable, a silently permissive one teaches distrust of the whole check.
        let own: Vec<String> = type_params.iter().map(|tp| tp.name.clone()).collect();
        for emb in embeds {
            for a in &emb.args {
                // An embed arg naming a type that does not exist (`Contains[T]` where the protocol
                // declares no `T`) resolves to `Ty::Unknown`, and an `Unknown` element type accepts
                // EVERY operand — `"oops" in b` type-checked on a `Bag` whose `contains` takes an
                // `int`, then faulted. Unknown-as-permissive is the same hazard as the nested case
                // below, reached by a typo instead of a nesting; both must be a hard error, not a
                // silently wide requirement. Every struct/enum/alias/protocol is hoisted before this
                // runs, so an unresolvable name here really is unresolvable.
                if let Some(bad) = first_unresolvable_name(a, &own, &|n| {
                    !self.resolve_ty_ro(&Type::named(n)).is_unknown()
                }) {
                    self.error(
                        span,
                        format!(
                            "unknown type '{bad}' in the type argument of embedded protocol '{}' \
                             in '{name}'",
                            emb.name
                        ),
                    );
                    continue;
                }
                if !matches!(a, Type::Named { name: n, .. } if own.contains(n))
                    && type_mentions_any(a, &own)
                {
                    self.error(
                        span,
                        format!(
                            "embedded protocol '{}' in '{name}' uses a type parameter nested inside \
                             a type argument, which is not supported — pass the parameter directly \
                             ({}[{}]) or spell the requirement as an own `fn`",
                            emb.name,
                            emb.name,
                            own.join(", ")
                        ),
                    );
                }
            }
        }
        // Flatten the transitive embed method set, detecting cycles + cross-embed signature conflicts.
        let mut path = vec![name.to_string()];
        let (required, cyclic, conflict) = self.flatten_embed_methods(embeds, &mut path);
        if cyclic {
            self.error(
                span,
                format!("cyclic protocol embedding involving '{name}'"),
            );
            return;
        }
        if let Some(m) = conflict {
            self.error(
                span,
                format!("conflicting signature for method '{m}' from embedded protocols"),
            );
        }
        // Rule 1: an own `fn` colliding with an embedded-required method name.
        for m in methods {
            if required.contains_key(&m.name) {
                self.error(
                    span,
                    format!(
                        "method '{}' conflicts with embedded protocol requirement",
                        m.name
                    ),
                );
            }
        }
    }

    /// Recursively collect the method signatures required by a list of embeds. Returns
    /// `(required, cyclic, conflict)`: `required` maps method name → signature (identical sigs deduped),
    /// `cyclic` is set if any embed revisits a protocol on the current `path`, and `conflict` names a
    /// method seen with two DIFFERING signatures (rule 2). Read-only over `self.protocols`.
    pub(super) fn flatten_embed_methods(
        &self,
        embeds: &[Bound],
        path: &mut Vec<String>,
    ) -> (HashMap<String, FnSig>, bool, Option<String>) {
        self.flatten_embed_methods_seen(embeds, path, &mut HashSet::new())
    }

    /// `seen` is what bounds the walk. `path` detects a CYCLE (a name revisited on the current
    /// branch) but does nothing about SHARING: a DAG where each protocol embeds two others re-walks
    /// every shared subtree once per route, which is exponential — a 42-protocol chain of
    /// `Pi: P(i+1), P(i+2)` hung `check` (and the LSP) past 25 s on the DECLARATION alone, before any
    /// use. Skipping an already-walked protocol is result-preserving: its methods are already merged
    /// into `required`, and re-merging identical signatures is what the diamond rule already dedups.
    /// The `path` check stays FIRST so a cycle is still reported rather than silently skipped.
    fn flatten_embed_methods_seen(
        &self,
        embeds: &[Bound],
        path: &mut Vec<String>,
        seen: &mut HashSet<String>,
    ) -> (HashMap<String, FnSig>, bool, Option<String>) {
        let mut required: HashMap<String, FnSig> = HashMap::new();
        let mut conflict: Option<String> = None;
        let mut merge = |mn: &str, ms: &FnSig, conflict: &mut Option<String>| {
            match required.get(mn) {
                Some(existing) if !fn_sig_eq(existing, ms) => {
                    if conflict.is_none() {
                        *conflict = Some(mn.to_string());
                    }
                }
                Some(_) => {} // identical sig — dedup silently (legal diamond)
                None => {
                    required.insert(mn.to_string(), ms.clone());
                }
            }
        };
        for emb in embeds {
            if path.iter().any(|p| p == &emb.name) {
                return (required, true, conflict);
            }
            let Some(pinfo) = self.protocols.get(&emb.name).cloned() else {
                continue; // unknown embed already errored in validate_protocol_embeds
            };
            if !seen.insert(emb.name.clone()) {
                continue; // already merged on this walk — a shared subtree, not new work
            }
            for (mn, ms) in &pinfo.methods {
                merge(mn, ms, &mut conflict);
            }
            path.push(emb.name.clone());
            let (sub, cyclic, sub_conf) =
                self.flatten_embed_methods_seen(&pinfo.embeds, path, seen);
            path.pop();
            if cyclic {
                return (required, true, conflict);
            }
            for (mn, ms) in &sub {
                merge(mn, ms, &mut conflict);
            }
            if conflict.is_none() {
                conflict = sub_conf;
            }
        }
        (required, false, conflict)
    }

    /// M22 — resolve `method` on protocol `pname`: OWN methods first, then transitively through the
    /// embeds, substituting each embed's args into the pulled-in signature so it speaks the OUTER
    /// protocol's type-param vocabulary (`protocol Bag[T]: Contains[T]` ⇒ `contains(self, T) -> bool`,
    /// so a `Bag[int]` receiver witnesses `int`). This is what makes an embedded method callable
    /// through an interface value and through a bound — `spec.md`'s "flattened at every use site".
    ///
    /// Deliberately NOT built on [`Self::flatten_embed_methods`]: that one builds the whole map and
    /// does not substitute `Bound.args` (its callers only inspect `is_static`), which would type an
    /// embedded method in the EMBEDDED protocol's vocabulary. Depth-capped like `bound_provides` —
    /// a cyclic embed is rejected at declare time but still reaches here after erroring.
    pub(super) fn protocol_method_sig(&self, pname: &str, method: &str) -> Option<FnSig> {
        self.protocol_method_sig_d(pname, method, &mut HashSet::new())
    }

    /// `seen` is what makes this terminate, not a depth cap: with branching ≥ 2 a depth bound of 64
    /// is 2^64 visits, so a diamond DAG (or a cyclic decl, which is an error but still reaches here)
    /// hangs `check` and the LSP on a MISS — every embed gets explored before `None` is returned.
    /// Visiting each protocol once is also semantically free: the first hit wins either way.
    fn protocol_method_sig_d(
        &self,
        pname: &str,
        method: &str,
        seen: &mut HashSet<String>,
    ) -> Option<FnSig> {
        if !seen.insert(pname.to_string()) {
            return None;
        }
        let pinfo = self.protocols.get(pname)?;
        if let Some((_, sig)) = pinfo.methods.iter().find(|(n, _)| n == method) {
            return Some(sig.clone());
        }
        for emb in &pinfo.embeds {
            let Some(sig) = self.protocol_method_sig_d(&emb.name, method, seen) else {
                continue;
            };
            // The recovered sig is spelled in `emb.name`'s params; re-spell it in ours.
            let etps = self
                .protocols
                .get(&emb.name)
                .map(|p| p.type_params.clone())
                .unwrap_or_default();
            let map: HashMap<String, Ty> = etps
                .into_iter()
                .zip(self.embed_arg_tys(pinfo, emb))
                .collect();
            return Some(subst_sig(&sig, &map));
        }
        None
    }

    /// M22 + object safety — the name of a method this protocol requires, own OR through any embed,
    /// whose signature takes `Self`. `Some(name)` ⇒ no existential can be a witness for it.
    ///
    /// The flattened set is the point: `protocol Vecish: Add` has NO own method taking `Self` — the
    /// `add(self, o: Self) -> Self` arrives through the embed — so an own-methods-only check let the
    /// commonest spelling of the hazard straight through. Embed-arg substitution is irrelevant here
    /// (`Self` is not a protocol type param, so no substitution can introduce or remove it), which
    /// is why `flatten_embed_methods` is enough and the re-spelling walk is not needed.
    pub(super) fn protocol_self_param_method(&self, p: &str) -> Option<String> {
        let pinfo = self.protocols.get(p)?;
        if let Some((n, _)) = pinfo
            .methods
            .iter()
            .find(|(_, s)| self_in_param_position(s))
        {
            return Some(n.clone());
        }
        let mut path = vec![p.to_string()];
        let (required, _cyclic, _conflict) = self.flatten_embed_methods(&pinfo.embeds, &mut path);
        let mut hits: Vec<&String> = required
            .iter()
            .filter(|(_, s)| self_in_param_position(s))
            .map(|(n, _)| n)
            .collect();
        hits.sort(); // `required` is a HashMap — pick deterministically so the diagnostic is stable
        hits.first().map(|n| (*n).clone())
    }

    /// Resolve an embed's type args in the OWNING protocol's vocabulary: a bare name that is one of
    /// `owner`'s own type params becomes `Ty::Param(name)`.
    ///
    /// [`Self::resolve_ty_ro`] reads `self.type_params`, which at a USE site is the *calling*
    /// function's params — the owning protocol's are long out of scope. So without this,
    /// `protocol Bag[T]: Contains[T]` resolved its `T` to `Ty::Unknown`, which made every embedded
    /// method's arg silently permissive (`"x" in b` type-checked on a `Bag[int]`), and resolved it
    /// to the CALLER's `T` whenever one happened to share the name.
    fn embed_arg_tys(&self, owner: &ProtocolInfo, emb: &Bound) -> Vec<Ty> {
        emb.args
            .iter()
            .map(|a| match a {
                Type::Named { name, .. } if owner.type_params.contains(name) => {
                    Ty::Param(name.clone())
                }
                _ => self.resolve_ty_ro(a),
            })
            .collect()
    }

    /// Does concrete `ty` structurally satisfy `protocol`? Read-only. Primitives intrinsically
    /// satisfy `Comparable`; structs satisfy any protocol whose methods they all implement.
    /// Valid `map` key / `set` element types: anything that satisfies the `Hashable` protocol —
    /// the scalars `int`/`str`/`bool` intrinsically, or a struct defining `hash(self) -> int`.
    /// `float` is rejected (NaN/equality footgun); `Unknown` is tolerated (no cascade). With this,
    /// user structs can be map keys / set elements, hashed via their `hash()` at runtime.
    pub(super) fn is_hashable_key(&self, t: &Ty) -> bool {
        self.satisfies(t, "Hashable").is_ok()
    }

    /// Refine-on-first-use (empty-slot half of the `Ty::Unknown` soundness family). A bare empty
    /// collection literal (`[]`/`{}`/`set()`), a nullary user-enum variant (`Box.Empty`), or the
    /// native nullary `None` types its element/key/value/type-arg slot as `Ty::Unknown`, which is
    /// permissive in both directions — so junk would flow into a check-blessed program and fault at
    /// runtime, and the float-key/Hashable ban would be bypassed. This hook fires at the top of
    /// `infer_method_call`, when a mutating method (`push`/`add`/`insert`/`extend`) on a
    /// **simple-variable** receiver supplies a CONCRETE type at an `Unknown` slot: it structurally
    /// merges the supplied shape into the binding, re-pins it in its owning scope, and runs the
    /// Hashable check on a newly-concrete set element. A later op supplying an incompatible concrete
    /// type then fails as a normal `check_args` mismatch against the now-pinned element — and the
    /// mismatch diagnostic is enriched (in `check_args`) to hint at annotating for a mixed/protocol
    /// collection.
    ///
    /// RESIDUAL HOLE (documented, not fixed here): refine only fires when the receiver is a simple
    /// `Ident` in scope. `obj.field.push(...)` / `f().push(...)` / `xss[0].push(...)` (non-Ident
    /// receivers) stay unrefined — struct fields are explicitly typed anyway, so the impact is low.
    pub(super) fn refine_receiver(&mut self, obj: &Expr, obj_ty: &Ty, method: &str, args: &[Expr]) {
        // (a) simple-variable receiver only (the documented limitation).
        let ExprKind::Ident(name) = &obj.kind else {
            return;
        };
        // Must be a real in-scope binding (not a function/global-type name).
        if self.lookup(name).is_none() {
            return;
        }
        // PART A: a slot-supplying mutator (`push`/`add`/`insert`/`extend` with an arg) constrains
        // this binding's element type, so clear any pending empty-collection annotation requirement.
        // Done BEFORE the `is_captured` early-return below so a `spawn:`/`Executor.submit` body that
        // supplies the element only via a mutator (`acc := []` outside, `acc.push(1)` captured) still
        // drops the site — the element WAS supplied, so requiring an annotation would be wrong. A
        // no-op when no site exists (`drop_empty_site` only removes a matching `(owner, name)`).
        if matches!(method, "push" | "add" | "insert" | "extend") && !args.is_empty() {
            self.drop_empty_site(name);
        }
        // Skip captured bindings: mirror the airlock reassignment ban — refine is a checker-side
        // narrowing, but skipping it here keeps behavior aligned and avoids a confusing diagnostic.
        if self.is_captured(name) {
            return;
        }
        // (b) the binding must have an Unknown in a SLOT position (not a bare top-level Unknown —
        // that's the cascade-suppression sentinel and must stay permissive).
        if !contains_unknown_in_slot(obj_ty) {
            return;
        }
        // (c) determine the supplied ELEMENT type from a slot-supplying mutator's args.
        // `push(x)`/`add(x)`/`insert(x)` supply the element directly; `extend(xs)` supplies a
        // list/set whose element refines ours.
        let mark = self.errors.len();
        let elem = match method {
            "push" | "add" | "insert" => args.first().map(|a| self.infer_value(a)),
            "extend" => args.first().map(|a| match self.infer_value(a) {
                Ty::List(e) | Ty::Set(e) => *e,
                other => other,
            }),
            _ => return,
        };
        let Some(elem) = elem else { return };
        // Wrap the element into a RECEIVER-SHAPED value so the structural merge lines up the slot:
        // a list receiver merges with `list[elem]`, a set receiver with `set[elem]`. Any other
        // receiver kind isn't a push/add/extend target, so nothing to refine.
        let shape = match obj_ty {
            Ty::List(_) => Ty::list(elem),
            Ty::Set(_) => Ty::set(elem),
            _ => return,
        };
        // (d) cascade invariant: if inferring the arg itself reported an error, don't refine — and
        // roll back the speculative diagnostics so the real dispatch path (check_args) reports them
        // exactly once. Leaving them here double-reports an erroring arg (e.g. `xs.push(undefined)`).
        if self.errors.len() != mark {
            self.errors.truncate(mark);
            return;
        }
        // A shape that is itself Unknown supplies nothing concrete; merge is a no-op, bail early.
        if shape.is_unknown() {
            return;
        }
        let merged = merge_unknown(obj_ty, &shape);
        if merged == *obj_ty {
            return; // nothing newly concrete
        }
        // Run the Hashable / float-key ban at the moment a SET element becomes concrete (the sig
        // tables don't). Map keys are handled in the `m[k]=v` index-assign refine path.
        if let Ty::Set(e) = &merged
            && !e.is_unknown()
            && !self.is_hashable_key(e)
        {
            self.error(
                obj.span,
                format!("set element type must implement Hashable (int, str, bool, or a struct/enum/newtype defining hash(self) -> int), found {e}"),
            );
        }
        self.repin(name, merged);
    }

    /// Refine-on-first-use for an index-assign `m[k]=v` / `xs[i]=v` (the assignment-statement
    /// sibling of [`Self::refine_receiver`]). When the receiver is a simple variable whose type has
    /// an `Unknown` key/value/element slot, merge the supplied (index type, value type) shape into
    /// the binding, re-pin it, and run the Hashable / float-key ban on a newly-concrete MAP key.
    /// `val_ty` is already inferred by the caller; we infer the index type here only when the
    /// receiver is actually refinable (so we don't double-report on the common already-typed path).
    pub(super) fn refine_index_receiver(&mut self, obj: &Expr, index: &Expr, val_ty: &Ty) {
        let ExprKind::Ident(name) = &obj.kind else {
            return;
        };
        let Some(obj_ty) = self.lookup(name) else {
            return;
        };
        if self.is_captured(name) || !contains_unknown_in_slot(&obj_ty) {
            return;
        }
        // PART A: an index-assign (`m[k]=v` / `xs[i]=v`) constrains this binding — clear any pending
        // empty-collection annotation requirement (BEFORE the speculative-index-infer truncate-return,
        // mirroring `refine_receiver`, so an erroring key like `m[undefined_k]=1` still drops the site).
        self.drop_empty_site(name);
        if val_ty.is_unknown() {
            return;
        }
        // The supplied shape mirrors the receiver kind: `Map(idx, val)` for a map, `List(val)` for a
        // list (index type is the int position, irrelevant to the element slot).
        let mark = self.errors.len();
        let shape = match &obj_ty {
            Ty::Map(..) => Ty::map(self.infer(index), val_ty.clone()),
            Ty::List(..) => Ty::list(val_ty.clone()),
            _ => return,
        };
        if self.errors.len() != mark {
            self.errors.truncate(mark); // roll back the speculative index-infer diagnostics; the
            return; // real index-assign path re-infers + reports them once (no double-report)
        }
        let merged = merge_unknown(&obj_ty, &shape);
        if merged == obj_ty {
            return;
        }
        // NOTE: the map-key Hashable / float-key ban is NOT run here — it is the direct
        // insertion-site check in `check_assign`'s Index branch (so it fires even while the key type
        // is still `Unknown`, e.g. `m:={}; m[1.5]=..`), keeping a single owner and no double-report.
        self.repin(name, merged);
    }

    /// Assignability with protocol-existential awareness. Like the free [`compatible`], but a
    /// concrete type is assignable to a `Protocol(P)` slot iff it satisfies `P` — which needs the
    /// protocol/struct registry, so it can't live in the context-free `compatible`. Recurses through
    /// compound types so a nested existential (the `E` in `Result[T, Error]`) is checked structurally.
    /// Strict assignability — NO int→float widening (the reverse `float`→`int` is always rejected).
    pub(super) fn assignable(&self, expected: &Ty, actual: &Ty) -> bool {
        use Ty::*;
        match (expected, actual) {
            (Unknown, _) | (_, Unknown) => true,
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
            (Protocol(p, pargs), a) => self.satisfies_args(a, p, pargs).is_ok() && self.sendable(a),
            // `Option`/`Result` are IMMUTABLE carriers — covariant element assignment stays sound
            // (no write-through alias), so they keep recursing via `assignable`. `List`/`Set`/`Map`
            // and user generic `Struct`/`Enum` are MUTABLE, by-reference containers: covariant type
            // args are a soundness hole (a `G[Sub]` bound aliased as `G[Super]` can have a Super value
            // written back into it — see `invariance_rejects_*` tests). Their type ARGUMENTS are
            // therefore compared with the context-free structural-equality primitive `compatible`
            // (= strict INVARIANCE), mirroring the M14 `compatible` Protocol/Struct/Enum arms and
            // `bound_args_match`. Docs: spec.md "strictly invariant"; future.md "no covariance holes".
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
            // Labels are surface-only: assignability matches on arity + param/ret only (`..`).
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
                    && p1.iter().zip(p2).all(|(a, b)| self.assignable(a, b))
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
            // A prior error, or an un-refined empty collection — never cascade off it.
            (Unknown, _) | (_, Unknown) => true,
            // ERASED: a generic body is checked once with `T` abstract, so any concrete pairing is
            // possible at some call site. (A `where T: <scalar>` param is NOT erased — it is an
            // equality bound pinning `T` to that scalar, and `infer_binary`'s `Eq` arm substitutes
            // those pins away BEFORE calling this, so `T: str` vs `int` still rejects.)
            (Param(_), _) | (_, Param(_)) => true,
            // CEILING, deliberate: two protocols with contradictory method signatures (`fn f() -> int`
            // vs `fn f() -> str`) are inhabited by NOTHING, yet this accepts the pair. Tightening it
            // means intersecting two method tables; the rationale for not doing so lives in
            // `docs/gaps.md` §B2 — read it there before narrowing this arm.
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
            // `[1.0, 2.0] == [1, 2]` and `{"k": 1.0} == {"k": 1}` answer `true` on both engines
            // (CPython agrees), so rejecting them would have been a lie about "provably disjoint".
            (Int, Float) | (Float, Int) | (Bytes, ByteArray) | (ByteArray, Bytes) => true,
            // The native generic HANDLES belong here too, not on the `_ => compatible` fall-through:
            // their `==` is the identity shortcut (`ha == hb`) at the top of `values_equal_guarded`,
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
            (Struct(n, a), Struct(m, b))
            | (Enum(n, a), Enum(m, b))
            | (NewType(n, a), NewType(m, b)) => {
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
            _ => compatible(l, r),
        }
    }

    /// Like [`Checker::assignable`], but accepts **one-way int→float widening** (`(Float, Int)` only)
    /// at a SCALAR value-DEFINITION sink (typed `let`, function/struct/method arg, return,
    /// param/field default).
    ///
    /// `widen` is NOT "this is a float sink" — it is "this expression is an untyped int CONSTANT"
    /// ([`crate::ast::untyped_int_const`]), which is what every caller must pass. Go's rule: an
    /// untyped constant adapts to a float context; a TYPED int value never implicitly converts (the
    /// user writes `float(x)`). That distinction is what the old blanket `widen=true` lacked — it
    /// accepted `i := 1; x: float = i`, which the type-blind compiler happily lowered as an `Int`
    /// sitting in a static `float` slot (int overflow under a float type, an unsorted `List[float]`,
    /// an `f64` load over an int payload once a JIT exists).
    ///
    /// Widening is still NOT propagated into ANY compound position (list/set/option element,
    /// map/result value, struct/tuple/func) — only a scalar `float` sink emits `Op::CoerceFloat`.
    /// Collection floats come instead from mixed-literal element inference (`[1, 2.3]` infers
    /// `list[float]`), whose own widen gate (`Checker::elem_widen_ok`) fires only where the compiler
    /// is guaranteed to coerce. `widen=false` ⇒ identical to [`Checker::assignable`].
    pub(super) fn assignable_w(&self, expected: &Ty, actual: &Ty, widen: bool) -> bool {
        if widen && matches!((expected, actual), (Ty::Float, Ty::Int)) {
            return true;
        }
        self.assignable(expected, actual)
    }

    pub(super) fn satisfies(&self, ty: &Ty, protocol: &str) -> Result<(), String> {
        self.satisfies_args(ty, protocol, &[])
    }

    /// Do a declared bound's type args (AST `Type`s) match the `required` ones (resolved `Ty`s) for a
    /// forwarded parameterized bound? Read-only — used inside `satisfies_args`. Conservative: only a
    /// *fully concrete* mismatch is rejected (so a still-generic arg like a sibling type param keeps
    /// forwarding loosely, as before), which is what closes the `Container[str]`→`Container[int]` hole
    /// without breaking valid `[S: Iterator[T], T]` forwards.
    pub(super) fn bound_args_match(&self, bound_args: &[Type], required: &[Ty]) -> bool {
        if bound_args.len() != required.len() {
            return false;
        }
        bound_args.iter().zip(required).all(|(ba, want)| {
            let bt = self.resolve_ty_ro(ba);
            !ty_fully_concrete(&bt) || !ty_fully_concrete(want) || compatible(&bt, want)
        })
    }

    /// M22 — does a declared bound `bound_name[bound_args]` PROVIDE `protocol[required]`, directly or
    /// transitively through embedded (super-)protocols? This is what makes `a + b` / `a / b` legal
    /// inside a `[T: Arithmetic]` body (the bound `Arithmetic` flattens to Add/Sub/Mul/Div) and lets a
    /// `[T: Arithmetic]` value forward into a `[U: Div]` call. Preserves the `Iterator`→`Iterable`
    /// subsumption. Depth-capped against a (declare-time-rejected, but still-checked) cyclic embed.
    pub(super) fn bound_provides(
        &self,
        bound_name: &str,
        bound_args: &[Type],
        protocol: &str,
        required: &[Ty],
        depth: usize,
    ) -> bool {
        if depth > 64 {
            return false;
        }
        // Direct: the bound names the required protocol with matching args.
        if bound_name == protocol && self.bound_args_match(bound_args, required) {
            return true;
        }
        // Subsumption: every `Iterator[T]` IS `Iterable[T]` (its `iter()` returns self).
        if protocol == "Iterable"
            && bound_name == "Iterator"
            && self.bound_args_match(bound_args, required)
        {
            return true;
        }
        // Transitive: any embed of the bound's protocol provides it.
        if let Some(pinfo) = self.protocols.get(bound_name) {
            return pinfo
                .embeds
                .iter()
                .any(|e| self.bound_provides(&e.name, &e.args, protocol, required, depth + 1));
        }
        false
    }

    /// M22 — does protocol `p`, with its own type params bound to `pargs`, BE or transitively EMBED
    /// `protocol` with args matching `required`? The `Ty`-level twin of [`Self::bound_provides`],
    /// which answers the same question for a declared BOUND (whose args are still AST `Type`s).
    /// Each embed's args are resolved and then re-spelled through `p`'s own bindings, so
    /// `protocol P[T]: Container[T]` at `P[int]` provides `Container[int]` and not `Container[T]`.
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
        if p == protocol
            && pargs.len() == required.len()
            && pargs.iter().zip(required).all(|(x, y)| compatible(x, y))
        {
            return true;
        }
        let Some(pinfo) = self.protocols.get(p) else {
            return false;
        };
        let map: HashMap<String, Ty> = pinfo
            .type_params
            .iter()
            .cloned()
            .zip(pargs.iter().cloned())
            .collect();
        pinfo.embeds.iter().any(|e| {
            let eargs: Vec<Ty> = self
                .embed_arg_tys(pinfo, e)
                .iter()
                .map(|t| subst(t, &map))
                .collect();
            self.protocol_provides_d(&e.name, &eargs, protocol, required, seen)
        })
    }

    /// Read-only type resolution (no error emission), for contexts that only hold `&self`. Returns
    /// `Ty::Unknown` for anything it can't resolve, which callers treat permissively.
    pub(super) fn resolve_ty_ro(&self, t: &Type) -> Ty {
        self.resolve_ty_ro_d(t, 0)
    }

    /// Depth-bounded read-only type resolution. `depth` guards against a recursive alias body
    /// (`type A = B; type B = A`): without `alias_resolving` (this is `&self`), a hard cap of 64
    /// expansions returns `Ty::Unknown` instead of overflowing the stack. 64 is far beyond any real
    /// alias chain.
    pub(super) fn resolve_ty_ro_d(&self, t: &Type, depth: usize) -> Ty {
        if depth > 64 {
            return Ty::Unknown;
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "bytes" => Ty::Bytes,
                "bytearray" => Ty::ByteArray,
                "nil" => Ty::Nil,
                "AtomicInt" => Ty::AtomicInt,
                "Executor" => Ty::Executor,
                "Socket" => Ty::Socket,
                "Listener" => Ty::Listener,
                "Writer" => Ty::Writer,
                "Reader" => Ty::Reader,
                "ptr" => Ty::Ptr,
                "owned_str" => Ty::Str,
                _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                // A fixed-width FFI integer name resolves to plain `int` (the width is a marshalling
                // detail). Needed so an exported alias body `type Len = int32` captures `Ty::Int`.
                _ if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) => Ty::Int,
                // A bare alias name resolves to its (recursively-resolved) body.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ty_ro_d(&body, depth + 1)
                }
                _ if self.imported_alias_tys.contains_key(n) => self.imported_alias_tys[n].clone(),
                _ if self.struct_names.contains(n) => Ty::strukt(self.bare_key(n)),
                _ if self.enum_names.contains(n) => Ty::Enum(self.bare_key(n), Vec::new()),
                _ if self.newtype_names.contains(n) => Ty::NewType(self.bare_key(n), Vec::new()),
                _ if self.protocols.contains_key(n) => Ty::Protocol(n.clone(), Vec::new()),
                _ => Ty::Unknown,
            },
            Type::Generic(n, args, ..) => match (n.as_str(), args.as_slice()) {
                ("List", [x]) => Ty::list(self.resolve_ty_ro_d(x, depth + 1)),
                ("Set", [x]) => Ty::set(self.resolve_ty_ro_d(x, depth + 1)),
                ("Option", [x]) => Ty::option(self.resolve_ty_ro_d(x, depth + 1)),
                ("Channel", [x]) => Ty::channel(self.resolve_ty_ro_d(x, depth + 1)),
                ("Shared", [x]) => Ty::shared(self.resolve_ty_ro_d(x, depth + 1)),
                ("RwShared", [x]) => Ty::rwshared(self.resolve_ty_ro_d(x, depth + 1)),
                ("Atomic", [x]) => Ty::atomic(self.resolve_ty_ro_d(x, depth + 1)),
                ("Result", [x]) => Ty::result(self.resolve_ty_ro_d(x, depth + 1)),
                ("Result", [x, e]) => Ty::result_e(
                    self.resolve_ty_ro_d(x, depth + 1),
                    self.resolve_ty_ro_d(e, depth + 1),
                ),
                ("Map", [k, v]) => Ty::map(
                    self.resolve_ty_ro_d(k, depth + 1),
                    self.resolve_ty_ro_d(v, depth + 1),
                ),
                // `Iterator[T]` is an existential ITERATOR value, represented as `Ty::Struct("Iterator",
                // [T])` — NOT a protocol. Mirror the mutable `resolve_type` arm (sig.rs) so the two
                // resolvers agree on identical syntax; without this it would fall to the protocol arm
                // below and mint `Protocol("Iterator",[T])`, which downstream Iterator logic (keyed on
                // `Ty::Struct(name=="Iterator")`) would not recognize.
                ("Iterator", [elem]) => Ty::Struct(
                    "Iterator".to_string(),
                    vec![self.resolve_ty_ro_d(elem, depth + 1)],
                ),
                _ if self.struct_names.contains(n) => Ty::Struct(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if self.enum_names.contains(n) => Ty::Enum(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if self.newtype_names.contains(n) => Ty::NewType(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                // A parameterized protocol used as a value type (`Container[int]`). Mint the carried
                // args so the read-only resolver no longer silent-accepts it as `Unknown` (which would
                // erase the witness). Mirrors the mutable `resolve_type` protocol arm.
                _ if self.protocols.contains_key(n) => Ty::Protocol(
                    n.clone(),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ => Ty::Unknown,
            },
            Type::Func {
                params,
                ret,
                labels,
            } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.resolve_ty_ro_d(p, depth + 1))
                    .collect(),
                ret: Box::new(self.resolve_ty_ro_d(ret, depth + 1)),
                labels: FnLabels(labels.clone()),
            },
            Type::Tuple(ts) => Ty::Tuple(
                ts.iter()
                    .map(|t| self.resolve_ty_ro_d(t, depth + 1))
                    .collect(),
            ),
            Type::Qualified { module, name, args } => {
                let resolved_args: Vec<Ty> = args
                    .iter()
                    .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                    .collect();
                self.resolve_qualified_ro(module, name, &resolved_args)
            }
        }
    }

    /// Read-only resolution of a module-qualified type `module.name[args]` to a `Ty`. Looks the bound
    /// module up in `imported_modules`, finds the type in its `ModuleSig`, and returns the matching
    /// `Ty` (struct / enum / alias body). `Ty::Unknown` if anything is missing (callers permissive).
    pub(super) fn resolve_qualified_ro(&self, module: &str, name: &str, args: &[Ty]) -> Ty {
        let Some(mid) = self.imported_modules.get(module) else {
            return Ty::Unknown;
        };
        let Some(sig) = self.module_sigs.get(mid) else {
            return Ty::Unknown;
        };
        // A RESERVED native type (Shared/RwShared/Atomic/Executor/Socket/Listener) has a harvested
        // `sig.struct_defs` METHOD-table entry but is NOT nominal — skip it here so it resolves to the
        // reserved builtin `Ty` via the `sig.types` branch below (mirrors `resolve_type`'s guard).
        if sig.struct_defs.contains_key(name) && self.qualified_builtin_ty(name, &[]).is_none() {
            Ty::Struct(self.type_key(mid, name), args.to_vec())
        } else if sig.enum_defs.contains_key(name) {
            Ty::Enum(self.type_key(mid, name), args.to_vec())
        } else if sig.newtype_defs.contains_key(name) {
            Ty::NewType(self.type_key(mid, name), args.to_vec())
        } else if let Some(asig) = sig.type_aliases.get(name) {
            asig.body.clone()
        } else if sig.types.contains(name) {
            // Mirror `resolve_type`'s qualified builtin branch on the READ-ONLY export path (so an
            // EXPORTED `type S = concurrency.Shared[int]` / `newtype MyS[T] = concurrency.Shared[T]`
            // resolved via `resolve_ty_ro_d` carries the right builtin `Ty`). Permissive: no errors,
            // and a non-type `sig.types` name (`timer`) returns `Ty::Unknown`.
            self.qualified_builtin_ty(name, args).unwrap_or(Ty::Unknown)
        } else {
            Ty::Unknown
        }
    }

    /// Resolve a surface extern `Type` to its WIDTH-BEARING [`CType`] in the CURRENT module's
    /// import/alias scope — the SINGLE resolver both backends consume (the FFI collision-fix root).
    /// Mirrors `resolve_ty_ro_d`'s alias / `from`-import / `Qualified` walk EXACTLY, but stops at the
    /// width-bearing leaf instead of collapsing every FFI integer to `Ty::Int`. Crucially:
    ///   * a LOCAL alias (`self.aliases`) recurses on its body — resolving each hop in THIS module's
    ///     scope (a colliding same-named alias in another module can never be reached);
    ///   * a `from`-imported alias reads `self.imported_alias_ctypes` (the alias's CType computed in
    ///     its DEFINING module's scope) — closing the named-import-hop hole;
    ///   * a `module.Alias` reads the TARGET module's `AliasSig.ctype` (likewise defining-scope).
    ///
    /// `depth > 64` returns `None` (cycle guard, matching `resolve_ty_ro_d`). `None` means "not
    /// C-marshallable here" — the marshallability gate (`assert_marshallable`) is the actual error,
    /// this is only the width carrier.
    pub(super) fn resolve_ctype(&self, t: &Type) -> Option<CType> {
        self.resolve_ctype_d(t, 0)
    }

    pub(super) fn resolve_ctype_d(&self, t: &Type, depth: usize) -> Option<CType> {
        if depth > 64 {
            return None; // cyclic alias — defended (the marshal gate rejects it cleanly).
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                "int" => Some(CType::Int),
                "float" => Some(CType::Float),
                "bool" => Some(CType::Bool),
                "str" => Some(CType::Str),
                "ptr" => Some(CType::Ptr),
                "owned_str" => Some(CType::OwnedStr),
                "int8" => Some(CType::Int8),
                "int16" => Some(CType::Int16),
                "int32" => Some(CType::Int32),
                "int64" => Some(CType::Int64),
                "uint8" => Some(CType::UInt8),
                "uint16" => Some(CType::UInt16),
                "uint32" => Some(CType::UInt32),
                "uint64" => Some(CType::UInt64),
                // A LOCAL transparent alias: recurse on its body in THIS module's scope.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ctype_d(&body, depth + 1)
                }
                // A `from`-imported alias: its CType was computed in the DEFINING module's scope.
                _ if self.imported_alias_ctypes.contains_key(n) => {
                    self.imported_alias_ctypes[n].clone()
                }
                // A bare-visible struct (local or `from`-imported): a by-value flat-scalar struct.
                // SAME-MODULE path (current scope == defining scope): a cache hit (already populated)
                // OR a field-walk in THIS scope for a not-yet-populated forward-reference nested struct
                // — both correct here because this is the struct's own defining scope.
                _ if self.struct_names.contains(n) => {
                    let key = self.bare_key(n);
                    self.resolve_struct_ctype(&key)
                        .or_else(|| self.struct_ctype_from_asts(&key, depth))
                }
                _ => None,
            },
            // A sync scalar callback param (callbacks #4): `fn(scalars...) -> scalar` lowers to
            // `CType::Callback`. Every part must lower to a C SCALAR (`is_scalar`) — a non-scalar
            // part (str/struct/nested callback) yields `None`, so the marshal gate rejects it cleanly.
            // (Param-only; a function-typed RETURN is rejected by `assert_marshallable`, never lowered.)
            Type::Func { params, ret, .. } => {
                let mut cparams = Vec::with_capacity(params.len());
                for p in params {
                    let cp = self.resolve_ctype_d(p, depth + 1)?;
                    if !cp.is_scalar() {
                        return None;
                    }
                    cparams.push(cp);
                }
                let cret = self.resolve_ctype_d(ret, depth + 1)?;
                if !cret.is_scalar() {
                    return None;
                }
                Some(CType::Callback {
                    params: cparams,
                    ret: Box::new(cret),
                })
            }
            // RETURN-ONLY nullable `char*` (`str?` / `owned_str?`): the inner type decides
            // borrowed (`str` → OptStr) vs owned (`owned_str` → OptOwnedStr).
            Type::Generic(n, args, ..) if n == "Option" && args.len() == 1 => {
                match self.resolve_ctype_d(&args[0], depth + 1) {
                    Some(CType::Str) => Some(CType::OptStr),
                    Some(CType::OwnedStr) => Some(CType::OptOwnedStr),
                    _ => None,
                }
            }
            // A module-qualified type `mod.Name` — resolved in the TARGET module's scope, so a width
            // alias carries its DEFINING module's width and a struct its identity key (collision-proof
            // by construction: never the bare flat alias map).
            Type::Qualified { module, name, .. } => {
                let mid = self.imported_modules.get(module)?;
                let sig = self.module_sigs.get(mid)?;
                // A qualified FFI marshalling TYPE name (`ffi.int32` / `ffi.ptr`) in an extern
                // signature: map to its WIDTH-bearing CType, exactly like the bare `Type::Named`
                // arm above. The surface name is unchanged (the backends' `ctype_of` reads the
                // resulting CType, never re-derives from a module prefix), so it marshals identically
                // to the bare width. Gated on `sig.types` membership — only `std.ffi`'s sig carries
                // these reserved names — so this fires solely for genuine FFI types.
                if sig.types.contains(name) {
                    let c = match name.as_str() {
                        "ptr" => Some(CType::Ptr),
                        "int8" => Some(CType::Int8),
                        "int16" => Some(CType::Int16),
                        "int32" => Some(CType::Int32),
                        "int64" => Some(CType::Int64),
                        "uint8" => Some(CType::UInt8),
                        "uint16" => Some(CType::UInt16),
                        "uint32" => Some(CType::UInt32),
                        "uint64" => Some(CType::UInt64),
                        _ => None,
                    };
                    if c.is_some() {
                        return c;
                    }
                }
                if sig.struct_defs.contains_key(name) {
                    // CROSS-MODULE: read the qualified struct's CType from the cache VERBATIM (computed
                    // in ITS defining module's scope, deps-first). NEVER field-walk here — the current
                    // scope is the IMPORTER's, where the struct's field aliases are invisible/colliding.
                    self.resolve_struct_ctype(&self.type_key(mid, name))
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    asig.ctype.clone()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// PURE CACHE READ of the by-value `CType::Struct` for the struct under IDENTITY `key`. The CType
    /// was pre-computed in the struct's OWN DEFINING module's scope (by `populate_struct_ctypes`, run
    /// after `hoist` in each module's `check_module` — deps-first AND before this module's own extern
    /// harvest, so a cross-module OR same-module struct is always cached before an extern needs it).
    /// This is the SINGLE-RESOLVER invariant that kills the FFI drift: `resolve_struct_ctype` NEVER
    /// re-resolves a struct's fields in the (wrong, importing) current scope — it only reads the cache,
    /// so a field typed via the defining module's local alias keeps its true width.
    pub(super) fn resolve_struct_ctype(&self, key: &str) -> Option<CType> {
        self.struct_ctypes.get(key).cloned().flatten()
    }

    /// Cache every struct DECLARED IN THIS MODULE under its identity key, its by-value `CType::Struct`
    /// computed HERE — in this (the DEFINING) module's import/alias scope (extending the
    /// `AliasSig::ctype` precedent to structs). Called once per module from `check_module` after
    /// `hoist` (so all of this module's aliases/`from`-imports are live) and BEFORE the check_stmt loop
    /// (so a same-module extern harvested in the loop reads the cache). Modules are checked deps-first,
    /// so a downstream importer's extern returning `mod.Struct` reads this cached, defining-scope CType
    /// verbatim. Gated to the extern-harvesting pass; a lone `check` never builds these.
    pub(super) fn populate_struct_ctypes(&mut self, stmts: &[Stmt], id: Option<&ModuleId>) {
        let Some(mid) = id else { return };
        let mid = mid.clone();
        for stmt in stmts {
            if let StmtKind::Struct { name, .. } = &stmt.kind {
                let key = self.type_key(&mid, name);
                if self.struct_ctypes.contains_key(&key) {
                    continue; // already cached (a cross-module forward-ref resolved it earlier).
                }
                let c = self.struct_ctype_from_asts(&key, 0);
                self.struct_ctypes.insert(key, c);
            }
        }
    }

    /// Build a by-value `CType::Struct` for the struct under IDENTITY `key` from its raw field ASTs,
    /// mapping each AST field type to its own width-bearing `CType` (so an `int32` field stays 4 bytes
    /// — the layout the C ABI expects) IN THE CURRENT SCOPE. `key` is the identity key (the tag a
    /// returned struct carries, so field lookup hits). `None` if a field isn't a scalar leaf (a
    /// non-marshallable struct — the marshal gate rejects it). The ONLY field-resolving path — only
    /// `populate_struct_ctypes` calls it, in the defining scope, so there is exactly one resolver.
    pub(super) fn struct_ctype_from_asts(&self, key: &str, depth: usize) -> Option<CType> {
        let fields = self.struct_field_asts.get(key)?;
        let mut cfields = Vec::with_capacity(fields.len());
        let mut field_names = Vec::with_capacity(fields.len());
        for (fname, fty) in fields {
            cfields.push(self.resolve_ctype_d(fty, depth + 1)?);
            field_names.push(fname.clone());
        }
        Some(CType::Struct {
            name: key.to_string(),
            field_names,
            fields: cfields,
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
    pub(super) fn scalar_bound_ty(name: &str) -> Option<Ty> {
        Some(match name {
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "str" => Ty::Str,
            "bytes" => Ty::Bytes,
            "bytearray" => Ty::ByteArray,
            "nil" => Ty::Nil,
            _ => return None,
        })
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
        let Some(pinfo) = self.protocols.get(protocol) else {
            // A `where T: <scalar>` EQUALITY bound: the name is a concrete scalar type, not a
            // protocol, so it constrains `ty` to be EXACTLY that type (e.g. `trip()`'s `where T: bool`).
            if let Some(expected) = Self::scalar_bound_ty(protocol) {
                return match ty {
                    // Don't cascade off an unresolved operand (mirrors the `Ty::Unknown` arm below).
                    Ty::Unknown => Ok(Grant::no_intrinsic_method()),
                    _ if *ty == expected => Ok(Grant::no_intrinsic_method()),
                    _ => Err(format!("expected {expected}, found {ty}")),
                };
            }
            // A `where T: List/Map/Set` constructor-kind bound: `ty`'s HEAD must equal the container.
            if let Some(ok) = Self::container_bound_matches(protocol, ty) {
                return match ty {
                    Ty::Unknown => Ok(Grant::no_intrinsic_method()),
                    _ if ok => Ok(Grant::no_intrinsic_method()),
                    _ => Err(format!("expected {protocol}[...], found {ty}")),
                };
            }
            return Err(format!("unknown protocol '{protocol}'"));
        };
        if let Ty::Unknown = ty {
            return Ok(Grant::no_intrinsic_method()); // don't cascade
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
        // (which knows, via `bound_provides`, that an `Arithmetic`-bound param provides Add/Sub/…).
        if !pinfo.embeds.is_empty() && !matches!(ty, Ty::Param(_)) {
            // A VISITED SET, not a depth cap: `ty` is fixed across the whole walk, so revisiting
            // one (protocol, args) pair can only re-derive the same answer, while a depth bound of
            // 64 over a branching graph is 2^64 visits — a 42-protocol DAG hung `check` (and the
            // LSP) past 25 s. Same class the sibling walkers close; this one is the third.
            {
                // The owner's params are re-spelled here too (`embed_arg_tys`, not a bare
                // `resolve_ty_ro`) and then bound to the args actually being required. Without both,
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
                    let eargs: Vec<Ty> = self
                        .embed_arg_tys(pinfo, emb)
                        .iter()
                        .map(|t| subst(t, &omap))
                        .collect();
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
                    let _: Grant = self.satisfies_args_d(ty, &emb.name, &eargs, seen)?;
                }
            }
            if pinfo.methods.is_empty() {
                // Pure bundle — all embeds satisfied, no own methods to check. Each embed already
                // registered its own intrinsic grant (or needed none), so this is not itself a grant.
                return Ok(Grant::no_intrinsic_method());
            }
        }
        if protocol == "Comparable" && matches!(ty, Ty::Int | Ty::Float | Ty::Str) {
            return self.grant_intrinsic(protocol, ty);
        }
        // **D1 — `Eq` satisfaction IS what `==` accepts.** Chezzi's structural `==` is an automatic
        // derive (`Vm::values_equal`) that never told the protocol system: `[1] == [1]`,
        // `(1,2) == (1,2)`, `Some(1) == Some(1)`, `P(1) == P(2)` for a plain struct — every one of
        // them works, while `where T: Eq` used to reject all of them because the grant was the four
        // scalars and nothing else. That is what made `where T: Eq` unwritable and got the first
        // W7-41 fix reverted (`docs/gaps.md` W7-41). **Rust owns this** — `Vec<i32>`, `(i32,i32)`,
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
        //   `Func`, `Module`, `Ty::Param` and `Ty::Protocol`. The handles compare by identity but
        //   have no constructible probe receiver; the last two MUST fall through — `may_be_equal`
        //   treats a `Param` as ERASED, so admitting it would make every UNBOUNDED `T` satisfy `Eq`
        //   (a soundness hole), and a protocol existential is decided by its own arm below.
        // * kind ≠ `"nil"`: a nil-typed expression cannot be used as a value at all, so `nil == nil`
        //   is not a writable program and the grant would have no probeable receiver.
        // * not the built-in cursor ([`Self::is_cursor_ty`]) — a `Ty::Struct` whose runtime arms
        //   expose only `next`/`iter`, so the `"struct"` row does not speak for it.
        // * the type does not DECLARE its own `eq`. One that does is decided structurally below,
        //   which is what keeps the ordinary-method escape hatch (`fn eq(self, x: T)`, a generic
        //   operand — not the hook) a wrong-signature rejection: an erased `[T: Eq]` body's
        //   `a.eq(b)` dispatches by NAME to that method, handing it an operand it never declared.
        //
        // …and then the real question: [`Self::eq_bounds_unsatisfied`] — nothing the structural
        // equality walk REACHES is a declared `eq` whose `where` bounds fail for this instantiation
        // (W7-41's actual defect). Its `Some` is the refusal, and **its `None` is this grant**, which
        // is why every "cannot tell" inside it answers `Some`.
        if protocol == "Eq"
            && !matches!(Self::intrinsic_recv_kind(ty), "?" | "nil")
            && !Self::is_cursor_ty(ty)
            && self
                .declared_methods(ty)
                .is_none_or(|m| !m.contains_key("eq"))
        {
            return match self.eq_bounds_unsatisfied(ty) {
                Some(why) => Err(format!("type {ty} does not satisfy Eq ({why})")),
                None => self.grant_intrinsic(protocol, ty),
            };
        }
        // `Stringable` (sole method `str(self) -> str`) is satisfied intrinsically by every scalar —
        // all four stringify (int/float/bool/str), so a `[T: Stringable]` generic accepts them (the
        // erased body's `v.str()` is dispatched by the scalar `str` branch in `Vm::do_method_call`).
        // Note the membership is all FOUR scalars — unlike Comparable (no Bool) / Hashable (no Float).
        // Structs/enums/newtypes still fall through to the structural `satisfies_methods` below (a type
        // WITHOUT a `str(self) -> str` method stays correctly rejected; newtypes stay opt-in).
        if protocol == "Stringable" && matches!(ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Str) {
            return self.grant_intrinsic(protocol, ty);
        }
        // `Hashable` is satisfied intrinsically by the scalar key types (mirrors the map/set key
        // restriction; float is excluded — its equality is a hazard). Struct conformance falls
        // through to the structural check (needs a `hash(self) -> int` method).
        if protocol == "Hashable" && matches!(ty, Ty::Int | Ty::Str | Ty::Bytes | Ty::Bool) {
            return self.grant_intrinsic(protocol, ty);
        }
        // A ZERO-FIELD struct WITHOUT an explicit `hash(self)` method is intrinsically `Hashable`: it
        // has no state to hash, so both engines return a constant hash for it (with `==`'s type-tag
        // guard keeping distinct empty-struct types unequal despite the hash collision). This lets
        // `struct S: pass` be used as a Set element / Map key without an explicit `hash(self)` method.
        // The `!methods.contains_key("hash")` clause MUST mirror the runtime `struct_hash` guard
        // (src/vm/mod.rs, src/interp/mod.rs): the runtime only substitutes the constant-0 hash when the
        // struct has NO `hash` method — a zero-field struct that DOES define `hash` gets its method
        // dispatched. So a zero-field struct WITH a `hash` method must fall through to the structural
        // check (which validates `hash(self) -> int`), or a mis-typed `hash` (wrong return / arity)
        // would pass the checker and fault at runtime (check-ok/run-diverge).
        if protocol == "Hashable"
            && let Ty::Struct(name, _) = ty
            && self
                .structs
                .get(name)
                .is_some_and(|d| d.fields.is_empty() && !d.methods.contains_key("hash"))
        {
            return self.grant_intrinsic(protocol, ty);
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
        // and a bare list/set/map/str/bytes holds no position, so the old `iter_elem` ("can be
        // iterated") predicate check-OK'd `c.next()` and then faulted at runtime (W6-3b). A raw
        // collection satisfies `Iterable` instead — `[S: Iterable[T], T]` is the migration form, and it
        // recovers `T` the same way (see `recover_iter_elems`). A `Ty::Param` falls through to the
        // declared-bounds check below (so a `[S: Iterator[T]]` value forwards into another
        // iterator-generic call), since neither predicate can see through a bare param.
        if protocol == "Iterator" && !matches!(ty, Ty::Param(_) | Ty::Protocol(..)) {
            return if Self::is_cursor_ty(ty) || self.struct_iter_elem(ty).is_some() {
                self.grant_intrinsic(protocol, ty)
            } else if self.iter_elem(ty).is_some() {
                // ITERABLE but position-less (a raw collection): every remedy below actually applies
                // to it, so spell them out — this is the W6-3b migration path.
                Err(format!(
                    "type {ty} does not satisfy Iterator — `next` needs a cursor that holds a \
                     position. Iterate it with `for`, take a cursor with `.iter()`, or bound the \
                     parameter `[S: Iterable[T], T]`"
                ))
            } else {
                // NOT iterable at all (`int`, a plain struct): `for`/`.iter()`/`Iterable` are all dead
                // ends for it, so appending that advice would misdirect. Keep the bare pre-W6-3b text.
                Err(format!("type {ty} does not satisfy Iterator"))
            };
        }
        // `Iterable` conformance is "can produce a fresh cursor". Built-in collections satisfy it
        // intrinsically; ANY `Iterator[T]`-satisfying type satisfies it too (every Iterator IS
        // Iterable — `iter()` returns self), so `iter_elem` (which already covers both) is reused as
        // the predicate. A user struct with a structural `iter(self) -> Iterator[E]` (but no `next`)
        // is caught by the `iterable_elem` helper. The bound's `[T]` arg, if supplied and concrete,
        // must match the element type (mirrors the parameterized-`Index` arg check). A `Ty::Param`
        // falls through to the declared-bounds check below (so `[S: Iterable[T]]` forwards), and so
        // does a `Ty::Protocol` existential (an `Iterable[T]`-ANNOTATED value): its runtime receiver
        // is whatever concrete thing witnesses it, whose own intrinsic row already exists, so it is
        // decided by the protocol-existential arm below — `Iterable[int]` satisfies `Iterable[int]`
        // and nothing wider (that arm is where the strict arg invariance lives).
        if protocol == "Iterable" && !matches!(ty, Ty::Param(_) | Ty::Protocol(..)) {
            let Some(elem) = self.iterable_elem(ty) else {
                return Err(format!("type {ty} does not satisfy Iterable"));
            };
            if let Some(want) = args.first()
                && !want.is_unknown()
                && !elem.is_unknown()
                && !compatible(want, &elem)
            {
                return Err(format!("type {ty} does not satisfy Iterable"));
            }
            return self.grant_intrinsic(protocol, ty);
        }
        // `Index`/`IndexSet`/`Slice` — built-in `list`/`map`/`str` conform intrinsically (a struct
        // conforms structurally, falling through to the matcher below; a `Ty::Param` forwards to its
        // declared bounds). `str` is immutable, so it satisfies `Index`/`Slice` but NOT `IndexSet`.
        if matches!(protocol, "Index" | "IndexSet" | "Slice")
            && !matches!(ty, Ty::Param(_) | Ty::Struct(..) | Ty::Protocol(..))
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
                        None => return Err(format!("type {ty} does not satisfy {protocol}")),
                    }
                }
            };
            // Any args the bound supplied must match what the built-in actually provides.
            for (want, got) in args.iter().zip(&provided) {
                if !want.is_unknown() && !got.is_unknown() && !compatible(want, got) {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
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
                Err(format!("type {ty} does not satisfy {protocol}"))
            };
        }
        // The numeric operator protocols are satisfied intrinsically by int/float (their `+ - * / %`
        // and unary `-` are the primitive ops), so a `[T: Add + Mul]` / `[T: Div]` / `[T: Neg]`
        // generic works over numbers as well as structs.
        if matches!(protocol, "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg")
            && matches!(ty, Ty::Int | Ty::Float)
        {
            return self.grant_intrinsic(protocol, ty);
        }
        // A bound type parameter satisfies a protocol if that protocol is among its declared bounds —
        // this is what lets a generic forward its `T: P` value into another `[U: P]` call. For a
        // parameterized protocol the bound's type args must also match the required ones, so a
        // `Container[str]` value is NOT accepted where `Container[int]` is required (forwarding hole).
        if let Ty::Param(name) = ty {
            let matched = self.type_params.get(name).is_some_and(|bs| {
                bs.iter()
                    .any(|b| self.bound_provides(&b.name, &b.args, protocol, args, 0))
            });
            return if matched {
                Ok(Grant::no_intrinsic_method())
            } else {
                Err(format!("type {ty} does not satisfy {protocol}"))
            };
        }
        match ty {
            Ty::Struct(sname, _) => {
                // MISS-ONLY identity-key fallback (gap #4): a named-fn-imported factory result carries
                // its owning module's `Ty::Struct` key but injects nothing into the local `self.structs`
                // table, so resolve the shape from the owning `ModuleSig` on a local miss — otherwise a
                // structurally-conforming value is spuriously rejected at a protocol bound (the same
                // three-import-forms inconsistency the member-access fix already closed).
                let Some(info) = self.struct_shape(sname) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, &info.methods)
                    .map(|()| Grant::no_intrinsic_method())
            }
            // Enum conformance is structural exactly like a struct's: the enum satisfies `protocol`
            // iff its `methods` map carries every protocol method with a matching signature. This
            // unlocks Stringable/Hashable/Add/Sub/Mul/Comparable for enums and protocol-bound generics.
            Ty::Enum(ename, _) => {
                // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported enum value's
                // method table from the owning `ModuleSig` on a local-table miss (see the struct arm).
                let Some(methods) = self.enum_methods_of(ename) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
                    .map(|()| Grant::no_intrinsic_method())
            }
            // A newtype satisfies a protocol structurally via its OWN methods (like struct/enum).
            // PLUS, when its underlying is numeric, it intrinsically satisfies the operator protocols
            // (`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Comparable`) — its same-type `+`/`<` use the native op
            // (unwrap→op→rewrap), so a `newtype Meters = float` flows into a `[T: Add]` generic with
            // no user `add` method. Hashable/Stringable stay strictly opt-in (the user's own method).
            Ty::NewType(ntkey, _) => {
                // The intrinsic numeric-operator satisfaction is for SCALAR newtypes only. A generic
                // newtype is methods-only — even `newtype Box[T] = T` gets no native Add/Sub/Mul/
                // Comparable; operators come strictly from its own methods (checked below).
                let numeric = !self.newtype_is_generic(ntkey)
                    && self
                        .newtype_underlying(ntkey)
                        .is_some_and(|u| u.is_numeric());
                // (`Eq` is NOT here: D1 grants it to EVERY newtype above, numeric or not — its `==`
                // unwraps to the underlying's native equality either way — so this arm never sees it.)
                if numeric
                    && matches!(
                        protocol,
                        "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Comparable"
                    )
                {
                    return self.grant_intrinsic(protocol, ty);
                }
                // SOUNDNESS: a newtype operator overload defined as a METHOD is NEVER dispatched at
                // runtime — the same-newtype operator arm (vm `newtype_arith` / `compare_op`, interp
                // `eval_binop`) always auto-flows to the UNDERLYING's native op, and unary `-` has no
                // newtype path at all. So an operator protocol on a newtype is satisfiable ONLY via the
                // numeric auto-flow above; admitting it structurally here would type-check a call that
                // diverges on every engine (`check` ok / `run` silently using the native op, or
                // "cannot apply <Op> to <underlying>"). A non-numeric (or generic) newtype therefore
                // does NOT satisfy these — its own `add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare` method
                // is intentionally unreachable as an operator. `Comparable` is in this list for the
                // same reason: same-newtype `<`/`<=`/`>`/`>=` always uses the underlying's NATIVE
                // ordering (`compare_op`'s `same_newtype_keys` fast path), never the user `compare`, so
                // a generic newtype (the only non-numeric case that reaches here after the numeric
                // short-circuit) must NOT claim `Comparable` via a method — that would be check-ok /
                // run-divergent. `Eq` is NOT in this list (D1): same-newtype `==` unwrapping to the
                // UNDERLYING's native equality is a WORKING `==`, so every newtype satisfies `Eq`
                // intrinsically at the D1 arm above — a method never enters into it, and this arm is
                // unreachable for `Eq`. (Hashable/Stringable/Iterable/etc. still resolve structurally
                // below.)
                if matches!(
                    protocol,
                    "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg" | "Comparable"
                ) {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                }
                // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported newtype value's
                // method table from the owning `ModuleSig` on a local-table miss (see the struct arm).
                let Some(methods) = self.newtype_methods_of(ntkey) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
                    .map(|()| Grant::no_intrinsic_method())
            }
            _ => Err(format!("type {ty} does not satisfy {protocol}")),
        }
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
        let pmap: HashMap<String, Ty> = pinfo
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        // The RECEIVING type's own param→arg substitution (e.g. `T→int` from `Box[int]`). The user's
        // stored methods carry the struct/enum/newtype's type params UNsubstituted (a method on
        // `Box[T]` is stored as `add(self, o: Box[T]) -> Box[T]`), so for a generic instantiation we
        // must bind those params to the instantiation's args before comparing against the (Self-bound)
        // protocol signature — otherwise `compatible(Box[int], Box[T])` fails and a perfectly good
        // operator method is rejected as "wrong signature". Non-generic types yield an empty map (no
        // params), and checking inside the generic's own def (`ty = Box[T]`) yields an identity map
        // `{T→T}` — both are no-op substitutions, so this only ever binds concrete args.
        // Each arm resolves the receiving type's params through the miss-only identity-key helpers
        // (gap #4), so a generic named-fn-imported instantiation (`Box[int]` from a factory whose TYPE
        // name is not imported) binds its params here identically to a whole-module import.
        let tymap: HashMap<String, Ty> = match ty {
            // A protocol existential witnessing another protocol (M22): bind its OWN params to its
            // carried args, and `Self` to the existential itself — its sigs spell `Self` exactly as
            // the requirement's do, and only the requirement side is `Self`-bound by `method_matches`.
            Ty::Protocol(name, targs) => {
                let mut m: HashMap<String, Ty> = self
                    .protocols
                    .get(name)
                    .map(|p| {
                        p.type_params
                            .iter()
                            .cloned()
                            .zip(targs.iter().cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                m.insert("Self".to_string(), ty.clone());
                m
            }
            _ => self.nominal_param_map(ty),
        };
        for (mname, msig) in &pinfo.methods {
            let subst_params: Vec<Ty> = msig.params.iter().map(|t| subst(t, &pmap)).collect();
            let min_params = subst_params.len();
            let want = FnSig {
                labels: Vec::new(),
                params: subst_params,
                ret: subst(&msig.ret, &pmap),
                type_params: Vec::new(),
                where_bounds: Vec::new(),
                min_params,
                // Carry the protocol requirement's static-ness (e.g. `Convert`'s `convert(x: S)` is
                // STATIC) so `method_matches` can reject an instance/self-slot witness of a static slot.
                // Instance-method requirements keep `is_static == false`, so this is inert for them.
                is_static: msig.is_static,
                doc: None,
                witness_params: Vec::new(),
                variadic: None,
            };
            let msig = &want;
            // Pre-substitute the receiving type's params into the ACTUAL (user) method signature so
            // its `Box[T]` becomes `Box[int]` before the comparison. Only the actual side is bound —
            // a genuinely wrong sig (`add(self, o: int) -> int`) stays a mismatch, no laundering.
            let actual_owned = methods.get(mname).map(|actual| {
                if tymap.is_empty() {
                    actual.clone()
                } else {
                    FnSig {
                        params: actual.params.iter().map(|t| subst(t, &tymap)).collect(),
                        ret: subst(&actual.ret, &tymap),
                        ..actual.clone()
                    }
                }
            });
            match actual_owned.as_ref() {
                Some(actual) if method_matches(msig, actual, ty) => {
                    // Conditional conformance: a method whose `where` bounds the RECEIVER's own type
                    // param (e.g. `compare(self, o: Box[T]) -> int where T: Comparable` on `Box[T]`)
                    // only makes the type satisfy `protocol` when that bound HOLDS for this concrete
                    // instantiation. Enforce it structurally here — so EVERY satisfies-based consumer
                    // (operator dispatch, generic bounds, protocol-typed params, `for`) is sound, not
                    // just explicit `.method()` calls (which enforce at the call site). `tymap` maps the
                    // receiver param (`T`) to the concrete arg; `Ty::Unknown` args defer (satisfies_args
                    // returns `Ok` on Unknown), matching `List.sort`'s late-inference behaviour.
                    // Recursion terminates by structural descent (each level checks a smaller type arg).
                    // Pre-conditional-methods code has no `where_bounds` on any method, so this loop is a
                    // no-op there — zero effect on existing structural conformance.
                    for wb in &actual.where_bounds {
                        let Some(concrete) = tymap.get(&wb.name) else {
                            continue;
                        };
                        for bound in &wb.bounds {
                            let bargs: Vec<Ty> =
                                bound.args.iter().map(|a| self.resolve_ty_ro(a)).collect();
                            if self.satisfies_args(concrete, &bound.name, &bargs).is_err() {
                                return Err(format!(
                                    "type {ty} does not satisfy {protocol} (method '{mname}' requires {}: {})",
                                    concrete, bound.name
                                ));
                            }
                        }
                    }
                }
                Some(_) => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol} (method '{mname}' has the wrong signature)"
                    ));
                }
                None => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol} (missing method '{mname}')"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Result type of an overloaded arithmetic operator (`+`/`-`/`*`) on two operands of the *same*
    /// struct or type-parameter that satisfies `protocol` (`Add`/`Sub`/`Mul`). The runtime dispatches
    /// to the `add`/`sub`/`mul` method; the result type is that same type. `None` ⇒ not overloadable.
    pub(super) fn op_overload_result(&self, l: &Ty, r: &Ty, protocol: &str) -> Option<Ty> {
        // A SAME newtype with a NUMERIC underlying auto-applies the underlying's NATIVE arithmetic op
        // (unwrap→op→rewrap, NOT a user `add`) and returns the newtype. `Meters + float` /
        // `Meters + Seconds` don't match (different/non-newtype operands) → the caller's "cannot
        // apply" error. (A user-defined `add` method also works via the satisfies() path below, but
        // the native numeric op is the no-method common case.)
        if let (Ty::NewType(a, _), Ty::NewType(b, _)) = (l, r)
            && a == b
            && !self.newtype_is_generic(a)
            && self.newtype_underlying(a).is_some_and(|u| u.is_numeric())
        {
            return Some(l.clone());
        }
        // SAME generic type REQUIRES matching type ARGS, not just the same name. The user's operator
        // method `add(self, o: Box[T]) -> Box[T]` on a `Box[int]` receiver needs `o: Box[int]`, so a
        // heterogeneous pair like `Box[int] + Box[str]` must NOT overload — admitting it would infer
        // the result `Box[int]` for a value carrying a `Box[str]` (the returned type the checker can't
        // honor → runtime type confusion). `compatible` checks name + pairwise-compatible targs; an
        // `Unknown` targ on a partially-inferred side still unifies (no false rejection there).
        let same = match (l, r) {
            (Ty::Struct(..), Ty::Struct(..))
            | (Ty::Enum(..), Ty::Enum(..))
            | (Ty::NewType(..), Ty::NewType(..)) => compatible(l, r),
            (Ty::Param(a), Ty::Param(b)) => a == b,
            // NOT `(Ty::Protocol, Ty::Protocol)`: every operator protocol's method is
            // `(self, Self) -> Self`, and two values of one protocol need not hold the same witness
            // (`Vecish + Vecish` over a `V` and a `W`), so the pair is un-dispatchable by object
            // safety — see `self_in_param_position`. Bind the operands together with a generic
            // parameter (`[T: Vecish](a: T, b: T)`) and this works, soundly.
            _ => false,
        };
        if same && self.satisfies(l, protocol).is_ok() {
            Some(l.clone())
        } else {
            None
        }
    }

    /// The resolved underlying `Ty` of a newtype (by runtime key), if known. Falls back to the owning
    /// module's `ModuleSig` on a local-table miss (gap #4), so a named-fn-imported newtype value's
    /// numeric auto-flow / operator satisfaction is decided identically to a whole-module import.
    pub(super) fn newtype_underlying(&self, key: &str) -> Option<Ty> {
        self.newtype_defs
            .get(key)
            .map(|(u, _)| u.clone())
            .or_else(|| self.owning_newtype_def(key).map(|nt| nt.underlying.clone()))
    }

    /// Is the newtype (by runtime key) type-parameterized? A generic newtype is METHODS-ONLY — it
    /// gets no native operator auto-flow (even over a numeric/`T` underlying); operators come strictly
    /// from its own methods + protocol satisfaction. Gates the scalar-newtype auto-flow paths.
    pub(super) fn newtype_is_generic(&self, key: &str) -> bool {
        // MISS-ONLY identity-key fallback (gap #4): a named-fn-imported newtype value injects nothing
        // into the local `newtype_type_params` table, so consult the owning `ModuleSig` on a miss.
        self.newtype_type_params
            .get(key)
            .map(|t| !t.is_empty())
            .or_else(|| {
                self.owning_newtype_def(key)
                    .map(|nt| !nt.type_params.is_empty())
            })
            .unwrap_or(false)
    }

    /// Are `l < r` etc. allowed? True for same-named comparable type params, or same-named structs
    /// that satisfy `Comparable` (operator overloading dispatches to their `compare` at runtime).
    pub(super) fn ordering_allowed(&self, l: &Ty, r: &Ty) -> bool {
        self.cmp_overload_allowed(l, r, "Comparable", "compare")
    }

    /// Shared body of [`Self::ordering_allowed`]: do `l` and `r` name the
    /// SAME type param / struct / enum / newtype, such that the comparison operator dispatches to
    /// `protocol`'s `method` (or, for a numeric newtype, to the underlying's native op)?
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
            (Ty::Struct(..), Ty::Struct(..)) | (Ty::Enum(..), Ty::Enum(..)) if compatible(l, r) => {
                self.satisfies(l, protocol).is_ok()
            }
            // Same SCALAR newtype with a numeric underlying: `Meters < Meters` uses the underlying's
            // native ordering (returns bool). A user `compare` method also enables it via satisfies()
            // (the only path for a generic newtype — methods-only, no native ordering auto-flow).
            (Ty::NewType(a, _), Ty::NewType(b, _)) if a == b => {
                (!self.newtype_is_generic(a)
                    && self.newtype_underlying(a).is_some_and(|u| u.is_numeric()))
                    || self.satisfies(l, protocol).is_ok()
            }
            // No `(Ty::Protocol, Ty::Protocol)` arm — `Comparable.compare(self, o: Self)` (and
            // `Eq.eq(self, o: Self)`) is `Self`-parameterized, so two values of one protocol are
            // un-orderable/un-dispatchable for the same object-safety reason `+` is (see
            // `op_overload_result`).
            _ => false,
        }
    }

    /// The methods a struct/enum/newtype DECLARES (`None` for anything else) — for diagnostics that
    /// need to ask "did the user write this method at all?", which conformance alone can't answer.
    fn declared_methods(&self, ty: &Ty) -> Option<&HashMap<String, FnSig>> {
        match ty {
            Ty::Struct(name, _) => self.struct_shape(name).map(|i| &i.methods),
            Ty::Enum(name, _) => self.enum_methods_of(name),
            Ty::NewType(key, _) => self.newtype_methods_of(key),
            _ => None,
        }
    }

    /// A `compare`-declaring NEWTYPE is a dead end, and the bare "does not satisfy Comparable" reads
    /// as "you forgot the comparator" — exactly wrong for someone who just wrote one. Name the real
    /// reason instead: a newtype's `<` ALWAYS auto-flows to the underlying's native ordering
    /// (vm `compare_op`'s same-newtype fast path), so a `compare` METHOD on one is never dispatched
    /// and can never make it conform. `None` for anything else, so a type that never wrote `compare`
    /// keeps the bare wording (the hint must not over-fire).
    ///
    /// The struct/enum half of this hint is GONE with the M23 use-site rule it advertised: "a type
    /// defining `compare` must define `eq` too" was enforced only through the `Comparable`→`Eq`
    /// embed, and D1 makes a plain struct satisfy `Eq`, so the rule no longer fires. It is dropped
    /// deliberately, not re-homed — its premise was falsified in both directions by measurement (a
    /// `compare` covering every field in declaration order agrees with structural `==` exactly, and
    /// the repo's own motivating `Ver` DOES define both and still disagrees), and Rust — which owns
    /// this — permits manual `Ord` beside a derived `Eq` (a clippy lint, not an error).
    fn newtype_compare_dead_end(&self, ty: &Ty) -> Option<String> {
        if !matches!(ty, Ty::NewType(..)) || !self.declared_methods(ty)?.contains_key("compare") {
            return None;
        }
        Some(
            "a newtype's `<` always uses the underlying's native ordering, never a `compare` \
             method, so a `compare` method can never make a newtype satisfy `Comparable` — use a \
             struct if you need your own ordering"
                .to_string(),
        )
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
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Unknown
            | Ty::Param(_) => true,
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
            // recoverable safety net identical on both engines — a checker-permissive type here is
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
            // A newtype is sendable iff its underlying type is (it crosses by deep-copy of the inner
            // value, like a 1-field struct). Cycle-guarded by the newtype key.
            Ty::NewType(name, _) => {
                if stack.contains(name) {
                    return true;
                }
                // Substitute the newtype's instantiated type args into its underlying, so a generic
                // `Stack[int]` checks `list[int]` (not bare `list[T]`) for sendability.
                match self.newtype_unwrap_target(ty) {
                    Some(under) => {
                        stack.push(name.clone());
                        let ok = self.sendable_rec(&under, stack);
                        stack.pop();
                        ok
                    }
                    None => true,
                }
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
    /// through the user `eq` while `cas` stays structural. A non-numeric newtype's `==` is still
    /// structural (it does not satisfy `Eq` — see `satisfies`), so for THAT arm the disagreement is
    /// between `cas` and the `p.eq(q)` METHOD spelling, not the operator — which is why the message
    /// says "the payload's own equality" rather than naming `==`: one wording, true on all three arms.
    ///
    /// Keyed on every type the payload REACHES, not just its own: structural equality recurses into
    /// elements, entries, tuple slots, struct fields and enum payloads, so `Atomic[List[P]]`,
    /// `Atomic[Option[P]]`, `Atomic[(int, P)]` and `Atomic[Wrapper]{p: P}` all reach `P`'s `eq` on
    /// the same compare that a bare `Atomic[P]` does. Keying on the payload's own `eq` let every one
    /// of those through the gate (`docs/gaps.md` — M23 review, CRITICAL 2).
    pub(super) fn reject_eq_atomic_payload(&mut self, elem: &Ty, span: Span) {
        if let Some(inner) = self.reaches_user_eq(elem, &mut Vec::new()) {
            let owner = if inner == elem.to_string() {
                "payload defines its own 'eq'".to_string()
            } else {
                format!("payload reaches '{inner}', which defines its own 'eq'")
            };
            let msg = format!(
                "Atomic[{elem}] {owner} — Atomic.cas compares stored values structurally, never through that 'eq', so cas and the payload's own equality would disagree; use Shared[{elem}] (no cas) instead"
            );
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

    /// The name of the first type reachable from `ty` by a STRUCTURAL equality walk that declares its
    /// own `eq`, or `None`. `stack` is the cycle guard (`sendable_rec`'s shape — a recursive
    /// `Node { next: Option[Node] }` terminates).
    ///
    /// Reachability here means exactly what `Vm::values_equal_guarded` recurses through: list/set
    /// elements, map keys AND values, `Option`/`Result` payloads, tuple slots, struct fields, enum
    /// variant payloads, and a newtype's underlying. NOT through a `Channel`/`Shared`/`Atomic`
    /// handle (comparing two handles compares the handles, never their contents) and not into a
    /// `Func`. `Protocol`/`Param`/`Unknown` are permissive holes by construction — the concrete
    /// witness is unknown here — which is why the RUNTIME `cas` also refuses to dispatch the hook
    /// (`vm/netio.rs`), rather than trusting this walk to be exhaustive.
    fn reaches_user_eq(&self, ty: &Ty, stack: &mut Vec<String>) -> Option<String> {
        let any = |s: &Self, ts: &[Ty], stack: &mut Vec<String>| -> Option<String> {
            ts.iter().find_map(|t| s.reaches_user_eq(t, stack))
        };
        match ty {
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) => self.reaches_user_eq(t, stack),
            Ty::Map(k, v) => self
                .reaches_user_eq(k, stack)
                .or_else(|| self.reaches_user_eq(v, stack)),
            Ty::Result(t, e) => self
                .reaches_user_eq(t, stack)
                .or_else(|| self.reaches_user_eq(e, stack)),
            Ty::Tuple(elems) => any(self, elems, stack),
            Ty::Struct(name, args) => {
                if let hit @ Some(_) = any(self, args, stack) {
                    return hit;
                }
                if self
                    .struct_shape(name)
                    .is_some_and(|i| i.methods.contains_key("eq"))
                {
                    return Some(ty.to_string());
                }
                if stack.contains(name) {
                    return None;
                }
                let fields = self.structs.get(name)?.fields.clone();
                stack.push(name.clone());
                let hit = fields
                    .iter()
                    .find_map(|(_, f)| self.reaches_user_eq(f, stack));
                stack.pop();
                hit
            }
            Ty::Enum(name, args) => {
                if let hit @ Some(_) = any(self, args, stack) {
                    return hit;
                }
                if self
                    .enum_methods_of(name)
                    .is_some_and(|m| m.contains_key("eq"))
                {
                    return Some(ty.to_string());
                }
                if stack.contains(name) {
                    return None;
                }
                let payloads: Vec<Ty> = self
                    .variants
                    .values()
                    .filter(|v| &v.enum_name == name)
                    .flat_map(|v| v.payload.clone())
                    .collect();
                stack.push(name.clone());
                let hit = payloads.iter().find_map(|p| self.reaches_user_eq(p, stack));
                stack.pop();
                hit
            }
            Ty::NewType(name, _) => {
                if self
                    .newtype_methods_of(name)
                    .is_some_and(|m| m.contains_key("eq"))
                {
                    return Some(ty.to_string());
                }
                if stack.contains(name) {
                    return None;
                }
                let under = self.newtype_unwrap_target(ty)?;
                stack.push(name.clone());
                let hit = self.reaches_user_eq(&under, stack);
                stack.pop();
                hit
            }
            _ => None,
        }
    }

    /// **W7-41 — is anything the structural equality walk REACHES a declared `eq` whose `where`
    /// bounds do not hold for this instantiation?** `None` = this type's equality is sound to reach;
    /// `Some(reason)` = it is not, and neither `==` nor a `[T: Eq]` bound may accept it.
    ///
    /// Same traversal as [`Self::reaches_user_eq`] — what `Vm::values_equal_guarded` recurses
    /// through — with two deliberate differences:
    ///
    /// 1. **The type's OWN `eq` is checked BEFORE its type args, and STOPS the descent.** At runtime
    ///    a declared `eq` *replaces* the structural walk (`src/vm/arith.rs`), so `Box[Tag]`'s own
    ///    unconditional `eq` may never touch `Tag`'s bounded one — descending first (which
    ///    `reaches_user_eq` does, deliberately over-conservative for `Atomic.cas`) would
    ///    over-REJECT. What that `eq`'s BODY does with the payload is guarded at its own `==` sites,
    ///    by the same rule, when the body is checked.
    /// 2. A satisfied bound does not end the search — a SIBLING branch (the other half of a `Map`,
    ///    the next struct field) may still be unsound.
    ///
    /// **It extracts the `where_bounds` walk and must never answer by calling `satisfies`/
    /// `satisfies_methods` on the receiver instead.** Doing so re-asks the whole conformance
    /// question and so rejects both in-tree ordinary-method escape hatches (`enum Opt2[T]: fn
    /// eq(self, x: T)` → *"method 'eq' has the wrong signature"*), which run fine today.
    ///
    /// # THIS PREDICATE FAILS CLOSED. `None` IS A GRANT.
    ///
    /// The sole caller ([`Self::satisfies_args_d`]'s D1 arm) turns `None` into
    /// [`Self::grant_intrinsic`] — a PROMISE that the runtime can dispatch `eq` on this receiver. So
    /// every path that cannot finish the proof — a table miss, an unresolvable shape, the
    /// in-progress budget — must return `Some`, never `None`. Reading `None` as "the safe default"
    /// is backwards here, and shipped three check-OK-then-runtime-fault holes when it was: this is
    /// the repo's `parked-is-not-stuck` rule (build the verdict from what is IMPOSSIBLE, and decline
    /// in the direction that cannot lie).
    ///
    /// **The ONE exception, and it is not a decline:** `Ty::Protocol` answers `None` because the
    /// concrete witness is unknowable here — and it is not an exotic corner, it is on the hot path.
    /// `Ty::result(inner)` is literally `Ty::Result(inner, Ty::Protocol("Error"))`
    /// (`src/checker/ty.rs`), so every bare `Result[T]` carries an existential in its error slot;
    /// refusing it would un-grant `Result` wholesale and break the `("Eq", "eq", "result")` ratchet
    /// row. Same hole, same reason, as [`Self::reaches_user_eq`]'s. (Re-asking an obligation already
    /// in progress also answers `None`, but that is a coinductive ASSUMPTION rather than a hole —
    /// see [`EQ_BOUNDS_IN_PROGRESS`].)
    pub(super) fn eq_bounds_unsatisfied(&self, ty: &Ty) -> Option<String> {
        // An EMPTY in-progress stack means this is an outermost query: no assumption can be in scope,
        // so nothing in the memo is worth keeping and nothing in it is safe to trust (the checker's
        // tables may have grown, and on a test thread the previous query may have been a different
        // PROGRAM whose `main::P` is not this one). Reset both; clearing is always sound, it only
        // costs work. Re-entrant calls — the hop through `satisfies` — leave them alone.
        if EQ_BOUNDS_IN_PROGRESS.with(|s| s.borrow().is_empty()) {
            EQ_BOUNDS_PROVEN.with(|m| m.borrow_mut().clear());
            EQ_BOUNDS_ASSUMED.set(false);
        }
        self.eq_bounds_unsatisfied_rec(ty)
    }

    /// [`Self::eq_bounds_unsatisfied`] with FREE type params erased to `Ty::Unknown` first — the
    /// spelling every *use-site* gate wants, as opposed to a declared `where T: Eq` bound, which is
    /// an obligation the caller discharges and so must keep the `Ty::Param` arm's refusal.
    ///
    /// A generic body is checked ONCE with `T` abstract, so a free `T` here is not a type that fails
    /// the bound, it is a type not yet chosen; erasing to `Ty::Unknown` (which `satisfies_args`
    /// treats as don't-cascade) keeps `fn f[T](xs: List[T], x: T): return xs.contains(x)` and
    /// `fn h[T: Hashable](xs: Set[T])` accepted while a CONCRETE part of the same type
    /// (`Map[T, Box[Tag]]`) is still judged. Shared by the `==`/`!=` gate (W7-41), the `values_equal`
    /// `List` methods and `in` (W7-45), and [`Self::key_ty_reject`] — one erasure rule, not three.
    pub(super) fn eq_bounds_unsatisfied_erased(&self, ty: &Ty) -> Option<String> {
        let mut names: Vec<String> = Vec::new();
        ty_collect_params(ty, None, &mut names);
        if names.is_empty() {
            return self.eq_bounds_unsatisfied(ty);
        }
        let erased = subst(ty, &names.into_iter().map(|n| (n, Ty::Unknown)).collect());
        self.eq_bounds_unsatisfied(&erased)
    }

    /// Enter `ty` on [`EQ_BOUNDS_IN_PROGRESS`] — the single cycle guard, used by BOTH levels of the
    /// recursion (see that constant for why it must be one).
    ///
    /// `Ok(guard)` = proceed, and the entry is popped when `guard` drops. `Err(verdict)` = the guard
    /// already answered:
    /// * `Err(None)` — `ty` is already being proven. This is the COINDUCTIVE assumption ("assume
    ///   `D: Eq` while proving `D: Eq`"), not a decline: the walk it returns to still checks every
    ///   remaining field and sibling, so nothing is skipped. **Rust, measured (rustc 1.97.0,
    ///   `scratchpad/cyc.rs`):** `struct C<T: Eq>` + `struct D { x: Vec<C<D>> }` with manual `Eq`
    ///   impls COMPILES and runs under `fn needs<U: Eq>`.
    /// * `Err(Some(_))` — out of budget, so REFUSE. A `None` here is consumed as a GRANT by
    ///   `satisfies_args_d`, and a grant is a promise the runtime must honour, so "I could not
    ///   finish the proof" must never be spelled the same way as "I finished it and it is sound".
    fn enter_eq_obligation(ty: &Ty) -> Result<EqObligation, Option<String>> {
        let (in_progress, over_budget) = EQ_BOUNDS_IN_PROGRESS.with(|s| {
            let st = s.borrow();
            (st.contains(ty), st.len() >= EQ_BOUNDS_MAX_IN_PROGRESS)
        });
        if in_progress {
            // The answer about to be returned RESTS ON AN ASSUMPTION, so mark the walk. Everything
            // between here and the frame that owns `ty` is now conditional, and none of it may be
            // memoized.
            EQ_BOUNDS_ASSUMED.set(true);
            return Err(None);
        }
        if over_budget {
            return Err(Some(Self::eq_budget_refusal(ty)));
        }
        EQ_BOUNDS_IN_PROGRESS.with(|s| s.borrow_mut().push(ty.clone()));
        Ok(EqObligation)
    }

    /// Walk the MEMBERS of a nominal `ty` (fields / payloads / underlying) under the shared cycle
    /// guard, memoizing a sound result. Every nominal arm funnels through here, so neither entering
    /// the guard nor the memo bookkeeping can be forgotten at a call site.
    fn walk_eq_members(&self, ty: &Ty, members: &[Ty]) -> Option<String> {
        if EQ_BOUNDS_PROVEN.with(|m| m.borrow().contains(ty)) {
            return None; // proven sound earlier in this query, unconditionally
        }
        let _guard = match Self::enter_eq_obligation(ty) {
            Err(verdict) => return verdict,
            Ok(g) => g,
        };
        // Ask whether THIS subtree consumes an assumption, independently of whatever the walk had
        // already recorded — then hand the outer frames the union, since a parent's result rests on
        // any assumption its children used.
        let outer_assumed = EQ_BOUNDS_ASSUMED.replace(false);
        let hit = members
            .iter()
            .find_map(|m| self.eq_bounds_unsatisfied_rec(m));
        let subtree_assumed = EQ_BOUNDS_ASSUMED.get();
        if hit.is_none() && !subtree_assumed {
            EQ_BOUNDS_PROVEN.with(|m| m.borrow_mut().push(ty.clone()));
        }
        EQ_BOUNDS_ASSUMED.set(outer_assumed || subtree_assumed);
        hit
    }

    /// [`Self::eq_bounds_unsatisfied`]'s recursive body. The cycle guard is the shared thread-local,
    /// entered by [`Self::walk_eq_members`], NOT a `stack` parameter — the walk and the outward hop
    /// through `satisfies` are the same recursion and must not keep two disagreeing views of it.
    fn eq_bounds_unsatisfied_rec(&self, ty: &Ty) -> Option<String> {
        let any = |s: &Self, ts: &[Ty]| -> Option<String> {
            ts.iter().find_map(|t| s.eq_bounds_unsatisfied_rec(t))
        };
        match ty {
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) => self.eq_bounds_unsatisfied_rec(t),
            Ty::Map(k, v) => self
                .eq_bounds_unsatisfied_rec(k)
                .or_else(|| self.eq_bounds_unsatisfied_rec(v)),
            Ty::Result(t, e) => self
                .eq_bounds_unsatisfied_rec(t)
                .or_else(|| self.eq_bounds_unsatisfied_rec(e)),
            Ty::Tuple(elems) => any(self, elems),
            Ty::Struct(name, args) => {
                // The BUILT-IN cursor is a `Ty::Struct` the struct tables know nothing about, so the
                // miss-below would REFUSE it. There is genuinely nothing to prove: a cursor holds no
                // user field and compares by identity. (It is refused the GRANT separately, at the
                // D1 arm — this is only about not poisoning a container that merely holds one.)
                if Self::is_cursor_ty(ty) {
                    return None;
                }
                // MISS-ONLY lookup (`struct_shape`, not `self.structs`): a named-fn-imported value
                // carries its owning module's identity key and injects nothing locally (gap #4), so
                // a bare-table read finds no fields and the walk would silently skip them.
                let Some(info) = self.struct_shape(name) else {
                    return Some(Self::shape_invisible(ty));
                };
                if let Some(sig) = info.methods.get("eq") {
                    return self.eq_where_unsatisfied(ty, sig);
                }
                if let hit @ Some(_) = any(self, args) {
                    return hit;
                }
                // INSTANTIATED, not declared: a field is stored as `Box[T]` and must be walked as
                // `Box[Tag]`. Leaving the decl-site `T` in place both over-rejected every generic
                // nominal (nothing binds `T`, so the `Ty::Param` arm refused `Wrap[int]`) and
                // under-rejected by NAME CAPTURE (a same-named param carrying the needed bound in
                // the CALLER's lexical scope answered for it) — one bug, both directions.
                let map = struct_param_map(info, args);
                let fields: Vec<Ty> = info.fields.iter().map(|(_, f)| subst(f, &map)).collect();
                self.walk_eq_members(ty, &fields)
            }
            Ty::Enum(name, args) => {
                if let Some(sig) = self.enum_methods_of(name).and_then(|m| m.get("eq")) {
                    return self.eq_where_unsatisfied(ty, sig);
                }
                if let hit @ Some(_) = any(self, args) {
                    return hit;
                }
                // Same two fixes as the struct arm: instantiated payloads, and the owning-module
                // fallback `self.variants` alone does not have.
                let Some(payloads) = self.enum_payloads_of(name, args) else {
                    return Some(Self::shape_invisible(ty));
                };
                self.walk_eq_members(ty, &payloads)
            }
            Ty::NewType(name, _) => {
                // Declaring `eq` on a newtype is a decl-site error, so the method arm is only ever
                // reached by an already-errored program — it is here for symmetry, not soundness.
                if let Some(sig) = self.newtype_methods_of(name).and_then(|m| m.get("eq")) {
                    return self.eq_where_unsatisfied(ty, sig);
                }
                // `newtype_underlying` + `nominal_param_map` rather than `newtype_unwrap_target`:
                // both halves carry the gap-#4 owning-module fallback, the direct helper does not.
                let under = self
                    .newtype_underlying(name)
                    .map(|u| subst(&u, &self.nominal_param_map(ty)));
                let Some(under) = under else {
                    return Some(Self::shape_invisible(ty));
                };
                self.walk_eq_members(ty, std::slice::from_ref(&under))
            }
            // A free type PARAMETER reached inside the walk must carry `Eq` among its declared
            // bounds. `may_be_equal` treats a `Param` as ERASED (`[T] == [T]` compiles once, with
            // `T` abstract) — right for the operator, wrong for a BOUND, which is an obligation the
            // CALL SITE must discharge. **Rust, measured (rustc 1.97.0):**
            // `fn f<T>(a: Vec<T>, b: Vec<T>) { g(a, b) }` against `fn g<U: Eq>` is E0277 *"required
            // for `Vec<T>` to implement `Eq`"*, and adding `T: Eq` compiles. Without this arm every
            // container of an unbounded `T` would satisfy `Eq`. (A bare `Ty::Param` never reaches
            // here — its kind is `"?"`, so the D1 arm skips it and its own declared-bounds arm
            // answers.)
            Ty::Param(_) => self
                .satisfies(ty, "Eq")
                .err()
                .map(|_| format!("{ty} is not bounded by Eq")),
            // Scalars, the identity handles, `Func`, `Module` — nothing a user `eq` can hide behind,
            // so there is genuinely nothing to prove. `Ty::Unknown` is the don't-cascade hole (a
            // prior error already reported). `Ty::Protocol` is the ONE permissive hole left, and it
            // is deliberate: the concrete witness is unknowable here, and it is not exotic — every
            // `Result[T]` carries `Ty::Protocol("Error")` in its error slot, so refusing it would
            // un-grant `Result` wholesale (and break the `("Eq", "eq", "result")` ratchet row). Same
            // hole, same reason, as `reaches_user_eq`'s.
            _ => None,
        }
    }

    /// The refusal for a type whose shape this walk cannot see. It is a REFUSAL, not a `None`: the
    /// caller reads `None` as a grant, and "the table missed" is not evidence of soundness.
    fn shape_invisible(ty: &Ty) -> String {
        format!("the declaration of {ty} is not visible here, so its equality cannot be checked")
    }

    /// Every variant payload of an enum, INSTANTIATED for `ty`'s args — with the miss-only
    /// owning-module fallback (gap #4) that a bare `self.variants` scan lacks, so a named-fn-imported
    /// enum's payloads are walked instead of silently skipped.
    fn enum_payloads_of(&self, name: &str, args: &[Ty]) -> Option<Vec<Ty>> {
        let map = self.enum_param_map(name, args);
        let subst_all = |ps: Vec<Ty>| ps.iter().map(|p| subst(p, &map)).collect();
        if self.enums.contains_key(name) {
            return Some(subst_all(
                self.variants
                    .values()
                    .filter(|v| v.enum_name == name)
                    .flat_map(|v| v.payload.clone())
                    .collect(),
            ));
        }
        let def = self.owning_enum_def(name)?;
        Some(subst_all(
            def.variants
                .iter()
                .flat_map(|v| v.payload.clone())
                .collect(),
        ))
    }

    /// One declared `eq`'s `where` bounds, resolved under the RECEIVER's own param→arg substitution
    /// (`{T -> Tag}` for a `Box[Tag]`). The same walk [`Self::satisfies_methods`] runs for a
    /// conditional protocol method — extracted rather than re-asked through `satisfies`, so a method
    /// that is not the `Eq` hook at all cannot turn into a conformance failure here.
    fn eq_where_unsatisfied(&self, ty: &Ty, sig: &FnSig) -> Option<String> {
        let tymap = self.nominal_param_map(ty);
        for wb in &sig.where_bounds {
            // A `where` naming the METHOD's own `[U]` is merged into `type_params`, never into
            // `where_bounds`, so a name missing from the receiver's map is not this rule's business.
            let Some(concrete) = tymap.get(&wb.name) else {
                continue;
            };
            for bound in &wb.bounds {
                let bargs: Vec<Ty> = bound.args.iter().map(|a| self.resolve_ty_ro(a)).collect();
                if let Err(why) = self.satisfies_args(concrete, &bound.name, &bargs) {
                    // Normally the inner text is redundant with what we say here, so it is dropped.
                    // A BUDGET refusal is the exception: rewording it into "requires X: Eq" would
                    // name a bound that did not actually fail, hiding the one verdict the user needs
                    // to act on. Carry it out verbatim instead.
                    if why.contains(EQ_BUDGET_MARKER) {
                        // Re-state it for THIS type rather than forwarding the inner text: the
                        // caller wraps whatever comes back in "type X does not satisfy Eq (…)", so
                        // forwarding would nest one wrapper per level and hand the user a message
                        // hundreds of frames deep. The verdict is what matters and it is preserved.
                        return Some(Self::eq_budget_refusal(ty));
                    }
                    return Some(format!("{ty}'s `eq` requires {concrete}: {}", bound.name));
                }
            }
        }
        None
    }

    /// The budget refusal, in one place because two sites emit it — [`Self::enter_eq_obligation`]
    /// where it originates and [`Self::eq_where_unsatisfied`] where it is re-stated on the way out.
    /// It carries [`EQ_BUDGET_MARKER`] so those two can recognise each other.
    fn eq_budget_refusal(ty: &Ty) -> String {
        // The type is ELIDED, because the shape that trips this budget is precisely the one whose
        // name is enormous: polymorphic recursion refuses at `N[List[List[…×128…[int]]]]`, ~1 KB on
        // one line, per diagnostic, streamed to the editor. Keep the head — that is what the user
        // wrote and can act on — and say plainly that the rest is omitted. (rustc has the same
        // problem and writes the full type to a side file; eliding is the cheaper half of that.)
        const HEAD: usize = 60;
        let shown = ty.to_string();
        let shown = match shown.char_indices().nth(HEAD) {
            Some((cut, _)) => format!("{}… (elided)", &shown[..cut]),
            None => shown,
        };
        format!("{shown} {EQ_BUDGET_MARKER} to prove its equality reaches no unmet `where` bound")
    }

    /// A struct/enum/newtype's own type params bound to the args of THIS instantiation
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
            Ty::NewType(key, targs) => self
                .newtype_type_params_of(key)
                .map(|tps| {
                    tps.iter()
                        .map(|tp| tp.name.clone())
                        .zip(targs.iter().cloned())
                        .collect()
                })
                .unwrap_or_default(),
            _ => HashMap::new(),
        }
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
                    self.error(
                        span,
                        format!("{name}[{t}]() expected element type {t}, found {inferred}"),
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
        // struct-ctor branch fully supports. Mirrors the newtype arm below + the ctor branch itself.
        if let Some(i) = self.structs.get(&self.bare_key(name)) {
            return !i.type_params.is_empty();
        }
        // A generic newtype constructor takes turbofish type args (`Stack[int]([])`) — report it so
        // the args aren't pre-rejected by `infer_named_call`'s non-generic gate.
        if self.newtype_names.contains(name) && self.newtype_is_generic(&self.bare_key(name)) {
            return true;
        }
        // Bare-name query (the qualified path resolves genericity directly). A variant name may now
        // belong to several enums; treat it as generic if any owner enum is generic. `variant_owners`
        // stores BARE enum names but `enum_type_params` is module-keyed, so go through `bare_key`
        // (same keying fix as the struct/newtype arms above — else a bare generic-enum variant like
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
        tps: &[TypeParam],
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
    pub(super) fn recover_iter_elems(
        &mut self,
        tps: &[TypeParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name).cloned() {
                for b in &tp.bounds {
                    if (b.name == "Iterator" || b.name == "Iterable")
                        && let Some(arg) = b.args.first()
                        && let Some(elem) = self.iter_elem(&concrete)
                    {
                        binds.push((self.resolve_bound_arg(arg, tps, span), elem));
                    }
                }
            }
        }
        for (arg_ty, elem) in &binds {
            // Bind the element param if it's still free; otherwise it was already pinned (an explicit
            // type arg, another argument position, or a concrete `Iterator[int]` bound) and the
            // recovered element MUST agree — `unify` is a silent no-op there, so check it ourselves.
            match arg_ty {
                Ty::Param(n) if !sub.contains_key(n) => {
                    if !elem.is_unknown() {
                        sub.insert(n.clone(), elem.clone());
                    }
                }
                _ => {
                    let pinned = match arg_ty {
                        Ty::Param(n) => sub.get(n).cloned().unwrap_or(Ty::Unknown),
                        other => other.clone(),
                    };
                    if !pinned.is_unknown() && !elem.is_unknown() && !self.assignable(&pinned, elem)
                    {
                        self.error(
                            span,
                            format!("iterator element type {elem} does not match the declared element type {pinned}"),
                        );
                    }
                }
            }
        }
    }

    /// Recover the `K`/`V` (`Index`/`IndexSet`) and `R` (`Slice`) type args of parameterized bounds
    /// from each type parameter's inferred binding — the indexing analogue of `recover_iter_elems`,
    /// so `fn first[C: Index[int, V], V](c: C) -> V` recovers `V` from the argument.
    pub(super) fn recover_index_args(
        &mut self,
        tps: &[TypeParam],
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
                                binds.push((self.resolve_bound_arg(a, tps, span), k));
                            }
                            if let Some(a) = b.args.get(1) {
                                binds.push((self.resolve_bound_arg(a, tps, span), v));
                            }
                        }
                    }
                    "Slice" => {
                        if let Some(r) = self.slice_result(&concrete)
                            && let Some(a) = b.args.first()
                        {
                            binds.push((self.resolve_bound_arg(a, tps, span), r));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (arg_ty, recovered) in &binds {
            // Bind the arg param if still free; otherwise it was already pinned and must agree.
            match arg_ty {
                Ty::Param(n) if !sub.contains_key(n) => {
                    if !recovered.is_unknown() {
                        sub.insert(n.clone(), recovered.clone());
                    }
                }
                _ => {
                    let pinned = match arg_ty {
                        Ty::Param(n) => sub.get(n).cloned().unwrap_or(Ty::Unknown),
                        other => other.clone(),
                    };
                    if !pinned.is_unknown()
                        && !recovered.is_unknown()
                        && !self.assignable(&pinned, recovered)
                    {
                        self.error(
                            span,
                            format!(
                                "index type {recovered} does not match the declared type {pinned}"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// Enforce each type parameter's declared protocol bounds against its inferred binding. A
    /// parameterized bound (`Container[int]`) supplies type args, resolved here (sibling params in
    /// scope) and checked structurally with the protocol's params substituted.
    pub(super) fn enforce_bounds(
        &mut self,
        tps: &[TypeParam],
        sub: &HashMap<String, Ty>,
        span: Span,
    ) {
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
                    // Resolve the bound's args, then substitute any params recovered into `sub` (e.g.
                    // `Index[int, V]` with `V` recovered to `int`) so the structural/intrinsic check
                    // sees concrete args, not a still-free `Ty::Param`.
                    let bargs: Vec<Ty> = bound
                        .args
                        .iter()
                        .map(|a| subst(&self.resolve_bound_arg(a, tps, span), sub))
                        .collect();
                    if let Err(msg) = self.satisfies_args(concrete, &bound.name, &bargs) {
                        // A `Comparable`-shaped bound over a newtype that WROTE `compare`: say why
                        // the method can never satisfy it. The gate is UNCHANGED from M23 — both
                        // names, which `Comparable` has (`eq` through its embed) and a user protocol
                        // with only its own `compare` does not — so the sentence keeps naming
                        // `Comparable` truthfully and cannot start firing on an unrelated bound.
                        let note = (self.protocol_has_method(&bound.name, "compare")
                            && self.protocol_has_method(&bound.name, "eq"))
                        .then(|| self.newtype_compare_dead_end(concrete))
                        .flatten();
                        self.error(
                            span,
                            match note {
                                Some(hint) => format!("{msg}: {hint}"),
                                None => msg,
                            },
                        );
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
            Ty::Struct(k, _) | Ty::Enum(k, _) | Ty::NewType(k, _) => k,
            _ => return None,
        };
        let tps = self
            .struct_shape(key)
            .map(|s| &s.type_params)
            .or_else(|| self.enum_type_params_of(key))
            .or_else(|| self.newtype_type_params_of(key))?;
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
        !self.is_local_binding(name) && self.type_params.contains_key(name)
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

    /// M24 — `T.method(args)` where `T` is an in-scope generic type PARAMETER: the static-witness
    /// call. The instance twin is `infer_method_call`'s `Ty::Param` arm; this one differs in that
    /// there is no receiver (every `msig.params` slot is a real argument) and `Self` maps to
    /// `Ty::Param(T)` rather than to a receiver type.
    ///
    /// Accepted only when BOTH hold, because both are what the compiler can actually lower:
    /// * one of `T`'s bounds declares `method` as a **static** requirement, and
    /// * `T`'s hidden `$w:T` witness binding is reachable here ([`Checker::witness_scope`]) — the
    ///   declaring body, or (Task 4) any nested body inside it, which captures it.
    ///
    /// Anything else keeps the pre-M24 "generics are erased" diagnostic.
    pub(super) fn infer_witness_static_call(
        &mut self,
        tname: &str,
        method: &str,
        args: &[Expr],
        span: Span,
    ) -> Ty {
        let in_scope = self.witness_scope.iter().any(|w| w == tname);
        // A type param declared by the ENCLOSING TYPE (`struct Bx[T]`) can never carry a witness,
        // WHATEVER its bounds: the witness lives in the calling frame, and a `Bx` value has no
        // frame — so this verdict is checked BEFORE the bound lookup. It used to sit after it, so an
        // UNBOUNDED enclosing param was told to "bound it by a protocol", which is advice that
        // produces a second rejection. A method may not reuse its host's type-param name
        // (`method_type_param_shadowing_struct_param_rejected`), so a hit here is always the host's.
        // The `in_scope` guard keeps a legitimately witnessed member param out of this arm.
        if !in_scope && let Some(host) = self.enclosing_type_declaring(tname) {
            self.infer_all(args);
            self.error(
                span,
                format!(
                    "`{tname}.{method}(...)`: '{tname}' is a type parameter of the enclosing type '{host}', which carries no hidden type witness — the concrete type is erased once a '{host}' value exists, so only a value could hold it, and no bound on '{host}' can change that. Declare the type parameter on the MEMBER instead (`fn {method}_of[{tname}: <bound>](self, ...)`, whose witness rides on the call), or pass a factory function (a `fn(...) -> {tname}` parameter/field)"
                ),
            );
            return Ty::Unknown;
        }
        let bounds = self.type_params.get(tname).cloned().unwrap_or_default();
        let found = bounds.iter().find_map(|b| {
            self.protocol_method_sig(&b.name, method)
                .filter(|s| s.is_static)
                .map(|s| (b.clone(), s))
        });
        let Some((bound, msig)) = found else {
            self.infer_all(args);
            // Name a protocol that ALREADY declares this static method rather than inventing a
            // signature for it — the invented `fn {method}(...) -> Self` was simply wrong for a
            // requirement returning anything else. rustc does the same ("the following trait defines
            // an item `tag`, perhaps you need to restrict type parameter `Item` with it").
            let mut hosts: Vec<&String> = self
                .protocols
                .keys()
                .filter(|p| {
                    self.protocol_method_sig(p, method)
                        .is_some_and(|s| s.is_static)
                })
                .collect();
            hosts.sort();
            let advice = match hosts.first() {
                Some(p) => format!("bound '{tname}' by '{p}', which declares a static '{method}'"),
                None => format!(
                    "no protocol in scope declares a static '{method}' — declare one and bound '{tname}' by it"
                ),
            };
            self.error(
                span,
                format!(
                    "cannot call a static method through the generic type parameter '{tname}' (`{tname}.{method}`): no bound on '{tname}' declares a static method '{method}', so generics are erased here and there is no concrete type to dispatch to — {advice}, call the concrete type's static method directly (e.g. `SomeType.{method}(...)`), or pass a factory function (a `fn(...) -> {tname}` parameter)"
                ),
            );
            return Ty::Unknown;
        };
        if !in_scope {
            self.infer_all(args);
            // Reached when the `enclosing_type_declaring` arm above cannot see the host — notably a
            // nested `fn` inside a method of `struct Bx[T]`, where `current_self_ty` is reset. So
            // this message must ALSO be true for a type-declared param: it states the rule, and
            // never claims the current body carries a witness.
            self.error(
                span,
                format!(
                    "`{tname}.{method}(...)`: no hidden type witness for '{tname}' is reachable here. One exists only where '{tname}' is declared by a FUNCTION or a MEMBER (`fn f[{tname}: <bound>](...)`) — in that body and in any closure, `spawn:`/`defer:` block or nested `fn` inside it. A type parameter of an enclosing TYPE (`struct Bx[{tname}]`) never has one: the concrete type is erased once a value exists. Declare '{tname}' on the function or member, or pass a factory function (a `fn(...) -> {tname}` parameter)"
                ),
            );
            return Ty::Unknown;
        }
        // `Self` is the type param itself (the body is still checked abstractly), plus the
        // parameterized protocol's own params mapped to the bound's concrete args (`Convert[int]`
        // ⇒ `S ↦ int`), so a requirement `fn convert(x: S) -> Self` types as `(int) -> T`.
        let mut map = HashMap::from([("Self".to_string(), Ty::Param(tname.to_string()))]);
        let ptps = self
            .protocols
            .get(&bound.name)
            .map(|p| p.type_params.clone())
            .unwrap_or_default();
        for (pn, parg) in ptps.iter().zip(&bound.args) {
            let resolved = self.resolve_type(parg, span);
            map.insert(pn.clone(), resolved);
        }
        // A STATIC requirement has NO receiver slot, so every declared param is a real argument.
        let expected: Vec<Ty> = msig.params.iter().map(|t| subst(t, &map)).collect();
        self.check_args(method, &expected, args, span);
        subst(&msig.ret, &map)
    }

    /// M24 — record (or reject) the witness arguments a call to `name` needs. Runs AFTER every
    /// type-param recovery pass in [`Self::infer_generic_call`], because a param recovered only by
    /// the loop-back would otherwise look un-determined here.
    ///
    /// Task 3 — the callee's module does NOT matter here. The entry is keyed by the CALLING module
    /// ([`crate::checker::witness_key`]), which is the module the compiler is emitting when it looks
    /// it up; the callee's own requirement rides on its [`FnSig::witness_params`], which crosses the
    /// boundary inside its `ModuleSig`. So a `from`-imported callee (`reset(c)`) and a qualified one
    /// (`lib.reset(c)`) record exactly like a local one. What is NOT recorded here — a `defer`/`spawn`
    /// target — is walled by [`Self::reject_witness_spawn_defer_target`], in every spelling.
    ///
    /// `key_span` is the [`crate::checker::witness_key_span`] of this call site (the member-name
    /// token for a `Field` callee, the call node otherwise) and is a TABLE KEY only; every
    /// diagnostic anchors on `span`, the call node — the two must never be conflated (`49bd9f80`).
    pub(super) fn record_witness_call(
        &mut self,
        name: &str,
        wparams: &[String],
        sub: &HashMap<String, Ty>,
        key_span: Span,
        span: Span,
        recv: WitnessCallee,
    ) {
        // A `spawn`/`defer` TARGET lowers at its own emit site (`Op::SpawnCall`/`SpawnMethod`/
        // `DeferCall`/`DeferMethod`), none of which push a hidden argument — so the call is refused
        // rather than lowered one `argc` short. Matched on the call site's own KEY span, which is
        // unique per call node, so only the target itself is refused: an ARGUMENT that is a witness
        // call (`spawn f(reset(c))`) evaluates eagerly in this frame and stays legal.
        if let Some((target, kw, reported)) = self.witness_indirect_target
            && target == key_span
        {
            // …unless `reject_witness_spawn_defer_target` already said exactly this, at exactly this
            // span (the two arms overlap on a bare free-fn target). One error, one message.
            if !reported {
                self.error(
                    span,
                    format!(
                        "'{name}' takes a static-protocol bound ({}), so it cannot be the target of \
                         `{kw}` yet — call it eagerly and `{kw}` the result, or wrap the call in a closure",
                        wparams.join(", ")
                    ),
                );
            }
            return;
        }
        let mut srcs = Vec::with_capacity(wparams.len());
        for w in wparams {
            // Presence in `sub` is NOT enough: `enforce_bounds` silently SKIPS a param missing from
            // the map, and a param bound to a partly-`Unknown` type has no runtime identity either.
            let concrete = sub.get(w).filter(|t| ty_fully_concrete(t));
            match concrete {
                // The `Ty::Struct`/`Ty::Enum` name IS the runtime identity key (`<module-key>::Name`,
                // bare for a std/native type) — the checker and the compiler derive it from the same
                // `resolver::module_keys`, so this is exactly what `do_static_call` resolves.
                Some(Ty::Struct(key, _) | Ty::Enum(key, _)) => {
                    srcs.push(WitnessSrc::Concrete(key.clone()));
                }
                Some(other) => {
                    let other = other.clone();
                    self.error(
                        span,
                        format!(
                            "type parameter '{w}' of '{name}' is bound to {other}, which cannot host a \
                             static method — only a struct or an enum can"
                        ),
                    );
                    return;
                }
                // Bound to the CALLER's own still-abstract type param — FORWARDING (slice 2). The
                // caller's `$w:p` binding becomes the argument, but ONLY when it is reachable here:
                // `witness_scope` is empty in a body that neither declares `p` nor is nested inside
                // one that does. Forwarding from such a body would push a name the callee reads as
                // an identity key (or nothing at all), so it stays an error.
                None if matches!(sub.get(w), Some(Ty::Param(_))) => {
                    let Some(Ty::Param(p)) = sub.get(w) else {
                        unreachable!("guarded by the arm's own pattern")
                    };
                    let p = p.clone();
                    if !self.witness_scope.contains(&p) {
                        self.error(
                            span,
                            format!(
                                "type parameter '{w}' of '{name}' is bound to {p}, which is still \
                                 abstract here, and no hidden type witness for '{p}' is reachable \
                                 at this call site. One exists only where '{p}' is declared by a \
                                 FUNCTION or a MEMBER — in that body and in any closure, \
                                 `spawn:`/`defer:` block or nested `fn` inside it. A type \
                                 parameter of an enclosing TYPE (`struct Bx[{p}]`) never has one: \
                                 the concrete type is erased once a value exists. Declare '{p}' on \
                                 the member instead (`fn m[{p}: <bound>](self, ...)`), or take a \
                                 factory parameter (`fn(...) -> {p}`) and call that"
                            ),
                        );
                        return;
                    }
                    srcs.push(WitnessSrc::Forward(p));
                }
                None => {
                    // The suggested spelling has to be one that PARSES. A member's type arguments
                    // go on the METHOD (`h.make[Counter]()`); `make[Counter](...)` is read as a free
                    // call and answers "'make' takes no type arguments", and an annotated result
                    // does not reach a method's own `[T]` either — so neither is offered there.
                    let pin = match &recv {
                        WitnessCallee::Free => format!(
                            "pin it with a type argument (`{name}[SomeType](...)`) or an annotated result"
                        ),
                        WitnessCallee::Dotted(prefix) => format!(
                            "pin it with a type argument (`{prefix}.{name}[SomeType](...)`) or an annotated result"
                        ),
                        WitnessCallee::Member => format!(
                            "pin it with a type argument ON THE METHOD (`<receiver>.{name}[SomeType](...)`)"
                        ),
                    };
                    self.error(
                        span,
                        format!(
                            "type parameter '{w}' of '{name}' is not determined here, so its static \
                             protocol method has no concrete type to dispatch to — {pin}"
                        ),
                    );
                    return;
                }
            }
        }
        if self.harvest_keywords {
            crate::checker::record_call_table_entry(
                &mut self.witnesses.calls,
                &mut self.table_conflicts,
                crate::checker::witness_key(
                    self.graph_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    key_span,
                ),
                srcs,
                "static-witness",
                span,
            );
        }
    }

    /// Type-check a call to a generic function: infer each type parameter from the arguments,
    /// enforce the declared bounds, and substitute into the return type. Shared by the bare callee
    /// (local or `from`-imported) and the module-qualified one (`m.f(...)`) — M24's witness record is
    /// keyed by the CALLING module either way, so the spelling makes no difference here (Task 3).
    /// `key_span` is this call site's [`crate::checker::witness_key_span`]: the call node's own span
    /// for the bare spelling, the member-name token for the module-qualified one (`lib.reset(c)`).
    /// `recv` is how the callee is SPELLED — the two spellings differ only in the pin the
    /// "not determined here" diagnostic may suggest ([`WitnessCallee`]).
    #[allow(clippy::too_many_arguments)] // the callee's sig pieces + call args + both spans + hint
    pub(super) fn infer_generic_call(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &[Expr],
        targs: &[Ty],
        key_span: Span,
        span: Span,
        hint: Option<&Ty>,
        recv: WitnessCallee,
    ) -> Ty {
        if args.len() != sig.params.len() {
            self.check_arity(name, sig.params.len(), args, span);
        }
        let arg_tys = self.infer_generic_arg_tys(args);
        // Explicit call-site type arguments (`max[int](…)`) seed the substitution; remaining (or
        // all, when none given) parameters are inferred from positional arguments. `unify` only
        // binds a parameter that isn't already in the map, so explicit args take precedence and a
        // conflicting argument is caught by the per-argument check below.
        let mut subst_map: HashMap<String, Ty> =
            self.seed_targs(name, &sig.type_params, targs, span);
        for (i, (decl, actual)) in sig.params.iter().zip(&arg_tys).enumerate() {
            // Bug D (free-fn analog): for an unannotated CLOSURE arg, unify against a RETURN-MASKED
            // copy so only its PARAMETER positions can bind a function type param in pass 1. Its
            // prepass return may be a leaked `Ty::Param` (an unannotated body that is a nested free
            // generic call, `fn(x): ident(x)`); letting it bind here would prematurely pin the fn's
            // return-position `[U]` to that leaked param, and the loop-back in
            // `recover_return_only_params` (which only fills params still FREE) could not correct it.
            // Non-closure args unify unchanged. Mirrors `infer_generic_method`.
            if matches!(
                args.get(i).map(|a| &a.kind),
                Some(ExprKind::Closure { ret: None, .. })
            ) {
                unify(decl, &mask_closure_ret(actual), &mut subst_map);
            } else {
                unify(decl, actual, &mut subst_map);
            }
        }
        // Bug 1 recovery (free-fn path only): after the masked pass-1 above has let every VALUE and
        // closure-PARAMETER arg pin its params, bind the HOF's return-only `[U]` from a bare closure
        // whose prepass return is ALREADY CONCRETE (`fn(): 5` → `int`, no leaked `Ty::Param`). This runs
        // BEFORE `report_uninferable_closure_params` so a `[U]` recoverable from such a closure RETURN is
        // not mis-reported as an un-inferable deadlock when a SIBLING closure uses the same `[U]` in
        // PARAMETER position (`pair(fn(): 5, fn(x): x + 1)` — adversarial-review bug 1). `unify` only
        // binds a param still FREE, so a sibling VALUE arg that already pinned `[U]` STILL WINS (no
        // closure-vs-value binding race, e.g. `apply(fn(x): str(x), 5, 99)` keeps `B` pinned to the
        // `sink: B` = 99 → `int` and the mismatching closure is rejected). A LEAKED-param prepass return
        // (`fn(x): ident(x)`) stays masked, deferred to the loop-back in `recover_return_only_params`.
        for (i, (decl, actual)) in sig.params.iter().zip(&arg_tys).enumerate() {
            // Gate on FULLY CONCRETE (no `Ty::Param` AND no `Ty::Unknown`, nested too), not merely
            // "contains no param": a param-dependent body prepass-types to an Unknown-CORED container
            // (`fn(x): [x]` → `List[Unknown]`), which has no `Ty::Param` — so a bare no-param gate would
            // `unify(U, List[Unknown])`, binding `U = List[Unknown]` (unify only skips a TOP-LEVEL
            // Unknown, not a nested one) and laundering `List[str]` onto it. A fully-concrete prepass
            // return (`fn(): 5` → `int`) still pre-binds here (needed so the `pair(fn(): 5, fn(x): x+1)`
            // ordering resolves); an Unknown/param-cored one defers to the loop-back's refined
            // checking-mode re-inference, which recovers the concrete type.
            if matches!(
                args.get(i).map(|a| &a.kind),
                Some(ExprKind::Closure { ret: None, .. })
            ) && matches!(actual, Ty::Func { ret, .. } if ty_fully_concrete(ret))
            {
                unify(decl, actual, &mut subst_map);
            }
        }
        // Recover element types from parameterized `Iterator[T]` bounds (bind `T` to the iterand's
        // element), then enforce every declared bound against its inferred binding.
        self.recover_iter_elems(&sig.type_params, &mut subst_map, span);
        self.recover_index_args(&sig.type_params, &mut subst_map, span);
        // Expected-type checking-mode: a `let`/return/param annotation seeds any type param the args
        // left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing) against the
        // hint — so `xs: List[int] = empty()` pins a return-only `T`, and the deadlock probe below
        // sees it bound. After arg-unification ⇒ turbofish/args win.
        seed_from_hint(hint, &sig.ret, &mut subst_map);
        // Same un-inferable closure-param deadlock guard as the struct-ctor path: report the cause
        // (and bind the params to Unknown) before the per-arg closure body is checked.
        self.report_uninferable_closure_params(
            name,
            &sig.type_params,
            &sig.params,
            args,
            &mut subst_map,
            span,
        );
        self.enforce_bounds(&sig.type_params, &subst_map, span);
        // Each argument must match its parameter's substituted type (catches a type param used in
        // two positions with conflicting types, e.g. `max(1, "x")`), AND recover a return-only type
        // param from an inferable closure/fn body (Bug D's loop-back, now shared with the method
        // path): the free-fn path formerly discarded the refined closure type, leaking `Ty::Param`
        // into the return so a downstream `+1`/`.upper()` was spuriously rejected.
        self.recover_return_only_params(
            name,
            &sig.params,
            &arg_tys,
            args,
            &sig.params,
            &sig.type_params,
            &mut subst_map,
            span,
            false,
        );
        // M24 — half two of the static-witness contract, recorded LAST: `recover_return_only_params`
        // above can still bind a param that `enforce_bounds` never saw, so anything earlier would
        // read a param as un-determined that the call actually pins.
        if !sig.witness_params.is_empty() {
            let wparams = sig.witness_params.clone();
            self.record_witness_call(name, &wparams, &subst_map, key_span, span, recv);
        }
        subst(&sig.ret, &subst_map)
    }

    /// Infer a generic *method*'s own type parameters from the call arguments. `params`/`ret` are the
    /// method signature already substituted with the receiver struct's type arguments, so only the
    /// method's own `[U]` params remain free; `params[0]` is the receiver (bound from `obj`, not an
    /// explicit arg). `targs` are the EXPLICIT member-level turbofish (`obj.method[A, B](...)`): they
    /// seed the `[U]` params first; the rest are inferred positionally. Mirrors `infer_generic_call`.
    ///
    /// M24 Task 5 — `wparams` are the method's [`FnSig::witness_params`] (empty for every native
    /// method and for a user method that never constructs through a bound), and `key_span` is this
    /// call site's [`crate::checker::witness_key_span`] — the method-name token, so two links of one
    /// postfix chain cannot collide on the shared call span.
    #[allow(clippy::too_many_arguments)] // the method's resolved signature pieces + receiver + targs + call
    pub(super) fn infer_generic_method(
        &mut self,
        method: &str,
        params: &[Ty],
        ret: &Ty,
        mtps: &[TypeParam],
        wparams: &[String],
        recv_ty: &Ty,
        targs: &[Ty],
        args: &[Expr],
        key_span: Span,
        span: Span,
    ) -> Ty {
        // The first parameter is the receiver (bound from `obj`). A method with NO params has no
        // receiver slot — reject, mirroring the non-generic path.
        let Some((receiver, expected)) = params.split_first() else {
            self.error(
                span,
                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
            );
            self.infer_all(args);
            return Ty::Unknown;
        };
        if args.len() != expected.len() {
            self.error(
                span,
                format!(
                    "'{method}' expects {} argument(s), got {}",
                    expected.len(),
                    args.len()
                ),
            );
        }
        let mut arg_tys = self.infer_generic_arg_tys(args);
        // Explicit member-level turbofish seeds the `[U]` params (arity-checked); `unify` only binds
        // a param not already in the map, so an explicit targ wins and a conflicting arg is caught by
        // the per-argument check below.
        let mut mmap: HashMap<String, Ty> = self.seed_targs(method, mtps, targs, span);
        // A method type param may appear in the receiver position (`fn f[U](u: U)`); bind it from the
        // actual receiver type so it isn't left unresolved.
        unify(receiver, recv_ty, &mut mmap);
        // Clamp to the shorter of the two lengths — `arg_tys.len() == args.len()` can be < `expected.len()`
        // when the method is called with too few arguments. The arity error is already reported above
        // (it does not early-return), so this loop must not index `args[i]`/`arg_tys[i]` out of bounds;
        // the base `expected.iter().zip(&arg_tys)` clamped implicitly, this preserves that.
        for i in 0..expected.len().min(arg_tys.len()) {
            let decl = &expected[i];
            // Scope-A-through-a-method-slot: a bare same-module GENERIC fn passed as a non-closure arg
            // is prepass-typed rigid (`fn(T) -> str`) because `infer_generic_arg_tys` has no expected
            // hint. Re-pin its OWN `[T]` from the slot with everything bound SO FAR (`fn(int) -> U` for
            // `.map`; `fn(int, int) -> int` for `.fold`'s arg1 once `init` bound `U`), replacing the
            // rigid prepass type with the concrete one so pass-1 unify + the loop-back compose. This is
            // interleaved (uses the LIVE `mmap`) so `.fold`'s accumulator ordering holds. The helper only
            // fires on a bare-ident generic fn that pins FULLY concrete; otherwise `arg_tys[i]` is
            // unchanged and behavior is byte-identical.
            let want = subst(decl, &mmap);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, span) {
                arg_tys[i] = refined;
            }
            let actual = &arg_tys[i];
            // Bug D: for a CLOSURE arg, unify against a RETURN-MASKED copy so only its PARAMETER
            // positions can bind a method type param in pass 1. Its prepass return may be a leaked
            // `Ty::Param` (an unannotated body that is a nested free generic call, `fn(x): ident(x)`);
            // letting it bind here would prematurely pin the method's return-position `[U]` to that
            // leaked param, and the loop-back below (which only fills params still FREE) could not
            // correct it. Masking defers `U` to the loop-back's checking-mode re-inference, which
            // recovers it as the CONCRETE return type. Non-closure args unify unchanged.
            if matches!(
                args.get(i).map(|a| &a.kind),
                Some(ExprKind::Closure { ret: None, .. })
            ) {
                unify(decl, &mask_closure_ret(actual), &mut mmap);
            } else {
                unify(decl, actual, &mut mmap);
            }
        }
        // Recover element types from `Iterator[T]` bounds, then enforce every declared bound.
        self.recover_iter_elems(mtps, &mut mmap, span);
        self.recover_index_args(mtps, &mut mmap, span);
        self.enforce_bounds(mtps, &mmap, span);
        // The receiver must still match its declared type AFTER substitution. Without this, a method
        // type param in receiver position (`fn m[U](self: U)`) turbofished to a contradicting type
        // (`b.m[str]()` on a `Box[int]`) is unchecked — `unify` silently drops the conflict once `[str]`
        // is seeded — and a wrong static type escapes onto the value (a soundness hole).
        let want_recv = subst(receiver, &mmap);
        if !self.assignable(&want_recv, recv_ty) {
            self.error(
                span,
                format!("receiver of '{method}' has type {recv_ty}, expected {want_recv}"),
            );
        }
        // Bug D closure-return recovery (shared with the free-fn path via `recover_return_only_params`):
        // re-infer each closure arg in checking-mode, reject a wrong CONCRETE return, loop-back-unify to
        // fill a still-free return-only `[U]`, re-enforce newly-recovered bounds, and degrade a still-
        // unbound param-position param to `Unknown`. `expected` = arg slots (sans receiver); `params` =
        // the full list incl receiver for the param-position degrade.
        self.recover_return_only_params(
            method, expected, &arg_tys, args, params, mtps, &mut mmap, span, true,
        );
        // M24 Task 5 — half two of the static-witness contract for a MEMBER-declared type param,
        // recorded LAST for the same reason the free-fn path does it last (`recover_return_only_params`
        // can still bind a param nothing else saw). The receiver's own type args are already
        // substituted into `params`/`ret` by the caller, so `mmap` holds only the METHOD's params —
        // which is exactly the set that can be witnessed.
        if !wparams.is_empty() {
            self.record_witness_call(
                method,
                wparams,
                &mmap,
                key_span,
                span,
                WitnessCallee::Member,
            );
        }
        subst(ret, &mmap)
    }

    /// Scope A, delivered through a generic METHOD's concrete parameter slot. `arg` is a call argument
    /// to a builtin/generic container method (`.map`/`.fold`/…); `want` is that argument's declared slot
    /// type substituted with everything the method has pinned SO FAR (for `.map` this is `fn(int) -> U`,
    /// for `.fold`'s arg1 `fn(int, int) -> int`). When `arg` is a BARE reference to a same-module generic
    /// fn (`[1,2,3].map(conv)`), its `[T]` params are pinned from `want`'s concrete positions and the
    /// FULLY-substituted concrete fn type returned — the direct analog of `infer_ident`'s Scope A (a HOF
    /// PARAMETER-slot hint), which fires for a user HOF but not for these own-`[U]`-carrying builtin
    /// methods (their arg goes through `infer_generic_arg_tys` with no `expected_hint`, so it stays
    /// rigid). Returns `None` (leaving the rigid prepass type untouched) unless EVERY arg-fn param binds
    /// AND the result is fully concrete — so a still-free `want` slot (`.fold`'s arg1 before `init` binds
    /// `U`) or a genuinely un-inferable return-only arg-fn param defers to the existing path unchanged,
    /// preserving the Category-1 leak guard and every clean reject. A FRESH substitution map per call
    /// means two distinct pins never launder.
    fn try_pin_generic_fn_value_arg(&mut self, arg: &Expr, want: &Ty, span: Span) -> Option<Ty> {
        // Only a bare identifier that is NOT shadowed by an in-scope binding (`lookup` None) and IS a
        // same-module generic fn — mirrors Scope A's gate exactly.
        let ExprKind::Ident(name) = &arg.kind else {
            return None;
        };
        if self.lookup(name).is_some() || !self.local_fn_names.contains(name) {
            return None;
        }
        let sig = self.functions.get(name)?;
        if sig.type_params.is_empty() {
            return None;
        }
        // The slot must be a concrete-arity `fn(..) -> ..` (everything pinned so far); its param slots
        // drive the pin (`unify` binds the arg fn's params from them).
        let Ty::Func { params: wp, .. } = want else {
            return None;
        };
        if wp.len() != sig.params.len() {
            return None;
        }
        // Clone sig fields before the &mut-self `enforce_bounds` call (mirrors infer_ident Scope A).
        let type_params = sig.type_params.clone();
        let declared = Ty::Func {
            params: sig.params.clone(),
            ret: Box::new(sig.ret.clone()),
            labels: FnLabels(sig.labels.clone()),
        };
        let mut m: HashMap<String, Ty> = HashMap::new();
        unify(&declared, want, &mut m);
        // Accept ONLY when every arg-fn param bound AND the refined type is fully concrete. A slot
        // position that is still a free method param (`.fold` arg1 before `init` binds `U`) or a
        // return-only arg-fn param never pinned leaves a `Ty::Param` in `refined` → bail, unchanged.
        if !type_params.iter().all(|tp| m.contains_key(&tp.name)) {
            return None;
        }
        let refined = subst(&declared, &m);
        if !ty_fully_concrete(&refined) {
            return None;
        }
        // Enforce the arg fn's declared bounds against the bindings, exactly as Scope A does.
        self.enforce_bounds(&type_params, &m, span);
        Some(refined)
    }

    /// Bug D's closure-return recovery, shared by the generic-METHOD (`infer_generic_method`) and
    /// generic FREE-FN/module-qualified-fn (`infer_generic_call`) HOF paths. After pass-1 unification
    /// has seeded `map` with everything the argument SHAPES pin, this:
    ///  1. re-infers each closure arg in checking-mode against its substituted slot type (binding its
    ///     unannotated params, re-reporting its body errors) and captures the REFINED type;
    ///  2. SOUNDNESS: when the closure's expected return is ALREADY concrete (a return-only `[U]` pinned
    ///     by a sibling value arg or an explicit slot), rejects a genuinely wrong body against it — the
    ///     free-fn analog of `fold`-init laundering; a mask alone would launder a mismatching return;
    ///  3. LOOP-BACK: feeds the refined types into a SECOND `unify`, filling a return-only param still
    ///     free after pass 1 (recovered from an inferable closure/fn body) so it no longer leaks a
    ///     `Ty::Param` into the return;
    ///  4. re-enforces bounds on params NEWLY bound by the loop-back only (each enforced exactly once);
    ///  5. (METHOD PATH ONLY, `degrade_unbound_param_pos`) degrades a still-unbound PARAMETER-position
    ///     param to the refinable `Unknown` (the empty-collection case, `[].map(fn(x): x*2)` → `List[?]`,
    ///     matching the retired `infer_list_hof`), while leaving a genuinely un-inferable RETURN-ONLY
    ///     param as a leaked `Ty::Param` so `assignable` still rejects a concrete assignment.
    ///
    /// `arg_decls` are the per-argument declared slot types (method: params sans receiver; free-fn: all
    /// params) — parallel to `arg_tys`/`args`. `all_params` is the full param list used only for the
    /// param-position degrade scan (method: params INCL receiver; free-fn: all params).
    ///
    /// `degrade_unbound_param_pos` gates step 5. It is `true` ONLY on the generic-METHOD path, whose
    /// receiver-collection HOFs (`[].map(...)`) intentionally degrade an empty-collection element param
    /// to `List[?]`. It is `false` on the generic FREE-FN path: `infer_generic_call` never degraded, so
    /// a still-unbound param-position free-fn type param (`first([])` — `U` from an empty `List[U]` arg
    /// that flows to the return) must stay a leaked `Ty::Param` that downstream concrete use REJECTS,
    /// and the deliberate Category-2 "un-inferred type parameter; bind at the construction site"
    /// diagnostic must survive. Degrading it there silently laundered a compile error into a runtime
    /// panic (adversarial-review bugs 1 & 2). Free-fn CLOSURE-param type params left un-inferable by an
    /// empty arg are already bound to `Unknown` by the caller's `report_uninferable_closure_params`, so
    /// omitting the degrade there is behavior-preserving.
    ///
    /// The caller must complete pass-1 state (turbofish/arg-unify/iter+index recovery, and for the
    /// free-fn path its `report_uninferable_closure_params` + pass-1 `enforce_bounds`) BEFORE this call,
    /// so `bound_after_pass1` is correct and pass-1 bounds are enforced exactly once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn recover_return_only_params(
        &mut self,
        name: &str,
        arg_decls: &[Ty],
        arg_tys: &[Ty],
        args: &[Expr],
        all_params: &[Ty],
        tps: &[TypeParam],
        map: &mut HashMap<String, Ty>,
        span: Span,
        degrade_unbound_param_pos: bool,
    ) {
        // Snapshot the params bound after pass 1, so the loop-back below only re-enforces bounds on
        // params NEWLY bound from a refined arg (pass-1 bounds are enforced by the caller).
        let bound_after_pass1: std::collections::HashSet<String> = map.keys().cloned().collect();
        for (decl, (actual, arg)) in arg_decls.iter().zip(arg_tys.iter().zip(args)) {
            let want = subst(decl, map);
            // For a closure whose UNANNOTATED body is a nested free generic call, the prepass return
            // leaks the callee's own `Ty::Param` (`fn(?) -> T`) — not the lenient `Unknown` a direct
            // body yields — so an unmasked fallback check would spuriously mismatch `want`'s return
            // whether that return is a still-free `[U]` OR already concrete. Mask an unannotated
            // closure's fallback return: `check_generic_arg`'s internal check stays on params + arity,
            // and the REAL return contract is enforced below against the REFINED type. Non-closure and
            // annotated-closure args use their prepass type unchanged.
            let is_bare_closure = matches!(arg.kind, ExprKind::Closure { ret: None, .. });
            let fallback = if is_bare_closure {
                mask_closure_ret(actual)
            } else {
                actual.clone()
            };
            let refined = self.check_generic_arg(name, &want, &fallback, arg);
            // SOUNDNESS: when the closure's expected return is ALREADY concrete (a return-only `[U]`
            // pinned by a sibling value arg or an explicit slot), enforce it explicitly here against the
            // REFINED return — rejecting a genuinely wrong body while ACCEPTING a nested-generic-call
            // body that merely leaked a rigid `Ty::Param` in the prepass. When the expected return is a
            // still-free `[U]` it is non-concrete here and deferred to the loop-back (no contract yet).
            if is_bare_closure
                && let (Ty::Func { ret: want_ret, .. }, Ty::Func { ret: got_ret, .. }) =
                    (&want, &refined)
                && ty_fully_concrete(want_ret)
                && !self.assignable(want_ret, got_ret)
            {
                self.error(
                    arg.span,
                    format!("closure argument to '{name}' returns {got_ret}, expected {want_ret}"),
                );
            }
            // LOOP-BACK (INTERLEAVED per-arg): a closure re-inferred WITH its expected param types has a
            // concrete return (`fn(int) -> int`); feed it straight back into a SECOND `unify` NOW, before
            // the NEXT arg's `want` is substituted. `unify` only binds a param NOT already in the map and
            // IGNORES an `Unknown` actual, so pass-1-resolved params are a strict no-op — it ONLY fills a
            // return-position param still free after pass 1 (recovered from the closure body). Interleaving
            // (vs a separate post-loop pass) is load-bearing for SOUNDNESS: once one closure binds a
            // return-only `[U]`, a SIBLING closure binding the SAME `[U]` to a CONFLICTING type sees `want`
            // now CONCRETE and is REJECTED by the soundness check above, instead of being silently dropped
            // by only-bind-unbound `unify` (adversarial-review bug 2: `two(fn(x): x*2, fn(x): str(x))`).
            unify(decl, &refined, map);
        }
        // Enforce bounds for params NEWLY bound by the loop-back only (pass-1-bound params were already
        // enforced by the caller — each enforced exactly once avoids a double-report). So a
        // `[U: Add]`/`[U: Comparable]` recovered from a closure body still has its bound checked.
        let newly_bound: Vec<TypeParam> = tps
            .iter()
            .filter(|tp| !bound_after_pass1.contains(&tp.name) && map.contains_key(&tp.name))
            .cloned()
            .collect();
        self.enforce_bounds(&newly_bound, map, span);
        // Degrade a STILL-unbound type param to `Unknown` ONLY when it appears in a PARAMETER position —
        // and ONLY on the method path (`degrade_unbound_param_pos`). It was in principle recoverable
        // from an argument, but that argument's relevant type was itself `Unknown` (the empty-collection
        // case, `[].map(fn(x): x*2)`), so degrading yields `List[?]` rather than a leaked `List[U]`. A
        // param appearing ONLY in the RETURN position and in NO parameter is genuinely un-inferable
        // (`fn make[U]() -> U`); it must stay a leaked `Ty::Param` so `assignable` rejects a concrete
        // assignment. On the FREE-FN path the whole degrade is skipped: `infer_generic_call` never
        // degraded, so a param-position free-fn type param left unbound by an empty-collection arg
        // (`first([])`) must stay a leaked `Ty::Param` too — degrading it laundered a clean compile
        // error into a runtime panic and silently suppressed the Category-2 construction-site diagnostic.
        if !degrade_unbound_param_pos {
            return;
        }
        let wanted: std::collections::HashSet<String> =
            tps.iter().map(|tp| tp.name.clone()).collect();
        let mut in_param_pos: Vec<String> = Vec::new();
        for p in all_params {
            ty_collect_params(p, Some(&wanted), &mut in_param_pos);
        }
        for tp in tps {
            if in_param_pos.contains(&tp.name) {
                map.entry(tp.name.clone()).or_insert(Ty::Unknown);
            }
        }
    }
}

/// Does `ty` mention the built-in `Error` protocol existential anywhere (the E side of a bare `T!`,
/// or nested inside a container/struct)? Drives the concrete-error-type hint on a non-sendable
/// channel element. Matches `Error` by name — it is a reserved protocol (`prebuilt_protocols`), so a
/// user protocol can never shadow it.
fn ty_mentions_error_existential(ty: &Ty) -> bool {
    match ty {
        Ty::Protocol(n, _) => n == "Error",
        Ty::List(t)
        | Ty::Set(t)
        | Ty::Option(t)
        | Ty::Channel(t)
        | Ty::Shared(t)
        | Ty::Atomic(t)
        | Ty::RwShared(t) => ty_mentions_error_existential(t),
        Ty::Map(a, b) | Ty::Result(a, b) => {
            ty_mentions_error_existential(a) || ty_mentions_error_existential(b)
        }
        Ty::Tuple(xs) | Ty::Struct(_, xs) | Ty::Enum(_, xs) | Ty::NewType(_, xs) => {
            xs.iter().any(ty_mentions_error_existential)
        }
        _ => false,
    }
}
