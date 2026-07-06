# Chezzi — Future Directions (brainstorm, NOT scheduled)

> **Status:** speculative design notes. Forward-looking and opinionated. Nothing here is committed
> work — [`PROGRESS.md`](../PROGRESS.md) is the source of truth for what's actually scheduled and done.
> This doc captures *what would make Chezzi an effective scripting language* and *how to make it
> faster*, with verdicts and rough implementation shape. Most of §1–§3 has since **shipped** (noted
> inline); §4 (optimizations) is the live M19 backlog.

The language **core** is broadly implemented and still evolving (scalars, `list`/`map`/`set`/`tuple`, generic structs +
enums, `Result`/`Option` + `?`, generics + structural protocols, exhaustive `match`, closures/HOF,
modules, GC, interpolation, pipe, panic recovery via `recover:`, the `Iterator[T]`
protocol bound). What follows is the gap between "core implemented" and "language you reach for to write
real scripts."

---

> **Promotion status:** §1 (`defer`) and the §3 scripting features have **shipped** (M15–M18); §2
> (concurrency) has **shipped through Tier-D**. They stay documented here for the design rationale.
> §4 (optimizations) is the live M19 backlog. See [`PROGRESS.md`](../PROGRESS.md) for landing detail.

## 1. `defer` (cleanup on scope exit) — ✅ **SHIPPED (M17)**, **block-scoped since M18** — see `examples/defer.chz`

> **M18 update:** shipped frame-scoped in M17, then moved to **block/lexical scope** — a `defer` runs
> when its enclosing indented block exits (loop body, branch, `recover:`, `match` arm, function body,
> module top level), not just at function return. Realises the "cleanup on scope exit" intent below
> more literally. See the M18 entry in `PROGRESS.md` and the `defer` section of `docs/syntax.md`.

Before M11 this was weak: no panic meant nothing to clean up after. **Now there is unwinding** —
the `recover:` boundary, `?` propagation, and runtime faults all unwind. So `defer` earns its keep
by running on **all three exit paths**: normal return, `?` short-circuit, and panic unwind. That is
exactly Go's value proposition.

**Implementation shape**
- Per-frame deferred-call stack, drained LIFO on *every* frame exit including unwind.
  - VM: drain at `Return` **and** inside the handler-stack unwind (`PushHandler`/`PopHandler` already exist).
- **Arg-evaluation timing:** evaluate `defer` arguments *at the `defer` statement* (Go semantics),
  not at exit. Less surprising; the deferred call closes over already-evaluated values.

**Alternative considered:** Python-style `with` (context-manager protocol `enter`/`exit`). More
Python-feel, but needs a new protocol + an indentation block. `defer` is simpler, adds no protocol,
and composes cleanly with `recover:`. **Recommend `defer`.**

---

## 2. Concurrency + parallelism — the shared-nothing (BEAM) model

> **Shipped through Tier-D.** The full design — `spawn`/`parallel:` nursery, `Channel[T]`,
> `Shared[T]`, sendability — lives in its own canonical doc **[`docs/concurrency.md`](concurrency.md)**,
> with phase history in [`docs/concurrency-tier-d.md`](concurrency-tier-d.md). Real OS-thread M:N
> engine via `--parallel`. **M-C implicit nurseries shipped** — bare `spawn` legal anywhere; every
> function body / module top level joins its tasks at `return`/end. Concurrency is feature-complete.

---

## 3. Missing features (ranked by leverage for scripting) → **mostly shipped (M12–M18)**

> Comprehensions, slicing, the iterator protocol, concat/merge, hex/bin/oct literals, optional
> chaining, tuple-destructuring `for`, match guards, `std.os.exit`, and runtime stack traces have all
> landed. Mutable closure capture was **resolved by decision** (kept snapshot-by-value by design, with
> `std.ref` `Ref[T]` as the idiomatic mutable box). Nothing in this list is still open — see
> [`PROGRESS.md`](../PROGRESS.md).

1. **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set). A Python-feel language without
   these feels broken. Pure parse-time desugar to loop + push. Cheap, large UX win.
2. **Slicing — DONE, and since upgraded to Python colon syntax.** Originally shipped as Rust-style
   `xs[1..3]` (half-open, bounds-clamped, reusing `..`). Mid-M19 (owner-requested language change) the
   subscript-slice form moved to **Python `xs[a:b:c]`** with the full surface: open bounds, step, reverse
   `[::-1]`, and negative indexing (plain index faults out of range, slice bounds clamp — Python's
   asymmetry). `ExprKind::Slice { obj, start, end, step }` (each `Option`); one shared resolver in
   `src/slice.rs` drives both engines. The `..` operator stays the for-loop/match range. See
   [`PROGRESS.md`](../PROGRESS.md) "Slice syntax → Python colon".
3. ~~**Iterator protocol + generators (`yield`)**~~ — **iterator DONE; generators removed.** The
   `Iterator[T]` parameterized protocol shipped (M13): user structs usable in `for`, generic
   `[S: Iterator[T], T]` bounds, and lazy `map`/`filter`/`take` written as **adapter structs** over it
   (Rust `std::iter` model — `examples/iter_adapters.chz`). **`yield`/generators have since landed as
   a complete, VM-only feature** (was a permanent non-goal): a `fn` declaring `-> Iterator[T]`
   may `yield`; the call returns a suspendable generator (one-shot cooperative coroutine, own private
   frame/stack swapped into the VM, resumed by an intrinsic `.next()`). Generators run on **both** VM
   engines (serial `--serial` and default M:N); the only caveat is runtime, not a parity waiver — a
   **live** generator holds a VM frame and so is not sendable across a task airlock on the M:N engine
   (passing one over a channel/`spawn` faults gracefully). The adapter-struct pattern stays the default for lazy streaming.
4. ~~**List concat + map merge**~~ — **DONE.** Method-based: list `.concat`/`.extend`, map
   `.merge`/`.update` (concat/merge new, extend/update mutate). No new syntax; spread/unpack stays
   dropped. `examples/concat_merge.chz`.
5. ~~**Hex / binary / octal literals**~~ — **DONE.** `0xFF`/`0b1010`/`0o17`, lexer-only via
   `i64::from_str_radix`, `_` between digits. `examples/hex.chz`.
6. ~~**Optional chaining + null-coalescing**~~ — **DONE.** `x?.field`/`x?.method()` + right-assoc
   `a ?? b` on `Option`, lowered to a `match` by the desugar pass (zero checker/engine code).
   `examples/optchain.chz`.
7. ~~**Tuple-destructuring `for` (+ `enumerate` / `zip`)**~~ — **DONE.** `for a, b in List[(A,B)]`
   (N-var over `List[tupleN]`); VM splits map vs list-of-tuples at runtime on a new `Op::IsMap`.
   `enumerate`/`zip` shipped as pure-Chezzi `std/iter.chz`. `examples/for_tuple.chz`.
8. ~~**Mutable closure capture**~~ — **RESOLVED by decision.** Capture stays **snapshot-by-value**
   *intentionally* (a bare scalar is copied when a closure closes over it); the idiomatic mutable box
   is `std.ref` `Ref[T]` (a one-field struct, shared by reference, mutated through a method). Documented
   loudly in `std/ref.chz`. So closure counters / accumulators *are* expressible — via `Ref[T]`, not a
   raw captured `int`.
9. **Match guards + range patterns** — `n if n>0:`, `1..10:`. Roadmap. Guards subsume the rest.
10. ~~**`std.os.exit(code)` + real exit codes**~~ — **DONE.** `std.os.exit(code)` is a hard, uncatchable
    halt (unwinds past `recover:`, bypasses `defer`), with the code threaded through both run drivers +
    the CLI; exit-wins precedence holds under `--parallel`. `examples/exit.chz`.
11. ~~**Runtime stack traces**~~ — **DONE.** Error + call chain + line numbers, both engines
    (`37f374a`).

12. **`ref T` — transparent reference bindings (DX sugar over `Ref[T]`) — ✅ LANDED.**
    Motivation was ergonomics: `Ref[T]` does everything but is a **viral wrapper** (`x: Ref[int]`, not
    `int`; `.get()`/`.set()` ceremony). `ref T` is the **binding MODIFIER** that lets a local/param be
    spelled and used as a plain `T` while carrying reference semantics:
    ```chezzi
    fn foo(x: ref int):
        x += 1        # auto-deref write — lowers to x.set(x.get() + 1)
    n: ref int = 0
    foo(n)            # alias the box (the design chose AUTO-DEREF: no `ref n` / `^` operator)
    print(n)          # 1
    ```
    **Shipped as Version A: pure desugar to a heap `Ref` cell + auto-deref.** Reads lower to `.get()`,
    writes to `.set()`, init to a fresh `Ref(v)` (or an alias when the RHS is already a `ref` binding).
    **No new VM op, no engine change, parity by construction** (the same desugar path optional-chaining /
    `??` took — all lowering lives in `src/desugar/mod.rs`, run inside `resolver::build_graph`, which
    both engines + the checker consume). Because the cell is heap/GC'd it is escape-safe: an inner fn /
    closure that captures a `ref` and outlives the frame shares the box (no dangle). **Design choices as
    built:** (a) **AUTO-DEREF, no call-site `ref` marker and no `^` deref operator** — the read/write
    site is inferred from the param/binding ref-ness (the earlier "explicit `ref` at the call site" /
    `r^` notes are superseded); (b) **`ref` restricted to locals + params only** — the parser bars it
    from return types, generic args, collection elements, struct fields, tuple elements, and
    destructuring bindings (those keep first-class `Ref[T]`); (c) coercion table + the non-sendable airlock
    boundary (a `ref`/`Ref` box can't cross a task — use `Shared[T]`). See `docs/syntax.md` (`ref T`),
    `gaps.md`, and examples `ref_binding.chz` / `ref_airlock.chz`.

13. **Static / associated protocol requirements (typeclass-style `T.default()`) — ⏸️ SHELVED
    (attempted twice 2026-06-24, both rejected; not worth the cost at the current model).**
    The goal: a protocol may declare a *static* (no-`self`) requirement, and a generic bounded by it can
    **construct** through the type param — the one thing instance-only protocols can't express:
    ```chezzi
    protocol Default:
        fn default() -> Self
    fn make[T: Default]() -> T:
        return T.default()      # T erased at runtime — needs dictionary passing
    ```
    **Direction (if ever revived): dictionary passing, NOT monomorphization** — Chezzi is a type-erased
    bytecode VM, so `T` has nothing to dispatch on; thread the conforming type's static-method
    dictionary in as a hidden trailing call arg (kept the one erased body + two-engine parity).
    **Why shelved:** two full auto-task runs both **rejected** with 5 criticals *each*, all the same
    class — the **checker's "accept" boundary keeps drifting out of lockstep with the compiler's
    "can-lower" boundary**, so every run half-covers the lowering surface and a prosecutor finds the next
    axis (cross-module call, `spawn:`/`parallel:` body, first-class value / `defer` (`g := make; g()`),
    inferred-T through a container `xs: List[T]`, non-leading bound param). Each shape either crashes the
    compiler or diverges the two VM engines. Making it sound needs a *complete* lowering contract enforced in one
    checker gate — a real design pass, not another blind run.
    **Current behavior on main (the sharp edge): a no-`self` protocol requirement is DECLARABLE but
    UNUSABLE.** Main does **not** reject the no-`self` rule — `protocol Default: fn default() -> Self`
    hoists fine, a struct's *static* `default()` satisfies the bound `[T: Default]` (an *instance*
    `default(self)` does not — `method 'default' has the wrong signature`), so you can declare and bound
    on it. But you can never **call** it: `T.default()` inside the body fails with `unknown name 'T'`
    (no dict-passing to dispatch the erased `T`). So such a bound is just a **dead marker** today — not
    unsound, only inert until the feature is revived. (Verified 2026-06-25 on main @ 503b6b8.)
    **Why it's low priority anyway:** the workaround already exists and is idiomatic — **pass a factory
    closure** (first-class-fn style instead of typeclass style), works today with zero new machinery:
    ```chezzi
    fn make[T](mk: fn() -> T) -> T: return mk()
    make(fn(): Counter(0))      # same power; dict-passing only buys the `make[Counter]()` sugar over this
    ```
    The two rejected attempts live unmerged as branches `auto-task/protocol-static-req` /
    `…-v2` (main is clean); discardable. Revisit only with a design-first pass + appetite for the sugar.

14. **`cast[T](val: Any) -> Option[T]` — a checked downcast off the `Any` top type — ⏸️ DEFERRED
    (DESIGN ONLY, no code).** The `Any` top type + variadics shipped (see `docs/syntax.md`); `Any`
    lets a value of any type into a universal slot, but there is currently **no way back out** — you can
    hold and display an `Any` but not recover its concrete type. The companion is a **checked downcast**:
    ```chezzi
    cast[int](x)          # -> Option[int]: Some(n) if x is really an int, else None
    match cast[Point](v):
        Some(p): print(p.x)
        None:    print("not a Point")
    ```
    Returning `Option[T]` (not a raw `T`) makes it fit `?` / `match` and keeps it total (no faulting
    downcast). **Why deferred — the runtime ERASES generics, so `cast` can only *honestly* witness what
    a runtime `Value` still carries:**
    - `Value` is `Int`/`Float`/`Bool`/`Nil`/`Obj` (`src/vm/value.rs`) — scalars and `str` witness fine.
    - `Obj::List(Vec<Value>)` (`src/vm/heap.rs`) carries **no element type**; `Obj::Struct{name,…}`
      carries only the **name**. So `cast` can witness a *bare container KIND* (is-it-a-list) and a
      *named struct/enum BY NAME*, but **not** a parameterized target.
    - Therefore `cast[List[int]]`, `cast[Map[str,int]]`, `cast[Box[int]]`, … are **unsound and must be
      REJECTED** at the checker: `List[int]` and `List[str]` are the same runtime shape, and an empty
      list is ambiguous for *any* element type. Only `cast[Scalar]`, `cast[str]`, `cast[List]`-kind, and
      `cast[NamedStructOrEnum]` (by name) are honest.
    Lifting the parameterized-target restriction needs **runtime type tags** on heap objects (element
    types on lists/maps, type args on structs) — its own milestone (also a prerequisite for reflection).
    Record this so a future `cast` implementation starts from the erasure contract, not a surprise.

15. **Type conversion protocol (`Convert[S]`) + scalar fills — 🚧 PARTIALLY LANDED (slices 1+2).** Today
    conversion is a fixed set of builtins (`int`/`float`/`str`/`ord`/`chr`, safe `to_int`/`to_float`)
    plus one-way `int`→`float` widening, and one-way newtype wrap/unwrap. The extensible mechanism is
    the reserved `Convert[S]` protocol (there is still no `as`, no `Into`/`TryFrom`). Full current-state
    inventory in `docs/spec.md` "Type conversions & casting". The intended direction, in leverage order:
    - **`Convert[S]` structural protocol** (the big one) — a type witnesses it with a **static** method
      `fn convert(x: S) -> Self`, witnessed structurally like `Comparable`/`Add` (reuses `satisfies_args`,
      made `is_static`-aware; no nominal trait-impl machinery — fits Chezzi's structural model). **Slices
      1+2 LANDED** (2026-07-07): the protocol is reserved + binds as `[T: Convert[S]]`, sound static-slot
      witnessing (an instance `convert(self,…)` does NOT witness it), and **bound-only** enforcement
      (rejected as a value-annotation type — a value can't invoke a static ctor). **Slice 3 PENDING:**
      calling `T.convert(x)` **through** the bound (still `unknown name 'T'`), which is what enables the
      generic struct↔struct / enum↔enum / newtype ergonomics. A fallible conversion would be
      `convert(x: S) -> Result[Self, E]` — so **no separate `TryFrom`** is needed. **Skip `Into`**
      initially: it needs expected-type threading into the receiver (Chezzi infers bottom-up), which is a
      larger, separate change.
    - **Cheap scalar fills — ✅ LANDED** (additive, low risk, landed independently ahead of the
      `From` protocol): `bool(x)` truthiness cast (int/float/bool/str, never faults on a scalar) +
      the `Result`-returning `s.parse_int() -> Result[int, str]` / `s.parse_float() -> Result[float,
      str]` siblings of the `Option`-returning `to_int`/`to_float`.
    Variance/soundness note: a `from`-based conversion is a value-producing call, not a subtype
    relation — no covariance holes. This is a language feature (own milestone), not a perf lever.

**Ecosystem (Tier 4, separate track):** REPL (huge for scripting iteration), formatter, `assert` +
built-in test runner, LSP.

---

## 4. Optimizations (ranked effort → payoff)

> **Live numbers:** `docs/benchmarks.md` tracks Chezzi vs CPython (reproducible via
> `benches/run.chz`). After the M19 phases (call-flatten + SSO incl.): **~1.3×–3.5× slower than
> CPython**, and a **standing startup win** (~11× faster cold). The gap scales with call density —
> `loop` (no calls) is 1.32×, `fib` (all calls) is 3.54×. The M19 levers below are marked landed;
> the **ranked not-started backlog is "Post-M19 next levers"** further down.

The original M5 baseline was ~4–6.5× over the then-existing (now-removed) tree-walker, near the safe-match-dispatch floor; the current live comparison is vs CPython (see `docs/benchmarks.md`). The two real costs are
**dispatch count** and **name lookup** — with **per-call allocation** a close third on call-heavy code.

**Cheap — do first:**
- ✅ **Peephole + constant folding (compiler)** — *landed M19 Phase 1* (`src/compiler/peephole.rs`):
  a jump-relocating pass that folds `ConstInt`/`ConstFloat` arith + `Neg`/`Not`, replicating the
  VM's checked semantics (overflow / div-by-zero stay unfolded so the runtime raises the same error).
- ✅ **Superinstructions** — *landed M19 Phase 1*: `BinLocalLocal` / `BinLocalConst` / `IncLocal`
  fuse the hot `GetLocal+GetLocal+BinOp`, `GetLocal+Const+BinOp`, and `i += k` windows (Int fast
  path inlined; non-Int falls back to the exact unfused op). Cut `loop` −36%, `primes` −25%.
  Remaining candidates: `GetLocal+GetField`, fuse compare+`AsBool`, the load-store accumulator.
- ✅ **Global-slotting (inline-cache equivalent for name lookup)** — *landed M19 Phase 2b*: the
  compiler assigns each module global a stable `u32` slot (`ModuleProto.global_slots`) and emits
  `GetGlobalSlot`/`SetGlobalSlot`/`DefineGlobalSlot`; `Obj::Module.globals` (a `HashMap<String,Value>`
  probed by name per read) became `{ slots: Vec<Value>, index }`, so a global read is a `Vec` index,
  no hash. The slot map lives in the shared `Arc<Program>`, so parent and faulted-worker agree on
  slot↔name by construction — removing the slot-order fragility rather than just guarding it.
  **Reality vs prediction:** it moved `fib` −9% (the call-heavy bench resolves its callee per call),
  but *not* `primes`/`loop` — their hot loops read locals, not globals, so the "moves `primes`" guess
  was wrong about where global-read density actually is. Still cheaper on every global read.
- ✅ **Struct-field caching (the other half of name-lookup ICs)** — *landed M19 Phase 4*: static
  slotting (P2b's model) is impossible for fields because the compiler is **type-erased** (it knows the
  field name but not the receiver's struct type at emit time), so `GetField`/`SetField` carry a
  per-call-site IC id into a per-`Vm` `field_ic: Vec<IcCell>` that caches the field index; a hit
  re-verifies `fields[idx].0 == name` and skips the name-probe. The cell holds an index, not a `GcRef`,
  so it touches no GC / snapshot / `swap_ctx` machinery; each access self-verifies, so it stays sound
  under any future polymorphism. **Reality vs prediction:** −13% on a field-access-bound bench
  (`struct`, 3.32×→2.89× CPython), but **~neutral to −3% on a method-bound shallow-field bench** — the
  cold `field_ic` indirection only pays off when field resolution is the actual bottleneck (wider /
  deeper structs), not when method dispatch dominates. **Follow-up — landed M19 Phase 5b (`bbdcb38`),
  measured neutral:** a struct **type-id guard** (stamp a numeric type id on `Obj::Struct`; guard on
  `obj.tid == cell.tid` — a pure-int compare with no name re-verify) replaced P4's name re-verify on a
  hit. It did *not* close the shallow-struct caveat (the cold-IC indirection, not the re-verify, is the
  cost), but was kept: cheaper hot path, VM-only ⇒ parity-clean.
- ✅ **Kill per-call clones in `invoke_value`** — *landed M19 Phase 1*: matches on `&Obj` (no whole-
  `Obj` / closure-`HashMap` clone) and drops the arity-check `name.clone()`. Cut `fib` −17%, `list`
  −22%.
- ✅ **Pass call args as a stack slice (no per-call `Vec`)** — *landed M19 Phase 2*: `do_call`'s
  `Func`/`Closure` fast path runs in place over the args already on the operand stack (`copy_within`
  drops the callee from beneath them), skipping the `split_off` `Vec` alloc + the re-push in
  `push_frame`. Native / non-callable callees keep the `Vec` path (`invoke_native` needs it). Cut
  `fib` −13%.

**Medium:**
- ✅ **Struct type-id guard for the field IC** — *landed M19 Phase 5b, measured **NEUTRAL***. Stamped a
  dense `tid` (layout id) on `Obj::Struct`; the field IC now guards on `obj.tid == cell.tid` (pure-int
  compare) instead of the `fields[idx].0 == name` string re-verify. **But it didn't move the benches**
  (struct 1.02×, method-bound 1.01× — noise): P4 had *already* collapsed the name-probe to a single
  verify-compare, and for short field names that string compare is already cheap, so swapping it for an
  int compare saves nothing measurable. Kept (correct, principled, no regression, future-proofs real
  polymorphic field sites), but **the field-IC lever is spent** — there is no cheaper guard to reach
  for. The "shallow-struct caveat" was a *prediction*, not a measured cost.
- ✅ **Small-string optimization (the real open `str` lever)** — *landed M19 (2026-06-12)*.
  `Obj::Str` now holds a `ChzStr` (`src/vm/chzstr.rs`): strings ≤ `INLINE_CAP` (22 UTF-8 bytes) are
  stored **inline** in the variant (no per-value `Box<str>` heap alloc), longer ones spill to a
  `Box<str>`. `Deref<str>` + `From` impls kept the ~100 `Obj::Str` sites compiling unchanged;
  `size_of::<Obj>()` stayed 88 B (pinned by a guard test). **`str` 217→174 ms, 2.62×→2.10× CPython
  (−20%)**; `list`/`loop`/`fib` neutral. **Note:** "concat / `split` / `+` builder/rope" is *not* a
  benched lever — the `str` bench is `BuildStr` + `,".join`, and `join` already buffers into one
  `String` (`mod.rs:4377`); `+`/`split` aren't exercised. A builder/rope only helps un-benched
  `s = s + x` loops. The pure-int `list` bench is at the **snapshot-parity floor** (both engines
  clone the iterand at `for`; ints are already unboxed) — no safe alloc lever there.
- ✅ **Faster `usize`/`u64` hasher** — *landed M19 Phase 5a*: `MapData`/`SetData`'s `index` (keyed by
  the cached content hash) and `str_intern` (pointer-keyed) swapped SipHash for an in-tree FxHash
  (`src/vm/fxhash.rs`, no dep). **`map` −7%** (3.04×→2.82× CPython; maps were unbenched, so a `map`
  bench was added). **Gotcha:** a naive multiply-only FxHash was **100× slower** — int keys store
  `f64::to_bits` (zero low bits), and FxHash mixes entropy only upward, collapsing hashbrown's low-bit
  bucket index; a splitmix64 finalizer in `finish()` fixed it. (The field/global IC paths don't use a
  HashMap — they're `Vec`-indexed already — so the lever reduced to the map/set + intern paths.)
- ✅ **`ConstStr` interning** — *landed M19 Phase 3*: a per-heap cache keyed by the literal's data
  pointer reuses the already-allocated handle, so a repeated `ConstStr` push is a pointer lookup,
  not a fresh box. (Cross-site compile-time dedup `Op::ConstStr(u32)` is marginal over this.)
- ✅ **Reduce string-op allocations (`stringify`-into-buffer)** — *landed M19 Phase 2*: `stringify`
  appends into a caller-owned buffer, so `BuildStr` reuses one `String` across all interpolation
  parts (cut `str` −5%).
- ✅ **Arithmetic specialization** — *largely shipped via P1 superinstructions*: `BinLocalLocal` /
  `BinLocalConst` / `IncLocal` inline the monomorphic int path for the hot `local op local` /
  `local op const` / `i += k` windows, so the int loops no longer re-dispatch per iteration. A
  general per-op type-guard cache is the only remaining slice, and it overlaps the superinstructions.
- ✗ **Frame pooling** — *low-ROI here*: `CallFrame`'s `deferred` / `defer_markers` are alloc-free
  `Vec::new()` and frames live in a capacity-reusing `Vec`, so there's no per-call frame alloc to pool
  (P2 already killed the per-call args `Vec`).

**Big (separate milestones):**
- ✅ **Flatten the call loop — LANDED (`634c6f5`); the `Arc::clone` warm-up — LANDED (2026-06-12).**
  `Op::Call` now pushes a frame and `continue`s the running `run_until` loop (no per-call Rust
  recursion / per-call `Arc::clone`); `run_proto_in_place` is kept only for native-initiated calls
  (HOFs). The stand-alone warm-up below — **hoist the per-entry `Arc::clone(&self.program)`** to a
  raw `*const Program` borrow (`mod.rs:2095`; sound because `self.program` is immutable + never
  reassigned) — also landed. Post-flatten the remaining entry is per top-level / native-reentry /
  fiber-resume, **not** per call, so it's neutral on the no-HOF standard suite but **1.05× on
  callback-heavy code** (`benches/chz/hof.chz`); see `benchmarks.md`. The original diagnosis follows.
- **Flatten the call loop (diagnosed 2026-06-12 — the top remaining lever for call-bound code).**
  Every Chezzi function call currently **recurses into a fresh Rust `run_until` loop**:
  `Op::Call` → `do_call` → `run_proto_in_place` (`mod.rs:1992`) → `run_until(base_level)`. Two costs
  ride every call as a result: (1) a **native Rust stack frame** per Chezzi call (push + the
  `frames.len() > base_level` bookkeeping + the `paused()`/result re-plumbing on unwind), and (2)
  **`Arc::clone(&self.program)` on every `run_until` entry** (`mod.rs:2115`) — a per-call atomic
  refcount bump+drop that exists purely as borrow-checker tax. fib(30) is ~2.7M calls ⇒ ~2.7M native
  recursions + ~2.7M atomic clones. **This is why `fib` is 3.85× CPython but `loop` is only 1.31×: the
  gap is the *call*, not the dispatch floor** (straight-line code is already near Python-par). `primes`
  (2.50×) is also call-bound and would move. The fix is what CPython 3.11 did for its jump ("zero-cost
  frames"): make the bytecode `Op::Call` **push a frame and `continue` the existing `run_until` loop**,
  and `Op::Return` **pop + push the result and continue** — no Rust recursion, one `Arc::clone` per
  whole `run_until` instead of per call. **Hard part / parity risk:** today pause/park (B1/D3),
  `recover:` unwind, and `defer` all lean on Rust-stack unwinding through the nested `do_call`/`?`
  chain. A flat loop must instead park by leaving `self.frames` intact and breaking the loop (the M:N
  engine already saves/restores frame state via `FiberCtx`, so the machinery exists). **Keep the
  re-entrant `run_proto_in_place` for native-initiated calls** (HOFs — `map`/`filter`/`sort` call
  `invoke_value` per element and need the callback's result *synchronously* mid-native-method); only
  the bytecode `Op::Call` path flattens. The two coexist: HOFs nest a sub-loop when they must, the
  common recursive/bytecode call no longer does. Cheap warm-up that stands alone even without
  flattening: **hoist the per-call `Arc::clone`** (raw-pointer/restructure the program borrow) — a free
  few-percent on every call-bound bench. Blast radius is **VM-only**;
  parity is testable against the existing fib / recover-in-recursion / defer-in-recursion / deep-
  recursion-overflow goldens. Bigger than a Medium item, smaller than the register-VM rewrite below.
- **NaN-boxing the `Value` — BLOCKED by full 64-bit ints (2026-06-12 reality-check).** The goal
  (16 B → 8 B, operand-stack cache density, moves `loop`/`list`/`fib`) is real, but `Value::Int` is a
  **full `i64`** (`src/vm/value.rs:18`). NaN-boxing packs every value into 8 bytes, and a full i64 +
  a type tag do **not** fit in 8 bytes alongside `f64` — the payload of a NaN-box is ~48–51 bits. To
  do it you must **box big ints** (small-int tagging): a branch + a heap alloc on every int outside
  the taggable range, plus a semantics-sensitive overflow path — i.e. *not* behavior-preserving for
  free, and an uncertain net win on the very int-heavy benches it targets. **Lua 5.4 made exactly
  this call** — it stayed at a 16-byte tagged union *because* it added 64-bit ints. Blast radius is
  **VM-only**, but it's still a milestone-sized design spike (box-big-ints scheme + measure), not a clean
  behavior-preserving session. Park until the int model is up for revisiting.
- **Register VM** instead of stack — fewer ops, less stack traffic. Effectively a VM rewrite; only
  if dispatch count is still the wall after superinstructions.
- **Generational / incremental GC** — current is stop-the-world full-heap (`next_gc = 2×live`).
  Generational cuts pause + rescan cost on allocation-heavy scripts.
- **Cranelift AOT/JIT** — already the stretch goal. Near-native, but a whole backend. Only after the
  language stops moving.

### Memory layout & access patterns (cache levers — diagnosed 2026-06-16)

> **Caveat first (measure, don't guess):** the bench bottleneck is **dispatch + calls + a few alloc
> paths, NOT the value/heap layout** — `loop` is at the match-dispatch floor, and the `struct` bench is
> *method-dispatch*-bound (the field IC already serves hot reads; the type-id guard was measured
> **neutral**). So these are **not** reliable standalone bench-movers. But #1 and #3 below double as
> **JIT-prep**: a method-JIT compiles a field/capture read to a **constant offset**, so it needs a
> canonical *positional* layout — landing them is groundwork for the JIT, not (necessarily) a speedup
> on its own.

**Compact aggregate representation — drop per-instance redundancy (the real layout lever):**

1. ✅ **Shared per-type struct layout (hidden-class / `__slots__`) — DONE (2026-06-16).**
   `Obj::Struct { name: Box<str>, tid, fields: Vec<Value> }` (`heap.rs:162`) now stores fields
   **positionally** (a flat `Vec<Value>`, declaration-order offsets) — no per-instance field-name
   strings. Names live only in `StructDef { fields: Vec<String>, tid }` (`op.rs:378`), resolved on the
   cold path (Display/stringify/probe-miss/wire/snap) by `name`→`StructDef`. Killed the N per-field
   `Box<str>` allocs/instance + the `==`-name-clone (`mod.rs` struct-eq is now a by-position value
   compare guarded by `na != nb`). The single top-level `name` is **kept** (consumed by ~8
   dispatch/Display/arith/hash paths — dropping it would need a `tid`→name map everywhere; out of
   scope for the primary win). The synthetic native structs `Match`/`Response` are now registered in
   `Program.structs` (`compiler/mod.rs` `Compiler::new`) so the runtime can recover their field names.
   Perf: bench-neutral (struct bench reuses instances, dispatch/alloc-bound) but a 4-field
   struct-construction micro went **827 ms → 510 ms (−38%)**. Hands the JIT a constant field offset.
   Interp left untouched (frozen oracle; parity by declaration order).
2. **✅ DONE — Enum variant id instead of names.** Was `Obj::Enum { ty: Box<str>, variant: Box<str>,
   payload }` — two `Box<str>` per instance, both global (`Program::variants`). Now `Obj::Enum {
   variant_id: u32, payload }` (the enum analogue of `tid`); the type + variant names resolve from the
   new `Program::variants_by_id` table on the cold path only (Display/stringify/error/wire/snap).
   Match-arm dispatch, equality, and `?` are pure-int compares (was variant-name string compares /
   `ty==ty && variant==variant`). Native `Ok`/`Err`/`Some`/`None` hold the **reserved** fixed ids
   `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3); user variants follow at `4..`, so the
   reserved range is **disjoint** from every user id. `?`/top-level-error gate on the constants, and the
   native construction path (`alloc_enum`) stamps the constant **directly** (never a `variants[name]`
   lookup) — so a user enum may shadow a native name (`enum Foo: Some(int)`) without a genuine native
   Option/Result ever being stamped with the user's id (was a parity bug: name-resolved native
   construction collapsed identity + broke `?`; fixed 2026-06-16). `Op::NewEnum`/`Op::MatchArm` carry the
   compile-time id; wire/snap carry the dense `variant_id` **directly** (shared `Arc<Program>` ⇒
   meaningful both sides; preserves identity under shadowing). **−20% (1.25×)** on an enum
   construct+match-dispatch micro (`benches/chz/enum.chz`),
   suite-neutral; `Obj::Enum` shrank 56→32 B (Module still caps `Obj` at 88 B). Hands the JIT a numeric
   variant id → constant/jump-table dispatch. See `docs/benchmarks.md`.
3. **✅ DONE — Closure captures: positional `Vec`, not a per-closure `HashMap`.** Was `Obj::Closure
   { captured: HashMap<String, Value>, .. }` (a `HashMap` ~48 B + string keys **per closure** + a
   string hash on every `GetCaptured`). Now `captured: Vec<Value>` indexed by a compile-time slot;
   `Op::GetCaptured(u32)` is a hash-free `captured[slot]` read; capture names live in
   `Proto.capture_names` (cold path: error fallback + wire/snap name carrying). Nested captures map by
   `CapSrc::Captured(parent_slot)`. **−45% (1.83×)** on a closure construct+capture-read micro
   (`benches/chz/closure.chz`), suite-neutral; `Obj::Closure` shrank 88→64 B (Module still caps `Obj`
   at 88 B). Hands the JIT a constant capture offset. See `docs/benchmarks.md`.

**Heap-slot layout (GC-side; principled, low priority — GC moves no bench):**

4. **Separate the mark bit from the object.** `Slot { obj: Option<Obj>, mark: bool }` (`heap.rs:234`)
   interleaves a 1-byte mark with the 88-byte `Obj`, so the sweep walks 88 B+ slots to read 1 bit and
   scans the whole `slots` Vec even on a sparse heap. A packed mark **bitvec** (1 bit/obj, 64/word) makes
   the sweep a dense sequential bitscan. Only worth it if GC becomes hot (generational/incremental
   territory — already #8, low-ROI).
5. **Shrink `Obj` below 88 B.** `size_of::<Obj>()==88` (guard, `chzstr.rs:205`), forced by the largest
   variants (Module ~80 B). Boxing the rare big ones densifies the heap — **but SSO deliberately sized
   strings to fill 88 B inline**, so shrinking un-inlines them. Net is a trade-off → measure first.

**Hot-loop access (mostly already addressed or parity-blocked — cross-ref):**

6. **HOF borrow-release clone (new finding).** List `map`/`filter`/`fold` do `self.heap.get(h).clone()`
   to release the heap borrow before `invoke_value(&mut self, …)` — an N×16 B copy per HOF call. A `Vm`
   split (`&mut ExecState` + `&Heap`) lets the borrow coexist. Structural refactor, not a one-session lever.
7. **`for`-loop snapshot (`ListClone`) + per-char alloc** — mandated by the for-loop's observable
   snapshot semantics (identical on both engines); `alloc_char` (Phase 3) already halved the string case. Behavior-blocked.
8. **Operand-stack 16 B/Value traffic** → NaN-box (blocked, full i64) / register VM (#8, low-ROI) — above.

**Land order:** **#1 ✅ → #3 ✅ → #2 ✅ — sequence complete** as **JIT groundwork** (the positional
layouts the JIT codegen wants), each measured against `struct`/`hof`/`enum` (read suite-neutral — they're
dispatch-bound, see caveat — with strong micro deltas: #1 −38%, #3 −45%, #2 −20%).
#4/#5/#6 are principled cleanups, post-JIT. Same discipline throughout: failing-then-green parity test →
keep two-engine parity → measure (`benches/run.chz`) → record the delta in `docs/benchmarks.md`.

**Highest payoff-per-effort (original M19 batch, all landed):** superinstructions + inline caching +
peephole/const-fold. They attacked dispatch count and name lookup — the two actual costs — without
touching the value model or the GC.

### Post-M19 next levers (ranked — diagnosed 2026-06-12; **status updated 2026-06-13**)

> **Status (2026-06-13):** Tier 1 is DONE — #1 method-IC (Phase 6) and #2 inline-hot-ops (Phase 7)
> landed; #3 `Op::Call` spec was analyzed and **deferred (no-gain after the Phase 7 inline)**. Tier 2 is
> underway — #4 adaptive quickening **v1 (binops) landed**, #5 **index specialization landed**
> (`GetIndex`/`SetIndex` Int-key fast path), and the #4 *CallMethod* extension **landed (2026-06-13):
> N-way polymorphic method-call IC + sticky-deopt + clone-free megamorphic slow path, `poly_method`
> −33% (6.0× → 4.28× CPython)** — this unifies the field/method caches under one adaptive form.
> **Genuinely remaining:** the **denser int-keyed `map`** representation **also landed (2026-06-13,
> `map` 2.68× → 1.94× CPython, −26% on merged HEAD)**, so what's left is the Tier-3 milestones (#6 JIT / #7 NaN-box (blocked) / #8 register
> VM). Per-lever tags below; landed details + measured deltas in `PROGRESS.md` "Current focus" and
> `docs/benchmarks.md`.

The M19 cheap batch + call-flatten + SSO are spent. Latest gap tracks **call density**: `loop` (no
calls) **1.32×**, `primes` 2.53×, `str` 2.65×, `struct` 2.71×, `map` 2.83×, `list` 2.97×, `fib` (all
calls) **3.54×**; startup **0.094× (11× win)**. The bottleneck is **call overhead + per-op dispatch +
a few alloc paths — NOT the value model or the GC** (confirmed: `loop` is already at the match-dispatch
floor; ints are unboxed; GC is share-nothing per-thread and moves no bench). Target is **CPython 3.14**
(specializing adaptive interpreter + optional copy-and-patch JIT), so the interpreter can *narrow* the
gap but a JIT is the only path to *match/beat* it on tight compute.

**Tier 1 — interpreter, cheap→medium, behavior-preserving, each hits a measured bench:**

1. **✅ DONE (Phase 6, 2026-06-13).** **Method-call IC + flatten `do_method_call`** *(hit `struct` −9%)*. `do_method_call`
   (`mod.rs:~3868`) still string-looks-up `def.methods.get(method)` per call **and** still recurses into
   a fresh `run_until` — call-flatten only covered `do_call`'s plain-fn fast path (see its own follow-up
   note). Add a per-call-site monomorphic cache (`tid → proto`, the same shape as the landed `field_ic`)
   and push the method frame in place. Symmetric to the field IC; reuses that machinery.
2. **✅ DONE (Phase 7, 2026-06-13) — landed as "inline hot ops"** *(moved every op-bound bench: `loop` −15%, `list` −17%, `primes` −8%, `fib` −6%)*. The inline-the-hottest-ops sub-lever shipped; the other two below (lazy `span`, serial/MN loop split) were left **unshipped** (predictably-false cheap branches, low payoff vs the inline win — revisit only if a profile shows them). **Trim per-op overhead in `run_until`** — three things run
   **every instruction** that are pure overhead on the serial (default, benchmarked) engine:
   - `span = proto_ref.lines[ip]` (`mod.rs:2157`) is loaded every op but used **only on fault** → pass
     `(pid, ip)` to the error path and reconstruct the span lazily there.
   - the `if self.mn.is_some()` reduction-count branch (`mod.rs:2137`) + the cancel check (`mod.rs:2122`)
     are MN-only → split a lean serial loop body from the MN body (or hoist them off the serial back-edge).
   - `self.step(op, span)` is a **separate fn call per opcode** → inline the ~6 hottest ops (GetLocal, the
     superinstrs, Jump, Call, Return) directly in the loop, delegate the long tail to `step`.
3. **⏸️ DEFERRED — no-gain (Phase 8 analysis, 2026-06-13).** **Call-site specialization for `Op::Call`** *(was aimed at `fib`)*. After the Phase 7 inline, `do_call`'s happy path is already lean (the deref a call-IC skips is ~2–3 instrs); fib's residual is frame-setup in `finish_frame`, which a dispatch cache doesn't touch — and a correct call-IC can't avoid a heap-specific callee handle ⇒ `swap_ctx` hazard for ~0 gain. fib's real lever is #4/#6. Each call re-checks Func/Closure/
   Native, derefs the heap callee, and re-validates arity (CPython `CALL_PY_EXACT_ARGS`); full rationale in `docs/benchmarks.md`.

**Tier 2 — structural, medium→large:**

4. **✅ v1 + CallMethod extension LANDED (2026-06-13).** **Adaptive opcode quickening (PEP 659)** *(the single most CPython-like lever)*. v1 specializes the un-fused generic binop arms (`Add..GtEq`, `Eq`/`NotEq`) to an int/int fast path behind a per-`Vm`, per-site `(proto,ip)` deopt guard (side table `quicken`/`quicken_base`, mirrors `field_ic`/`method_ic` — no `Op`/compiler/interp change ⇒ parity by construction). Measured **`primes` −7–8%**. **CallMethod extension (done):** the method-call IC's single `MethodIcCell` is widened to an N-way (4-way) `MethodIcSite` carrying the *same* one-way sticky-deopt discipline — a bounded-megamorphic site (≤4 receiver types) HITS a way per type and flattens; a 5th distinct type latches `sticky` and goes slow (now clone-free: borrows `Arc<Program>.structs` instead of cloning the whole `StructDef`). This **unifies** the field+method caches under one adaptive form (`GetIndex` is already covered by #5). Measured **`poly_method` −33% (6.0× → 4.28× CPython)** on a new megamorphic bench; side table still int-only (no `GcRef`) ⇒ parity by construction. After an op runs once, rewrite-in-place to a type-specialized form behind a deopt guard. **Constraint
   (same one P2b/P4 hit):** bytecode is shared `Arc<Program>` read-only across `--parallel` workers, so
   quickened cells must live in a per-`Vm` side table keyed by site, not mutate the `Op`.
5. **✅ DONE (2026-06-12).** **map/list index specialization** *(`list` −4%; `map` neutral — it's FxHashMap-probe-bound, not dispatch-bound, so the predicted `map` win needs a **denser int-keyed map** representation, a separate lever, not this tweak)*. `GetIndex`/`SetIndex`
   got an Int-key fast path (skips `hash_key_rooted` rooting) + inline dispatch in the `run_until` hot arm; 7 `idxspec_*` parity guards.

**Tier 3 — big, separate milestones:**

6. **Cranelift method-JIT** — the only path to *match/beat* CPython 3.14 on compute. Counter-triggered,
   JIT the hot protos (Python's tier-2 model). End-game; only once the language is fully frozen. #4 is the
   lower-risk stepping stone toward it.
7. **NaN-boxing — stays BLOCKED** (full i64; see the dedicated note above).
8. **Register VM / generational+incremental GC — low ROI** (dispatch is already near the match floor; GC
   moves no bench). Deprioritized; revisit only if a real workload proves otherwise.

**Sequencing (updated 2026-06-13):** Tier 1 is **done** (#1, #2 landed; #3 deferred), and Tier 2 is
**done** — #4 (v1 binops **and** the `CallMethod` N-way extension) + #5 (index spec **and** the denser
int-keyed `map`) all landed. With both the `CallMethod` adaptive quickening and the denser `map`
shipped, the high-ceiling play left is **#6 (Cranelift method-JIT)** as the JIT end-game (#7 NaN-box stays
blocked; #8 register VM / gen-GC stays low-ROI). All steps: behavior-preserving, two-engine-parity-clean,
measure-first, each targeting a named bench.

**M19 Phase 1 done (2026-06-11):** peephole/const-fold + superinstructions + `invoke_value` clone
kill — all behavior-preserving (1516 tests + full two-engine parity green). Results in
`docs/benchmarks.md`.

**M19 Phase 2 done (2026-06-11):** in-place call args in `do_call` (per-call `Vec` gone, `fib` −13%)
+ `stringify`-into-buffer for `BuildStr` (`str` −5%) — both behavior-preserving (1518 tests + full
two-engine parity green, 4-agent S++ panel clean). Results in `docs/benchmarks.md`. Remaining `str`
lever is `ConstStr` interning; the next dispatch win is inline caching (Phase 2b, below).

### M19 Phase 2b — inline caching via global-slotting (✅ landed 2026-06-11)

Landed as designed below. Net: `fib` −9%; other microbenches flat (their hot loops are local-bound).
Implementation notes vs the plan: `Module` became `{ slots: Vec<Value>, index: HashMap<Box<str>,u32> }`
(the `index` is kept alongside the `Vec`, not discarded — it backs `module.member`/imports/native
population, and reverse-iterating it in slot order via `module_slot_pairs` is how the snapshot stays
deterministic); the old name-keyed ops were *replaced*, not kept. The historical design note follows.

The next dispatch win, deliberately split out from Phase 2 because it is **not** a local opcode
tweak. Today `Op::GetGlobal(String)` does a `HashMap<String,Value>` probe by name every read
(`mod.rs` `module_global`). A monomorphic IC would cache the resolved location, but two facts block
the naive "cache in the opcode" approach:

1. **Bytecode is shared read-only across threads.** Under `--parallel` the `Program` is an
   `Arc<Program>` and every worker fiber reads the *same* `Op` slices — so an opcode cannot carry a
   per-site mutable cache cell without synchronization.
2. **Globals are name-keyed, not slotted.** `Obj::Module { globals: HashMap<String,Value> }` has no
   stable index to cache.

**Plan:** resolve globals to slots at compile time. The compiler assigns each module global a stable
`u32` slot and emits `GetGlobalSlot(u32)` / `SetGlobalSlot(u32)` / `DefineGlobalSlot(u32)`;
`Module.globals` becomes a `Vec<Value>` (name→slot map kept only for `module.member` field reads and
error messages). The read becomes a bounds-checked `Vec` index — no hashing, no string.

**The concurrency constraint (the reason it is its own milestone):** the lazy module-fault path
(`ensure_module_faulted` / `fault_module` / the worker module snapshot) reconstructs a worker's home
module on first access. Slot order must be **identical** between the parent's compiled module and any
faulted worker copy, or a worker reads the wrong global. The snapshot (`to_snap`/`replay_snap`) and
`ModuleInline`/`ModuleAlias` replay must round-trip slots, not names. This needs its own two-engine
parity pass + the `--parallel` module-fault tests, so it is scheduled separately rather than bundled
with the Phase 2 allocation kills.
