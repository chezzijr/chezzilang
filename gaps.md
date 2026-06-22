# Chezzi — Language Gaps

Known limitations found by writing real programs against both engines (`chezzi run` / `--interp`).
**Open** entries carry what they block + a fix sketch with `file:line` anchors. Resolved gaps collapse
to a one-line log — full detail lives in `PROGRESS.md` + the cited `examples/*.chz`.

Legend: 🔴 blocks real apps · 🟡 notable friction · ⚪ latent (not currently reachable) · 🟢 works.

Last updated: 2026-06-21 (adversarial fuzz pass, 6 parallel hunters — 9 issues filed under "🔴 Soundness
holes" below, all verified on both engines: **2 Rust panics** (interpolation w/ undefined name; list
`map`/`filter`/`fold` when the callback shrinks the list), the **`Ty::Unknown` permissive-sentinel
family** (empty collections + recursive-return inference + generic nullary variants — one root fix
closes all three) incl. the float-key/NaN-footgun bypass, import name-collision mis-resolution,
duplicate-binding-in-pattern last-wins, `--serial` generator-airlock parity divergence, inconsistent
int→float coercion (the inconsistent int→float coercion is now RESOLVED — one-way C-like widening
landed 2026-06-22; the `ref` shared-method expression-receiver false rejection is RESOLVED too).
Concurrency core (airlock, channel
close, deadlock detect, CAS, 100k spawns, cancel-tree) fuzzed clean.). Baseline: post-M21 (`newtype`), plus enum methods, raw string literals
(`r"…"`), the `extern "lib"` header, module-scoped user types + module-qualified match patterns, and
the `chezzi docs` / `chezzi init` CLI + `module:function` manifest entrypoint. Earlier baseline
(post-M20): `assert`/`test fn`/`chezzi test`, Python-colon slicing, std.math trig + request verbs;
concurrency D6 complete (Path C resolved). Gaps pass III.

> **Core constructs are all in place** (the language **still evolves** — new features land via their
> own milestone; M19 is **pre-JIT perf, not a feature freeze**): scalars, `list`/`map`/`set`/`tuple`,
> generic structs + enums (+ enum methods), nominal `newtype` (M21),
> `Result`/`Option` + `?`, generics + structural protocols
> (`Comparable`/`Add`/`Sub`/`Mul`/`Hashable`/`Stringable`/`Error`/`Iterator[T]`/`Iterable[T]`/`Index[K,V]`/`IndexSet[K,V]`/`Slice[R]`),
> exhaustive `match` (literals/wildcard/nested/tuple/guards/ranges/qualified variants), closures/HOF,
> methods, module-scoped types, GC, two backends, interpolation, raw strings, pipe, `recover:`,
> `defer` (block-scoped), default + named args, comprehensions, optional-chaining/`??`. What remains is
> **stdlib breadth + a few runtime-depth nits** (plus new features as they're scoped).
>
> **This doc is the unified actionable backlog** — open language/stdlib gaps *and* the M19 perf +
> runtime track (memory layout, JIT, GC, NaN-box) with `file:line` anchors. Full design detail +
> the purely-speculative tracks (BEAM shared-nothing concurrency, far-out ideas) live in
> [`docs/future.md`](docs/future.md); live perf numbers in [`docs/benchmarks.md`](docs/benchmarks.md).

---

## Open gaps — START HERE

### 🔴 Soundness holes (found 2026-06-21, adversarial fuzz pass — `check` greenlights code that breaks at runtime)

- **✅ RESOLVED (2026-06-21) — String interpolation expressions bypass the checker.** The `ExprKind::Str`
  arm now parses the literal into chunks via the shared `crate::interpolation` parser (the same one the
  compiler emits from) and runs every `{…}` fragment expr through the normal `infer_value` path
  (`checker/mod.rs::check_interpolation`). Undefined names (`print("{nope}")`), type/method/arity errors
  (`"{x + y}"` int+list, `"{x.no_such()}"`, `"{f(1,2,3)}"`), and void-call fragments now surface as
  compile errors at the string's span, so `compiler/mod.rs` `global_slot`'s "checker rejected undefined
  names" invariant holds and the prior panic is impossible. The interpolation parser was extracted from
  the compiler into `src/interpolation.rs` so checker and compiler chunk strings byte-identically
  (two-engine parity preserved). Same-quote nesting (`"{"a"}"`) still errors at the *lexer*
  (pre-3.12-Python limit) — out of scope. Pinned by `checker::tests::interpolation_*`.

- **✅ RESOLVED (2026-06-21) — Empty collection literals (`[]` / `{}` / `set()`) no longer ship an
  unrefinable `Ty::Unknown` element.** Closed via **full refine-on-first-use** (`checker/mod.rs`
  `refine_receiver` at the top of `infer_method_call`; `refine_index_receiver` in `check_assign`'s
  Index branch). When a LOCAL binding (simple variable only) has a type carrying `Unknown` in an
  element/key/value/type-arg slot (recursing through list/set/map/Option/Result/tuple/Channel/Shared/
  Atomic and user generic struct/enum via `contains_unknown_in_slot` + `merge_unknown`), the FIRST
  mutating op that supplies a concrete type (`.push`/`.add`/`.insert`/`.extend` / `x[k]=v`) re-pins the
  binding at that slot; a later op supplying an INCOMPATIBLE concrete type is then a normal mismatch,
  enriched to hint at annotating for a mixed/protocol collection.
  - **Mixed-type elements now rejected:** `x := []; x.push(1); x.push("s")` is a type error. So is the
    nested-typeparam (`[None]` then conflicting `Some`) and nullary-variant (`[Box.Empty]` then
    conflicting `Box.Full`) form. Heterogeneous/protocol collections now REQUIRE an explicit
    annotation (`shapes: list[Shape] = []`) — intended; an annotated binding has a concrete element
    type so refinement never engages and pushes check against the protocol as before.
  - **Float-key / NaN footgun closed:** `m := {}; m[1.5] = "b"` and `s := set(); s.add(1.5)` (and the
    NaN `inf - inf`) are rejected — via a DIRECT insertion-site `is_hashable_key` check at `m[k]=v`
    (so it fires even while the key type is still `Unknown`) and at set-element concrete-ification in
    `refine_receiver`.
  - **Refinement is BLOCK-LOCAL flow-sensitive** (`snapshot_refinable`/`restore_refinable` wrapping
    every conditionally-run body: `check_block` (if/else/while/defer), the `for` body, and `match`/
    `if`-expr arms): a refinement inside one branch arm does NOT leak into a sibling arm or post-branch.
    `xs := []` + `if c: xs.push(1) else: xs.push("s")` is accepted (each arm refines independently);
    a straight-line on-all-paths refinement still persists (catches the real bug).
  - **Residuals (documented):** (1) **simple-variable receiver only** — `obj.field.push(…)` /
    `f().push(…)` / `xss[0].push(…)` are not refined (struct fields are explicitly typed, low impact);
    (2) **cross-branch / post-branch mixing is not caught** (e.g. push int in one arm, str after the
    branch) — that needs full join reconciliation, a follow-up — but there is NO false rejection and
    NO leak.
  - **Golden-test checker-bypass fixed:** the golden example tests drive `run_capture`, which BYPASSES
    the Checker, so a checker regression on a shipped example shipped falsely green. Added
    `checker::tests::all_shipped_examples_typecheck` (build_graph + check_graph over every
    `examples/*.chz`, mirroring `chezzi check`; two intentional run-only demos allow-listed) so example
    type-errors are caught from now on. `examples/poly_method.chz` was annotated `list[Shape]` under
    the new rule. Pinned by `checker::tests::{empty_list_*, empty_map_*, empty_set_*, flow_sensitive_*,
    heterogeneous_struct_list_unannotated_rejected, …}`.

- **✅ RESOLVED (2026-06-22) — one-way C-like `int`→`float` implicit widening landed.** Was: coercion
  was *inconsistent* — silently allowed in binary operators, forbidden everywhere else. Now Chezzi has a
  uniform **one-way** rule: an `int` value widens into a `float` SLOT (never the reverse), so the binary
  operators that already widened (`5 + 2.0`, `5 == 5.0`, `5 < 5.5`) are now consistent with the rest of
  the language. The compiler emits a **real** runtime conversion (`Op::CoerceFloat`; the interp applies
  the equivalent `coerce_float` helper) at each value-DEFINITION sink, driven by the static annotation —
  so the stored value is a genuine `f64`, identical on the checked CLI path AND the checker-bypassing
  parity harness (byte-identical two-engine parity by construction). **Covered sinks:** typed `let`
  (`x: float = 3`), function / method / closure args incl. an int VARIABLE (coerced at the callee
  prologue), `-> float` returns (incl. inline-expr bodies), `float` struct fields, native std.math float
  params (`sqrt(16)`; already runtime-promoted — this resolves the old "inconsistent" complaint),
  `extern` C `double` params (FFI host promotes), and **mixed-numeric-literal** collections — a list/map
  literal with ≥1 float literal infers `list[float]`/`map[_, float]` and its int literals are coerced
  (`xs: list[float] = [1, 2.3]`, bare `[1, 2.3]`, `map[_, float]` values). **Anti-lossy stays a type error:**
  `y: int = 2.3`, `f() -> int: return 2.3`, a `float` into a `list[int]`, an int→float into a NEWTYPE
  (nominal). **Scoped carve-outs (documented, not holes):** (1) an UN-annotated NON-literal mixed
  collection (`xs := [a, b]` with `a:int`, `b:float`) — the checker now infers `list[float]`, but the
  compiler is type-blind about the non-literal `a`, so `xs[0]` stays `Int` at runtime (never widened
  before; rare). (2) a plain reassignment `x = 3` to a `float`-declared local stays a STRICT assign
  target (rejected by the checker) — like the type-blind `p.x = 3` / `xs[0] = 3` / `m[k] = 3` targets,
  which the compiler cannot coerce against and which therefore remain rejected (no runtime hole). (3) a
  generic field/param typed `T` bound to `float` is not widened (matches existing no-generic-widening).
  (4) widening is **scalar-at-the-sink** only: a COMPOUND/nested/wrapped float annotation does not widen
  an int-bearing value — `list[list[float]] = [[1]]`, `float? = Some(3)`, `float! = Ok(3)`, an all-int
  literal `list[float] = [1, 2]`, and a non-literal RHS (`list[float] = f()` where `f` returns
  `list[int]`) all stay type errors. Write explicit floats (`[[1.0]]`, `Some(3.0)`, `[1.0, 2.0]`) or a
  mixed literal. The checker cannot tell a safe literal `[1, 2]` from an unsafe non-literal `f()`, and
  only a scalar `float` sink is compiler-coerced — so compound widening is deliberately strict to avoid
  an `Int` in a `float` slot.
  - Sibling: **`1 << 63` wraps silently to `INT_MIN`** (no overflow check) while `+ - * / %` are
    checked (recoverable-panic on overflow) — so the "Verified working" line below ("integer overflow →
    recoverable panic (never wraps)") is false for shifts. Also `INT_MIN` is unwritable as a literal
    (`-9223372036854775808` lexes as unary-minus over `i64::MAX` → "number too large"; same as Rust).

- **✅ RESOLVED — `list.map` / `.filter` / `.fold` no longer panic when the callback shrinks the
  receiver list.** `list_hof` (`vm/mod.rs`) now allocates a **rooted snapshot** of the receiver's
  elements at call time and indexes that (mirroring `list_sort_by` and the interp, which already
  snapshots `elems` before dispatch). Chosen semantics: **snapshot** — map/filter/fold iterate the
  receiver's elements as of call time; a callback that shrinks **or** grows the receiver does not
  perturb the iteration (consistent with comprehensions, `for`-loops, and Python `map`/`filter`).
  ```chezzi
  xs := [1,2,3,4,5]
  fn f(x: int) -> int:
      xs.pop()
      return x * 2
  ys := xs.map(f)        # was: panic OOB at vm/mod.rs:6840; now → [2, 4, 6, 8, 10] on BOTH engines
  ```
  Regression tests: `map`/`filter`/`fold`_shrinking_callback_no_panic (vm), golden
  `examples/list_hof_shrink.chz` (VM==interp byte-identical).

- **🔴 `Ty::Unknown` is treated as assignable to/from ANY concrete type — every source of `Unknown`
  is a soundness hole (generalizes the empty-collection hole above).** `Unknown` is the checker's
  "already-errored / permissive" sentinel; `compatible(Unknown, T)` is true in *both* directions, so any
  `Unknown`-typed value flows, fully check-blessed, into an `int`/`float`/`str`/struct/`fn`/map-key slot
  and then faults at runtime (the VM is defensive — recoverable error, not a Rust panic, but the static
  guarantee is gone). Beyond empty collections, two more reachable producers:
  - **Recursive / forward-reference return inference. ✅ RESOLVED** (fixpoint inference,
    `checker/mod.rs` `infer_returns`). Return-type inference is now ORDER-INDEPENDENT: the per-pass walk
    runs to a bounded FIXPOINT (re-infer every un-annotated fn/method until no stored `FnSig.ret`
    changes, cap = un-annotated-count + 1), so a forward-reference or mutually-recursive callee resolves
    on a later pass instead of leaking `Unknown`. A self-recursive call contributes no type (skipped);
    the non-recursive returns decide. After convergence, any un-annotated fn/method still `Unknown`
    (pure recursion / mutual recursion with no concrete base anywhere) is genuinely un-inferable and
    keeps a **permissive** type — it is NOT rejected (a blanket "leftover Unknown ⇒ require annotation"
    over-reaches onto non-recursive Unknown sources like `return x[0]` of an empty collection and
    double-reports on already-errored bodies; soundly rejecting only the recursive-no-base case needs
    call-graph cycle detection — a follow-up). The example below now correctly REJECTS `v: int = rec(2)`
    (`rec` infers `str`):
    ```chezzi
    fn rec(n: int):
        if n <= 0: return base(0)
        return rec(n - 1)
    fn base(n: int): return "hello"   # forward ref ⇒ rec now infers str (fixpoint)
    v: int = rec(2)                   # ✅ check REJECTS: cannot assign str to variable of type int
    ```
    (Divergent CONCRETE returns remain the user's job to annotate — `-> T` or a protocol existential
    `-> Stringable`; with no annotation conflicting concretes are a `expected return type …, found …`
    error. No union types.)
  - **Nullary variant of a generic enum (and `None`). ✅ RESOLVED** by the same refine-on-first-use
    pass (the slot Unknown is nested in the user-enum / Option type arg; `merge_unknown` recurses into
    it). `xs := [Box.Empty]; xs.push(Box.Full("hi"))` pins `list[Box[str]]`, so a conflicting
    `Box.Full(5)` push is a type error; likewise `[None]` + conflicting `Some` pushes:
    ```chezzi
    enum Box[T]:
        Full(T)
        Empty
    xs := [Box.Empty]
    xs.push(Box.Full("hi"))   # pins list[Box[str]]
    xs.push(Box.Full(5))      # ✅ check REJECTS (expected Box[str], found Box[int])
    ```
  **Status:** the recursive/forward-reference-return producer is **✅ RESOLVED** via fixpoint inference
  (above). The **empty-collection** AND **generic-nullary-variant** (`Box.Empty` / `None`) producers are
  now **✅ RESOLVED** via full refine-on-first-use + insertion-site Hashable check + block-local
  flow-sensitivity (see the resolved entry above; residuals: simple-variable-receiver-only, and
  cross/post-branch mixing needs join reconciliation). All three reachable `Unknown`-in-slot producers
  are closed; `Unknown` remains permissive only as the bare cascade-suppression sentinel (which is by
  design — `contains_unknown_in_slot` returns FALSE for a bare top-level `Unknown`, so refinement never
  fights cascade-suppression).

- **✅ RESOLVED — Import name collisions are now rejected (checker ↔ runtime no longer disagree).**
  `bind_import` records every import bind-name (across values / functions / modules / type-names) into a
  per-module `import_binds` set; a second import binding an already-bound name is rejected with `'<name>'
  is already imported`. Catches both the unsound value-then-fn case (`import v` value + `import v` fn) and
  the wrong/silent duplicate `from`-import case (`import f from lib` + `import f from lib2`, two `Point`
  structs, etc.). Distinct names + `import mod as alias` still pass; a missing member stays its own error.
  Tests in `src/checker/tests.rs` (`import_value_then_fn_same_name_rejected`, `duplicate_from_import_*`,
  `import_alias_collision_rejected`, + the `*_ok` regression fences).

- **✅ RESOLVED — Duplicate binding in a single pattern (`(x, x)`, `V(a, a)`) is now a compile error.**
  `bind_match_arm` runs `first_duplicate_binder` over each (non-Or, non-Wildcard) pattern and rejects a
  repeated binder with `identifier '<name>' is bound more than once in this pattern` (Rust's rule).
  Covers tuple, enum-variant payload, and nested patterns; `_` repeated stays OK (binds nothing), a name
  reused across SEPARATE arms stays OK, and an or-pattern `A(x) | B(x)` (same names across distinct alts)
  stays OK. Tests: `duplicate_tuple_pattern_binder_rejected`, `duplicate_enum_payload_binder_rejected`,
  `duplicate_nested_pattern_binder_rejected`, `wildcard_repeated_in_pattern_ok`, `same_binder_across_arms_ok`,
  `or_pattern_same_names_across_alts_ok`.

- **🟡 `--serial` lets a generator cross the `spawn`/`parallel:` airlock — engine-parity divergence; the
  oracle is the unsound one.** A live generator captured as a module global and driven from inside a
  `spawn`ed task is correctly rejected at runtime by the default OS-thread engine
  (`a generator cannot be sent across tasks`), but `--serial` (the deprecated cooperative oracle) runs it,
  sharing one mutable generator across the task boundary (parent reads 0, task advances to 1, parent reads
  2). Same program → different observable output on the two engines, violating the byte-identical
  invariant; `--serial` permits the airlock-forbidden behavior. Low urgency (serial is the
  slated-for-removal parity oracle and the *default* engine is correct), but it is a real divergence.
  **Pre-emptive fix:** mirror the VM's send-time generator-sendability check in the serial engine's
  spawn/closure-capture path.

- **✅ RESOLVED (2026-06-22) — `ref` shared-method-name dispatch no longer falsely rejects an expression
  receiver.** When ≥2 structs share a method name with differing param ref-ness (so the receiver type must
  disambiguate, per docs §3), the call type-checks both when the receiver is a *named local* (`a := A(0);
  a.apply(r)`) AND when it is an *inline expression* (`A(0).apply(r)`, or a struct-returning fn call
  `mk().apply(r)` where `fn mk() -> A`). Previously the inline-expression form was falsely rejected
  ("argument 1 of 'apply': expected Ref[int], found int") — an over-rejection of valid code (safe, not
  unsound). **Fix (desugar-only, pre-type):** `callee_param_is_ref`'s method-call arm now resolves the
  receiver's struct name via a new `receiver_struct_ty` helper covering a named local, an inline ctor call
  (`struct_value_ty`), and a struct-returning free fn (new `ModReg::fn_ret_struct` map, populated from the
  declared return type), then keys `methods_by_struct` uniformly — so the alias survives into the checker.
  Covered by `lowers_ref_arg_through_ctor_receiver_typed_method` / `..._fn_call_receiver_typed_method`
  (desugar), `ref_through_shared_method_name_ctor_receiver_ok` / `..._fn_receiver_ok` (checker), and the
  extended `examples/ref_indirect.chz` golden (two-engine parity, stdout `42`). Limit: the fn-call receiver
  resolves only through the declared return-type annotation (syntactic, pre-type) — same narrow scope as the
  existing pre-type method-resolution.

### 🟡 Type-system + runtime depth

- **✅ RESOLVED (2026-06-17) — was MISDIAGNOSED as "bare `fn` name not callable".** The original report
  blamed the bare-`Ident`-of-a-`fn` load/call-dispatch path. That was wrong: dispatch was always correct —
  a named fn with an explicit `return` works fine as a value (`g := a; g()`), via `.map`, and through an
  HOF. The real root cause was a **missing-return check**: a function body is a sequence of statements, so
  an inline `fn a() -> int: 10` parses `10` as a *discarded* expr-statement and silently falls off the end
  to `nil`. Fixed via **Option B** (see "declared-non-void fn must return on every path" below): the checker
  now rejects a fn with a *declared* non-void return type that can fall off the end without a value
  `return`, with a hint to add `return` or use a closure `fn() -> T: <expr>` (whose body IS an expression,
  so it implicitly returns). The silent-wrong-`nil` is now a loud type error.
  - **✅ FOLLOW-UP RESOLVED (2026-06-17) — inline-expr body now implicitly returns + nil rejected in
    value position** (Option A inline-only; `parser/mod.rs` `inline_expr_body` marker on `FnDecl`,
    `compiler/mod.rs` + `interp/mod.rs` `compile_fn`/`call`, `checker/mod.rs` `infer_fn_ret`/`check_fn_body`
    + `infer_value`). The void-discard footgun is gone two ways: (1) a bare `fn a(): 10` (inline-expr body)
    now **implicitly returns** `10` — like a closure — so `10` is no longer discarded; `fn a() -> int: 10`
    is valid (Option B's fall-off check is exempted for inline-expr bodies). (2) Using a void result as a
    value (`x := print(...)`, `[log(...)]`, `1 + sort()`) is now a loud type error *"expression returns no
    value (nil) and cannot be used as a value"*. A multiline 1-stmt body still does not implicitly return.
    `examples/inline_fn.chz`; two-engine golden byte-identical. `docs/syntax.md` §5.

- **⚪ Checker `=` to a by-value captured local — latent, UNREACHABLE on HEAD** (verified 2026-06-16).
  `infer_closure` pushes no capture floor (`checker/mod.rs:2841`); an inner fn/closure writing an
  enclosing local *would* type-check then misroute the store to `SetGlobalSlot` (`emit_store`,
  `compiler/mod.rs:932` — no `SetCaptured` arm). But no surface syntax reaches it: closures parse a
  single expression (assignment is a statement), nested named `fn` is rejected (`unknown name`), and
  `spawn:`/`defer:` blocks are already gated (`capture_floors`). Re-open only if block-bodied closures
  or nested-fn names land. **Pre-emptive fix:** push a capture floor in `infer_closure` → turn the
  silent misroute into the documented `cannot reassign captured binding` error (`docs/syntax.md:828`
  promises read-only captures, enforced today only across `spawn`/`submit`).

- **⚪ Module-scoped types: a few error paths leak the qualified identity key — latent, UNREACHABLE on
  HEAD** (verified 2026-06-20). Module-scoped types key every user struct/enum by an always-qualified
  identity (`mod::Point`) with a separate bare display name (`Point`) rendered in all *reachable*
  user-facing output. A handful of *defensive* error strings still interpolate the raw key instead of
  `bare_display`/`display_name`: `dispatch_index_method` (`vm/mod.rs:12121`) and the `next`-dispatch
  errors (`interp/mod.rs:4636/4833/5003` + the VM twins) say `struct 'mod::Point' has no method '…'`.
  But no well-typed program reaches them: the checker statically gates indexing/slicing/iteration on the
  protocol method existing, so these are `unreachable!()`-adjacent. Two adversarial reviewers (build +
  run) could not trigger any. Not fixed pre-emptively because the VM strings are contracted
  byte-identical with their interp twins (parity-tested stdout), so a fix must touch every mirrored
  site at once for zero observable gain. Re-open only if an index/slice/iter path becomes reachable
  without the protocol-method check. **Pre-emptive fix:** wrap the interpolated key in
  `crate::compiler::bare_display(...)` at all mirrored sites in lockstep.

### 🟡 Concurrency (deeper tracking in `docs/concurrency.md` §11 + `docs/concurrency-tier-d.md`)

The `--parallel` engine is a true M:N scheduler through D6 (fibers, work-stealing, dirty pool,
netpoller + `std.net`). Remaining items are correctness/semantics nits, not missing breadth.

> **Planned engine consolidation (mooting note).** The intent is to **remove `--interp` (frozen
> tree-walk) and `--serial` (cooperative single-thread VM) later, leaving M:N `--parallel` as the sole
> engine.** Both items marked **[mooted-by-removal]** below are *cooperative-engine-only* limitations
> that M:N already handles — once the cooperative engines are gone they aren't gaps, so they're
> effectively **WON'T FIX pending consolidation** (don't invest in a coop fix). **Tradeoff to weigh
> first:** `--interp`/`--serial` are the **parity oracles** — the "two engines asserted equal"
> differential testing that caught most of the resolved-log bugs. Removing the oracle ends that net.
> `--serial` is the cheap one to keep (shares the VM compiler/opcodes, still byte-identical); consider
> dropping only `--interp` and giving `--serial` the D3 back-edge yield budget so it keeps the oracle
> *and* closes CPU-preempt.

- **🟡 [mooted-by-removal] Cross-nursery wakeups — cooperative-engine flatten pending.** RESOLVED under
  `--parallel` (M:N flat scheduler — multi-level nesting + late-spawn,
  `examples/parallel_cross_nursery_circular.chz`); the **cooperative** engine still can't flatten a fiber
  in an outer nursery woken by an inner one (`docs/concurrency-tier-d.md:342`). Output correct under
  `--parallel`; the gap is coop-only liveness → vanishes when the cooperative engines are removed.
- **🟡 [mooted-by-removal] Cooperative + `--interp` engines cannot preempt CPU-bound tasks.** They
  switch fibers only at yield points (channel ops, blocking `recv`, back-edge-to-scheduler). A pure-CPU
  loop with no channel op monopolizes the single thread → a sibling canceller/timeout never runs.
  **Same source diverges by engine:** a 2e9-iter CPU worker under manual-cancel aborts mid-flight on
  `--parallel` (~0.5s) but runs to completion on coop/`--interp` (~69s). This is why cancellation
  examples carry no golden `.expected` (`examples/parallel_cancel.chz`) — single-engine would unblock
  goldens. **M:N already preempts** (reduction-counting, D3, `vm:2871`), so removing the cooperative
  engines closes this for free; the only coop-side fix worth considering is the back-edge yield budget
  *if* `--serial` is kept as the oracle.
- **🟡 `Shared.update` same-box hold-and-wait — WON'T FIX by design.** Path C resolved the *general*
  case (a `recv` inside `update(f)` waiting on a **different** box thread-demotes + resumes). The
  carve-out: `update(f)` holds `update_lock` across `f`, so a `recv` needing the **same** box deadlocks
  any sender for it. The universal hold-and-wait class (Go #13759 global-only; Rust
  `clippy::await_holding_lock`; BEAM no shared locks). Rule: don't block on a value needing the same
  `Shared` box. Future: a lint when the tooling track lands.
- **🟡 (residual) `break`/`continue` out of `parallel:` inside a loop reclaims its nursery only at fn
  return**, not block-scoped like interp's `exec_parallel` pop — bounded to frame lifetime, output always
  correct. Closing it needs loop-exit-jump codegen to emit a nursery-drain across a nursery boundary.

### 🟡 Stdlib breadth (low priority — core constructs are in place; this is library fill)

Current stdlib (`std.fs`/`io`/`os`/`process`/`time`/`request`/`regex`/`json`/`math`/`cmp`/`str`/`iter`/
`cancel`/`ref`/`net`) covers read-and-transform scripting well. Gaps below block write-heavy
automation, randomness, binary/crypto, CLI tooling. Ranked by leverage.

**Must be native (Rust):**
- **`std.rand`** *(highest leverage)* — no RNG anywhere. Unblocks shuffle/sample/test-data/tokens/sims/
  games. Small: OS entropy → seedable PRNG.
- **fs mutations** — `std.fs` is read-only (`exists`/`is_dir`/`is_file`/`list_dir`/`size`/`glob` +
  `io.read_file`/`write_file`). Missing `mkdir`/`remove`/`rename`/`copy`/`append`.
- **Encoding / crypto** — no base64, hex, sha/md5, uuid, url-encode.
- **`std.process` polish** — only `cmd(line)` via `sh -c` (injection-prone), `Ok(stdout)`/`Err(stderr)`,
  stdout discarded on failure. Want a structured result (both streams + exit code) + an args-array form.
- **`std.request` nit** — verbs + custom headers done; remaining: per-call timeout override + query
  (`?k=v`) builder (timeouts hardcoded 10s/30s/30s).

**Writable as pure-Chezzi `std/*.chz` now (good dogfood / bootstrap probes):**
- **path ops** (`join`/`basename`/`dirname`/`ext`/`normalize`), **`argparse`** (raw `os.args` only),
  **CSV**, **duration / date decomposition** (timestamps only — no y/m/d/parse/duration math),
  **data structures** (heap/priority-queue, deque, counter, ordered map).

**Language-level:** `i64`-only (no `byte`/`u8` scalar). Both byte-sequence types (Python model, not
scalars) are **SHIPPED**: the immutable **`bytes`** (`b"..."` literal w/ `\xHH`, `b[i]`→int,
`b[a:b:c]`→bytes, `for`→int, `len`, `==`/`!=`, `Hashable`, `b'...'` repr) AND the mutable
**`bytearray`** (constructor-only — `bytearray()`/`(N)`/`(b)`/`([ints])`, no literal; `ba[i]`→int,
`ba[i]=x` IndexSet, slice→bytearray, `for`→int, `len`, `push`/`pop`/`extend`, `==` incl. cross-type
content-equality, `bytearray(b'...')` repr; NOT `Hashable` — mutable, like `list`; deep-copied across
the airlock). Conversion bridge: `bytes(ba)` / `bytearray(b)`. **str ↔ bytes (UTF-8) — SHIPPED:**
`str.encode() -> bytes`, `bytes.decode()`/`bytearray.decode() -> str` (UTF-8 only; invalid UTF-8 is a
recoverable fault). **`list()`/`set()`/`map()` constructors — SHIPPED:** `list(it)`/`set(it)` over any
for-iterable, `map(it)` from 2-tuples (element types via the for-loop iterable union). **Formal
`Iterable[T]` protocol + `.iter()` — SHIPPED:** every collection (`list`/`set`/`map`→keys/`str`→char/
`bytes`/`bytearray`→int) exposes `.iter()`, returning a COMPOSABLE cursor (`Obj::Iter` / interp
`Value::Iter` — a `Vec<Value>` snapshot + a `pos`, typed as the existing `Iterator[T]` existential,
GC-NON-LEAF so its snapshot stays alive) with `.next() -> Option[T]` (Some… then idempotent None). So a
plain `list` now composes into the same Take/Mapped adapter pipeline as a hand-written struct iterator
(`examples/iterable.chz`). Every `Iterator` IS `Iterable` (`iter()` returns self — generators + user
`next` structs flow into `[S: Iterable[T]]` bounds); a struct with only `iter(self) -> Iterator[E]`
(no `next`) is for-iterable via a one-time `.iter()`. NON-GOAL (unfixable without move/ownership):
multi-pass/single-pass TYPE SAFETY — `count_twice([list]) == 6` (two independent `.iter()` cursors) but
`count_twice(generator) == 3` (a generator is consumed once). The cursor itself IS sendable — it
crosses the `spawn`/channel airlock as a deep copy, like a `list`. Remaining: no `byte`/`u8` scalar, no
non-UTF-8 codecs (latin1/utf16) and no base64/hex/sha (separate `std.*` gap), no `tuple()`/`bool()`
constructors. `bignum` and `yield`/generators are non-goals.

### Tier 4 — ecosystem (toolchain, not the language)
REPL (huge for scripting iteration), formatter, LSP, package manager / registry, debugger, doc
comments + docgen. (`assert` + test runner shipped M20.)

### ⚙️ Performance + runtime backlog (M19 perf track — detail in [`docs/future.md` §4](docs/future.md) + [`docs/benchmarks.md`](docs/benchmarks.md))

M19 is a **pre-JIT perf track, not a feature freeze** (new features still land via their own
milestone). Every item in *this section* is therefore **behavior-preserving + two-engine parity** —
a perf change must not alter observable behavior (a VM speedup that diverges from the interp is a bug).
Current gap to CPython 3.14: **~1.3×–3.5×** slower (worst on call-bound `fib` 3.54×; `loop` 1.32× is at
the dispatch floor), startup ~11× **faster**. Discipline per item: failing-then-green parity test →
keep parity → measure `benches/run.chz` → record the delta in `docs/benchmarks.md`. **Already landed**
(don't re-flag): peephole/const-fold, superinstructions, global-slotting, `ConstStr` interning,
struct-field IC, FxHash, SSO, method-call IC, inline-hot-ops, adaptive quickening (PEP 659),
map/list-index specialization, **positional closure captures (memory lever #3)** — all in
`docs/benchmarks.md`.

> **Sequencing by JIT-coupling, not perf payoff (added 2026-06-16).** The Cranelift method-JIT
> (end-game, below) hardcodes its codegen against **value representation, memory layout, calling
> convention, opcode set, and the GC invariant**. So rank the remaining items by *lock-in cost*: an
> item the JIT bakes in must land **before** codegen exists, or adding it later forces a JIT rewrite.
> An item that doesn't touch those costs the same before or after. This is orthogonal to a lever's
> bench payoff — several Tier-A items read perf-neutral *today* (the bench is dispatch/call/alloc-bound,
> not layout) yet are still must-do-first because they're cheap now, expensive post-JIT.
> - **Tier A — MUST precede JIT (defer = codegen rewrite):** (1) ✅ **positional struct/enum/closure
>   layout** (the memory levers #1→#3→#2 below — ALL LANDED) — JIT emits **constant field offsets** /
>   numeric variant-id jump tables; the old name-keyed `Box<str>`/`HashMap` layout made that impossible.
>   *Doc-stated JIT groundwork, now in place.* (2) the
>   **GC invariant** — gen/incremental GC needs write barriers the JIT must emit at every store, and
>   even stop-the-world needs safepoint placement baked into codegen; lock the GC contract before
>   codegen even if gen-GC is never built. *(coupling inferred — design pass to confirm.)* (3) any new
>   **`Value`/`Obj` variant** (`bytes` AND `bytearray` — **LANDED**: `Obj::Bytes(Box<[u8]>)` +
>   `Obj::ByteArray(Vec<u8>)` VM / `Value::Bytes(Rc<[u8]>)` + `Value::ByteArray(Rc<RefCell<Vec<u8>>>)`
>   interp / `WireValue::Bytes` + `WireValue::ByteArray`, both GC leaves; `bytes` immutable + hashable,
>   `bytearray` in-place-mutable via the heap slot + NOT hashable + deep-copied across the airlock;
>   **plus the `Iterable[T]` cursor — LANDED**: `Obj::Iter { items: Vec<Value>, pos }` VM /
>   `Value::Iter(Rc<RefCell<IterCursor>>)` interp / `WireValue::Iter` + `SnapValue::Iter`, the first GC
>   **NON-LEAF** addition (`children()` traces `items` so the snapshot's elements stay alive — contrast
>   the bytes/bytearray leaves), sendable by deep-copy like a `list`; all within the 88B `Obj` cap
>   (Module still dominates), so the SSO/Obj-size calculus (#5 below) is unchanged. Codegen enumerates
>   value types for typed fast paths, so these were the pre-JIT must-do — done) — a
>   new scalar added later touches every one. *(inferred.)* (4) **NaN-box** — highest
>   coupling but ⛔ blocked by full i64; pin only as "if the i64 model is ever revisited, it MUST be
>   pre-JIT."
> - **Tier B — JIT-neutral (equal cost before/after):** all **stdlib breadth** (native or pure-Chezzi
>   — same call path regardless), the **string concat/split builder**, and the **ecosystem/tooling**
>   track. (**`ref T` sugar has LANDED** — auto-deref lowering to `Ref[T]` get/set, parser+checker+
>   desugar only, JIT-neutral.) These belong to the feature-freeze phase (which itself gates JIT) but
>   carry **no** JIT-rewrite risk.
> - **Tier C — superseded / non-work:** **register VM** (JIT is built on the bytecode it would rewrite
>   → pre-JIT-or-never, but JIT supersedes it → don't); the mooted/won't-fix concurrency items and the
>   unreachable type-system latent.

- **🟡 Memory layout & access levers** *(diagnosed 2026-06-16, `7e4fc42`; `docs/future.md` §4 "Memory
  layout & access patterns")* — **caveat: bench is dispatch/call/alloc-bound, NOT layout, so these read
  mostly neutral as speedups; their real value is JIT groundwork** (positional layouts → constant
  offsets the JIT codegen needs). Land order **#1 ✅ → #3 ✅ → #2 ✅ — sequence complete**.
  1. ✅ **Shared per-type struct layout** (hidden-class/`__slots__`) — **DONE** (see resolved log). VM
     `Obj::Struct.fields` is now positional `Vec<Value>`; names resolve from `StructDef` on the cold
     path (Display/probe/wire). Kept the single top-level `name` (8 dispatch/display/arith paths).
  2. **✅ Enum `variant_id: u32`** — LANDED (`auto-task/enum-variant-id`). Was `Obj::Enum` holding two
     `Box<str>` per instance (type + variant name, both global). → a single dense `variant_id: u32`
     (the enum analogue of struct `tid`); names resolve from `Program::variants_by_id` on the cold path
     (Display/stringify/error/wire/snap). Match dispatch + equality + `?` are now pure-int compares;
     native `Ok`/`Err`/`Some`/`None` hold the fixed ids `VID_OK..VID_NONE_VARIANT`. **−20% (1.25×) on
     an enum construct+match-dispatch micro**; suite-neutral. `Obj::Enum` shrank 56→32B (Module still
     caps `Obj` at 88B). See resolved log + `docs/benchmarks.md`. **Completes the #1→#3→#2 sequence.**
  3. **✅ Closure captures positional** — LANDED (`auto-task/positional-closure-captures`). Was
     `Obj::Closure.captured: HashMap<String,Value>` (HashMap alloc/inst + string-hash per
     `GetCaptured`). → `captured: Vec<Value>` indexed by a compile-time slot; `Op::GetCaptured(u32)`;
     names live in `Proto.capture_names` (cold path only). **−45% (1.83×) on a closure
     construct+capture-read micro**; suite-neutral (no closure bench). `Obj::Closure` shrank 88→64B
     (Module still caps `Obj` at 88B). See resolved log + `docs/benchmarks.md`.
  4. **GC mark-bit bitvec** — `Slot{obj,mark:bool}` (`heap.rs:234`) interleaves 1 B in 88 B; packed
     bitvec = dense sweep scan. Only if GC becomes hot (post-JIT).
  5. **Shrink `Obj` <88 B** — guard `chzstr.rs:205`; box rare big variants. **Trades against SSO** (sized
     to fill 88 B inline) → measure first.
  6. **HOF borrow-release clone** — `map`/`filter`/`fold` clone the list to release the heap borrow
     before `invoke_value`. A `Vm` split (`&mut ExecState` + `&Heap`) fixes it. Structural refactor.
  7. **`for`-loop snapshot (`ListClone`) + per-char alloc** — parity-blocked by interp snapshot semantics.
  8. **Operand-stack 16 B/Value traffic** → NaN-box (blocked) / register VM (low-ROI) — below.

- **🔵 Big / separate end-game tracks** (only once the language has truly stopped moving):
  - **Cranelift method-JIT** *(`future.md` §4 #6)* — the only path to *match/beat* CPython 3.14 on
    compute; counter-triggered, a whole backend. The stretch end-game. #1/#3/#2 above are its groundwork.
  - **NaN-boxing the `Value` (16 B → 8 B) — ⛔ BLOCKED by full `i64`** (`future.md:264`, `value.rs:18`):
    a full i64 + a type tag don't fit in 8 B (NaN payload ~48–51 bits). Was billed the biggest remaining
    lever; the i64 model rules it out without a tagged-small-int compromise.
  - **Register VM** — low ROI (dispatch already near the match floor).
  - **Generational / incremental GC** — low ROI (GC currently moves no bench).
  - **String concat/`split` builder** — medium lever; `join` already buffers (`mod.rs:4377`), `+`/`split`
    aren't benched yet.

- **✅ `ref T` — transparent reference bindings** — **RESOLVED** (landed). A binding MODIFIER (locals
  + params only) that lowers to the existing `std.ref` `Ref[T]` box, entirely in
  parser→checker→desugar (no new runtime). Two coexisting surfaces:
  - **`ref T`** (`r: ref int = 0`, `fn f(x: ref int)`) — the new modifier. **Auto-deref** (no `^`
    operator): a read `r` lowers to `r.get()`, a write `r = v` to `r.set(v)`, `r += 1` to
    `r.set(r.get()+1)`. Init creates a fresh box `Ref(v)` unless the RHS is already a `ref` binding
    (then it **aliases** the same box). ≈ C++ `int&`. Barred (parse/type error) from return types,
    generic args, collection elements, struct fields, tuple elements, and destructuring lets.
  - **`Ref[T]`** (`r: Ref[T] = Ref(0)`, explicit `.get()/.set()/.update()`) — UNCHANGED, the
    first-class box usable anywhere a type goes. ≈ Rust `Rc<RefCell>`.
  Coercion (type-directed — follows the resolved callee, including a local fn-value, a closure `ref`
  param, and a method resolved by receiver type): `ref→ref` param aliases the box; `ref→T` param
  auto-derefs to a copy; a by-value local or a literal into a `ref` param is a type error. Closure and
  protocol `ref` params are honored identically. Concurrency: a `ref T` is a `Ref[T]`, which is
  **non-sendable** — capturing/passing the box across the spawn/`parallel:`/`Channel` airlock is
  rejected (use `Shared[T]`); deref to a value first to send a copy. Diagnostics about a `ref` binding
  render `ref T`, not the lowered `Ref[T]`. (The old `r^` deref idea is superseded by auto-deref.) See
  `docs/syntax.md` (`ref T` section), examples `ref_binding.chz` / `ref_indirect.chz` / `ref_airlock.chz`.

### 🟠 Deferred — will resolve later (real work, lower urgency)

Tracked in other docs; surfaced here so they aren't lost. None scheduled, but each is genuine backlog.

- **FFI surface expansion** (`std.cffi`, `src/native/cffi.rs`; spec.md §FFI, syntax.md:1232) — v1 is
  **scalars only**. Deferred: structs-by-value, callbacks / function pointers, varargs, opaque pointers /
  **userdata** (`Box<dyn Any>` for opaque `File`/`Regex` handles — io is whole-string today), `char*`
  ownership transfer / `free`. Needed for richer C interop / any future self-host.
- **✅ [RESOLVED] Comprehension nested clauses** — `[x for x in xs for y in ys]` now shipped
  (`auto-task/comprehension-nested-clauses`): 2+ `for` clauses (cartesian/nested, first outermost,
  later clauses see earlier bindings), `if` guards after any clause, across list/set/map. Both engines
  + grammar (`<compClauses>`/`<compGuards>`). `examples/comprehensions_nested.chz`. Also fixed a
  pre-existing interp/VM divergence: a comprehension over a STATEFUL struct iterator now drives
  `next()` lazily on the interp (was eager-drain), so the element/guard see per-step state byte-for-
  byte with the VM (`examples/comprehension_iter_state.chz`).
- **✅ RESOLVED — `std.cancel` tree propagation.** `Token.derive()` (and `cancel.derive(parent)`)
  builds a CHILD token: cancelling/timing-out a parent cancels every transitively-derived child
  (root-to-leaves), while cancelling a child never touches the parent (one-directional). Live link
  (parent `Shared` flag + `Shared` descendant-`done()` registry cross the airlock as live cores, so a
  parent flip is seen by an already-spawned child), tightest-deadline inheritance, nearest-cause-wins
  `reason()`. `done()` cascades transitively too — `derive()` registers a descendant's `done()` into
  EVERY ancestor's registry (atomic `Shared.update()` per insert), so a cancel at any depth above trips
  it directly (a grandchild's `done()` is ready on a grandparent cancel). Pure Chezzi (`std/cancel.chz`)
  → three-engine identical. Goldens `examples/cancel_tree.chz` + `*.expected`. See PROGRESS.md +
  docs/concurrency.md §6e. (Known limit: the per-ancestor registry only grows — no token-drop hook;
  tokens are request-scoped, a future prune-on-cancel could clear it.)
- **Graceful shutdown of accept loops** + a per-connection handler→acceptor signal channel — future work
  for long-running servers (concurrency-tier-d.md:297).
- **Reduction-constant tuning** (D3) — pick `CONTEXT_REDS` + per-op vs per-back-edge accounting
  (concurrency-tier-d.md:363).

### ⚫ May-or-may-not consider (revisit only if it bites; mostly by-design non-goals)

Recorded for completeness — likely stay as-is unless a real program forces the issue.

- **Concurrency, BEAM-flavored:** priority classes; restart/supervision policies (Elixir-style, out of
  scope C5). Narrow cross-nursery M:N limits beyond the coop-flatten above: contended shared channel
  (2+ receivers racing one channel — concurrent-divergent **by design**, never panics/hangs), inline-body
  *blocking* recv (case B — put it in a `spawn:`), eager-nursery cross-wake (concurrency.md §11).
- **`int32` / unsigned C ints** — no such scalar (feature-frozen by design; FFI widens at the boundary).
- **Defaults / named args on built-in methods** (`map`/`push`/`len`/…) — unsupported by design
  (syntax.md:216); user methods + free fns + ctors have them.

---

## 🟢 Verified working (so we don't re-flag)

- **Struct equality** (structural), **string indexing/ops** (`s[i]`, `len/upper/lower/trim/split/join/
  contains/starts_with`, `s.chars()`, `for c in s`), **list-of-structs + nested-list read**, by-ref list
  sharing across calls.
- **`if`/`match` as expressions** (incl. in interpolation), **`Result`/`Option` + `?`**, exhaustive-match,
  deep recursion, **integer `+ - * / %` overflow → recoverable panic** (never wraps; **but shifts DO wrap
  silently** — see the int→float entry above), int-div truncation, `%` on negatives.
- **Fuzz-pass robustness (verified 2026-06-21, both engines):** parser nesting-depth guard ("expression
  nested too deeply" — no stack overflow on 5000-deep parens / blocks), structural-depth guard on
  print/`==` ("maximum structural depth (10000)" — no infinite loop on a self-referential struct cycle),
  call-depth cap (10000, no native stack overflow), index/slice OOB faults + Python-asymmetric clamp,
  `slice step 0` fault, generator idempotent-`None` after drain, per-iteration closure capture of a loop
  var, deep-copy `spawn`/`parallel:` airlock (mutations don't leak back), default-arg expressions fully
  checked (undefined name / type mismatch caught — NOT a bypass like interpolation), generic bound
  enforcement, char-indexed multibyte `str` (`len`/index/slice by codepoint), unicode identifiers,
  empty file, `bytearray` byte-range fault, `assert` fault, missing-module + missing-stdlib resolve error.
- **All std modules on both engines**, **recursive/self-referential structs** (BST, list) GC-clean,
  **mutable `self` across methods**, **nested-list DP**, **empty-map `K,V` inference from later use**.
- **User-struct iterator protocol** (`next() -> Option[T]`, lazy, early-`break`-safe) + **`Iterator[T]`
  parameterized bound** + **user parameterized protocols** (`protocol Container[T]`); lazy adapter structs
  compose without `yield`. `examples/iterator_bound.chz`, `iter_adapters.chz`.
- **Python-colon slicing** `xs[a:b:c]` — open bounds / step / reverse / negative index, on `list`/`str`,
  as assignment target. **Comprehensions** (list/set/dict + guard). **Tuple-destructuring `for`** +
  `enumerate`/`zip`. **Optional chaining `x?.f` + `??`**.
- **✅ RESOLVED — declared-non-void fn must return on every path** (Option B; `checker/mod.rs`
  `block_terminates`/`block_has_break`). Originally mis-sketched as a "bare fn name not callable /
  dispatch bug"; the true root cause was a **missing-return check**: a fn body is statements, so an inline
  `fn a() -> int: 10` parses `10` as a discarded expr-statement and silently falls off the end to `nil`
  (`compiler/mod.rs` emits Nil+Return on fall-off) — dispatch was always correct (works via `.map`/HOFs).
  The checker now rejects a fn with a *declared* non-void return type whose body can fall off the end
  without a value `return` (sound, path-aware: if/else-all-return, exhaustive-match-all-return,
  `while true`-no-break, `exit` tail all terminate). Closures (`fn() -> T: <expr>`) and — since the
  2026-06-17 follow-up (Option A inline-only) — **inline-expr fn bodies** (`fn a(): <expr>` /
  `fn a() -> int: <expr>`) are exempt: their single bare expression is an implicit return. Only a
  multiline body whose declared non-void return can fall off the end is rejected.
  `examples/edge_cases.chz` rewritten to multiline `return <expr>`; two-engine golden byte-identical.
  `docs/syntax.md` §5. (See the bare-fn entry above for the inline-return + nil-in-value-position follow-up.)

---

## Resolved log (one line each — full detail in `PROGRESS.md` + cited examples)

**Scripting essentials ✅** · comprehensions (`481514b`), Python-colon slicing (mid-M19, `src/slice.rs`,
`examples/slicing.chz`), `Iterator[T]` protocol (M13) + generators a permanent non-goal, `os.exit(code)`
(`481514b`, `examples/exit.chz`).

**Scripting ergonomics ✅** · list `.concat`/`.extend` + map `.merge`/`.update` (`concat_merge.chz`);
hex/bin/oct literals (`hex.chz`); tuple-`for` + `enumerate`/`zip` (`for_tuple.chz`); optional-chaining
`x?.f` + `??` (`optchain.chz`); `defer` block-scoped (M16→M18) + `defer:` block form (`defer.chz`).

**Type-system + runtime depth ✅** · one-way C-like `int`→`float` implicit widening (2026-06-22) —
real runtime `Op::CoerceFloat`/`coerce_float` at every value-definition sink (typed `let`, fn/method/
closure args incl. an int variable via callee prologue, `-> float` returns, `float` struct fields,
native/extern float params, `float`/all-literal collection literals); anti-lossy `float`→`int` stays a
type error; scoped carve-outs = un-annotated non-literal mixed collection + plain reassign to a float
local + generic `T` (see entry above); two-engine byte-identical parity. non-const default exprs (relaxed; param-ref defaults still out);
calling a `fn`-typed field (`fn_field.chz`); `sort_by_key` (`sort_by_key.chz`); `Ref[T]` mutable box
(`std/ref.chz`, `ref.chz`); runtime stack traces (both engines identical, `stack_trace.chz`); integer
overflow policy (every `i64` overflow recoverable, `overflow.chz`); loop-var reassignment rejected;
`break`/`continue` in a `spawn:`/`defer:` block nested in an outer loop now rejected at check time
(`checker/mod.rs` save-zero-restore `loop_depth` across both block arms, ending a three-way `check`/VM/
interp divergence; 2026-06-16).

**Concurrency ✅** · pending-`spawn` drop on early `parallel:` escape → cancel-and-report (both engines);
VM nursery-leak on `?`/return escape (truncate to frame depth); **Path C** recv-in-native-callback
thread-demotion — #1 false-positive + #3 sleep/socket (`f828ef7`, D-tier complete; #2 same-box is the
WON'T-FIX carve-out above); **`std.cancel`** task cancellation/timeout `Token` + `Channel.trip()` latch
(flat tokens v1; `docs/concurrency.md` §6e); cross-nursery M:N flat scheduler (coop flatten still open above);
`Channel.close()`, per-socket `timeout_ms` (D6c), per-connection `spawn` all landed.

**Round 1 (#1–9) ✅** · index/field assignment, HOF params, list methods (`map`/`filter`/`fold`),
`map` type, literal/wildcard `match`, `break`/`continue`, tuples + destructuring, strict compound assign.

**Round 2 (#10–15) ✅** · `ord`/`chr`, `sort_by`, int `abs`/`min`/`max` (→ `std.cmp`), bitwise ops,
map iteration, nested/tuple match patterns.

**Memory-layout levers ✅** · **#1 positional struct layout** (hidden-class/`__slots__`, 2026-06-16) —
VM `Obj::Struct.fields` is now a flat `Vec<Value>` (declaration-order offsets); field names live only
in `StructDef`, resolved on the cold path (Display/probe-miss/wire/snap). Kills the N per-field
`Box<str>` allocs/instance + the `==`-name-clone. Synthetic native structs (`Match`/`Response`) are
now registered in `Program.structs` so the runtime can resolve their names. Perf: bench-neutral
(dispatch/alloc-bound), but a 4-field struct-construction micro went **827 ms → 510 ms (−38%)**; JIT
groundwork (constant field offsets). Interp left untouched (frozen oracle; parity by declaration order).
`examples/struct_layout.chz`; `docs/benchmarks.md`.

**Milestones ✅** · M7 generics (fns/structs/protocols/enums, multi-bound) · M8 Tier-1 stdlib
(`json`/`time`/`fs`/`process`, `set`) · M9 Tier-2 (`regex`/`request`) · M10 type-depth
(`Hashable`/`Stringable`/operator protocols/type aliases) · M11 robustness (`recover:`, `T!` errors,
iterator protocol, match guards + ranges, default/named args) · M14 generics-depth (method type params,
user parameterized protocols, method defaults) · M15 slicing + `Index`/`IndexSet`/`Slice` protocols ·
**M20 in-language tests** (`assert`, `test fn`, `chezzi test` w/ suites/fixtures, `examples/assert.chz`).
**M21 nominal `newtype`** (`newtype Name = <type>`, distinct-type wrap; construct/cast-unwrap;
numeric same-type ops; methods/protocols; qualified `mod.NewType(x)` ctor — `31f2f85`/`adc2a9c`,
`examples/newtype.chz`) · **enum methods** (`match self`, generic `T`, `Stringable`/`Add`/`Comparable`
— `008444f`/`d11a353`, `examples/enum_methods.chz`) · **raw string literals** (`r"…"`/`r'…'`/triple,
verbatim, no interp/escapes — `7a645b8`, `examples/raw_string.chz`) · **`extern "lib"` header** +
raw strings in match patterns (`15e7818`) · **module-scoped user types** (struct/enum/alias qualified
access) + **module-qualified match patterns** (`geo.Color.Red` — `e269f16`/`01ddd6f`) · **CLI:**
`chezzi init` scaffolder + `chezzi docs` reference dump + `module:function` manifest entrypoint +
`--interp` flag dropped (`7a8cc2e`/`f92bcc4`/`b0fe92c`).
**std.math fill** (`5a25a5c`: trig/exp/log + `pi`/`e`) · **std.request verbs** (put/patch/delete/head +
header map). **Tech debt:** parser `MAX_DEPTH` 128→64, dup type-param rejected, nested-`set` equality,
call-site type args, `?`-in-closure return checking.

**Perf — M19 memory layout ✅** · **Closure captures positional (lever #3)** — `Obj::Closure.captured`
`HashMap<String,Value>` → `Vec<Value>` indexed by a compile-time slot; `Op::GetCaptured(String)` →
`GetCaptured(u32)` (hash-free `captured[slot]` hot read); capture names moved to `Proto.capture_names`
(cold path: error fallback + wire/snap). Nested captures map by `CapSrc::Captured(parent_slot)`.
Behavior-preserving + three-engine parity (`examples/closure_capture.chz`); **−45% (1.83×)** on a
closure construct+capture-read micro, suite-neutral; `Obj::Closure` 88→64B (Module still caps `Obj` at
88B). JIT groundwork: positional captures → constant offsets for Cranelift codegen.

**Perf — M19 memory layout ✅** · **Enum `variant_id` (lever #2 — completes #1→#3→#2)** — `Obj::Enum`
held two per-instance `Box<str>` (type + variant name, both global) → a single dense `variant_id: u32`
(the enum analogue of struct `tid`). Match-arm dispatch, equality, and `?` are now pure-int compares
(was variant-name string compares / `ty==ty && variant==variant`); names resolve from a new
`Program::variants_by_id` table on the cold path only (Display/stringify/error/wire/snap). Native
`Ok`/`Err`/`Some`/`None` get the RESERVED fixed ids `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3);
user variants follow at `4..`, so the reserved range is **disjoint** from every user id. `?`/top-level-error
gate on the constants, and native construction (`alloc_enum`) stamps the constant **directly** (never a
`variants[name]` lookup the user shadows) — so a user enum may reuse a native name (`enum Foo: Some(int)`,
allowed) without a genuine native Option/Result being stamped with the user's id. (Parity bug fixed
2026-06-16: the first cut name-resolved native construction, collapsing native-vs-user `==` and breaking
`?` under shadowing.) `Op::NewEnum` / `Op::MatchArm` carry the compile-time id. Wire/snap carry the dense
`variant_id` **directly** (shared `Arc<Program>` ⇒ meaningful both sides; preserves identity under
shadowing). Behavior-preserving + three-engine parity (`examples/enum_layout.chz`, incl. a shadowing
section); **−20% (1.25×)** on an enum construct+match-dispatch micro, suite-neutral; `Obj::Enum` 56→32B
(Module still caps `Obj` at 88B). JIT groundwork: numeric variant id → constant / jump-table dispatch for
Cranelift codegen + match-on-enum.
