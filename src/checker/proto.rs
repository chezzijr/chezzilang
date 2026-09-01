// checker::proto — split out of checker/mod.rs. `super::*` == the `checker` module.
// Protocol hoisting/embedding, satisfies, receiver refinement, hashability.

use super::*;
use std::collections::{HashMap, HashSet};

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

    /// O(1) membership index onto [`EQ_BOUNDS_IN_PROGRESS`] — see [`Checker::enter_eq_obligation`].
    /// The `Vec` stays the source of truth (ORDER matters: [`Checker::eq_budget_refusal`] reads its
    /// FIRST entry to name the walk's root), this only avoids scanning it. Kept in lockstep with the
    /// `Vec` at its only two mutation sites (`enter_eq_obligation`'s push, `EqObligation::drop`'s
    /// pop); the guard above it (`ty` already `in_progress`) means a `Ty` is never pushed while
    /// already present, so push/pop counts always match 1:1 and a plain `HashSet` (not a multiset)
    /// is exact, never just an over-approximation.
    static EQ_BOUNDS_IN_PROGRESS_SEEN: std::cell::RefCell<HashSet<Ty>> =
        std::cell::RefCell::new(HashSet::new());

    /// **The size budget's running total — [`Checker::ty_nodes`] summed over everything currently on
    /// [`EQ_BOUNDS_IN_PROGRESS`].** Added on push, subtracted on pop, so it always describes the
    /// CURRENT PATH and never accumulates across siblings. This is the whole of the second guard; see
    /// [`EQ_BOUNDS_MAX_NODES`] for why the thing being bounded is nodes rather than depth, and why a
    /// *classifier* of "is this shape non-terminating" was deleted in favour of it.
    static EQ_BOUNDS_IN_PROGRESS_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Types already proven sound during the CURRENT outermost query — the memo that makes the walk
    /// linear instead of exponential. The guard above is PATH-based: it stops a type being re-entered
    /// while it is on the stack, but says nothing about one already finished and popped. So a struct
    /// with two fields of the same nested type re-walks that subtree twice per level — 4x per level
    /// measured, i.e. 2^N, on the path the LSP runs on every keystroke.
    /// [`EQ_BOUNDS_MAX_NODES`] cannot catch it: that bounds the PATH, and this shape stays shallow.
    ///
    /// # Why a result reached under a coinductive assumption is still safe to cache HERE
    ///
    /// A `None` produced while "assume `A: Eq` while proving `A: Eq`" was in scope is only valid
    /// while that assumption holds — cache one carelessly and a type is treated as sound without
    /// ever being proven, which is C4/C5 in a new disguise. The reason it is nonetheless safe within
    /// one query is a property of the walk, not an optimism: **the first `Some` ENDS the query.**
    /// Every combinator on the path propagates it — `find_map` over fields/payloads/args, `or_else`
    /// on `Map`/`Result`, the early `return hit`, `eq_where_unsatisfied`'s `return Some(…)` on the
    /// first failing bound, `satisfies_args_d`'s `?` on an embed. So an assumption can only be
    /// invalidated by its owner completing with `Some`, and the instant that happens the query
    /// unwinds — there is no later moment at which a stale entry could be consulted. Conversely a
    /// frame that POPS having returned `None` has discharged its assumption truthfully.
    ///
    /// That argument is exactly why the per-query reset below is load-bearing and not housekeeping:
    /// it is what confines the property to the window where it holds. An earlier version also
    /// tracked "did this subtree consume an assumption?" and refused to memoize if so — which was
    /// redundant with the reset (measured: removing either alone still rejected the escape fixture)
    /// **and disabled the memo on every cyclic type graph**, since one back-pointer anywhere below a
    /// node poisoned the whole path above it. That is an ordinary shape — a node holding a reference
    /// back to its root — and it put the 2^N behaviour straight back: 26s at N=24 versus 0.003s
    /// pre-D1, while the acyclic graph the timing test used stayed at 0.003s and saw nothing.
    ///
    /// Scoped to one outermost query — reset by [`Checker::eq_bounds_unsatisfied`] whenever the
    /// in-progress stack is empty on entry. That also keeps it honest across the two things a
    /// longer-lived cache would get wrong: the checker's tables still being filled in (a type's `eq`
    /// may not be hoisted yet), and one thread checking several PROGRAMS in sequence, where
    /// `main::P` means something different each time.
    static EQ_BOUNDS_PROVEN: std::cell::RefCell<Vec<Ty>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// **THE termination guard for the `Eq` walk: a cumulative SIZE BUDGET over the in-progress path.**
/// [`Checker::ty_nodes`] summed over every entry currently on [`EQ_BOUNDS_IN_PROGRESS`] (running total
/// in [`EQ_BOUNDS_IN_PROGRESS_NODES`]) may not exceed this. Exceeding it REFUSES, like every other
/// decline here — a `None` would be consumed as a grant.
///
/// # It bounds the RESOURCE. It used to CLASSIFY the shape, and that was defeated twice in each
/// direction, so the classifier is deleted rather than patched again.
///
/// The predecessor (`is_growing_over` + a consecutive-growth streak, "Guard A") tried to decide
/// whether a type graph was non-terminating by comparing each new instantiation of a nominal against
/// the nearest in-progress one of the same name. It was wrong in BOTH directions, twice each, and
/// every wrong cut shipped fully green:
///
/// * **Evaded → OOM.** It only fired when EVERY type argument was non-decreasing, so a nominal that
///   PERMUTES its parameters while growing one of them never accumulated a streak: `struct M[A, B]: v:
///   A; w: B; next: Option[M[B, List[A]]]` at root `M[int, Map[int, int]]` walked to the depth cap with
///   an O(k)-sized `Ty` per level and was **OOM-killed at a 2 GiB cgroup cap in ~3 s** (measured on the
///   release binary; `Option[M[U, Option[T]]]` is the same class). `editor::diagnostics` runs this walk
///   per keystroke, so that was an OOM of the language server.
/// * **False-refused a finite graph, ORDER-DEPENDENTLY.** Two CONSTANT-argument self-fields each reach
///   a fixed point, but each was measured against the ancestor the previous sibling had induced rather
///   than against the root, so ascending sizes counted as consecutive growth: `struct W[T]: v: T; a:
///   Option[W[List[int]]]; b: Option[W[Map[int, int]]]` was refused at `W[int]` — and **swapping the
///   `a`/`b` declarations made the same program compile** (measured; rustc 1.97.0 accepts the mirror).
///
/// Before those, the same classifier had already been defeated by a `Ty` variant nobody remembered
/// (`ty_contains_or_eq`'s `_ => false` arm treated `Ty::Func` as a childless leaf) and had already
/// over-refused `struct Wrapper[T]: v: T; ints: Option[Wrapper[List[int]]]` asymmetrically in its root
/// (`Wrapper[int]` refused, `Wrapper[str]` accepted). Four failures, one root cause: predicting WHICH
/// SHAPES blow up is an open-ended classification problem, and each fix only closed the shape it was
/// shown.
///
/// A size budget cannot fail those ways, because it measures the hazard instead of predicting it:
/// * **Permutation-proof** — any walk whose types grow accumulates nodes, whichever argument position
///   grows and however the positions are shuffled.
/// * **Variant-proof** — [`Checker::ty_nodes`] is EXHAUSTIVE (no `_` arm; a new `Ty` variant is a
///   compile error until sized), so nothing can be invisible to it.
/// * **Order-independent** — the total is over the current PATH (pops subtract), so sibling fields
///   never accumulate against each other and no verdict can depend on declaration order.
/// * **Root-independent** — the budget knows nothing about which instantiation the query started from.
///
/// # What it is sized for
///
/// **The hazard is MEMORY, and it is quadratic in DEPTH for a growing graph** — that is the finding
/// that made "just raise the cap" the wrong fix and OOM-killed the machine on the first attempt at
/// W7-55. For polymorphic recursion (`struct N[T]: v: T; next: Option[N[List[T]]]`, which never repeats
/// an instantiation, so the exact-match cycle guard cannot close it) level *k*'s `Ty` has size O(k), and
/// [`EQ_BOUNDS_IN_PROGRESS`] holds a CLONE of every in-progress `Ty` — so a DEPTH cap of *c* admits
/// O(c²) nodes. Measured on the release binary under a depth cap: 160 → 14 ms / a few MB; 2 000 →
/// **1 342 ms / 621.8 MB**; the non-growing control (`S0: v: int` … `S159: v: S158`) at the same cap was
/// 17 ms / 13.6 MB. A NODE budget of *b* admits *b* nodes whatever the shape, which is the point:
/// polymorphic recursion costs ~*k* nodes at level *k*, so it reaches the budget at depth ~√(2*b*) —
/// a few hundred levels — and refuses there, in bounded work, without a classifier.
///
/// **Rust agrees polymorphic recursion is unprovable** — measured, rustc 1.97.0 (`scratchpad/polyrec.rs`):
/// `#[derive(PartialEq, Eq)] struct N<T: Eq> { v: T, next: Option<Box<N<Vec<T>>>> }` is `error[E0320]:
/// overflow while adding drop-check rules for N<i32>`. Refusing is the owning ancestor's answer, not a
/// Chezzi quirk.
///
/// # The number
///
/// `50_000` nodes. Derived from what it must admit and what it must not cost, both measured on the
/// release binary at this budget:
///
/// | fixture | wall | peak RSS | verdict |
/// |---|---|---|---|
/// | plain chain `S0..S10000` (~1 node/level) | 300 ms | 121 MB | accepted |
/// | polyrec `Option[N[List[T]]]` | 45 ms | 34 MB | refused |
/// | polyrec through `Ty::Func` | 32 ms | 25 MB | refused |
/// | arg-permuting `M[A, B]` (the OOM fixture) | 76 ms | 63 MB | refused |
/// | mutually recursive `A[T]`/`B[T]` | 43 ms | 32 MB | refused |
/// | two-constant-sibling `W[T]`, either field order | 5 ms | 12 MB | accepted |
///
/// It admits the honest shapes: a plain `S{k}: v: S{k-1}` chain costs ~1 node per level, so a
/// 10 000-deep chain — the depth the VM's own [`crate::vm::MAX_STRUCTURAL_DEPTH`] equality walks — fits
/// five times over.
///
/// **It also subsumes the old depth cap, so there is exactly one number here now.** Every entry costs
/// at least one node, so depth ≤ budget unconditionally; stack safety therefore comes for free from the
/// same constant. Measured floor (cap lifted for probing, DEBUG build, on the 1 GiB
/// [`crate::on_frontend_stack_scoped`] thread — see `checker::tests::stack_probe_eq_bounds_depth`, the
/// `#[ignore]`d harness): a chain of CONDITIONAL `eq` types, the expensive shape at ~5-10 Rust frames
/// per level, survives 118 000 obligations and overflows by 119 000; a plain-struct chain survives
/// 570 000. A 50 000-node budget cannot reach either.
///
/// That floor is a floor for EVERY caller because the stack is entered inside the checker, not by
/// convention at each call site: all five fns that drive `run_graph_pass`
/// ([`checker::check`](super::check), [`checker::check_graph_with_entry`](super::check_graph_with_entry),
/// [`checker::hover_type`](super::hover_type), [`checker::resolve_extern_signatures`](super::resolve_extern_signatures)
/// and [`checker::resolve_call_tables`](super::resolve_call_tables)) wrap themselves in
/// [`crate::on_frontend_stack_scoped`]. `hover_type` was the one exception until an adversarial review
/// found it — it relied on `editor::hover` wrapping instead, which is precisely the convention that
/// already failed once when hover ran the checker on a ~2 MiB LSP tokio worker. The only other callers
/// of `run_graph_pass` are two `#[cfg(test)]` helpers in `checker::tests` that check a fixed one-line
/// program (`print(1)`) to harvest a table; they declare no user types and enter no `Eq` obligation.
///
/// **This budget is NOT the VM's `MAX_STRUCTURAL_DEPTH`, and the two are not in the same unit.** An
/// earlier version of this constant was defined as `crate::vm::MAX_STRUCTURAL_DEPTH` and claimed the
/// two "literally cannot drift apart again". That claim was false: this counts checker OBLIGATION
/// NODES on a TYPE graph, the VM counts VALUE nesting depth, and tying the numerals does not tie the
/// measures. Measured counter-example: `struct Node: v: int; next: Option[Node]` builds a 20 000-deep
/// value whose `chezzi check` is `ok: no type errors` (obligation depth 2 — the type graph is tiny)
/// while `chezzi run` is `runtime error: maximum structural depth (10000) exceeded`. That runtime fault
/// is clean and `recover:`-able, so it is not a soundness hole — but the checker-VM window it leaves is
/// real, and pretending the constants close it is worse than admitting they do not. What this budget
/// DOES buy is the direction W7-55 actually complained about: the checker no longer REFUSES type graphs
/// the VM compares happily (the old 160-obligation cap refused at `S160` what the runtime handled to
/// 10 000).
///
/// W8-43 widens the same gap without changing this constant: the VM's budget is now CONTEXT-DEPENDENT
/// (a walk entered from inside an `eq`/`str` hook starts with the enclosing walk's depth already
/// charged), so the runtime depth a given compare gets is not a fixed 10 000. Still not a
/// `checker-superset-of-compiler` hole — an over-budget compare was already a recoverable runtime
/// fault, never a soundness break — but the correspondence is looser than a reader might infer.
///
/// **`docs/gaps.md` W7-55 is closed by this.**
const EQ_BOUNDS_MAX_NODES: usize = 50_000;

/// The marker every budget refusal carries, so [`Checker::eq_where_unsatisfied`] can tell "the bound
/// genuinely failed" from "I ran out of budget proving it" and not reword the second into the first.
///
/// **One message for one cause.** It used to be `"nests too deeply"` while a SECOND guard refused
/// growing type graphs at depth ~3 through the same wording — so a 3-level graph was reported as
/// nesting too deeply, which was simply untrue, and the shared marker made `eq_where_unsatisfied`
/// misclassify a growth refusal as budget exhaustion. Under the size budget the two causes are the same
/// cause (nodes on the path), so they get one honest phrase naming both ways to reach it.
pub(super) const EQ_BUDGET_MARKER: &str = "grows without bound or nests too deeply";

/// One live entry on [`EQ_BOUNDS_IN_PROGRESS`], popped on every exit path (including `?` and panic).
/// Minted only by [`Checker::enter_eq_obligation`], so an entry cannot be pushed without its pop.
/// Carries the entry's [`Checker::ty_nodes`] size so the pop can subtract exactly what the push added
/// without re-walking the `Ty`.
struct EqObligation(usize);

impl Drop for EqObligation {
    fn drop(&mut self) {
        EQ_BOUNDS_IN_PROGRESS.with(|s| {
            let popped = s.borrow_mut().pop();
            if let Some(ty) = popped {
                EQ_BOUNDS_IN_PROGRESS_SEEN.with(|seen| {
                    seen.borrow_mut().remove(&ty);
                });
                EQ_BOUNDS_IN_PROGRESS_NODES.with(|n| n.set(n.get().saturating_sub(self.0)));
            }
        });
    }
}

/// **The intrinsic-grant ↔ runtime-arm pairing table (W6-3's structural ratchet).**
///
/// One row per `(protocol, method, receiver-kind)` a built-in is granted conformance to
/// *intrinsically* — i.e. with NO user method behind it — by the [`Checker::grant_intrinsic`]
/// early-outs in [`Checker::satisfies_args_d`]. A row is a PROMISE that **an erased generic body under
/// that bound** (`fn f[T: Eq](x: T)` calling `x.eq(y)`) may CALL that method on that receiver kind, so
/// each row MUST be **callable at runtime** or the program type-checks and then faults `has no method`
/// (bug-hunt wave-6 W6-3, the check-OK-then-run-fault class).
///
/// **The erased bound is the whole of the promise — a row does NOT make the method callable on a bare
/// PROTOCOL-TYPED receiver.** `s.eq(t)` where `s: Shape` is `type Shape has no method 'eq'`, and that
/// rejection is correct: it is the object-safety rule (a protocol-typed value only offers the methods
/// its own protocol declares), not a gap in this table. The distinction matters for the
/// `("Eq", "eq", "protocol")` row, whose receiver KIND is itself a protocol: what it grants is
/// `T = <some protocol>` satisfying `Eq` inside an erased `[T: Eq]` body, where the concrete witness
/// settles the answer at runtime. An earlier wording said "an erased generic body (or a protocol-typed
/// value)", which overstated the row by exactly that second half.
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
///   the VM, so a row must be genuinely granted *and* genuinely callable.
///
/// Many-to-many on purpose: `IndexSet` contributes two methods, and `Index`/`IndexSet` share `index`.
/// `"struct"` is a coarsening — the `Hashable` grant is only for a ZERO-FIELD struct without its own
/// `hash`, and the `Iterable`/`Iterator` grants only for a struct with `iter`/`next` — so the probe
/// receiver for that kind is one struct that satisfies all three at once.
///
/// **Stays reserved-protocol-only.** A built-in witnessing a USER protocol (TICKET-024, W8-32,
/// `Checker::satisfies_native`) is a DIFFERENT kind of grant — it goes through
/// [`Grant::no_intrinsic_method`], not this table, because a user protocol's bare name can never be
/// a row key here (every row key is one of the ~20 [`RESERVED_PROTOCOLS`](super::RESERVED_PROTOCOLS)
/// names), and the method behind that grant is a REAL native method with a runtime arm already, not
/// a promise this ratchet needs to police.
pub const INTRINSIC_PROTO_METHODS: &[(&str, &str, &str)] = &[
    // Comparable — int/float/str scalars + a numeric newtype (its `<` unwraps to the underlying).
    ("Comparable", "compare", "int"),
    ("Comparable", "compare", "float"),
    ("Comparable", "compare", "str"),
    ("Comparable", "compare", "newtype"),
    // Eq — D1: EVERY receiver kind whose `==` this table can key a row on except `nil` (not
    // spellable as a value). Most rows are the structural derive (`Vm::values_equal`): the four
    // scalars are all here because `==` is defined on `bool` too, unlike `Comparable`; a newtype's
    // `==` unwraps to the underlying's native equality, exactly as its `<` unwraps to the ordering;
    // `option`/`result` land on `Obj::Enum` at runtime, same as `enum`. `func` is the one exception —
    // a function value's `==` is IDENTITY (two loads of the same top-level `fn`/nested `fn` def are
    // equal, two calls to a factory are not — W7-54), not a structural walk, but it is still the same
    // `values_equal_guarded` worker `==` uses, so `.eq()` can never disagree with `==`. `func` covers
    // BOTH `Ty::Func` (a user closure/free fn) and `Ty::BuiltinFn` (`ord`/`chr`/`panic`/first-class
    // `print`) — they render identically and compare by the same `Obj::Builtin`/`Obj::Func` identity
    // rule, so one row and one kind speaks for both (W7-54 follow-up: `Ty::BuiltinFn` was the same
    // defect, just a separate `Ty` variant, and was missed the first time). `protocol` is a SECOND
    // exception, and a deliberately open one: a protocol-typed (existential) value's `==` DEFERS to
    // its concrete witness at runtime, exactly as Go's interface `==` defers to the dynamic type and
    // panics if that type is uncomparable — so this row grants the CHECK and the witness settles the
    // ANSWER, cleanly faulting when it can't (W7-52). No other protocol row exists for `"protocol"`:
    // `[T: Stringable]`/`[T: Hashable]`/… at `T = <protocol>` all still reject, because `Eq` is the one
    // protocol whose grant means exactly what `==` already accepts, and `==` already accepts a
    // protocol-typed operand (`may_be_equal`'s `(Protocol, Protocol)`/`(Protocol, concrete)` arms).
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
    ("Eq", "eq", "func"),
    ("Eq", "eq", "protocol"),
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
    /// itself, structural satisfaction via a user method table, and — a FOURTH case, TICKET-024,
    /// W8-32 — a built-in (`List`/`Map`/`Set`/`str`/`bytes`/`bytearray`, or a scalar with no method
    /// table) witnessing a USER protocol out of its own harvested `native struct` method table
    /// (`Checker::satisfies_native`). That fourth case is still the right constructor even though the
    /// method IS callable: the method is a real native `fn` the direct-call path already type-checks
    /// and the VM already dispatches by name, so it needs no NEW runtime arm and no
    /// `INTRINSIC_PROTO_METHODS` row — unlike this table's rows, it carries no promise about a method
    /// with no code behind it.
    ///
    /// If the arm you are writing makes a BUILT-IN satisfy a RESERVED protocol with no user method
    /// behind it, this is the WRONG constructor — use [`Checker::grant_intrinsic`] and add the row.
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
            // `BuiltinFn` (`ord`/`chr`/`panic`/first-class `print`) renders identically to `Ty::Func`
            // ("fn(...) -> ..." — see `impl Display for Ty`) and compares by the same runtime
            // identity (`Obj::Builtin` name-equality, `vm/arith.rs`'s `(Obj::Builtin(a),
            // Obj::Builtin(b))` arm); it is a DISTINCT `Ty` variant only for sendability
            // (`docs/syntax.md`: all four universe builtins are sendable, a plain closure is not), so
            // it shares `Ty::Func`'s kind rather than minting a second one (W7-54 follow-up).
            Ty::Func { .. } | Ty::BuiltinFn { .. } => "func",
            // A protocol EXISTENTIAL — at runtime this is always its concrete witness object, so
            // `.eq()`/`==`/`[T: Eq]` on it defers to whatever the witness's own `eq` dispatch does
            // (structural derive or a declared `eq`), exactly as Go's interface `==` defers to the
            // dynamic type and panics if that type is uncomparable (W7-52). Classified ONLY so the D1
            // `Eq` arm below can grant it — every OTHER protocol row in `INTRINSIC_PROTO_METHODS`
            // still excludes `"protocol"` (no row registered), so this does not widen anything but Eq.
            Ty::Protocol(..) => "protocol",
            _ => "?",
        }
    }

    /// The `self.structs` key a built-in's harvested `native struct` method table sits under, for
    /// the ONE receiver kinds a USER protocol may witness against (TICKET-024, W8-32) — `None` for
    /// everything else (handles, tuples, `Option`/`Result`, functions), which keeps falling to the
    /// catch-all message. The three scalars have no `self.structs` entry (`std/prelude.chz:29-31`
    /// declares them `native ctor` only), so routing them here anyway reaches an empty table and
    /// IMPROVES the message: "type int does not satisfy Sized (missing method 'len')" instead of
    /// the bare clause.
    fn native_witness_key(ty: &Ty) -> Option<&'static str> {
        match ty {
            Ty::List(_) => Some("List"),
            Ty::Map(..) => Some("Map"),
            Ty::Set(_) => Some("Set"),
            Ty::Str => Some("str"),
            Ty::Bytes => Some("bytes"),
            Ty::ByteArray => Some("bytearray"),
            Ty::Int => Some("int"),
            Ty::Float => Some("float"),
            Ty::Bool => Some("bool"),
            _ => None,
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
        let key = self.bare_key(name);
        if self.protocols.contains_key(&key) {
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
            key,
            ProtocolInfo {
                type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                methods: sigs,
                embeds: self.key_bounds(embeds),
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
            if self.protocol_shape(&emb.name).is_none() {
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
            let Some(pinfo) = self.protocol_shape(&emb.name).cloned() else {
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
        let pinfo = self.protocol_shape(pname)?;
        if let Some((_, sig)) = pinfo.methods.iter().find(|(n, _)| n == method) {
            return Some(sig.clone());
        }
        for emb in &pinfo.embeds {
            let Some(sig) = self.protocol_method_sig_d(&emb.name, method, seen) else {
                continue;
            };
            // The recovered sig is spelled in `emb.name`'s params; re-spell it in ours.
            let etps = self
                .protocol_shape(&emb.name)
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

    /// The method names a protocol requires, own OR through any embed, flattened. Copies
    /// `protocol_method_sig_d`'s `seen` cycle guard so an embed cycle can't recurse forever.
    pub(super) fn protocol_method_names(&self, pname: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        self.protocol_method_names_d(pname, &mut seen, &mut out);
        out.sort();
        out.dedup();
        out
    }

    fn protocol_method_names_d(
        &self,
        pname: &str,
        seen: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(pname.to_string()) {
            return;
        }
        let Some(pinfo) = self.protocol_shape(pname) else {
            return;
        };
        out.extend(pinfo.methods.iter().map(|(n, _)| n.clone()));
        for emb in &pinfo.embeds {
            self.protocol_method_names_d(&emb.name, seen, out);
        }
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
        let pinfo = self.protocol_shape(p)?;
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

    /// Why `t` may not be a `map` key / `set` element — as the diagnostic's TAIL, so each of the nine
    /// call sites keeps its own prefix (`"Map key type "`, `"map key type "`, `"Set element type "`,
    /// `"set element type "`). `None` = it may. `Unknown` is tolerated (no cascade).
    ///
    /// **Two obligations, not one (W7-45).** `Hashable` — the scalars `int`/`str`/`bool`
    /// intrinsically, or a struct/enum/newtype defining `hash(self) -> int`; `float` is refused
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
    /// `refine_receiver`'s late-concrete element, and thereby every `Map`/`Set`/`RwShared` method).
    ///
    /// **W7-53.** NOT erased any more: a free `Ty::Param` reaching the second conjunct
    /// (`eq_bounds_unsatisfied`, not `_erased`) is judged directly, so `fn mk[T: Hashable](x: T) ->
    /// Set[T]` that builds `Set[T]` in its own body — never spelling the element type — is now
    /// caught at ITS OWN definition instead of handing back a `Set[Cond[Tag]]` no gate ever saw
    /// (measured, was check-clean; W7-53's third instance). The `Eq` conjunct stays second because a
    /// non-`Hashable` type must keep reporting the `Hashable` text.
    pub(super) fn key_ty_reject(&self, t: &Ty) -> Option<String> {
        if self.satisfies(t, "Hashable").is_err() {
            return Some(format!(
                "must implement Hashable (int, str, bool, or a struct/enum/newtype defining hash(self) -> int), found {t}"
            ));
        }
        self.eq_bounds_unsatisfied(t)
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
        // `None`: this site pins ITSELF below, after its own gates (the cascade guard on an
        // erroring argument, the receiver-shape match, and the `Set` Hashable ban) — the drop must
        // run FIRST for the captured-binding reason just stated, so the two halves cannot be one
        // call here.
        if matches!(method, "push" | "add" | "insert" | "extend") && !args.is_empty() {
            self.drop_empty_site(name, None);
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
        // This inference is SPECULATIVE: the real dispatch path re-infers the same args, so every
        // diagnostic emitted here is a duplicate. Roll back UNCONDITIONALLY — no `return` between
        // the mark and the rollback, so a later exit path can't be added that forgets one. (It used
        // to end at the shape match below, whose `_` arm leaked: `m := {}` + `m.insert(undefined_v)`
        // reported `unknown name` twice.)
        let mark = self.diag_mark();
        let elem = match method {
            "push" | "add" | "insert" => args.first().map(|a| self.infer_value(a)),
            "extend" => args.first().map(|a| match self.infer_value(a) {
                Ty::List(e) | Ty::Set(e) => *e,
                other => other,
            }),
            _ => None,
        };
        let arg_erred = self.errors.len() != mark.errors;
        self.diag_rollback(mark);
        // (d) cascade invariant: if inferring the arg itself reported an error, don't refine (the
        // real dispatch path reports it, exactly once).
        if arg_erred {
            return;
        }
        let Some(elem) = elem else { return };
        // Wrap the element into a RECEIVER-SHAPED value so the structural merge lines up the slot:
        // a list receiver merges with `list[elem]`, a set receiver with `set[elem]`. Any other
        // receiver kind isn't a push/add/extend target, so nothing to refine.
        let shape = match obj_ty {
            Ty::List(_) => Ty::list(elem),
            Ty::Set(_) => Ty::set(elem),
            _ => return,
        };
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
            && let Some(why) = self.key_ty_reject(e)
        {
            self.error(obj.span, format!("set element type {why}"));
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
        // `None` for the same reason as `refine_receiver`: this site pins itself below, after the
        // speculative index-infer's cascade guard.
        self.drop_empty_site(name, None);
        if val_ty.is_unknown() {
            return;
        }
        // The supplied shape mirrors the receiver kind: `Map(idx, val)` for a map, `List(val)` for a
        // list (index type is the int position, irrelevant to the element slot).
        // Speculative, same contract as `refine_receiver`: the real index-assign path re-infers the
        // index and reports its diagnostics, so roll back unconditionally with no exit in between.
        let mark = self.diag_mark();
        let shape = match &obj_ty {
            Ty::Map(..) => Some(Ty::map(self.infer(index), val_ty.clone())),
            Ty::List(..) => Some(Ty::list(val_ty.clone())),
            _ => None,
        };
        let index_erred = self.errors.len() != mark.errors;
        self.diag_rollback(mark);
        if index_erred {
            return;
        }
        let Some(shape) = shape else { return };
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
            // Parameter NAMES are surface-only, but the OPTIONAL ARITY riding on the same wrapper is
            // directional — see the matching arm in `compatible`. `expected` says how the slot will be
            // called, so a value requiring MORE arguments than the slot promises cannot be stored in
            // it: `h := a; h = b` over `fn a(x: int = 1)` / `fn b(x: int)` was check-clean and then
            // `function 'b' expects 1 argument(s), got 0` at runtime. The reverse
            // (a defaulted fn into a plain `fn(int) -> int`) is strictly more permissive and stays legal.
            (
                Func {
                    params: p1,
                    ret: r1,
                    labels: l1,
                },
                Func {
                    params: p2,
                    ret: r2,
                    labels: l2,
                },
            ) => {
                p1.len() == p2.len()
                    && l2.min_or(p2.len()) <= l1.min_or(p1.len())
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
            // `[1.0, 2.0] == [1, 2]` and `{"k": 1.0} == {"k": 1}` answer `true`
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

    /// W8-21 — which implicit success-coercion (if any) a bare value of type `ty` gets at a declared
    /// return sink of type `ret`. `None` means "no coercion" — the caller keeps its existing
    /// `assignable`/`assignable_w` diagnostic unchanged.
    ///
    /// Order matters: `assignable` first (rule a — an already-legal return is never coerced), then
    /// "already a carrier" (never re-wrap `Option[Option[T]]`/`Result[Option[T],E]`), then
    /// `ty_fully_concrete(ret)` (rule c — a generic sink `T?` declines). The wrap arms use plain
    /// `assignable`, NEVER `assignable_w` (rule b — no chaining onto int→float:
    /// `float?: return 1` must keep erroring).
    pub(super) fn ret_coerce_mode(&self, ret: &Ty, ty: &Ty) -> Option<crate::checker::RetCoerce> {
        if self.assignable(ret, ty) {
            return None;
        }
        if matches!(ty, Ty::Option(_) | Ty::Result(..)) {
            return None;
        }
        if !crate::checker::ty_fully_concrete(ret) {
            return None;
        }
        match ret {
            Ty::Option(inner) if self.assignable(inner, ty) => {
                Some(crate::checker::RetCoerce::WrapSome)
            }
            Ty::Result(t, _) if self.assignable(t, ty) => Some(crate::checker::RetCoerce::WrapOk),
            _ => None,
        }
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

    pub(super) fn satisfies(&self, ty: &Ty, protocol: &str) -> Result<(), String> {
        self.satisfies_args(ty, protocol, &[])
    }

    /// The value-slot twin of the generic-bound conformance sentence (`satisfies_args`'s `Err`
    /// string) — TICKET-008 / `docs/gaps.md` W8-16. Called only from an already-failing
    /// `assignable` branch, so it cannot change what the checker accepts: it only appends WHY a
    /// value slot rejected a type, when that type was rejected because it fails a protocol.
    /// Empty when `expected` is not a protocol, and empty when `actual` DOES satisfy the
    /// protocol — a witness that satisfies but is not sendable still reaches the error path via
    /// `assignable`'s `sendable` check, and claiming a missing method there would be false.
    pub(super) fn protocol_note(&self, expected: &Ty, actual: &Ty) -> String {
        let Ty::Protocol(p, pargs) = expected else {
            return String::new();
        };
        match self.satisfies_args(actual, p, pargs) {
            Ok(()) => String::new(),
            Err(why) => format!(" \u{2014} {why}"),
        }
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
        if self.protocol_key(bound_name) == self.protocol_key(protocol)
            && self.bound_args_match(bound_args, required)
        {
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
        if let Some(pinfo) = self.protocol_shape(bound_name) {
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
        if self.protocol_key(p) == self.protocol_key(protocol)
            && pargs.len() == required.len()
            && pargs.iter().zip(required).all(|(x, y)| compatible(x, y))
        {
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
                _ if self.protocol_shape(n).is_some() => {
                    Ty::Protocol(self.protocol_key(n), Vec::new())
                }
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
                _ if self.protocol_shape(n).is_some() => Ty::Protocol(
                    self.protocol_key(n),
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
                labels: FnLabels::new(labels.clone()),
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
        } else if sig.protocol_defs.contains_key(name) {
            // Permissive mirror of `resolve_type`'s qualified protocol arm: no arity error and no
            // static-ctor gate — the mutable resolver owns both diagnostics.
            Ty::Protocol(self.type_key(mid, name), args.to_vec())
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
        // TICKET-027: `protocol` may carry a module-qualified KEY (e.g. from a re-keyed stored
        // bound); every user-facing message below renders the BARE name, matching `Ty::Protocol`'s
        // own `Display`.
        let protocol_display = crate::compiler::bare_display(protocol);
        let Some(pinfo) = self.protocol_shape(protocol) else {
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
                    _ => Err(format!("expected {protocol_display}[...], found {ty}")),
                };
            }
            return Err(format!("unknown protocol '{protocol_display}'"));
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
        //   `Module` and `Ty::Param` (`Func`/`BuiltinFn` graduated into the `"func"` kind in W7-54,
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
        //   two-spellings-disagree shape Tasks 1/2 closed for `Ty::Func`/`Ty::BuiltinFn`/
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
        if protocol == "Eq"
            && !matches!(Self::intrinsic_recv_kind(ty), "?" | "nil")
            && !Self::is_cursor_ty(ty)
            && self
                .declared_methods(ty)
                .and_then(|m| m.get("eq"))
                .is_none_or(|sig| !Self::eq_sig_is_hook(sig))
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
        // has no state to hash, so the runtime returns a constant hash for it (with `==`'s type-tag
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
                        None => {
                            return Err(format!("type {ty} does not satisfy {protocol_display}"));
                        }
                    }
                }
            };
            // Any args the bound supplied must match what the built-in actually provides.
            for (want, got) in args.iter().zip(&provided) {
                if !want.is_unknown() && !got.is_unknown() && !compatible(want, got) {
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
                Err(format!("type {ty} does not satisfy {protocol_display}"))
            };
        }
        // A built-in witnesses a USER protocol out of its own harvested `native struct` method
        // table (TICKET-024, W8-32) -- reserved protocols are excluded because each already has a
        // hand-written intrinsic arm above whose grant is a W6-3 promise about a method with no
        // user code behind it; routing them through the native table too would silently re-decide
        // conformance for every built-in (`Set.add` sits one signature check from witnessing `Add`).
        if !is_reserved_protocol(protocol)
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
            Ty::Enum(ename, _) => {
                // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported enum value's
                // method table from the owning `ModuleSig` on a local-table miss (see the struct arm).
                let Some(methods) = self.enum_methods_of(ename) else {
                    return Err(format!("type {ty} does not satisfy {protocol_display}"));
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
                    return Err(format!("type {ty} does not satisfy {protocol_display}"));
                }
                // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported newtype value's
                // method table from the owning `ModuleSig` on a local-table miss (see the struct arm).
                let Some(methods) = self.newtype_methods_of(ntkey) else {
                    return Err(format!("type {ty} does not satisfy {protocol_display}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
                    .map(|()| Grant::no_intrinsic_method())
            }
            _ => Err(format!("type {ty} does not satisfy {protocol_display}")),
        }
    }

    /// A native method's DISPATCH-TIME residual that a `FnSig` alone can't express (TICKET-024,
    /// W8-32) — mirrors the two gates `src/checker/expr.rs` applies at `Ty::List`'s direct-call arm
    /// (`:2981`'s numeric-`sum` gate, `:3001`'s `eq_bounds_unsatisfied` gate for
    /// `contains`/`index_of`/`dedup`/`unique`), reused rather than re-derived so a protocol-erased
    /// call can't type-check where a direct call would fault. No other covered built-in
    /// (`Map`/`Set`/`str`/`bytes`/`bytearray`) has one. `Some(reason)` refuses; `None` defers to the
    /// ordinary signature match.
    fn native_dispatch_residual(&self, ty: &Ty, method: &str) -> Option<String> {
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
        // TICKET-027: `protocol` may carry a module-qualified KEY — render the BARE name in messages.
        let protocol_display = crate::compiler::bare_display(protocol);
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
                    .protocol_shape(name)
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
                                    "type {ty} does not satisfy {protocol_display} (method '{mname}' requires {}: {})",
                                    concrete,
                                    crate::compiler::bare_display(&bound.name)
                                ));
                            }
                        }
                    }
                }
                // Name the REAL reason when the witness is generic, rather than blaming a signature
                // that is otherwise correct — the same message `satisfies_native` gives for a generic
                // native method. See `method_matches` for why a generic method cannot witness.
                Some(actual) if !actual.type_params.is_empty() => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol_display} (method '{mname}' is generic \
                         and cannot witness a protocol requirement)"
                    ));
                }
                Some(_) => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol_display} (method '{mname}' has the wrong signature)"
                    ));
                }
                None => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol_display} (missing method '{mname}')"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Does a built-in (`List`/`Map`/`Set`/`str`/`bytes`/`bytearray`, or a scalar with no method
    /// table) satisfy a USER protocol out of its own harvested `native struct` method table
    /// (TICKET-024, W8-32)? `key` is [`Checker::native_witness_key`]'s answer for `ty`.
    ///
    /// The receiver param is PREPENDED to each harvested sig before it reaches
    /// [`Checker::satisfies_methods`] — load-bearing, not cosmetic: `harvest_native_fn_sig` strips
    /// the leading bare `self` (`src/checker/setup.rs:673-717`) while a protocol requirement keeps
    /// it as a `Ty::Unknown` `params[0]` (`src/checker/setup.rs:887-894`), and `method_matches`
    /// (`src/checker/mod.rs`) compares `params.len()` first. Without the prepend every requirement
    /// is refused as `(method '<name>' has the wrong signature)`.
    fn satisfies_native(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        key: &str,
    ) -> Result<(), String> {
        // TICKET-027: `protocol` may carry a module-qualified KEY — render the BARE name in messages.
        let protocol_display = crate::compiler::bare_display(protocol);
        let native_methods = self.structs.get(key).map(|i| &i.methods);
        let mut table: HashMap<String, FnSig> = HashMap::new();
        for (mname, _) in &pinfo.methods {
            let Some(sig) = native_methods.and_then(|m| m.get(mname)) else {
                continue; // left out of the table -- satisfies_methods reports "(missing method ...)"
            };
            if !sig.type_params.is_empty() {
                return Err(format!(
                    "type {ty} does not satisfy {protocol_display} (native method '{mname}' is generic \
                     and cannot witness a protocol requirement)"
                ));
            }
            if let Some(why) = self.native_dispatch_residual(ty, mname) {
                return Err(format!(
                    "type {ty} does not satisfy {protocol_display} ({why})"
                ));
            }
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            params.push(ty.clone());
            params.extend(sig.params.iter().cloned());
            table.insert(
                mname.clone(),
                FnSig {
                    params,
                    min_params: sig.min_params + 1,
                    ..sig.clone()
                },
            );
        }
        self.satisfies_methods(ty, protocol, args, pinfo, &table)
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

    /// W7-53 I1′ — is this `.eq(x)` call PROTOCOL dispatch (the receiver is a generic type parameter
    /// whose bound exposes `eq`) rather than an ordinary by-name method call?
    ///
    /// Only a `Ty::Param` receiver can be: a concrete receiver keeps Rust's inherent-wins rule
    /// (`Key(1).eq(Key(2))` is an ordinary method call), and a `Ty::Protocol` receiver never reaches
    /// `eq` at all — `eq(self, o: Self)` puts `Self` in a parameter slot, which the object-safety
    /// gate in `infer_method_call` already rejects with a message pointing at `[T: Eq]`.
    ///
    /// The bound is asked by RESOLUTION, not by name, so an EMBEDDING bound (`[T: Comparable]`,
    /// which embeds `Eq`) dispatches the protocol too — `protocol_method_sig` walks embeds, so this
    /// is the same resolution the `Ty::Param` arm of `infer_method_call` uses to type the call in
    /// the first place.
    ///
    /// …but the resolved method must carry the `Eq` HOOK SIGNATURE `fn eq(self, other: Self) ->
    /// bool` (`Self` is `Ty::Param("Self")` in a registered protocol sig). A user protocol is free
    /// to declare an unrelated `fn eq(self, o: int) -> bool`, and lowering THAT to `==` would
    /// compare the receiver against the argument instead of calling the method — an over-fire in
    /// the granting direction, which is the one that produces a silent wrong value.
    pub(super) fn eq_is_protocol_dispatch(&self, obj_ty: &Ty) -> bool {
        let Ty::Param(pname) = obj_ty else {
            return false;
        };
        self.type_params.get(pname).is_some_and(|bounds| {
            bounds.iter().any(|b| {
                self.protocol_method_sig(&b.name, "eq")
                    .is_some_and(|s| Self::is_eq_hook_protocol_sig(&s))
            })
        })
    }

    /// Is a PROTOCOL's declared `eq` the `Eq` hook `fn eq(self, other: Self) -> bool`? Deliberately
    /// separate from [`Self::eq_sig_is_hook`], which asks the mirror question of a CONCRETE type's
    /// declared `eq` (there the escape hatch is a bare type param; here `Self` IS one).
    ///
    /// **Defence in depth, not a live bug** — stated precisely because the first version of this
    /// comment implied otherwise. Without it, a user protocol's unrelated `fn eq(self, o: int) ->
    /// bool` would lower to `Op::Eq` and compare the receiver against the argument. But no such
    /// protocol is satisfiable today: `validate_eq_shape` rejects any concrete witness at its own
    /// declaration (*"its operand must be S, found int"*), and the `Eq`-embed variant is caught by
    /// the embedded-requirement conflict — so the over-fire is unreachable and this gate is what
    /// keeps it that way if `validate_eq_shape` ever loosens.
    fn is_eq_hook_protocol_sig(sig: &FnSig) -> bool {
        sig.params.len() == 2
            && sig.params[1] == Ty::Param("Self".to_string())
            && sig.ret == Ty::Bool
    }

    /// Record one `.eq(x)` site's dispatch for the backend, under the same key derivation the `?.`
    /// carriers use (the method-NAME token — see [`crate::checker::CarrierKey`]).
    pub(super) fn record_proto_eq(&mut self, name_span: Span, proto: bool, span: Span) {
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        );
        crate::checker::record_call_table_entry(
            &mut self.proto_eq_calls,
            &mut self.table_conflicts,
            key,
            proto,
            "'.eq()' dispatch",
            span,
        );
    }

    /// The `T(0)` seed a `List[T].sum()` needs, or `None` for a plain `List[int]`/`List[float]`:
    /// `Some((runtime type key, underlying-is-float))` exactly when `elem` is a SCALAR numeric
    /// newtype. The predicate is deliberately the SAME one that grants a numeric newtype its
    /// intrinsic `Add` (the `Ty::NewType` arm of [`Self::satisfies`]) — non-generic, numeric
    /// underlying — so `sum`'s `where T: Add` bound and this seed can never disagree. A newtype OF a
    /// newtype has a non-numeric underlying and so is `None`: it is rejected, exactly as `Cents(1) +
    /// Cents(1)`'s outer wrapper and `.min()`'s `Comparable` bound already reject it.
    pub(super) fn newtype_sum_seed(&self, elem: &Ty) -> Option<(String, bool)> {
        let Ty::NewType(key, _) = elem else {
            return None;
        };
        if self.newtype_is_generic(key) {
            return None;
        }
        let under = self.newtype_underlying(key)?;
        under
            .is_numeric()
            .then(|| (key.clone(), matches!(under, Ty::Float)))
    }

    /// Record one `.sum()` site's seed for the backend, under the same key derivation
    /// [`Self::record_proto_eq`] uses (the method-NAME token — see [`crate::checker::CarrierKey`]).
    pub(super) fn record_newtype_sum(
        &mut self,
        name_span: Span,
        seed: Option<(String, bool)>,
        span: Span,
    ) {
        let key = crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            name_span,
        );
        crate::checker::record_call_table_entry(
            &mut self.newtype_sums,
            &mut self.table_conflicts,
            key,
            seed,
            "'.sum()' element",
            span,
        );
    }

    /// Is a struct/enum's declared `eq` [`FnSig`] the `Eq` HOOK `==`/`!=` dispatch to, or the
    /// ordinary-method escape hatch (a generic operand)? Delegates to [`Self::eq_operand_is_hook`]
    /// (`sig.rs`) — the same predicate `validate_eq_shape` already enforced at the declaration, so a
    /// program that compiled has `eq`'s operand as either `Self` or a type parameter, nothing else.
    fn eq_sig_is_hook(sig: &FnSig) -> bool {
        sig.params.len() == 2 && Self::eq_operand_is_hook(&sig.params[1])
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
        }
        self.eq_bounds_unsatisfied_rec(ty)
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
        // The exact-match cycle guard, unchanged and unrelated to the budget below: it reads the O(1)
        // `EQ_BOUNDS_IN_PROGRESS_SEEN` index rather than scanning the `Vec` (see that thread-local's
        // doc for why the two never drift), and a hit is the COINDUCTIVE assumption, not a decline.
        if EQ_BOUNDS_IN_PROGRESS_SEEN.with(|seen| seen.borrow().contains(ty)) {
            return Err(None);
        }
        // **The size budget — one guard, bounding the RESOURCE.** `ty`'s nodes plus everything already
        // on the path; refuse if that would exceed `EQ_BOUNDS_MAX_NODES`. This replaced a classifier
        // that tried to decide whether the graph was non-terminating and was defeated in both
        // directions, twice each (argument permutation evaded it into an OOM; two constant-argument
        // sibling fields false-refused a finite graph, order-dependently). See the constant's doc.
        let nodes = Self::ty_nodes(ty);
        let total = EQ_BOUNDS_IN_PROGRESS_NODES.with(|n| n.get()) + nodes;
        if total > EQ_BOUNDS_MAX_NODES {
            return Err(Some(Self::eq_budget_refusal(ty)));
        }
        EQ_BOUNDS_IN_PROGRESS.with(|s| s.borrow_mut().push(ty.clone()));
        EQ_BOUNDS_IN_PROGRESS_SEEN.with(|seen| seen.borrow_mut().insert(ty.clone()));
        EQ_BOUNDS_IN_PROGRESS_NODES.with(|n| n.set(total));
        Ok(EqObligation(nodes))
    }

    /// Total node count of `t` — how big a `Ty` is, counting itself and every child it carries. The
    /// unit [`EQ_BOUNDS_MAX_NODES`] budgets, and deliberately EXHAUSTIVE with **no `_` catch-all
    /// arm**: adding a `Ty` variant is a compile error here until someone decides how it counts.
    /// An ancestor of this fn (`ty_contains_or_eq`, part of the deleted growth classifier) had a
    /// `_ => false` default that silently treated every variant it didn't list as a childless leaf —
    /// WRONG for `Ty::Func` (a legal `Eq` type argument since W7-54, with `params`/`ret` children that
    /// can themselves grow), so a type parameter substituted into a fn type (`N[T]` with a field
    /// `Option[N[fn(T) -> int]]`) grew invisibly to the guard and walked to the depth cap with an
    /// O(k)-sized `Ty` per level. A fail-open default inside a termination guard means "assume this
    /// can't be part of the hazard", which is exactly the assumption that must never be made silently
    /// — and being the SIZE METRIC of a resource budget rather than an input to a shape classifier is
    /// what makes exhaustiveness here sufficient as well as necessary.
    fn ty_nodes(t: &Ty) -> usize {
        match t {
            // Genuine leaves: no `Ty` children to visit.
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::AtomicInt
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Unknown
            | Ty::Param(_)
            | Ty::Module(_) => 1,
            // One child.
            Ty::List(t)
            | Ty::Set(t)
            | Ty::Option(t)
            | Ty::Channel(t)
            | Ty::Shared(t)
            | Ty::Atomic(t)
            | Ty::RwShared(t) => 1 + Self::ty_nodes(t),
            // Two children.
            Ty::Map(k, v) | Ty::Result(k, v) => 1 + Self::ty_nodes(k) + Self::ty_nodes(v),
            // Fn-shaped: params (a `Vec`) + one return type. `labels` is surface-only and
            // equality-neutral (see `FnLabels`), so it carries no size.
            Ty::Func { params, ret, .. } | Ty::BuiltinFn { params, ret } => {
                1 + params.iter().map(Self::ty_nodes).sum::<usize>() + Self::ty_nodes(ret)
            }
            // A `Vec` of children; the name (where present) carries no size.
            Ty::Tuple(elems)
            | Ty::Struct(_, elems)
            | Ty::Enum(_, elems)
            | Ty::NewType(_, elems)
            | Ty::Protocol(_, elems) => 1 + elems.iter().map(Self::ty_nodes).sum::<usize>(),
        }
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
        let hit = members
            .iter()
            .find_map(|m| self.eq_bounds_unsatisfied_rec(m));
        if hit.is_none() {
            EQ_BOUNDS_PROVEN.with(|m| m.borrow_mut().push(ty.clone()));
        }
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
                // A declared `eq` has its `where` bounds checked WHATEVER its shape — but only the
                // HOOK shape ends the walk here.
                //
                // The non-hook escape hatch is still REACHABLE, which is why skipping its bounds was
                // wrong: `==` on such a type is structural (it never dispatches the method), but an
                // erased `[T: Eq]` body's `a.eq(b)` dispatches BY NAME straight to it, and its
                // `where` clause then runs unproven. Measured on the release binary before this
                // guard was split: `struct Box[T]` with `fn eq[U](self, o: U) -> bool where T:
                // Comparable` fed `Box[Tag]` through `fn eqm[T: Eq](a, b) -> bool: return a.eq(b)`
                // was `ok: no type errors` then `runtime error: struct 'Tag' has no method
                // 'compare'` — verbatim the class W7-41/W7-45/W7-53 exist to close,
                // re-opened in the mirror direction while closing the escape hatch's grant.
                //
                // So: bounds first (a `where`-less escape hatch has none, so it still costs
                // nothing), then the hook alone short-circuits; a non-hook `eq` falls through to the
                // structural walk below, because structural equality really is what its `==` does.
                if let Some(sig) = info.methods.get("eq") {
                    if let hit @ Some(_) = self.eq_where_unsatisfied(ty, sig) {
                        return hit;
                    }
                    if Self::eq_sig_is_hook(sig) {
                        return None;
                    }
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
                // Bounds-then-hook, exactly as the struct arm above — see its comment for why a
                // non-hook `eq`'s `where` clause must still be proven. (`.cloned()` because
                // `enum_methods_of` borrows `self`, which `eq_where_unsatisfied` needs mutably-free.)
                if let Some(sig) = self.enum_methods_of(name).and_then(|m| m.get("eq")).cloned() {
                    if let hit @ Some(_) = self.eq_where_unsatisfied(ty, &sig) {
                        return hit;
                    }
                    if Self::eq_sig_is_hook(&sig) {
                        return None;
                    }
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
            // answers.) W7-53: since Chezzi fixes this at the DEFINITION (matching rustc/Go, not
            // call-site inference), the message names the fix, the way rustc's `help:` does —
            // every one of the four gates that reach this arm wraps `why` behind an em-dash, so
            // the added clause reads naturally at each: "cannot compare T and T for equality — T is
            // not bounded by Eq (…)".
            //
            // I2 — this arm is SITE-BLIND: it is reached both from a fn body (`==`/`in`/`contains`)
            // where `where T: Eq` is grammar, AND from a decl-site type annotation
            // (`struct Reg[K: Hashable]: m: Map[K, int]`, via `key_ty_reject` → `resolve_type`'s
            // `Map`/`Set` arms) where `where` does not exist at all (`<whereClause>` is `fn`/`native
            // fn`-only, `docs/grammar.bnf`) — `struct Reg[K: Hashable] where K: Eq:` is a PARSE
            // error. So the message must name both spellings rather than assume a fn body.
            Ty::Param(_) => self.satisfies(ty, "Eq").err().map(|_| {
                format!(
                    "{ty} is not bounded by Eq (add an `Eq` bound to {ty}: `[{ty}: Eq]`, or `where {ty}: Eq` on a fn)"
                )
            }),
            // Scalars, the identity handles, `Func`, `Module` — nothing a user `eq` can hide behind,
            // so there is genuinely nothing to prove. `Ty::Unknown` is the don't-cascade hole (a
            // prior error already reported). `Ty::Protocol` is the ONE permissive answer that is not
            // "nothing to prove" but "cannot be proven here", and it is deliberate: the concrete
            // witness is unknowable at this walk (existentials are erased by design), and it is not
            // exotic — every `Result[T]` carries `Ty::Protocol("Error")` in its error slot, so
            // refusing it would un-grant `Result` wholesale (and break the `("Eq", "eq", "result")`
            // ratchet row). Since W7-52 this arm is also what `[T: Eq]`/`.eq()`/`==` over a
            // protocol-typed value ITSELF (not just one nested inside a container/`Result`) routes
            // through — the D1 gate above now classifies `Ty::Protocol` as kind `"protocol"` instead
            // of refusing it, so this `None` is the grant, and the runtime witness is what actually
            // answers `eq`/faults, exactly as Go's interface `==` defers to the dynamic type. Same
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
                    // to act on. Carry it out verbatim instead. (The marker used to be shared with a
                    // second, GROWTH refusal that meant something else, so a growth refusal arrived
                    // here misclassified as budget exhaustion; there is one cause and one message
                    // now, so this test means exactly what it says.)
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

    /// The budget refusal. It names the type the WALK STARTED FROM, not the entry that happened to be
    /// the one over the line — those are different, and the second is useless: for a chain
    /// `S{N} → … → S0` the entry that exhausts the budget is `S0`, whose declaration (`struct S0: v:
    /// int`) nests not at all. The user asked about the root, so the root is what the message names.
    /// Emitted from two sites — [`Self::enter_eq_obligation`] where it originates and
    /// [`Self::eq_where_unsatisfied`] where it is re-stated on the way out — which recognise each
    /// other by [`EQ_BUDGET_MARKER`].
    fn eq_budget_refusal(ty: &Ty) -> String {
        // The ROOT of the current walk: the bottom of the in-progress stack, falling back to `ty`
        // when the stack is empty (the budget cannot actually trip then — this is just total).
        let root = EQ_BOUNDS_IN_PROGRESS.with(|s| s.borrow().first().cloned());
        let named = root.as_ref().unwrap_or(ty);
        // The type is ELIDED, because one shape that trips this budget has an enormous name:
        // polymorphic recursion refuses at `N[List[List[…×160…[int]]]]`, ~1 KB on one line, per
        // diagnostic, streamed to the editor. Keep the head — that is what the user wrote and can
        // act on — and say plainly that the rest is omitted. (rustc has the same problem and writes
        // the full type to a side file; eliding is the cheaper half of that.)
        const HEAD: usize = 60;
        let shown = named.to_string();
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
            // Built-in containers (TICKET-024, W8-32): binds a harvested native sig's `Ty::Param`
            // (e.g. `List[T]`'s `T`) to this instantiation's element type, from the SAME harvested
            // `StructInfo` `native_handle_method` reads — never a hardcoded "T"/"K"/"V" — so a
            // rename in `std/prelude.chz` can't silently unbind the substitution.
            Ty::List(e) => self.builtin_param_map("List", &[(**e).clone()]),
            Ty::Set(e) => self.builtin_param_map("Set", &[(**e).clone()]),
            Ty::Map(k, v) => self.builtin_param_map("Map", &[(**k).clone(), (**v).clone()]),
            _ => HashMap::new(),
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
                    self.error(
                        span,
                        format!("{name}[{t}]() expected element type {t}, found {inferred}{note}"),
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
    /// tables `satisfies_args_d`'s struct/enum/newtype arms feed to `satisfies_methods`, including
    /// their miss-only identity-key fallbacks (gap #4). A builtin/native or existential witness
    /// returns `None`: those satisfy through `satisfies_native` / `protocol_method_sig`, whose
    /// signatures are not the user method table this recovery unifies against.
    fn nominal_method_table(&self, ty: &Ty) -> Option<&HashMap<String, FnSig>> {
        match ty {
            Ty::Struct(k, _) => self.struct_shape(k).map(|i| &i.methods),
            Ty::Enum(k, _) => self.enum_methods_of(k),
            Ty::NewType(k, _) => self.newtype_methods_of(k),
            _ => None,
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
        // back RE-SPELLED in this protocol's own type-param vocabulary (`embed_arg_tys`), so
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
    pub(super) fn recover_protocol_args(
        &mut self,
        tps: &[TypeParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        // This pass is SPECULATIVE — it only decides what to bind — but `resolve_bound_arg` reports
        // into the diagnostic channels, and `enforce_bounds` re-resolves the very same bound args
        // straight after. Without the rollback a bad bound arg is reported twice per call site
        // (measured: `T: Produces[Bogus]` printed `unknown type 'Bogus'` twice). Roll back through
        // the paired helpers rather than truncating a channel by hand, so `warnings` stays in step.
        let mark = self.diag_mark();
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
                    binds.push((self.resolve_bound_arg(arg, tps, span), rec));
                }
            }
        }
        self.diag_rollback(mark);
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
            // `p` is the qualified IDENTITY key (`<module-key>::Name`, TICKET-027); render the BARE
            // display name in this advice, matching every other protocol-name diagnostic.
            let mut hosts: Vec<String> = self
                .protocols
                .keys()
                .filter(|p| {
                    self.protocol_method_sig(p, method)
                        .is_some_and(|s| s.is_static)
                })
                .map(|p| crate::compiler::bare_display(p))
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
            .protocol_shape(&bound.name)
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
    /// (`lib.reset(c)`) record exactly like a local one. M24-5 — a `defer`/`spawn` STATEMENT TARGET
    /// records here too: `compile_defer`/`compile_spawn` thread the witness at their own emit sites,
    /// widening `Op::DeferCall`/`DeferMethod`/`SpawnCall`/`SpawnMethod`'s `argc` exactly as a plain
    /// call widens `Op::Call`'s.
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

    /// PROBE for a result-only type parameter that another parameter's protocol bound needs, and
    /// bind each candidate to `Unknown` so the caller can ask whether the bound is satisfiable
    /// *given a hole there*. Returns the candidates in declaration order; the caller decides
    /// whether they are the real cause (see the call site in `infer_generic_call`).
    ///
    /// `recover_protocol_args` has already pinned every param recoverable from a witnessing
    /// method, so reaching here means the bound genuinely supplied nothing. Expected-result and
    /// explicit-type-argument inference have also run. Params mentioned by an argument slot are
    /// excluded because the closure-return loop-back later in the call may still recover them.
    ///
    /// The `Unknown` binding is load-bearing, not cleanup: `method_matches` compares through
    /// `compatible`, where `Unknown` is a wildcard, so it turns the follow-up `enforce_bounds`
    /// into exactly the question "would this conform if the param were inferable?".
    fn probe_uninferable_dependent_result_params(
        &mut self,
        sig: &FnSig,
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) -> Vec<String> {
        let wanted: std::collections::HashSet<String> =
            sig.type_params.iter().map(|tp| tp.name.clone()).collect();
        let mut in_result = Vec::new();
        ty_collect_params(&sig.ret, Some(&wanted), &mut in_result);
        let mut in_params = Vec::new();
        for param in &sig.params {
            ty_collect_params(param, Some(&wanted), &mut in_params);
        }

        let candidates: std::collections::HashSet<String> = in_result
            .into_iter()
            .filter(|n| !in_params.contains(n) && !sub.contains_key(n))
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }

        // Same speculative-pass rollback as `recover_protocol_args`: `resolve_bound_arg` REPORTS,
        // and `enforce_bounds` re-resolves these very args immediately after, so without the mark a
        // bad bound arg is printed twice on the call-site span. This helper is the second of the two
        // pre-`enforce_bounds` passes that resolve bound args; both need the pair.
        let mark = self.diag_mark();
        let mut needed_by_bound = Vec::new();
        for tp in &sig.type_params {
            if !sub.get(&tp.name).is_some_and(|ty| !ty.is_unknown()) {
                continue;
            }
            for bound in &tp.bounds {
                for arg in &bound.args {
                    let resolved = self.resolve_bound_arg(arg, &sig.type_params, span);
                    ty_collect_params(&resolved, Some(&candidates), &mut needed_by_bound);
                }
            }
        }
        self.diag_rollback(mark);

        // Preserve declaration order and probe each independently if multiple result params feed
        // parameterized bounds.
        let mut probed = Vec::new();
        for tp in &sig.type_params {
            if needed_by_bound.contains(&tp.name) {
                sub.insert(tp.name.clone(), Ty::Unknown);
                probed.push(tp.name.clone());
            }
        }
        probed
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
        // A bare same-module GENERIC fn read as an ARGUMENT here is NOT the final word on its type:
        // this call re-pins it below, exactly as `infer_generic_method` does for `[1,2,3].fold(0,
        // pick)`. So `infer_ident`'s "not determined here" wall must stay silent for the prepass and
        // the DEFERRED end-of-call check owns the verdict instead. Set HERE, not in the shared
        // `infer_generic_arg_tys` — its ctor callers pin nothing afterwards, so there the read IS
        // final. The helper scopes what is set here to the immediate bare-identifier arguments.
        let saved = std::mem::replace(&mut self.generic_fn_value_prepass, true);
        let mut arg_tys = self.infer_generic_arg_tys(args);
        self.generic_fn_value_prepass = saved;
        // Explicit call-site type arguments (`max[int](…)`) seed the substitution; remaining (or
        // all, when none given) parameters are inferred from positional arguments. `unify` only
        // binds a parameter that isn't already in the map, so explicit args take precedence and a
        // conflicting argument is caught by the per-argument check below.
        let mut subst_map: HashMap<String, Ty> =
            self.seed_targs(name, &sig.type_params, targs, span);
        // Bare generic-fn args this pass could not pin YET — re-pinned once everything else has had
        // its turn (see the second pass below).
        let mut deferred_fn_args: Vec<usize> = Vec::new();
        // Clamp to the shorter of the two, exactly as the `zip` this loop replaced did: `arg_tys.len()
        // == args.len()` can be < `sig.params.len()` when the call is short an argument (the arity
        // error is already reported above and does not early-return), so nothing may index past it.
        for i in 0..sig.params.len().min(arg_tys.len()) {
            let decl = &sig.params[i];
            // The GENERIC-callee ordering: a bare same-module generic fn is prepass-typed rigid
            // (`fn(T) -> T`, its OWN free params) because the prepass has no expected hint. Re-pin its
            // `[T]` from the slot with everything bound SO FAR; when that is not enough yet, the rigid
            // type must NOT unify either — it carries no information about this call, and `unify` is
            // first-binding-wins, so binding the callee's `[U]` to that leaked `T` is permanent and a
            // LATER argument that really determines `U` could never correct it (`applyg(ident, 5)`).
            // Same shape as `infer_generic_method`'s deferral, one level up. `try_pin_…` only fires on
            // a bare-ident generic fn that pins FULLY concrete; otherwise nothing changes here.
            if self.bare_generic_fn_value_arg(&args[i]).is_some() {
                let want = subst(decl, &subst_map);
                if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, span) {
                    arg_tys[i] = refined;
                } else {
                    deferred_fn_args.push(i);
                    continue;
                }
            }
            let actual = &arg_tys[i];
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
        // …and the same recovery for a USER parameterized bound (`T: Produces[R]` pins `R` from the
        // bound type's own `produce`), so it no longer needs an annotation the way `Iterator[T]`
        // never did.
        self.recover_protocol_args(&sig.type_params, &mut subst_map, span);
        // Expected-type checking-mode: a `let`/return/param annotation seeds any type param the args
        // left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing) against the
        // hint — so `xs: List[int] = empty()` pins a return-only `T`, and the deadlock probe below
        // sees it bound. After arg-unification ⇒ turbofish/args win.
        seed_from_hint(hint, &sig.ret, &mut subst_map);
        // Second pass for the deferred bare generic-fn args, placed HERE — after every sibling value
        // argument, the closure-return recovery, the `Iterator`/index recovery AND the annotation
        // hint have bound what they bind, and before `enforce_bounds` so a bound on a param this pass
        // fills is still checked. That is the same "last moment that can still pin" the method path
        // uses; it just has more sources to wait for (the method path has no hint to seed). A pin
        // replaces the rigid prepass type and unifies the CONCRETE result; one that still cannot pin
        // unifies its rigid type after all, so the existing assignability diagnostic still fires — by
        // now every real binding has already won, so the leak can no longer displace one.
        for i in std::mem::take(&mut deferred_fn_args) {
            let want = subst(&sig.params[i], &subst_map);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, span) {
                arg_tys[i] = refined;
            }
            unify(&sig.params[i], &arg_tys[i].clone(), &mut subst_map);
        }
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
        // An un-inferred type param and a genuinely non-conforming method BOTH surface as a bound
        // failure, and only one of them is the user's problem — so decide which by measurement, not
        // by the shape of the signature. Bind the candidates to a hole, then ask `enforce_bounds`:
        // if the bound conforms with the hole, the missing inference IS the cause; if it still
        // fails, the method could not conform for ANY instantiation and its own message is the true
        // one. Measured, the structural test alone is too wide — a wrong ARITY or a wrong PARAM
        // type leaves the param un-inferred too, and there "add a result annotation" is advice that
        // does not work (`docs/gaps.md` W8-43's neighbour table).
        let probed = self.probe_uninferable_dependent_result_params(sig, &mut subst_map, span);
        let before = self.errors.len();
        self.enforce_bounds(&sig.type_params, &subst_map, span);
        if self.errors.len() == before {
            for pname in probed {
                self.error(
                    span,
                    format!(
                        "cannot infer type parameter {pname} for '{name}'; add a result annotation or explicit type arguments"
                    ),
                );
            }
        }
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
        // …and NOW — with `subst_map` as bound as it will ever get — the deferred half of the
        // uninstantiated-generic-fn-value rule, the verdict the silenced prepass wall handed over.
        // The SAME reporter the method path calls, so `applyg(ident, 5)` and `[1,2,3].fold(0, pick)`
        // get one answer from one derivation.
        self.report_undetermined_generic_fn_value_args(args, &sig.params, &subst_map, span);
        // PART A — the empty-collection pin, at the LAST moment the substitution can still change.
        // Neither generic path routes through `check_args_range_decl`, so a bare empty binding passed
        // into a parameter that a SIBLING argument made concrete used to pin nothing: measured
        // check-clean at rc=0, `fn move_first[T](a: List[T], b: List[T])` called
        // `move_first(["x"], xs)` then `xs.push(1)` printed `['x', 1]`. Running it here — after every
        // recovery — is what makes the sibling-argument case work; `constrain_empty_arg` is a no-op
        // on a slot still carrying a `Ty::Param` or an `Unknown`, so a genuinely generic parameter
        // (`fn ident[T](xs: List[T])`) still pins nothing.
        for (i, arg) in args.iter().enumerate() {
            if let Some(decl) = sig.params.get(i) {
                let want = subst(decl, &subst_map);
                self.constrain_empty_arg(arg, &want);
            }
        }
        // M24 — half two of the static-witness contract, recorded LAST: `recover_return_only_params`
        // above can still bind a param that `enforce_bounds` never saw, so anything earlier would
        // read a param as un-determined that the call actually pins.
        if !sig.witness_params.is_empty() {
            let wparams = sig.witness_params.clone();
            self.record_witness_call(name, &wparams, &subst_map, key_span, span, recv);
        }
        self.report_uninferable_result_params(
            name,
            &sig.params,
            &sig.ret,
            &sig.type_params,
            &mut subst_map,
            span,
        );
        subst(&sig.ret, &subst_map)
    }

    /// A type parameter that appears in the RETURN type and is still unbound after every inference
    /// source has run is un-inferable at this call site: nothing downstream can pin it, so the value
    /// carries a rigid `Ty::Param` incompatible with every concrete type. Rust refuses the same
    /// shape up front — `fn make<U>() -> U; let z = make();` is `E0282: type annotations needed`,
    /// and `let xs = empty();` on `fn empty<T>() -> Vec<T>` is `E0282 … for Vec<_>`.
    ///
    /// Reported at the CONSTRUCTION site rather than left to leak. The leak was never unsound —
    /// every typed USE of the value already errored (`z + 1` → *cannot apply + to U and int*,
    /// `xs.push(1)` → a message that already said "bind it at the construction site") — but a value
    /// never used in a typed position slipped through entirely, and the blame landed downstream of
    /// the call that actually needed the annotation.
    ///
    /// **Where this must stay silent, derived by running each context, not from its shape.** A
    /// DECL-SITE default copy (`fn f(x: T = mkl())`, `struct S: f: T = mkl()`) is checked once at the
    /// declaration purely to catch a wrong-typed default; the expression's real home is the
    /// synthesized provider or the splice at each call, where the enclosing generic IS bound. Firing
    /// there rejected `fn tot[U](self, xs: List[Self] = mkl())` — a declaration correct at every real
    /// call site. Same context W7-51 had to neutralize for `?`, for the same reason.
    fn report_uninferable_result_params(
        &mut self,
        name: &str,
        params: &[Ty],
        ret: &Ty,
        tps: &[TypeParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        if self.decl_site_default {
            return;
        }
        let wanted: std::collections::HashSet<String> =
            tps.iter().map(|tp| tp.name.clone()).collect();
        let mut in_ret = Vec::new();
        ty_collect_params(ret, Some(&wanted), &mut in_ret);
        // RETURN-ONLY: a param that also appears in a PARAMETER slot is a different rule with a
        // different, already-tuned diagnostic. `fn tag[U](xs: List[U]) -> List[U]` called `tag([])`
        // leaves `U` unbound (an empty literal binds nothing), but each later `x.push(v)` already
        // reports it AND already names the fix — "the collection's element type is the un-inferred
        // type parameter U; bind it at the construction site with a turbofish or annotation". Firing
        // here too would add a third error saying the same thing. This rule owns only the case with
        // no parameter to blame at all.
        let mut in_params = Vec::new();
        for p in params {
            ty_collect_params(p, Some(&wanted), &mut in_params);
        }
        // Declaration order, so a multi-param signature reads left-to-right.
        let unbound: Vec<&TypeParam> = tps
            .iter()
            .filter(|tp| {
                in_ret.contains(&tp.name)
                    && !in_params.contains(&tp.name)
                    && !sub.contains_key(&tp.name)
            })
            .collect();
        if unbound.is_empty() {
            return;
        }

        // DEFER instead of rejecting when the result is a REFINABLE shape — the param sits in a
        // container SLOT, so filling it with `Unknown` produces exactly what an empty literal
        // produces (`fn empty[T]() -> List[T]` ⇒ `List[Unknown]`, the same type as `[]`), and the
        // existing refine-on-first-use pinning then lets a LATER statement fix the element type:
        // `xs := empty()` / `xs.push(1)` now infers `List[int]`, matching both Rust (which infers
        // from the later use) and Chezzi's own `xs := []` / `xs.push(1)`.
        //
        // **The gate must be exactly the shape the hand-off machinery pins — no wider.**
        // `Ty::Unknown` is universally assignable, so an `Unknown` that nothing ever pins and
        // nothing ever demands an annotation for is a silent hole: any value read out of it
        // type-checks against any annotation. Refine-on-first-use pins, and `empty_coll_sites`
        // requires an annotation for, exactly `Checker::is_unrefined_empty_coll` — a `List`/`Set`
        // with a DIRECT `Unknown` element, or a `Map` with a direct `Unknown` key/value.
        //
        // The first cut used the broader `contains_unknown_in_slot`, which also accepts a
        // `Struct`/`Enum`/`Tuple`/`Result` type argument. Nothing pins those, so
        // `struct Box[T]: v: List[T]` / `fn mk[T]() -> Box[T]` / `b := mk()` / `b.v.push(1)` let
        // `s: str = b.v[0]` type-check while `s` held an int — check-clean, then
        // `cannot apply Add to str and int` at runtime. Measured rejected before this rule existed.
        //
        // A param carrying a declared BOUND is never deferred either: `enforce_bounds` has already
        // run by this point with the param unbound, and the later pin goes through `repin`, which
        // re-checks no bound — so `fn empty[T: Show]() -> List[T]` / `xs := empty()` / `xs.push(1)`
        // would accept `int` for a `T: Show`. Also measured rejected before.
        let mut probe = sub.clone();
        for tp in &unbound {
            probe.insert(tp.name.clone(), Ty::Unknown);
        }
        if unbound.iter().all(|tp| tp.bounds.is_empty())
            && Self::is_unrefined_empty_coll(&subst(ret, &probe))
        {
            *sub = probe;
            return;
        }

        for tp in unbound {
            self.error(
                span,
                format!(
                    "cannot infer type parameter {} for '{name}'; add a result annotation or explicit type arguments",
                    tp.name
                ),
            );
        }
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
        // The declaration's `FnSig::min_params` — how few arguments it accepts INCLUDING the
        // receiver slot. `usize::MAX` for a caller with no signature to hand over (a synthesized
        // builtin sig), which collapses to the exact-arity check below.
        min_params: usize,
        key_span: Span,
        span: Span,
        // The enclosing `let`/annotation's expected type, same role as on the free-fn path: it pins
        // a type param that appears ONLY in the return. Threaded here so `v: int? = w.take(xs)`
        // solves `R` — before this the method path had no hint at all, so a result annotation could
        // not pin a return-only param and only a turbofish worked.
        hint: Option<&Ty>,
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
        // Trailing defaulted parameters are filled by the CALLEE, so the accepted count is a range;
        // `min_params` counts the receiver slot, which `expected` has already dropped.
        let min_args = min_params.saturating_sub(1).min(expected.len());
        if !(min_args..=expected.len()).contains(&args.len()) {
            let want = if min_args == expected.len() {
                format!("{}", expected.len())
            } else {
                format!("{min_args}-{}", expected.len())
            };
            self.error(
                span,
                format!("'{method}' expects {want} argument(s), got {}", args.len()),
            );
        }
        // One of the TWO prepasses whose bare-ident arg is re-pinned afterwards
        // (`try_pin_generic_fn_value_arg` below; the other is `infer_generic_call`), so a same-module
        // generic fn read here is NOT the final word on its type and `infer_ident`'s "not determined
        // here" wall must stay silent for it. Set at THIS call site: the ctor `infer_generic_arg_tys`
        // callers (struct/qualified/enum/newtype) pin nothing afterwards, so there the read IS final
        // and the wall must fire — setting the flag inside the shared helper silenced it at all seven,
        // which let `Bx(ident)` through to the very "argument 1 of 'f': expected T, found int" this
        // rule exists to replace. (The helper does SCOPE what is set here to the immediate bare-ident
        // arguments, so a nested `Bx(ident)` still faces the wall.)
        let saved = std::mem::replace(&mut self.generic_fn_value_prepass, true);
        let mut arg_tys = self.infer_generic_arg_tys(args);
        self.generic_fn_value_prepass = saved;
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
        // Bare generic-fn args whose slot could not pin them YET (see the `continue` below) — re-pinned
        // in the second pass after the loop, once the VALUE args have bound what they bind.
        let mut deferred_fn_args: Vec<usize> = Vec::new();
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
            } else if self.bare_generic_fn_value_arg(&args[i]).is_some() {
                // …and when the slot can NOT pin it yet, the rigid prepass type (`fn(T) -> T`, the
                // CALLEE's own free params) must not unify either: it carries no information about
                // this call, and `unify` is first-binding-wins, so binding the method's `[U]` to that
                // leaked `T` is permanent — a LATER argument that really determines `U` can never
                // correct it (`b.app(id, 5)` on `fn app[U](self, f: fn(U) -> U, x: U) -> U` reported
                // "argument to 'app' has type int, expected T"). Defer it past the value args, the
                // mirror of `.fold`'s accumulator ordering for a slot the other way round. Same shape
                // as Bug D's `mask_closure_ret` for an unannotated closure.
                deferred_fn_args.push(i);
                continue;
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
        // Second pass for the deferred bare generic-fn args: every value argument has now bound what
        // it binds, so their slots are as substituted as they will get. A pin here replaces the rigid
        // prepass type and unifies the CONCRETE result; one that still cannot pin unifies its rigid
        // type after all, so the existing assignability diagnostic still fires — by now every real
        // binding has already won, so the leak can no longer displace one.
        for i in std::mem::take(&mut deferred_fn_args) {
            let want = subst(&expected[i], &mmap);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, span) {
                arg_tys[i] = refined;
            }
            unify(&expected[i], &arg_tys[i].clone(), &mut mmap);
        }
        // Recover element types from `Iterator[T]` bounds, then enforce every declared bound.
        self.recover_iter_elems(mtps, &mut mmap, span);
        self.recover_index_args(mtps, &mut mmap, span);
        // Same user-parameterized-bound recovery as the free-fn path. It matters MORE here: this
        // path has no `seed_from_hint`, so before the recovery a `[R, T: Produces[R]]` METHOD could
        // not be pinned by a result annotation either — only turbofish worked.
        self.recover_protocol_args(mtps, &mut mmap, span);
        // Expected-type checking-mode, after the recoveries so precedence stays
        // turbofish > arguments > recovery > annotation (`seed_from_hint` only fills a param still
        // FREE). The free-fn path has done this all along; the method path had no hint plumbed to
        // it, which is why `v: int? = w.take(xs)` used to report a false conformance error PLUS
        // `cannot assign R to variable of type Option[int]`.
        seed_from_hint(hint, ret, &mut mmap);
        // …and the same probe-gated inference diagnostic as the free-fn path, so a recovery miss
        // names the un-inferable param instead of blaming the witnessing method's signature. The
        // gate is the `enforce_bounds` error-count delta: bind the candidates to a hole, and report
        // only if the bound conforms GIVEN that hole (a method that could not conform for ANY
        // instantiation keeps its own true message instead).
        // The probe reads only `params` / `ret` / `type_params`; the rest is inert padding.
        let msig = FnSig {
            params: expected.to_vec(),
            labels: Vec::new(),
            ret: ret.clone(),
            type_params: mtps.to_vec(),
            where_bounds: Vec::new(),
            min_params: expected.len(),
            is_static: false,
            doc: None,
            witness_params: Vec::new(),
            variadic: None,
        };
        let probed = self.probe_uninferable_dependent_result_params(&msig, &mut mmap, span);
        let before = self.errors.len();
        self.enforce_bounds(mtps, &mmap, span);
        if self.errors.len() == before {
            for pname in probed {
                self.error(
                    span,
                    format!(
                        "cannot infer type parameter {pname} for '{method}'; add a result annotation or explicit type arguments"
                    ),
                );
            }
        }
        // …and the general return-only case, same rule and same wording as the free-fn path: a param
        // in the return that nothing bound is un-inferable here, bound or not.
        self.report_uninferable_result_params(method, expected, ret, mtps, &mut mmap, span);
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
        // …and NOW — with `mmap` as bound as it will ever get — the deferred half of the
        // uninstantiated-generic-fn-value rule. This is the LAST possible moment, which is the whole
        // design: `[1,2,3].fold(0, pick)` is pinned by the accumulator, argument ZERO, while `pick` is
        // argument one.
        self.report_undetermined_generic_fn_value_args(args, expected, &mmap, span);
        // PART A — the empty-collection pin, at the LAST moment the substitution can still change.
        // Neither generic path routes through `check_args_range_decl`, so a bare empty binding passed
        // into a parameter that a SIBLING argument made concrete used to pin nothing: measured
        // check-clean at rc=0, `fn move_first[T](a: List[T], b: List[T])` called
        // `move_first(["x"], xs)` then `xs.push(1)` printed `['x', 1]`. Running it here — after every
        // recovery — is what makes the sibling-argument case work; `constrain_empty_arg` is a no-op
        // on a slot still carrying a `Ty::Param` or an `Unknown`, so a genuinely generic parameter
        // (`fn ident[T](xs: List[T])`) still pins nothing.
        for (i, arg) in args.iter().enumerate() {
            if let Some(decl) = expected.get(i) {
                let want = subst(decl, &mmap);
                self.constrain_empty_arg(arg, &want);
            }
        }
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
        let (_, sig) = self.bare_generic_fn_value_arg(arg)?;
        let declared = Ty::Func {
            params: sig.params.clone(),
            ret: Box::new(sig.ret.clone()),
            labels: FnLabels::new(sig.labels.clone()),
        };
        // Accept ONLY the fully-pinned verdict. A slot position that is still a free method param
        // (`.fold` arg1 before `init` binds `U`) or a return-only arg-fn param never pinned leaves the
        // shared derivation at `Undetermined`/`Skip` → bail, arg type unchanged. Reporting an
        // `Undetermined` here would be an EAGER check and would refuse `[1,2,3].fold(0, pick)`; the
        // verdict is re-asked once, at the end of the call, by
        // [`Checker::report_undetermined_generic_fn_value_args`].
        let FnValuePin::Pinned(m, refined) =
            pin_generic_fn_value(&sig.type_params, &declared, want)
        else {
            return None;
        };
        // Enforce the arg fn's declared bounds against the bindings, exactly as Scope A does.
        self.enforce_bounds(&sig.type_params, &m, span);
        Some(refined)
    }

    /// The gate both halves of the argument-position rule share: is `arg` a BARE reference to a
    /// same-module GENERIC fn — an identifier not shadowed by an in-scope binding (`lookup` None)?
    /// Mirrors `infer_ident`'s Scope A gate exactly. Returns the name and a clone of its signature.
    fn bare_generic_fn_value_arg(&self, arg: &Expr) -> Option<(String, FnSig)> {
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
        Some((name.clone(), sig.clone()))
    }

    /// The DEFERRED half of the uninstantiated-generic-fn-value rule: after EVERYTHING that could pin
    /// a bare generic fn passed as an argument has had its chance — every sibling argument, the
    /// receiver, the turbofish, the loop-back — re-ask [`pin_generic_fn_value`] with the FINAL
    /// bindings and report each argument nothing determined.
    ///
    /// WHY AT THE END, and not per-argument: `[1,2,3].fold(0, pick)` (`pick[T](a: T, b: T) -> T`) is
    /// pinned by the FIRST argument, the accumulator, while `pick` is the SECOND. An eager check at
    /// the moment the argument is encountered refuses a program that runs today (`3`) and that Go
    /// accepts. `map`'s `U` is likewise only known after the loop-back. Being deferred also makes
    /// this idempotent with the interleaved pin above: `mmap` only grows (`unify` is
    /// first-binding-wins), so anything that pinned there still pins here.
    ///
    /// `arg_decls` are the per-argument declared slot types (receiver already dropped), parallel to
    /// `args`; `map` is the call's completed substitution.
    fn report_undetermined_generic_fn_value_args(
        &mut self,
        args: &[Expr],
        arg_decls: &[Ty],
        map: &HashMap<String, Ty>,
        span: Span,
    ) {
        for (decl, arg) in arg_decls.iter().zip(args) {
            let Some((name, sig)) = self.bare_generic_fn_value_arg(arg) else {
                continue;
            };
            // The witness wall (`reject_witness_fn_value`) is a stricter, unconditional refusal with
            // different advice, and it already fired at the READ — do not stack a second message on
            // top of it.
            if !sig.witness_params.is_empty() {
                continue;
            }
            // The empty-collection carve-out, asked of the UNSUBSTITUTED slot (see
            // `fn_slot_params_have_unknown`): `[].map(ident)`'s `fn(?) -> U` carries the receiver's
            // sentinel and runs fine. Asking it of the SUBSTITUTED slot instead let the rule's own
            // subject escape — `Bx(0).two(ident, ident)` on `fn two[U](f: fn(U) -> U, g: fn(U) -> U)
            // -> List[U]` degrades `U` to `?` and then read as the sentinel, so the check went silent
            // and printed a `List[U]` nothing determines (Go: "cannot infer U").
            if fn_slot_params_have_unknown(decl) {
                continue;
            }
            let declared = Ty::Func {
                params: sig.params.clone(),
                ret: Box::new(sig.ret.clone()),
                labels: FnLabels::new(sig.labels.clone()),
            };
            if matches!(
                pin_generic_fn_value(&sig.type_params, &declared, &subst(decl, map)),
                FnValuePin::Undetermined
            ) {
                // The argument's own span, not the call's: the mistake is this read.
                let at = if arg.span == Span::default() {
                    span
                } else {
                    arg.span
                };
                self.reject_undetermined_generic_fn_value(
                    &name,
                    &sig.type_params,
                    &sig.params,
                    &sig.ret,
                    &sig.labels,
                    sig.min_params,
                    at,
                );
            }
        }
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
