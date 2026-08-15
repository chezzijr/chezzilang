# Design: Value structs + escape analysis (memory/perf milestone)

**Status:** proposed, not started. Decision-support doc — read to decide whether this is worth a
dedicated milestone. Extends [`future.md §4`](future.md) ("value-struct representation") and the
memory campaign recorded in [`benchmarks.md`](benchmarks.md).

**One-paragraph summary.** After four merged memory levers (box `Module`, inline `fields`,
mark→bitset, drop `Struct.name`), struct-heavy RSS is down **−55%** (327→149 MB on `many_struct`) and
the Go gap is **2.2×** (was 4.9×). The remaining ~128 MB on that bench is *live struct data*: 2M
structs each occupying its own 64 B GC `Slot`, referenced by a `GcRef` from the container. No further
per-object trimming touches this — it is the cost of the **"every composite is a boxed `GcRef`"**
model. Go stores `[]P{x,y}` inline (2M×16 B, zero per-element objects); that inlining is the whole
gap. Closing it means letting some structs live **unboxed** — on the stack or inline in their
container — which is only sound where the struct's reference identity is never observed. Determining
that is **escape/alias analysis**. So this milestone is one project with two parts: the *analysis*
(escape) and the *representation* it unlocks (unboxed/value storage). It is memory **and** perf (it
also removes the allocation, the GC trace, and the free), but it is **soundness-critical** — a wrong
answer is a use-after-free — and touches the checker, compiler, VM value model, GC, and the airlock.

---

## 1. The problem, measured

`benches/chz/many_struct.chz` builds `List[P]` with 2M `P{x:int, y:int}`. Post-campaign breakdown at
149 MB RSS (Go: 67 MB):

| component | size | note |
|---|---|---|
| `Slot` array | 2M × 64 B ≈ **128 MB** | one boxed `Obj::Struct` per element — the floor |
| outer `List` `Vec<Value>` | 2M × 8 B ≈ 16 MB | `GcRef`s pointing at the slots |
| GC headroom / allocator | remainder | |

Each `P` is a separate heap object addressed by a `GcRef` (`src/vm/value.rs:38`, a slot index) stored
as a `Value` in the list's `Vec<Value>` (`src/vm/heap.rs` `Obj::List(Vec<Value>)`). Go's `[]P` has
**no** per-element object: the 16 B struct sits directly in the slice's backing array.

The four shipped levers all shrank the *per-object* overhead (header, name string, mark bit, field
malloc). They cannot remove the object itself. Only changing the *representation* — struct bytes
inline in the container instead of a `GcRef` to a slot — removes it.

## 2. Why this is an optimization, not a language change

Chezzi structs are **reference types** (verified — `docs/design-value-structs` scratch test, and the
`heap.get_mut` in-place mutation model, `src/vm/heap.rs`):

```
a := P(1); b := a; b.x = 42        # a.x is now 42   (assignment aliases)
fn bump(q: P): q.x = 99            # bump(c) mutates the caller's c
y := xs[0]; y.x = 7                # xs[0].x is now 7 (list holds shared refs)
```

This is load-bearing, Python-like semantics. Therefore an unboxed representation **must be
transparent**: it may be used only where no code can observe that the struct has no independent
identity — i.e. where the value does **not escape** and is **not aliased** in a way that outlives or
shares its storage. Where escape *can* happen, the struct stays boxed exactly as today. The analysis
is what makes the optimization safe; the representation is what makes it pay.

## 3. The two parts

### 3a. Escape analysis (the enabling pass)

A dataflow analysis (new pass, hooks after the checker has types — `src/checker/`, feeding the
compiler `src/compiler/`) that classifies each struct-producing site as **non-escaping** (may be
unboxed) or **escaping/unknown** (must be boxed — the conservative default). A value **escapes** if
any of:

1. **Returned** from the function (outlives the frame).
2. **Stored into a heap object** — pushed to a `List`/`Map`/`Set`, assigned to another struct's
   field, put in a tuple that escapes, sent to a `Channel`, stored in a `Shared`/cell.
3. **Captured by a closure that itself escapes** — capture is *by reference* (`docs/syntax.md:258`),
   so a captured struct's identity leaks to the closure's lifetime.
4. **Sent across the airlock** — `spawn`/`Channel`/`Shared` deep-copy via `WireValue`/`SnapValue`
   (`src/vm/sched.rs`); a value that crosses cannot live in a frame that unwinds.
5. **Aliased out** — bound to another name (`b := a`), or read out of a container and mutated, in a
   way that shares mutation with a longer-lived binding.

Conservative rule: **when unsure, escapes.** A false "escapes" only forgoes an optimization; a false
"doesn't escape" is a use-after-free. This is the same soundness posture as the checker↔compiler
agreement class in `[[checker-superset-of-compiler]]` — the analysis and the codegen must never
disagree about who is boxed.

### 3b. Unboxed representation (what the analysis unlocks)

Two forms, in increasing difficulty:

- **(i) Frame-local / arena allocation** for a struct that is *created and fully consumed within one
  function* and never escapes (a scratch `Point` used for a computation, a struct literal passed to a
  function that doesn't retain it). Store it in a per-frame arena (bump-allocated, dropped wholesale
  on return); the GC never sees it. Buys: no `Slot`, no alloc, no trace, no free. This is mostly a
  **perf** win (kills allocation churn) with a memory side-benefit.
- **(ii) Inline-in-container** for `List[P]` (and `Map`/`Set` values) where the *elements* don't
  escape individually. Store the struct bytes directly in the container's backing store instead of a
  `GcRef` per element. This is the **memory** win that closes the Go gap on `many_struct`. Harder:
  the container needs a typed, fixed-stride inline layout, and indexing (`xs[i]`) must return a
  *view*, not a fresh boxed copy, to keep mutation semantics.

Both need a way to represent "an unboxed struct value." Today a `Value` is an 8 B tagged word
(`src/vm/value.rs`); an unboxed struct is `n` words. Options: a second `Value` kind that points into
an arena/inline-store (a fat handle: base + tid), or keep `Value` 8 B and make containers of a known
element-type carry a separate typed backing (`Vec<u8>` strided by the struct's field layout, with the
`List` knowing its element `tid`). The latter mirrors Go's `[]P` most closely and is the target for
(ii).

## 4. Interactions that make this hard (the soundness surface)

- **GC tracing.** Today `collect()` (`src/vm/exec.rs:1290`) marks from roots and traces `GcRef`
  children. An inline struct in a `List` is not a `GcRef` — the container must trace *its* fields
  (which may themselves be `GcRef`s to boxed sub-objects). `children()` (`src/vm/heap.rs`) grows a
  new case: "trace the inline elements of a typed container." A missed trace = collected-live bug;
  the `*_gc_stress` suite (`src/vm/gc_tests.rs`) is the gate.
- **Airlock.** An unboxed struct that must cross a `spawn`/`Channel` has to be **boxed or copied at
  the crossing** — the frame arena is thread-local and won't survive. The escape analysis already
  forbids unboxing anything that crosses (rule 4), so the crossing paths (`WireValue::Struct`,
  `SnapValue::Struct` — `src/vm/sched.rs:2399/2769/3662/3881`) stay as-is; the analysis just must not
  under-approximate what reaches them. See `[[airlock-sendability-architecture]]`.
- **Mutation through a view.** For (ii), `y := xs[0]; y.x = 7` must still mutate `xs[0]` (proved
  above). So `xs[0]` yields a **view/handle into the inline store**, not a copy. The moment `y`
  escapes (returned, stored, captured), the analysis must force `xs` to a boxed representation — or
  the view dangles. This is the subtlest case and the main reason (ii) is stage-3, not stage-1.
- **Observable behaviour is fixed.** Every change must keep the goldens and `tests/chz` byte-identical
  (`chz_suite_passes` in `tests/chz_suite.rs`, and the `CHEZZI_THREADS=2` re-run). Representation
  choice must not leak into observable output (Display, `==`, iteration order, hash).
- **`match` / destructuring** on an unboxed struct must bind fields without materializing a boxed
  object (or must box on demand transparently).

## 5. Staged rollout

Each stage lands behind failing-then-green correctness tests + full parity + gc-stress, and is
measured against the `many_struct`/`many_map` baseline before proceeding. **Stop at any stage** — each
is a shippable increment.

- **Stage 0 — analysis only, no representation change.** Implement escape/alias classification; emit
  it as metadata; assert nothing changes behavior. Instrument: how many struct sites in the corpus
  classify non-escaping? This *measures the ceiling* before investing in representation. If few sites
  qualify, stop here — the payoff isn't there.
- **Stage 1 — frame-arena for non-escaping local structs (3b-i).** The safest slice: structs created
  and consumed in one frame. Perf win (alloc/GC churn) + modest memory. Small blast radius, no
  container/view complexity.
- **Stage 2 — inline `List[P]` for non-escaping-element lists (3b-ii, read-mostly).** The memory win.
  Start with lists whose elements are never individually aliased-out-and-mutated (the analysis proves
  it) — indexing returns a copy-on-read where safe, falls back to boxed `List` otherwise.
- **Stage 3 — inline with mutable views + `Map`/`Set` values.** The general case; the view/aliasing
  machinery. Highest risk; only if stages 1–2 justify it.

## 6. Risks, gates, kill-criteria

- **Soundness (highest).** A wrong "non-escaping" = UAF/heap corruption, likely intermittent and
  M:N-only. Gate: full `*_gc_stress`, two-engine parity, and a dedicated escape-analysis adversarial
  test corpus (every escape route, each proven to force boxing). Treat like the checker-soundness work
  in `[[soundness-task-execution-pattern]]` — subagent-driven with per-delta adversarial review, **not**
  a single auto-task.
- **Complexity debt.** A second representation means every struct-touching op (field get/set, method
  dispatch, `==`, hash, Display, wire, snap, `match`) must handle both boxed and unboxed, or box-on-
  demand. This is a permanent tax on the VM. Kill-criterion: if Stage 0 shows the analysis rarely
  fires on real code, the tax isn't worth it — stop.
- **Determinism.** Any behaviour that changes with the worker count is a bug, not a win (repo discipline).
- **Effort.** Realistically multi-session: Stage 0 (analysis) is itself a checker-grade pass; Stages
  1–3 each touch the value model + GC. This is a milestone, not a task.

## 7. Alternatives considered

- **NaN-boxing `Value` 16→8 B** — already done (this session's predecessor work; `Value` is 8 B).
- **Compacting/generational GC** — does **not** help: the 128 MB is *live* data, not garbage or
  fragmentation. GC changes buy pause/throughput, not footprint. (See `benchmarks.md` reasoning.)
- **Shrink `Obj` <64 B** (box `MapData`/`SetData`, narrow `Fields` inline 3→2) — a further ~25% off
  the slot array (~32 MB) but with map/set indirection + re-spilled 3-field structs; diminishing and
  perf-touchy. A cheaper partial step, not a substitute — it still keeps one object per struct.
- **OS memory return** (`shrink_to_fit` on sweep, `malloc_trim`/mimalloc) — orthogonal; helps spiky
  programs return peak RSS, doesn't shrink steady-state live data. Cheap; can land independently.

## 8. Recommendation

Do **Stage 0 first, as its own scoped effort**, before committing to the milestone. It is the
honest gate: it measures how much real code would actually benefit, at a fraction of the cost of the
representation work, and it produces the analysis every later stage needs. If Stage 0 shows a healthy
fraction of struct sites are non-escaping, proceed to Stage 1 (perf-flavored, low risk) and reassess
before the memory-flavored Stages 2–3. If it shows little, bank the −55% already achieved plus the
cheap OS-return lever and close the memory track — 2.2× Go on a pathological all-structs benchmark is
a fine place to stop, since real programs mix scalars, strings, and structs and sit closer to parity
(see the gentle single-object benches in `benchmarks.md`, all ≈1.0× Go).
