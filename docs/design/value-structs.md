# Design: Value structs + escape analysis (memory/perf milestone)

**Status: proposal, not started.** Decision support: read it to decide whether this is worth a
milestone. Related: the memory levers in [`roadmap.md` §4](roadmap.md#4-optimizations-open-levers),
the memory campaign in [`benchmarks.md`](../contributing/benchmarks.md), and the explicit-type
alternative [`packed-storage.md`](packed-storage.md).

**Summary.** Four merged memory levers (box `Module`, inline `fields`, mark → bitset, drop
`Struct.name`) cut struct-heavy RSS by 55% (327 → 149 MB on `many_struct`); the Go gap is 2.2× (was
4.9×). The remaining ~128 MB is live struct data: 2M structs, each in its own 64 B GC `Slot`,
referenced by a `GcRef`. Go stores `[]P{x,y}` inline (2M × 16 B, no per-element objects). Closing the
gap means letting some structs live **unboxed** (on the stack or inline in their container). That is
sound only where no code observes the struct's reference identity, and **escape/alias analysis**
decides where. So the milestone has two parts: the analysis and the representation it unlocks. It is
soundness-critical (a wrong answer is a use-after-free) and touches the checker, compiler, VM value
model, GC and the task boundary.

## 1. The problem, measured

`benches/chz/many_struct.chz` builds `List[P]` with 2M `P{x:int, y:int}`. At 149 MB RSS (Go: 67 MB):

| component | size | note |
|---|---|---|
| `Slot` array | 2M × 64 B ≈ **128 MB** | one boxed `Obj::Struct` per element: the floor |
| outer `List` `Vec<Value>` | 2M × 8 B ≈ 16 MB | `GcRef`s to the slots |
| GC headroom / allocator | remainder | |

The shipped levers shrank per-object overhead. They cannot remove the object. Only a representation
change (struct bytes inline in the container) removes it.

## 2. Why this is an optimization, not a language change

Chezzi structs are reference types:

```
a := P(1); b := a; b.x = 42        # a.x is now 42   (assignment aliases)
fn bump(q: P): q.x = 99            # bump(c) mutates the caller's c
y := xs[0]; y.x = 7                # xs[0].x is now 7 (list holds shared refs)
```

So an unboxed representation must be transparent. Use it only where the value does not escape and is
not aliased in a way that outlives or shares its storage. Elsewhere the struct stays boxed.

## 3. The two parts

### 3a. Escape analysis

A new pass after the checker has types (`src/checker/`, feeding `src/compiler/`) classifies each
struct-producing site as **non-escaping** (may be unboxed) or **escaping/unknown** (boxed, the
default). A value escapes if it is:

1. returned from the function;
2. stored into a heap object: a `List`/`Map`/`Set`, another struct's field, an escaping tuple, a
   `Channel`, a `Shared` or cell;
3. captured by a closure that escapes (capture is by reference);
4. handed to another task (`spawn`/`Channel`/`Shared`: by handle since the shared heap, so the task
   holds the same object);
5. aliased out: bound to another name, or read out of a container and mutated so that the mutation
   is shared with a longer-lived binding.

**When unsure, escapes.** A false "escapes" only loses an optimization; a false "does not escape" is
a use-after-free. The analysis and the codegen must never disagree about who is boxed (the
checker-superset-of-compiler class in [`lessons.md`](../contributing/lessons.md)).

### 3b. Unboxed representation

- **(i) Frame-local arena** for a struct created and consumed in one function. Bump-allocate per
  frame; drop on return; the GC never sees it. Mostly a perf win (no alloc, trace or free).
- **(ii) Inline-in-container** for `List[P]` (and `Map`/`Set` values) whose elements do not escape
  one by one. The container stores struct bytes in a typed fixed-stride buffer and knows its element
  `tid`. `xs[i]` must return a **view**, not a copy, to keep mutation semantics. This is the memory
  win that closes the Go gap.

A `Value` is an 8 B tagged word (`src/vm/value.rs`); an unboxed struct is `n` words. Options: a fat
handle (base + tid) into an arena or inline store, or a typed container backing (`Vec<u8>` strided by
the field layout). The second mirrors Go's `[]P` and is the target for (ii).

## 4. The soundness surface

- **GC tracing.** An inline struct is not a `GcRef`; the container must trace its fields.
  `children()` (`src/vm/heap.rs`) gets a case for typed containers. A missed trace collects a live
  object. Gate: the `*_gc_stress` tests (`src/vm/gc_tests.rs`).
- **Tasks.** The frame arena is thread-local. Rule 4 forbids unboxing anything another task can
  reach; the analysis must not under-approximate what reaches it.
- **Mutation through a view.** `y := xs[0]; y.x = 7` must mutate `xs[0]`. When `y` escapes, `xs` must
  fall back to boxed, or the view dangles. This is why (ii) is stage 3.
- **Observable behaviour is fixed.** Goldens and `tests/chz` stay byte-identical at both worker
  counts. The representation must not leak into Display, `==`, iteration order or hash.
- **`match` / destructuring** binds fields without a boxed object, or boxes on demand.

## 5. Staged rollout

Each stage lands with failing-then-green tests and gc-stress, and is measured against the
`many_struct`/`many_map` baseline. Each stage can ship alone.

- **Stage 0: analysis only.** Emit the classification as metadata; change nothing. Count how many
  corpus struct sites are non-escaping. If few qualify, stop.
- **Stage 1: frame arena (3b-i).** Smallest blast radius; perf plus some memory.
- **Stage 2: inline `List[P]`, read-mostly (3b-ii).** Lists whose elements are never aliased out and
  mutated; copy-on-read where safe, boxed `List` otherwise.
- **Stage 3: mutable views + `Map`/`Set` values.** Highest risk; only if stages 1-2 justify it.

## 6. Risks and kill criteria

- **Soundness.** A wrong "non-escaping" corrupts the heap, likely intermittently and only under M:N.
  Gate: `*_gc_stress`, the two-worker-count suite, and an adversarial corpus with one test per escape
  route. Work in small reviewed deltas with an adversarial review per delta, not one autonomous run.
- **Complexity.** Every struct op (field get/set, dispatch, `==`, hash, Display, `match`)
  must handle boxed and unboxed, or box on demand. Kill criterion: stop if Stage 0 shows the analysis
  rarely fires.
- **Determinism.** A behaviour that changes with the worker count is a bug.
- **Effort.** Multi-session. Stage 0 is a checker-grade pass; stages 1-3 each touch the value model
  and the GC.

## 7. Alternatives considered

- **8 B `Value`:** done (int-favoring pointer tag); it does not remove per-struct objects.
- **Compacting/generational GC:** no help; the 128 MB is live data, not garbage or fragmentation.
- **Shrink `Obj` below 64 B** (box `MapData`/`SetData`, narrow inline `Fields` 3 → 2): ~25% off the
  slot array (~32 MB), with map/set indirection and re-spilled 3-field structs. A partial step that
  keeps one object per struct.
- **OS memory return** (`shrink_to_fit` on sweep, `malloc_trim`/mimalloc): returns peak RSS for spiky
  programs; does not shrink live data. Cheap; can land alone.
- **An explicit packed collection:** [`packed-storage.md`](packed-storage.md) proposes
  `PackedList[T]` with copy semantics, which needs no escape analysis.

## 8. Recommendation

Do **Stage 0 first** as its own effort. It measures how much real code would benefit, at a fraction of
the cost, and every later stage needs it. If a healthy fraction of sites are non-escaping, do Stage 1
and reassess before stages 2-3. If not, keep the 55% already won, land the OS-return lever, and close
the memory track: 2.2× Go on an all-structs benchmark is acceptable, and the single-object benches in
`benchmarks.md` sit near 1.0× Go.

## 9. Explicit value and reference structs (owner preference, 2026-10-22)

**Status: idea, not started. The owner prefers this route over escape analysis (§3) as the
long-term model.** It is a language change, so it waits until after the JIT. The `PackedList[T]`
experiment in [`packed-storage.md`](packed-storage.md) comes first, because it measures the gain
without a language change.

The program states which structs are values and which are references, so the compiler needs no
escape proof:

```chezzi
struct P:
    x: int
    y: int

xs: List[P] = []          # value elements: x, y stored inline, 16 B each
rs: List[*P] = []         # reference elements: one GC slot each, today's behaviour

a := P(1, 2)
b := a                    # value: b is a copy
b.x = 42                  # a.x is still 1
r := ref P(1, 2)          # reference (the spelling is open)
s := r
s.x = 42                  # r.x is 42, as today
```

### What it gives

| Gain | Source |
|---|---|
| Inline storage: 2M two-int structs in about 64 MB instead of 150 MB | `packed-storage.md` measurement, Go `[]P` |
| No per-struct allocation or GC trace for value structs | one fewer object per element |
| Value fields in machine registers under the JIT | a value has no identity to keep |
| No unsafe proof: the program states the fact that §3a must prove | a wrong escape answer is a use-after-free |

### What it costs

1. **The meaning of `struct` changes.** Today `b := a; b.x = 42` changes `a`. Under a value
   default it does not, and an old program changes meaning without an error. A migration needs a
   checker pass that finds each aliasing site.
2. **Method receivers.** Go has value and pointer receivers, and the method set of `T` differs from
   that of `*T`. Chezzi needs one rule for which receiver a method takes and when a call copies.
3. **Interior pointers.** Go's `&xs[0]` points into a list buffer. The GC addresses whole slots
   (`GcRef` is a slot index, `src/vm/heap.rs`), and a `push` that grows the buffer leaves Go's
   pointer on the old array. The first version takes no `&` of an element or a field: a reference
   is made only by `ref T(...)` (or the chosen spelling), so it is always a whole slot.
4. **Mutation of an element.** `xs[0].x = 7` on a value list must write the element in place
   (Go allows it, the element is addressable), or the checker rejects it as a write to a
   temporary (the `PackedList` v1 rule). Pick one.
5. **Every struct operation** (field get and set, `==`, hash, Display, `match`) handles an inline value and a slot. The rule "the representation never shows in output"
   stays.
6. **A task shares a reference by handle**, as a struct is today and as Go shares a pointer: every task of a run
   uses one heap ([`go-shared-heap.md`](go-shared-heap.md)).

### Two spellings to choose from

| Variant | Ancestor | Default | Marked form |
|---|---|---|---|
| A. Value by default, `*T` for a reference | Go | `struct P` is a value | `*P`, made by `ref P(...)` or `new` |
| B. Reference by default, a keyword for a value | Swift and C# `struct` vs `class` | `struct P` stays a reference | `value struct P` |

Variant B breaks no existing program. Variant A follows Go, the concurrency ancestor, but changes
every struct. The owner decides after the `PackedList` measurement.

### Order

1. The JIT ships first: the VM dispatch cost (`packed-storage.md`, 0.79 s against Go's 0.04 s for an
   int loop with no structs) is larger than the layout cost.
2. Run the `PackedList[T]` experiment and record RSS, build time and scan time.
3. If the gain holds on scans as well as on builds, pick variant A or B and write its full rules
   (receivers, element mutation, `==`, generic `T` that may be a value or a reference).
