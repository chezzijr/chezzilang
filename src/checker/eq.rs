//! Checker: structural `Eq`: reachability, bounds, obligations and the node budget.
//! Split out of `src/checker/proto.rs`. `super::*` is the parent module.

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

    /// Test-only counter of SUCCESSFUL [`Checker::enter_eq_obligation`] pushes -- how much bounded
    /// work a growth-refusal walk did before it was refused. Per-THREAD like the guard above, so it
    /// must be read inside the same `crate::on_frontend_stack_scoped` closure the checker itself runs
    /// in (`check_graph_diags` spawns a fresh thread whose thread-locals start at zero); reading it on
    /// the test's own thread measures nothing (TICKET-059).
    #[cfg(test)]
    static EQ_OBLIGATION_ENTRIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

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

    /// **TICKET-053 — the WHERE-ONLY memo, kept separate from [`EQ_BOUNDS_PROVEN`] on purpose.** A
    /// `None` from the where-only walk ([`Checker::eq_where_bounds_unsatisfied`]) is a WEAKER proof
    /// than a strict `None`: it means only "no `eq` `where` bound reachable from `ty` is provably
    /// unmet", not "this type's equality is sound to reach" (an invisible shape or an unbounded
    /// `Ty::Param` both answer `None` here, where the strict walk would refuse). Writing a where-only
    /// result into `EQ_BOUNDS_PROVEN` would hand the next strict `==` in the same query an unproven
    /// grant, so the two memos never mix. Reset alongside `EQ_BOUNDS_PROVEN`, same rule, same reason.
    static EQ_WHERE_PROVEN: std::cell::RefCell<Vec<Ty>> =
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
/// 10 000-deep chain — the depth the VM's own [`crate::base::words::MAX_STRUCTURAL_DEPTH`] equality walks — fits
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
/// earlier version of this constant was defined as `crate::base::words::MAX_STRUCTURAL_DEPTH` and claimed the
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
/// **`docs/contributing/gaps.md` W7-55 is closed by this.**
pub(super) const EQ_BOUNDS_MAX_NODES: usize = 50_000;

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
/// What [`Checker::reaches_user_eq`] found: a type that has the `eq` hook, or a type parameter that
/// does not carry the sealed bound `StructuralEq`. Each holds the rendered name.
pub(super) enum EqReach {
    Hook(String),
    Param(String),
}

/// Minted only by [`Checker::enter_eq_obligation`], so an entry cannot be pushed without its pop.
/// Carries the entry's [`Checker::ty_nodes`] size so the pop can subtract exactly what the push added
/// without re-walking the `Ty`.
pub(super) struct EqObligation(usize);

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

impl Checker {
    /// TICKET-250 (D5) — is `==` on `ty` structural all the way down? The one answer behind the
    /// sealed bound `StructuralEq` and the `Atomic` payload rule.
    pub(super) fn structural_eq(&self, ty: &Ty) -> Result<(), String> {
        match self.reaches_user_eq(ty, &mut Vec::new()) {
            None => Ok(()),
            Some(EqReach::Hook(inner)) if inner == ty.to_string() => Err(format!(
                "type {ty} does not satisfy StructuralEq (it has the 'eq' hook)"
            )),
            Some(EqReach::Hook(inner)) => Err(format!(
                "type {ty} does not satisfy StructuralEq (it reaches '{inner}', which has the 'eq' hook)"
            )),
            Some(EqReach::Param(p)) if matches!(ty, Ty::Param(_)) => {
                Err(format!("type {p} does not satisfy StructuralEq"))
            }
            Some(EqReach::Param(p)) => Err(format!(
                "type {ty} does not satisfy StructuralEq ('{p}' does not carry the bound)"
            )),
        }
    }

    /// The first type reachable from `ty` by a STRUCTURAL equality walk that has the `eq` hook,
    /// or the first type parameter that does not carry `StructuralEq`, or `None`. `stack` is the cycle guard (`sendable_rec`'s shape — a recursive
    /// `Node { next: Option[Node] }` terminates).
    ///
    /// Reachability here means exactly what `Vm::values_equal_guarded` recurses through: list/set
    /// elements, map keys AND values, `Option`/`Result` payloads, tuple slots, struct fields, enum
    /// variant payloads. NOT through a `Channel`/`Shared`/`Atomic`
    /// handle (comparing two handles compares the handles, never their contents) and not into a
    /// `Func`. `Protocol`/`Unknown` are permissive holes by construction — the concrete
    /// witness is unknown here — which is why the RUNTIME `cas` also refuses to dispatch the hook
    /// (`vm/netio.rs`), rather than trusting this walk to be exhaustive. A type parameter is a hit
    /// unless it carries the sealed bound `StructuralEq` (TICKET-250): its instantiations are then
    /// checked at each call.
    pub(super) fn reaches_user_eq(&self, ty: &Ty, stack: &mut Vec<String>) -> Option<EqReach> {
        let any = |s: &Self, ts: &[Ty], stack: &mut Vec<String>| -> Option<EqReach> {
            ts.iter().find_map(|t| s.reaches_user_eq(t, stack))
        };
        match ty {
            Ty::Param(name) => {
                let carries = self.type_params.get(name).is_some_and(|bs| {
                    bs.iter()
                        .any(|b| self.protocol_provides(&b.name, &b.args, STRUCTURAL_EQ_KEY, &[]))
                });
                (!carries).then(|| EqReach::Param(name.clone()))
            }
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
                if self.declares_hook(ty, "Eq", "eq") {
                    return Some(EqReach::Hook(ty.to_string()));
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
                if self.declares_hook(ty, "Eq", "eq") {
                    return Some(EqReach::Hook(ty.to_string()));
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
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Func { .. }
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

    /// The rendered type of the first `fn` value reachable from `ty` by the same structural walk as
    /// [`Self::reaches_user_eq`] (same cycle guard, same holes), or `None`. Every `Ty::Func` is a hit,
    /// a builtin fn value (`ord`) included (owner answer 2026-10-17). TICKET-144 (W14-24): `Atomic.cas` on a payload
    /// holding a closure can never succeed — every `load()` builds a fresh copy and closures compare by
    /// identity. `Param`/`Protocol` payloads hide the fn from this walk; the runtime `cas` backstops
    /// them (`Vm::slot_identity_in`).
    pub(super) fn reaches_func(&self, ty: &Ty, stack: &mut Vec<String>) -> Option<String> {
        let any = |s: &Self, ts: &[Ty], stack: &mut Vec<String>| -> Option<String> {
            ts.iter().find_map(|t| s.reaches_func(t, stack))
        };
        match ty {
            Ty::Func { .. } => Some(ty.to_string()),
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) => self.reaches_func(t, stack),
            Ty::Map(k, v) => self
                .reaches_func(k, stack)
                .or_else(|| self.reaches_func(v, stack)),
            Ty::Result(t, e) => self
                .reaches_func(t, stack)
                .or_else(|| self.reaches_func(e, stack)),
            Ty::Tuple(elems) => any(self, elems, stack),
            Ty::Struct(name, args) => {
                if let hit @ Some(_) = any(self, args, stack) {
                    return hit;
                }
                if stack.contains(name) {
                    return None;
                }
                let fields = self.structs.get(name)?.fields.clone();
                stack.push(name.clone());
                let hit = fields.iter().find_map(|(_, f)| self.reaches_func(f, stack));
                stack.pop();
                hit
            }
            Ty::Enum(name, args) => {
                if let hit @ Some(_) = any(self, args, stack) {
                    return hit;
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
                let hit = payloads.iter().find_map(|p| self.reaches_func(p, stack));
                stack.pop();
                hit
            }
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
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
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
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
    /// TICKET-053 added a second, WEAKER mode of this same walk —
    /// [`Self::eq_where_bounds_unsatisfied`], for the erasure gate in `Checker::assignable`'s protocol
    /// arm — but this method (the strict mode, called with `strict = true`) keeps its original single
    /// caller: [`Self::satisfies_args_d`]'s D1 arm turns `None` into [`Self::grant_intrinsic`] — a
    /// PROMISE that the runtime can dispatch `eq` on this receiver. So
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
        self.eq_bounds_unsatisfied_rec(ty, true)
    }

    /// **TICKET-053 — the WHERE-BOUND-ONLY half of [`Self::eq_bounds_unsatisfied`], for the erasure
    /// gate in `Checker::assignable`'s protocol arm.** `None` here is NOT the strict grant: it means
    /// "no `eq` `where` bound reachable from `ty` is provably unmet", not "this type's equality is
    /// sound to reach". The strict walk also refuses an invisible shape and an unbounded `Ty::Param`
    /// — both mean "cannot prove", not "unsound" — and at an erasure site refusing on those would
    /// reject programs that can never fault (a witness with no `eq` compares structurally, and
    /// structural equality is always defined). Measured before this gate existed:
    /// `fn wrap[T](x: Box[T]) -> Tagged: return x` was `ok: no type errors` rc=0.
    ///
    /// Declines outright (`None`) while an outer query is already in progress, rather than sharing
    /// [`EQ_BOUNDS_IN_PROGRESS`]'s cycle guard with the strict walk: no user-visible erasure site is
    /// ever asked from inside the `eq` walk itself, so the two modes never need to interleave, and
    /// keeping them apart means a where-only decline can never be mistaken for a strict one further
    /// up an in-progress stack.
    pub(super) fn eq_where_bounds_unsatisfied(&self, ty: &Ty) -> Option<String> {
        if !EQ_BOUNDS_IN_PROGRESS.with(|s| s.borrow().is_empty()) {
            return None;
        }
        EQ_WHERE_PROVEN.with(|m| m.borrow_mut().clear());
        self.eq_bounds_unsatisfied_rec(ty, false)
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
    fn enter_eq_obligation(ty: &Ty, strict: bool) -> Result<EqObligation, Option<String>> {
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
            // TICKET-053: a budget refusal means "could not prove", never "this bound is unmet" — in
            // the strict walk that must still fail closed (`Some`, a REFUSAL consumed as a grant would
            // be unsound), but the where-only mode is not proving soundness, only looking for an
            // UNMET bound, so running out of budget here is a decline, not a rejection.
            return Err(if strict {
                Some(Self::eq_budget_refusal(ty))
            } else {
                None
            });
        }
        EQ_BOUNDS_IN_PROGRESS.with(|s| s.borrow_mut().push(ty.clone()));
        EQ_BOUNDS_IN_PROGRESS_SEEN.with(|seen| seen.borrow_mut().insert(ty.clone()));
        EQ_BOUNDS_IN_PROGRESS_NODES.with(|n| n.set(total));
        #[cfg(test)]
        EQ_OBLIGATION_ENTRIES.with(|c| c.set(c.get() + 1));
        Ok(EqObligation(nodes))
    }

    /// Test-only: reset [`EQ_OBLIGATION_ENTRIES`] to 0 for the current thread (TICKET-059).
    #[cfg(test)]
    pub(super) fn reset_eq_obligation_entries() {
        EQ_OBLIGATION_ENTRIES.with(|c| c.set(0));
    }

    /// Test-only: read [`EQ_OBLIGATION_ENTRIES`] for the current thread (TICKET-059).
    #[cfg(test)]
    pub(super) fn eq_obligation_entries() -> u64 {
        EQ_OBLIGATION_ENTRIES.with(|c| c.get())
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
    pub(super) fn ty_nodes(t: &Ty) -> usize {
        match t {
            // Genuine leaves: no `Ty` children to visit.
            Ty::Int
            | Ty::Width(_)
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
            | Ty::Var(_)
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
            Ty::Func { params, ret, .. } => {
                1 + params.iter().map(Self::ty_nodes).sum::<usize>() + Self::ty_nodes(ret)
            }
            // A `Vec` of children; the name (where present) carries no size.
            Ty::Tuple(elems)
            | Ty::Struct(_, elems)
            | Ty::Enum(_, elems)
            | Ty::Protocol(_, elems) => 1 + elems.iter().map(Self::ty_nodes).sum::<usize>(),
        }
    }

    /// Walk the MEMBERS of a nominal `ty` (fields / payloads / underlying) under the shared cycle
    /// guard, memoizing a sound result. Every nominal arm funnels through here, so neither entering
    /// the guard nor the memo bookkeeping can be forgotten at a call site.
    fn walk_eq_members(&self, ty: &Ty, members: &[Ty], strict: bool) -> Option<String> {
        let memo = if strict {
            &EQ_BOUNDS_PROVEN
        } else {
            &EQ_WHERE_PROVEN
        };
        if memo.with(|m| m.borrow().contains(ty)) {
            return None; // proven sound (or where-clean) earlier in this query, unconditionally
        }
        let _guard = match Self::enter_eq_obligation(ty, strict) {
            Err(verdict) => return verdict,
            Ok(g) => g,
        };
        let hit = members
            .iter()
            .find_map(|m| self.eq_bounds_unsatisfied_rec(m, strict));
        if hit.is_none() {
            memo.with(|m| m.borrow_mut().push(ty.clone()));
        }
        hit
    }

    /// [`Self::eq_bounds_unsatisfied`]'s recursive body. The cycle guard is the shared thread-local,
    /// entered by [`Self::walk_eq_members`], NOT a `stack` parameter — the walk and the outward hop
    /// through `satisfies` are the same recursion and must not keep two disagreeing views of it.
    fn eq_bounds_unsatisfied_rec(&self, ty: &Ty, strict: bool) -> Option<String> {
        let any = |s: &Self, ts: &[Ty]| -> Option<String> {
            ts.iter()
                .find_map(|t| s.eq_bounds_unsatisfied_rec(t, strict))
        };
        // TICKET-053: a table miss / unresolvable shape means "cannot prove", which the STRICT walk
        // must refuse (its caller reads `None` as a grant) but the WHERE-ONLY walk must decline —
        // it is not proving soundness, only looking for an unmet `where` bound, and a shape it cannot
        // see cannot name one.
        let invisible = |ty: &Ty| -> Option<String> {
            if strict {
                Some(Self::shape_invisible(ty))
            } else {
                None
            }
        };
        match ty {
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) => self.eq_bounds_unsatisfied_rec(t, strict),
            Ty::Map(k, v) => self
                .eq_bounds_unsatisfied_rec(k, strict)
                .or_else(|| self.eq_bounds_unsatisfied_rec(v, strict)),
            Ty::Result(t, e) => self
                .eq_bounds_unsatisfied_rec(t, strict)
                .or_else(|| self.eq_bounds_unsatisfied_rec(e, strict)),
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
                    return invisible(ty);
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
                if let Some(sig) = self.hook_candidate(ty, Hook::Eq) {
                    if let hit @ Some(_) = self.eq_where_unsatisfied(ty, &sig) {
                        return hit;
                    }
                    if self.hook_method(ty, "Eq", "eq").is_some() {
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
                self.walk_eq_members(ty, &fields, strict)
            }
            Ty::Enum(name, args) => {
                // Bounds-then-hook, exactly as the struct arm above — see its comment for why a
                // non-hook `eq`'s `where` clause must still be proven. (`.cloned()` because
                // `enum_methods_of` borrows `self`, which `eq_where_unsatisfied` needs mutably-free.)
                if let Some(sig) = self.hook_candidate(ty, Hook::Eq) {
                    if let hit @ Some(_) = self.eq_where_unsatisfied(ty, &sig) {
                        return hit;
                    }
                    if self.hook_method(ty, "Eq", "eq").is_some() {
                        return None;
                    }
                }
                if let hit @ Some(_) = any(self, args) {
                    return hit;
                }
                // Same two fixes as the struct arm: instantiated payloads, and the owning-module
                // fallback `self.variants` alone does not have.
                let Some(payloads) = self.enum_payloads_of(name, args) else {
                    return invisible(ty);
                };
                self.walk_eq_members(ty, &payloads, strict)
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
            // fn`-only, `docs/reference/grammar.bnf`) — `struct Reg[K: Hashable] where K: Eq:` is a PARSE
            // error. So the message must name both spellings rather than assume a fn body.
            // TICKET-053: the where-only mode declines here too. An unbounded `T` reached through an
            // erasure gate — `fn wrap[T](x: Box[T]) -> Tagged` — is never compared unless `Box`'s own
            // `eq` names a `where T: ...` bound, and that case is already caught above by
            // `eq_where_unsatisfied` on `Box`'s hook before this arm is ever reached for `T` itself.
            // Refusing an unbounded `T` here in where-only mode would reject working generic code
            // that never compares anything (Gotcha 1: measured `ok: no type errors` rc=0 before this
            // gate existed).
            Ty::Param(_) if strict => self.satisfies(ty, "Eq").err().map(|_| {
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
            // hole, same reason, as `reaches_user_eq`'s. TICKET-053: this `None` is no longer the
            // WHOLE story for the erasure-into-a-protocol-slot shape — `Checker::assignable`'s
            // protocol arm now closes the hole one step earlier, at the erasure itself, via
            // [`Self::eq_where_bounds_unsatisfied`], so an unsatisfied witness is refused there and
            // never reaches this arm's runtime defer at all.
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::ByteArray | Ty::Nil | Ty::Func { .. } | Ty::Param(_) | Ty::Channel(_) | Ty::Shared(_) | Ty::Atomic(_) | Ty::AtomicInt | Ty::RwShared(_) | Ty::Executor | Ty::Socket | Ty::Listener | Ty::Writer | Ty::Reader | Ty::Ptr | Ty::Width(_) | Ty::Protocol(..) | Ty::Module(_) | Ty::Unknown | Ty::Var(_) => None,
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
                let bargs: Vec<Ty> = bound.args.iter().map(|a| subst(a, &tymap)).collect();
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
}
