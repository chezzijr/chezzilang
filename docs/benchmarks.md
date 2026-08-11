# Chezzi — Benchmarks (vs CPython)

Living perf-tracking doc. The harness that produces these numbers is **[`benches/`](../benches/)**
(driver: `benches/run.chz`, run via `cargo run -- run benches/run.chz`). Re-run it and
re-stamp this table when the runtime changes. The optimization backlog these numbers
justify lives in **[`future.md §4`](future.md)**; the scheduled work is roadmap **M19**
(`spec.md` / `PROGRESS.md`).

## Memory baseline — Go vs Chezzi — 2026-07-25

Peak process RSS (honest apples-to-apples: full runtime + stacks + heap; the internal
`CHEZZI_HEAP_STATS=1 peak_live_bytes` probe under-measures — it ignores off-heap `Vec`/`HashMap`
capacity and the operand/frame stacks). Measured with `benches/maxrss.py` (`os.wait4` ru_maxrss,
median of 3) against Go twins in `benches/go/` (`go build` first). Chezzi = M:N default `run`.

Machine: i5-11400H, x86_64, Linux; Go 1.26.5, rustc 1.97.0 (release).

| bench  |  Go MB | Chezzi MB | ratio | note                                    |
|--------|-------:|----------:|------:|-----------------------------------------|
| empty  |    8.8 |       8.9 | 1.01× | runtime floor — dead even               |
| loop   |    8.8 |       8.8 | 1.00× | compute, no heap                        |
| struct |    8.8 |       8.8 | 1.00× | one struct reused in place              |
| fib    |    8.9 |       8.8 | 0.99× | recursion, stack only                   |
| list   |   33.3 |      37.8 | 1.14× | 2M-elem `Vec` — inline ints, ONE object |
| map    |    8.8 |      19.6 | 2.23× | `MapData` index/`Vec` capacity          |

The single-big-object benches above are GENTLE — each keeps one heap object alive, so
Chezzi is near Go parity. The real gap shows with **many separate boxed objects**:

| bench                       |  Go MB | base  | #1 box Module | #2 inline fields | #3 mark bitset | #4 drop name | cum. vs base | vs Go |
|-----------------------------|-------:|------:|--------------:|-----------------:|---------------:|-------------:|-------------:|------:|
| many_struct (2M `P{x,y}`)   |   67.4 | 327.6 |  281.9 (−14%) |    220.7 (−22%)  |  205.6 (−7%)   |  148.9 (−28%) |    −55%      | 2.2×  |
| many_map   (1M int→struct)  |   66.9 | 242.9 |  222.1 (−9%)  |    194.2 (−13%)  |  187.1 (−4%)   |  163.5 (−13%) |    −33%      | 2.4×  |
| many_list  (2M `[i,i]`)     |  163.1 | 266.5 |  220.7 (−17%) |    220.9 (±0%)   |  205.8 (−7%)   |  205.8 (±0%)  |    −23%      | 1.3×  |

(`base` = pre-fix; `#1` = `0100153` (`Obj` 88→64B); `#2` = `c1f4d0e` (inline ≤3 `fields`); `#3` = `e66a1f5`
(`Slot` 72→64B, mark→bitset); `#4` = `c3b7b1c` (drop per-instance `Obj::Struct.name`, resolve from `tid`).
#4 is the biggest single lever: dropping the `name: Box<str>` allocated per struct killed 2M name-string
mallocs → −56.7MB (−28%) `many_struct`, −13% `many_map`, byte-identical output (== and Display now
resolve name from `tid`, mirroring the enum `variant_id` lever). `many_list` flat under #2/#4 (its
elements are lists, not structs). Cumulative **−55% / −33% / −23%**, Go gap 4.9×→**2.2×**. The ~128MB
`Slot` array (2M × 64B) — live struct data — is now the floor; only the structural value-struct lever
(inline-in-container) closes the rest.)

Root cause is **structural, not slot padding**: Go stores `P{x,y}` INLINE in the slice
(2M×16B, zero per-element heap objects). Chezzi boxes every struct as its own GC `Slot`
(after boxing `Module`, `size_of::<Obj>()` is **64B** — capped by `MapData`/`SetData`, `heap.rs`).
The `fields` buffer's separate malloc is now GONE for ≤3-field structs (lever "inline small `fields`":
`Fields::Inline` folds them into the 64B `Obj` slot); only `>3`-field structs still spill to a boxed
slice. So 2M `P{x,y}` (2 fields) = the `Slot` array + the outer 16MB list, no per-struct field buffer.
The `Slot` array (2M × 64B ≈ 128MB, after the GC mark bit moved to a parallel `marks: Vec<u64>` bitset
— was 72B/137MB) is now the dominant remaining cost — only the structural #3 lever shrinks it further. `many_list` is only 1.4× because Go also heap-allocates each inner slice — the gap shrinks
wherever Go can't inline either.

Levers, ranked for this gap:
- **box `Module`** ✅ DONE (`0100153`) → `size_of::<Obj>()` 88→64B; measured −14%/−9%/−17% RSS on
  many_struct/map/list. Cheap; doesn't close the 4.2× (structural, not slot size).
- **inline small `fields`** ✅ DONE → `Obj::Struct.fields` is a hand-rolled `Fields` enum
  (`Inline { len: u8, vals: [Value; 3] }` folds ≤3 fields into the 64B `Obj` slot, `Spill(Box<[Value]>)`
  for `>3`), killing the per-struct second malloc for the ≤3-field majority. No `smallvec` dep; `Obj`
  stays 64B (`size_of::<Fields>() == 32`). Measured (`c1f4d0e`): −22% (−61.2MB) `many_struct`, −13%
  `many_map`, ±0% `many_list` — matched the ~61MB prediction. Doesn't close the 3.3× (that's the `Slot`
  array now, → #3).
- **mark bit → parallel bitset** ✅ DONE → dropped the `mark: bool` field from `Slot`, moved the GC
  mark to a dense `marks: Vec<u64>` bitset on `Heap`. `Slot` 72→64B (`Option<Obj>` niche-packs `None`
  free; the bool was pure padding), ≈16MB off `many_struct`'s 2M-slot array. (Sweep still scans every
  slot's `obj` to find garbage — the bitset does not avoid that; the win is the per-slot byte + tighter
  mark test-and-set locality, not a sweep-scan cache win.) VM/GC-internal, no observable change; all
  GC-stress + two-engine parity green. RSS delta measured post-merge.
- **drop struct name → resolve from `tid`** ✅ DONE → removed the per-instance `name: Box<str>` from
  `Obj::Struct` (the type IDENTITY KEY, a **second heap alloc per struct**), resolving it from `tid` via
  `Program::struct_names` on the cold path (mirrors the enum `variant_id` lever). Probe: nulling the name
  alloc took `many_struct` 205.6→148.9 MB (~28% of RSS), `many_map` 187→163 MB. `Obj` stays 64B.
  VM-only, behavior-preserving, all `*_gc_stress` + two-engine parity green. RSS delta measured
  post-merge (predicted ~−57MB `many_struct` / ~−24MB `many_map`).
- **value-struct representation** (Go-style inline-in-container) → closes 4.9×→~1.5×, but a deep
  change to the every-object-is-a-`GcRef` aliasing model. Its own milestone. See `future.md §4`.

Re-run `many_struct`/`many_map` after each lever to track the close.

## The M23 review fixes — 2026-08-08 — correctness fixes, **free**

The eight adversarial-review fixes (probe position re-validation, snapshot rooting, the `Atomic.cas`
hook switch, the nested-payload checker walk). A/B against `f50deb56`, hyperfine `-N --warmup 2/3`,
release binaries built into **separate** `CARGO_TARGET_DIR`s and confirmed distinct
(`strings … | grep "payload reaches"` = 1 vs 0).

| bench     | runs | before   | after    | delta   |
|-----------|-----:|---------:|---------:|--------:|
| map       |   30 | 255.9 ms | 254.8 ms |  −0.4%  |
| struct    |   20 | 666.9 ms | 667.8 ms |  +0.1%  |
| loop      |   12 | 1467.9 ms| 1451.7 ms|  −1.1%  |
| list      |   12 | 587.5 ms | 586.7 ms |  −0.1%  |
| enum      |   12 | 3152.0 ms| 3163.7 ms|  +0.4%  |
| many_map  |   12 | 466.5 ms | 464.9 ms |  −0.3%  |
| primes    |   12 | 956.9 ms | 942.0 ms |  −1.6%  |
| str       |   12 | 255.8 ms | 255.6 ms |  −0.1%  |
| fib       |   12 | 390.6 ms | 388.5 ms |  −0.5%  |

Free because the two hot additions are gated on `Vm::eq_may_reenter` — the probes' position
re-validation and the snapshot rooting both collapse to nothing when the program declares no `eq`
hook, which is every bench.

**One measured surprise, worth the note.** The first cut regressed **`struct` by +3.3%** (669.5 →
690.9 ms, 2.5σ, reproducible) — a bench with **no `==` in it at all**. Nothing semantic reached it:
the six new container-arm closures had inlined into `values_equal_guarded` and bloated it enough to
degrade the neighbouring codegen in `arith.rs` (where the hot arithmetic/superinstruction paths live).
`#[inline(never)]` on `Vm::with_elem_roots` put it back to flat with no other change. Another entry
for "don't trust a lever's a-priori blast radius" — this one wasn't a lever at all.

## The container `Eq` ripple (M23 slice 4) — 2026-08-08 — correctness fix, cost measured

Not a lever — the correctness fix that makes `y in xs` / `m[y]` agree with `x == y`. Its price is
structural: `values_equal_guarded`/`elem_equal`/`*_slot` had to become `&mut self` (they can now call
user code), so a heap Map/Set probe can no longer hold its `entries` slice borrowed across the compare.
`Vm::map_probe`/`set_probe` re-index the candidate list per probe step instead — allocation-free, one
extra index lookup on the terminating step (a distinct key almost always has exactly one candidate).
Rooting the probe's in-flight values is skipped entirely when the program declares no `eq` hook at all
(`Vm::eq_may_reenter`, both hook tables empty ⇒ no compare can re-enter ⇒ no collection).

A/B against `035de7ee`, hyperfine `-N --warmup 2/3`, release binaries, one at a time. `map` is the only
bench that moves, and it is the one that pays for the ripple:

| bench     | runs | before        | after         | delta   |
|-----------|-----:|--------------:|--------------:|--------:|
| map       |   30 | 249.0 ± 15.9 ms | 259.3 ± 11.0 ms | **+4.1%** |
| many_map  |   10 | 455.0 ± 10.7 ms | 463.5 ± 18.1 ms |  +1.9%  |
| loop      |   12 | 1.476 ± 0.018 s | 1.467 ± 0.014 s |  −0.6%  |
| struct    |   12 | 680.2 ±  4.8 ms | 670.1 ± 12.0 ms |  −1.5%  |
| enum      |   12 | 3.247 ± 0.085 s | 3.160 ± 0.027 s |  −2.7%  |
| list      |   12 | 588.2 ±  5.3 ms | 582.1 ±  2.5 ms |  −1.0%  |
| str       |   12 | 256.6 ±  2.2 ms | 255.0 ±  2.6 ms |  −0.6%  |
| primes    |   12 | 960.4 ± 15.6 ms | 958.0 ± 10.4 ms |  −0.2%  |

`map` (200k int inserts + 1M lookups) is ~4% slower — at the edge of its own σ (6%) but reproducible
across three A/B runs. Everything else is flat-to-faster, i.e. noise. Accepted: the alternative is a
`==` that disagrees with `in` and `m[k]`. If map probing later needs the 4% back, the lever is a
`!eq_may_reenter()` fast path that keeps the old borrow-holding slice loop (it needs a second copy of
the probe body, which is why it was not taken here).

## `Eq`-hook lookup table (M23 slice 3 follow-up) — 2026-08-08 — correctness fix that also pays

Not a lever either — the `==` operator must be able to tell the `Eq` HOOK (`fn eq(self, o: Self) ->
bool`) from an ordinary method that merely shares the name, which a `methods.get("eq")` name lookup
cannot do. The compiler now records the hook in `Program::eq_struct`/`eq_enum`, dense
`Vec<Option<(proto, module)>>` indexed by the `tid`/`variant_id` the operands already carry, so the
string hash leaves the **miss** path of every struct/enum `==` — including every `Option`/`Result`
compare — as a side effect of the fix.

Micro-bench (the only place the change can show: 2M struct `==` + 2M `Some(1) == Some(1)`, neither type
declaring `eq`), hyperfine `--warmup 3 -N --runs 12`, release binaries, `before` = `0dec27bd`:

| micro                                    | before        | after         | delta            |
|------------------------------------------|--------------:|--------------:|-----------------:|
| 4M `==` on the MISS path (struct + enum)  | 474.0 ± 21.3 ms | 426.6 ± 17.4 ms | **1.11× faster** |

The nine standard benches are the no-regression check — none of them compares a struct or enum with
`==`, so flat is the expected result (hyperfine `--runs 8`, all within σ):

| bench       | before | after  | ratio |
|-------------|-------:|-------:|------:|
| fib         |  247.7 |  249.0 | 1.005 |
| str         |  162.0 |  166.7 | 1.029 |
| primes      |  625.3 |  619.6 | 0.991 |
| loop        |  954.7 |  949.8 | 0.995 |
| list        |  397.6 |  378.0 | 0.951 |
| struct      |  436.7 |  441.2 | 1.010 |
| poly_method | 1350.0 | 1297.9 | 0.961 |
| map         |  145.3 |  142.3 | 0.979 |
| empty       |    2.2 |    2.1 | 0.967 |

## Fresh per-task module-global snapshot (gaps.md W6-2) — 2026-07-25 — correctness fix, cost measured

Not a lever — a P0 correctness fix (a task now snapshots the module globals fresh, pinned at its own
`spawn`, at every depth, instead of replaying the first nursery's frozen `Arc` forever). Measured because
it puts work back on the nursery path. Machine as above; best-of-3 wall clock per row, `before` = `main`
@ `12cb25a`, release binaries, one at a time.

| bench       | before  | after   | delta   |
|-------------|--------:|--------:|--------:|
| fib         |  246.1  |  248.1  |  +0.8%  |
| str         |  165.2  |  165.0  |  −0.1%  |
| primes      |  588.1  |  598.9  |  +1.8%  |
| loop        |  906.6  |  930.4  |  +2.6%  |
| list        |  386.4  |  368.9  |  −4.5%  |
| struct      |  416.8  |  405.3  |  −2.8%  |
| poly_method | 1198    | 1194    |  −0.3%  |
| map         |  132.7  |  134.3  |  +1.2%  |
| empty       |    1.9  |    1.9  |    0%   |

All ms (hyperfine mean, `--warmup 2`), all **within noise** (σ 2–8%; `map` re-measured with `-m 20` in
both directions: 132.7/136.0 before vs 134.3/133.7 after). None of the 9 benches opens a nursery, so flat
is the expected result and this table is a no-regression check, not a delta.

The cost lives where it should — on the nursery path. Micro-benches, release binaries, best-of-3;
`2nd cut` is the rejected review-fix revision (pin deferred to the next slot write / the join) kept in the
table because it is what the current shape had to beat:

| micro                                                                   | main    | this fix       | 2nd cut (rejected) |
|-------------------------------------------------------------------------|--------:|---------------:|-------------------:|
| 200k nurseries × 1 task, scalar/`str` globals only, `--serial`          | 0.598s  | 0.594s         | 0.608s             |
| …+ one 20-element `List[int]` global (the aggregate case)                | 0.799s  | 0.842s (+5.4%) | 1.000s (+25%)      |
| 40k spawns + 40k global writes in one nursery, `--serial`                | 0.074s  | 0.090s         | 1.721s (23×)       |
| 2k spawns + 200k global writes in one nursery, `--serial`               | 0.026s  | 0.026s         | 0.231s (8.9×)      |
| 3000 EAGER spawns, 20000-element `List[int]` global, M:N (server shape)  | 0.014s  | 0.018s         | 1.272s (91×)       |
| the same nested shape on `--serial` (3000 tasks × 20000-element copies)  | 4.03s   | 4.46s (+10.6%) | 4.06s              |
| 200k nurseries × 1 task, M:N (pool overhead dominates)                   | 8.58s   | 8.66s (+0.9%)  | 8.46s              |

Row 1 is the snapshot cache short-circuiting: ONE build for the whole run, asserted by build COUNT
(`vm::tests::snapshot_cache_short_circuits_per_epoch_not_per_spawn`) rather than by timing. Row 2 is the
designed price of fresh-per-nursery when a global holds a mutable aggregate: in-place mutation
(`q.push(1)`) writes no module slot to invalidate on, so the cache is dropped at every `EnterNursery` and
each nursery rebuilds once. Rows 3–5 are the rejected cut's regressions (a per-write O(pending-tasks) scan,
and a per-spawn full rebuild on the eager per-connection path) — gone: the pin now reads the cache.
Row 6 is the one measurable regression left, +10.6% on a pathological shape (a nested nursery, 3000 tasks,
a 20000-element global ⇒ 10GB of per-task deep copies): the snapshot is built at the first `spawn` instead
of at the join, and the changed allocation ORDER costs ~10% there. It is **not** extra snapshot work — the
build count is 2 either way (measured), peak RSS is identical (10.10 vs 10.10GB), and neither disabling the
`EnterNursery` rule nor the `install_snapshot` cache seed moves it; with a realistically-sized global
(20 elements) the same shape is 0.015s vs 0.016s. Precise per-mutation invalidation (hooking the mutating
intrinsics in `src/vm/call.rs`, unfenced now that W6-3 has merged) is the recorded follow-up in
`docs/gaps.md §W6-2`, with row 2's +5.4% as its bar.

## Baseline — 2026-06-11

- **Machine:** 11th Gen Intel Core i5-11400H @ 2.70GHz (12 threads)
- **Toolchain:** rustc 1.94.1 (release build) · Python 3.14.4 · hyperfine 1.20.0
- **Method:** `hyperfine --warmup 2 -N`, mean of ≥10 runs per command.

| bench    | what it stresses        | chezzi   | python  | chezzi slower |
|----------|-------------------------|----------|---------|---------------|
| fib(30)  | recursive calls         | 470 ms   | 80 ms   | **5.9×**      |
| str      | f-string + join, 500k   | 300 ms   | 83 ms   | **3.6×**      |
| list     | push + sum, 2M          | 576 ms   | 150 ms  | **3.8×**      |
| primes   | `while` + `%`, < 200k   | 985 ms   | 284 ms  | **3.5×**      |
| loop     | int add, 20M            | 1806 ms  | 843 ms  | **2.1×**      |
| startup  | empty program           | 0.79 ms  | 9.0 ms  | **0.09× (win)** |

> Ratios are the portable signal; absolutes move with hardware. The numbers above are this
> machine on this date — regenerate before drawing conclusions on a different box.

## After M19 Phase 1 — 2026-06-11 (same machine)

Phase 1 landed three behavior-preserving VM/compiler optimizations: **(1)** killed the per-call
`Obj`/`HashMap` clone in `invoke_value`; **(2)** a jump-relocating peephole pass with constant
folding; **(3)** superinstructions for the hot numeric windows (`BinLocalLocal`, `BinLocalConst`,
`IncLocal`). Re-run of the same harness:

| bench    | chezzi (before → after) | python  | slower (before → after) |
|----------|-------------------------|---------|-------------------------|
| fib(30)  | 470 ms → **391 ms**     | 80 ms   | 5.9× → **4.9×**         |
| str      | 300 ms → **283 ms**     | 82 ms   | 3.6× → **3.5×**         |
| list     | 576 ms → **447 ms**     | 154 ms  | 3.8× → **2.9×**         |
| primes   | 985 ms → **738 ms**     | 285 ms  | 3.5× → **2.6×**         |
| loop     | 1806 ms → **1164 ms**   | 856 ms  | 2.1× → **1.4×**         |
| startup  | 0.79 ms → **0.80 ms**   | 9.0 ms  | 0.09× (win, unchanged)  |

The benches moved exactly where the fixes aimed: **`loop` −36%** and **`primes` −25%** from the
superinstructions (fused compare/arith/increment cut dispatch count in the inner loop); **`fib`
−17%** and **`list` −22%** from the `invoke_value` clone kill (both are call-heavy). **`str` barely
moved** — its cost is string allocation (`BuildStr`/`ConstStr`), untouched this phase and the
target of a later one. Startup unchanged (still the standing ~11× win).

## After M19 Phase 2 — 2026-06-11 (same machine)

Phase 2 landed two behavior-preserving allocation kills, both TDD'd under the two-engine parity
suite: **(1)** the **per-call args `Vec`** in `do_call` is gone — a `Func`/`Closure` callee now runs
in place over the args already on the operand stack (`copy_within` drops the callee from beneath
them; native / non-callable callees keep the old `Vec` path that `invoke_native` needs); **(2)**
**`stringify`-into-buffer** — `stringify`/`stringify_obj`/`stringify_seq` were rewritten to append
into a caller-owned `String`, so `BuildStr` reuses one buffer across all interpolation parts instead
of allocating an intermediate `String` per part. Re-run of the same harness:

| bench    | chezzi (P1 → P2)        | python  | slower (P1 → P2)        |
|----------|-------------------------|---------|-------------------------|
| fib(30)  | 391 ms → **333 ms**     | 79 ms   | 4.9× → **4.2×**         |
| str      | 283 ms → **264 ms**     | 84 ms   | 3.5× → **3.15×**        |
| primes   | 738 ms → **714 ms**     | 281 ms  | 2.6× → **2.54×**        |
| loop     | 1164 ms → **1106 ms**   | 851 ms  | 1.4× → **1.30×**        |
| list     | 447 ms → **458 ms**     | 150 ms  | 2.9× → **3.05×** (flat) |
| startup  | 0.80 ms → **0.80 ms**   | 9.0 ms  | 0.09× (win, unchanged)  |

Moved where the fixes aimed: **`fib` −13%** (it is *all* calls — killing the per-call `Vec` is the
single biggest lever) and **`str` −5%** (the buffer kills one `String` alloc per interpolated part;
the residual cost is `ConstStr` boxing the literal part on every push — the next `str` lever).
`primes`/`loop` drift down a little (they carry some call/return traffic). **`list` is flat** — its
cost is `.push` (a method call, not `do_call`) + GC, untouched here; the 447→458 wobble and the
2.9→3.05 ratio are run-to-run noise (CPython also measured 150 vs 154 ms this run, σ≈10 ms on both).

## After M19 Phase 2b — 2026-06-11 (same machine)

Phase 2b is **global-slotting**: the compiler assigns each module global a stable `u32` slot and
emits `GetGlobalSlot`/`SetGlobalSlot`/`DefineGlobalSlot`; the runtime read is now a bounds-checked
`Vec` index (`Obj::Module { slots: Vec<Value>, index }`) instead of a `HashMap<String,Value>` probe
by name. Because the slot map lives in the shared `Arc<Program>`, this also removes the latent
parent↔worker slot-order fragility the snapshot path carried (slot↔name is identical by
construction, not by HashMap iteration luck). Behavior-preserving — all 1520 tests green, both
engines. `fib`/`str`/`primes` re-measured isolated (`--warmup 3 -N -r 20`); `loop`/`list`/`empty`
from the sweep:

| bench    | chezzi (P2 → P2b)       | python  | slower (P2 → P2b)       |
|----------|-------------------------|---------|-------------------------|
| fib(30)  | 333 ms → **302 ms**     | 80 ms   | 4.2× → **3.78×**        |
| str      | 264 ms → **273 ms**     | 84 ms   | 3.15× → **3.24×** (flat)|
| primes   | 714 ms → **740 ms**     | 285 ms  | 2.54× → **2.60×** (flat)|
| loop     | 1106 ms → **1105 ms**   | 854 ms  | 1.30× → **1.29×** (flat)|
| list     | 458 ms → **486 ms**     | 153 ms  | 3.05× → **3.18×** (flat)|
| startup  | 0.80 ms → **0.84 ms**   | 9.0 ms  | 0.09× (win, unchanged)  |

**`fib` −9%** is the whole story — and it's the right story. The lever lands on *global-read
density*: `fib(n-1) + fib(n-2)` resolves the `fib` callee twice per call, so every call paid a
name-keyed map probe before and a `Vec` index now. The rest are **flat within noise** because their
hot loops read *locals*, not globals: `primes`/`loop` are inner-loop integer arithmetic (one global
call per *outer* iteration is too small a fraction to register — the a-priori "moves `primes`" guess
in `future.md` was wrong about where the read density is), `list` is `.push`+GC, `str` is string
allocation. CPython measured identically on `primes` (281→285 ms) this run, confirming the box was
stable — the small +ms drifts on the local-bound benches are thermal/run-to-run, not regressions
(nothing got a *slower* code path; the slot read is strictly cheaper than the map probe it replaced).

## After M19 Phase 3 — 2026-06-11 (same machine)

Phase 3 is the **`str` allocation lever**: `Op::ConstStr` now interns its heap string in a per-heap
cache keyed by the literal's data pointer (`s.as_ptr()`, stable because the `String` lives in the
immutable `Arc<Program>`), so re-pushing the same literal op is a pointer lookup instead of
`clone`+box+`heap.alloc`. Sound — strings are immutable and there's no identity operator, so aliasing
is unobservable; the cached `GcRef`s are GC-rooted and travel heap-keyed across `swap_ctx` like
`module_objs`. Also a `alloc_char` single-alloc helper for every 1-char-string site (iteration /
`chars()` / indexing / `chr`). Behavior-preserving — 1525 tests green, both engines + a 2-agent review
panel (SOUND). `fib`/`str`/`primes` isolated (`--warmup 2 -N -r 10`), rest from the sweep:

| bench    | chezzi (P2b → P3)       | python  | slower (P2b → P3)       |
|----------|-------------------------|---------|-------------------------|
| fib(30)  | 302 ms → **308 ms**     | 80 ms   | 3.78× → **3.85×** (flat)|
| str      | 273 ms → **227 ms**     | 83 ms   | 3.24× → **2.71×**       |
| primes   | 740 ms → **713 ms**     | 285 ms  | 2.60× → **2.50×** (flat)|
| loop     | 1105 ms → **1107 ms**   | 847 ms  | 1.29× → **1.31×** (flat)|
| list     | 486 ms → **482 ms**     | 153 ms  | 3.18× → **3.16×** (flat)|
| startup  | 0.84 ms → **0.80 ms**   | 9.1 ms  | 0.09× (win, unchanged)  |

**`str` −17%** is the whole story, and it's the right story: the str bench re-pushes its f-string
literal chunks ~500k times, so killing the per-push `clone`+box+alloc lands directly. Everything else
is **flat within noise** — interning only bites where the *same literal op repeats*, which the
arithmetic/call benches don't do (their hot loops push locals and computed values, not literals). No
regressions (the cache lookup is strictly cheaper than the alloc it replaced).

## After M19 Phase 4 — 2026-06-12 (same machine)

Phase 4 is the **struct-field lever**: `GetField`/`SetField` now carry a per-call-site inline-cache
id into a per-`Vm` `field_ic` vector that caches the field's index. A hit re-verifies the cached
index against the live field name (`fields[idx].0 == name`) and collapses the `O(field-position)`
name-probe to one verify-compare; a miss re-probes and refills. Static slotting (the P2b model) is
impossible here — the compiler is type-erased, so the field's struct type is unknown at emit time —
hence a runtime IC. Sound + thread-safe: the cell holds an index (no `GcRef`), so it is invisible to
GC / snapshots / `swap_ctx`; cooperative fibers run sequentially and `--parallel` workers each own a
`Vm`; every access self-verifies, so a stale/cross-type cell can never return a wrong field. The
parity is checked serial-VM (`parallel=false`) vs M:N-VM (`parallel=true`) — both are the same
`Vm`, only the scheduler differs. (Historical note: this landed when a frozen tree-walk interpreter,
since removed, was the oracle.)

New **`struct`** bench (8-field accumulator, eight reads + four writes/iter, 1M iters) — the
field-access-bound case the IC targets — measured IC-on vs IC-off (`-N --warmup 5 -r 20`):

| bench    | chezzi (P3 → P4)        | python  | slower (P3 → P4)        |
|----------|-------------------------|---------|-------------------------|
| struct   | 549 ms → **477 ms**     | 165 ms  | 3.32× → **2.89×**       |
| fib(30)  | flat (1.02× IC, noise)  | —       | unchanged               |
| list     | flat (1.01×, noise)     | —       | unchanged               |
| loop     | flat (1.03×, noise)     | —       | unchanged               |

**`struct` −13%** is the win, and it lands exactly where predicted — on *field-probe density*. The
non-struct benches are flat because the IC never engages there (no struct `GetField`), and `Op`'s
size is unchanged (`GetField`'s added `u32` stays under the max-payload variant), so dispatch is
untouched.

> **Caveat, measured honestly (the a-priori-guess discipline):** an earlier *method-bound* bench — a
> 6-field particle whose hot op is a `self.*` method call, with shallow field access — showed the IC
> **~neutral to −3%**. When field access is a small fraction of the loop (method dispatch dominates)
> and fields are shallow, the IC's cold `field_ic` indirection isn't amortized. The IC wins where
> field resolution is actually the bottleneck (wider structs, deeper fields); it is not a free win on
> every struct. A struct **type-id guard** (pure-int compare, no name re-verify) landed as Phase 5b
> (`bbdcb38`, below) — measured **neutral**: it tightened the hit path but did **not** close this
> caveat (the cost is the cold-IC indirection, not the name re-verify). Kept anyway (cheaper, parity-clean).

## After M19 Phase 5a — FxHash map/set index — 2026-06-12 (same machine)

The **`usize`/`u64` hasher lever**: `MapData`/`SetData`'s `index` (`cached-hash → positions`) and
`str_intern` (pointer-keyed) swapped stdlib SipHash for a tiny in-tree FxHash (`src/vm/fxhash.rs`, no
new dependency). The hasher only routes the probe; `values_equal` confirms every hit, so it's
behavior-preserving — VM **and** interp parity (interp's map/set are unaffected; new parity tests
lock map int/str keys, a constant-`hash()` collision struct, and set ops). Maps/sets were previously
**unbenched** — added `benches/chz/map.chz` (200k int inserts + 1M lookups; int keys hash straight to
their `f64` bits, isolating the index hasher) + `benches/py/map.py`.

| bench    | chezzi (P4 → P5a)       | python  | slower (P4 → P5a)       |
|----------|-------------------------|---------|-------------------------|
| map      | 252 ms → **234 ms**     | 84 ms   | 3.04× → **2.82×**       |
| str      | flat (~2.6×, str_intern noise) | — | unchanged              |
| fib/list/loop/struct | flat (no map/intern traffic) | — | unchanged   |

**`map` −7%** is the win, on the lever's target. Other benches are flat (none touch map/set; the
`str_intern` get/insert is one lookup per `ConstStr`, lost in noise).

> **Footgun, found by measuring (the a-priori-guess discipline):** a *naive* FxHash (multiply only,
> no finalizer) made the map bench **100× slower** (252 ms → **24 s**). Cause: int keys store
> `f64::to_bits`, whose **low mantissa bits are zero** for a run of integers (entropy is all in the
> high bits). FxHash's multiply mixes entropy only *upward*, so hashbrown's low-bit bucket index
> collapsed → O(n) probe chains. Fix: a splitmix64 finalizer in `finish()` avalanches high bits down.
> Lesson logged: "the index `u64` is already a good hash" is true for **string** keys (avalanched
> `DefaultHasher`) but **false** for int/float keys (raw `to_bits`) — the hasher must finalize.

## After M19 Phase 5b — struct type-id guard — 2026-06-12 (measured NEUTRAL)

The logged P4 follow-up: stamp a dense numeric `tid` (layout id) on every `Obj::Struct` (from
`StructDef::tid`, assigned in declaration order at compile), and make the field IC hit guard on
`cell.tid == obj.tid` (a pure-int compare) instead of P4's `fields[idx].0 == name` string re-verify.
Sentinel `TID_NONE` (unregistered/native structs, empty cells) never matches, so the guard can't
false-hit across distinct unregistered layouts. VM-only ⇒ parity automatic; 1549 tests green.

Same-session A/B (P5a binary vs P5b binary, `-N --warmup 5 -r 30..40`):

| bench         | P5a → P5b            | ratio        | verdict            |
|---------------|----------------------|--------------|--------------------|
| struct (8-field, field-bound) | 459 ms → 451 ms | 1.02× | neutral (in noise) |
| method-bound (6-field `self.*` in a hot method call) | 1.206 s → 1.191 s | 1.01× | neutral (in noise) |

**Honest result: neutral.** The win this lever was supposed to capture (the P4 "shallow-struct
caveat") didn't materialise — because **P4 already collapsed the O(field-position) name-probe to a
single verify-compare**, and for short field names (`a`..`h`, 1 byte) that string compare is already
cheap (length check + a 1-byte memcmp). Replacing it with a `u32` compare saves nothing measurable;
method-call / dispatch overhead dominates both benches. No regression anywhere (construction reuses
the already-fetched `def.tid`; the guard is int-compare). **Kept** as the principled guard (tid =
layout identity removes the last string compare from the field hot path and is future-proof for any
real polymorphic field site) — but the field-IC lever is now **spent**: no cheaper guard remains.
Lesson logged (a-priori-guess discipline): the P4 caveat was a *prediction*, not a measured cost;
once P4 removed the probe loop, the residual string compare was already in the noise.

## After M19 call-flattening — 2026-06-12 (same machine)

The top remaining call-bound lever (diagnosed in [`future.md §4`](future.md)): every Chezzi call
recursed into a **fresh Rust `run_until` loop** (`do_call` → `run_proto_in_place` → `run_until`), so
each call cost a native Rust stack frame **and** an `Arc::clone(&self.program)` per call (`mod.rs`
loop-entry, an atomic). The bytecode `Op::Call` fast path now **pushes the callee frame and lets the
running `run_until` loop execute it** (CPython-3.11 "zero-cost frames"); `Op::Return`/`do_return`
already push the result to the caller stack and pop the frame, so the loop just continues. HOFs /
methods keep the re-entrant `run_proto` (they need the callee result synchronously mid-Rust-method).
Behavior-preserving, VM-only, full two-engine parity green (1550 tests + conformance 7/7).

| bench    | slower (before → after) | verdict                                  |
|----------|-------------------------|------------------------------------------|
| fib(30)  | 3.85× → **3.54×** (−8%) | the worst/most call-bound bench — moved most |
| list     | 3.16× → **2.97×** (−6%) | per-element push + a call per row        |
| primes   | 2.50× → 2.53×           | flat — hot path is inner-loop arith, ~1 call/outer-iter |
| str      | 2.71× → 2.65×           | flat (noise) — alloc-bound, not call-bound |
| loop     | 1.31× → 1.32×           | flat — no calls, pure dispatch floor     |
| struct   | 2.89× → 2.71×           | method calls still use `run_proto` (follow-up); within noise |

The shape is exactly as predicted: **`fib` (all calls) moved most, `loop` (no calls) stayed put.**
The win is modest because flattening removes only the *per-call Rust recursion + atomic*, not the
per-op dispatch of the call body (still ~7 ops/call) or the frame-setup cost — those are the next
walls. **Robustness bonus (not a bench number):** deep *plain* recursion no longer consumes host
stack (frames live in the heap `frames` `Vec`), so it runs bounded only by `MAX_CALL_DEPTH` (10_000),
not the 384 MiB `VM_STACK_BYTES` thread — a recursion that SIGABRT'd a 1 MiB stack pre-change now
completes (regression-guarded by `deep_plain_recursion_runs_on_small_host_stack`). **Follow-up:**
flatten `do_method_call` (still `run_proto`) for the `struct`/method benches; `VM_STACK_BYTES` could
shrink once method/HOF re-entry is the only host-stack recursor.

## Reading it

The shape matches what `future.md §4` predicted from first principles: **the gap is
dispatch count, per-call allocation, and name lookup — not the value model or the GC.**

- **Startup is a standing win.** No interpreter warmup, no bytecode load from disk for a
  hosted VM — a cold `chezzi run` of an empty file beats `python3` by ~11×. This is worth
  protecting: it's what makes Chezzi pleasant for one-shot scripts.
- **The gap widens with call density.** `loop` (pure arithmetic, almost no calls) is only
  2.1× off; `fib` (nothing *but* calls) is 5.9× off. Function-call overhead is the single
  biggest lever.
- **String and list work sit in the middle** (~3.6–3.8×) — allocation-bound, helped by
  interning / builders / general `Value` density rather than any one targeted fix.

## Per-bench bottleneck → fix

Hot spots found by reading `src/vm/mod.rs` and `src/vm/heap.rs`. Each maps to a ranked
item in `future.md §4`.

| bench   | dominant cost | concrete hot spot | fix (future.md §4) |
|---------|---------------|-------------------|--------------------|
| fib     | recursive call overhead | per-call `Vec<Value>` arg alloc (`mod.rs:3181`); full `Obj` enum clone incl. the captured env in `invoke_value` (`:3198`; the env is now a positional `Vec<Value>`, not a `HashMap`, since the 2026-06-16 captures lever); `name.clone()` for the arity check (`:3200`); per-call slot pre-fill (`push_frame`) | frame pooling; pass args as a stack slice; match on `&Obj` (no clone); kill `name.clone()` |
| str     | f-string build + join | ✅ per-element `Box<str>` heap alloc for each `"item-N"` — **killed by SSO** (inline ≤22-byte strings, `ChzStr`, 2026-06-12; `str` −20%). NOT concat/split builder — `join` already buffers (`:4377`), `+`/`split` aren't benched |
| primes  | `while` + `%` | binary ops re-dispatch on operand type every iteration; per-access name lookup for globals/locals | specialize arithmetic (monomorphic int guard); inline caching; superinstructions |
| loop    | 20M int adds | raw dispatch count (arith re-dispatch already killed by P1 superinstructions) | ✅ superinstructions (fuse `GetLocal+GetLocal+BinOp`) + arith specialize landed. NaN-box `Value` (16→8 B) would help but is BLOCKED by full i64 — see reality-check below. Floor likely needs register VM / JIT |
| list    | push + sum | near the safe match-dispatch floor | general dispatch / `Value`-density work; no targeted fix |
| startup | — | tree of small allocs, no warmup | already the strength — keep it |

Cross-cutting allocators worth fixing alongside: list clone per `for` iteration
(`ListClone`, `mod.rs:2514`), per-char `String` alloc on string iteration (`:2530`),
field-name `Box<str>` stored per struct instance and cloned on `==` (`heap.rs:97`,
`mod.rs:3133`).

## Highest payoff-per-effort

Per `future.md §4`: **superinstructions + inline caching + peephole/const-fold**, plus the
**per-call clone kills** in `invoke_value` (cheap, and `fib` is the worst bench). All four
attack dispatch count, name lookup, and call overhead without touching the value model or
the GC — and `fib`/`loop`/`primes` are exactly the benches they move.

**Landed (M19 Phase 1):** ✅ per-call clone kill in `invoke_value`, ✅ peephole + constant
folding, ✅ superinstructions (`BinLocalLocal` / `BinLocalConst` / `IncLocal`).
**Landed (M19 Phase 2):** ✅ in-place call args in `do_call` (per-call `Vec` gone — `fib` −13%),
✅ `stringify`-into-buffer for `BuildStr` (`str` −5%). **Landed since:** ✅ global-slotting (P2b),
✅ `ConstStr` interning + per-char single-alloc (P3), ✅ struct-field inline cache (P4).

## Backlog reality-check (2026-06-12)

A pass over the three "remaining big levers" before picking the next perf task. Recording so the
next session doesn't re-discover these:

- **NaN-boxing `Value` is BLOCKED by full 64-bit ints.** `Value::Int` is a full `i64`
  (`src/vm/value.rs:18`); an i64 + a type tag don't fit in 8 bytes alongside `f64`. Doing it means
  boxing big ints (branch + alloc per int, semantics-sensitive overflow) — not a behavior-preserving
  session, and an uncertain win on the int benches it targets. **Lua 5.4 stayed 16-byte for this
  exact reason.** Blast radius is VM-only (the frozen interp has its own `Rc`-based `Value`), but
  it's a milestone spike, not a lever. Demoted to "Big/separate" in `future.md §4`.
- **The "concat / `split` builder/rope" lever does NOT move any bench.** The `str` bench is
  `BuildStr` + `,".join`, and `join` already buffers into one `String` (`mod.rs:4377`); `+`/`split`
  aren't exercised at all. The real open `str` lever is **small-string optimization** — `"item-N"`
  are all ≤12 bytes, each a `Box<str>` heap alloc (`alloc_str`, `mod.rs:4697`); inlining short
  strings in the `Obj` slot kills the per-element alloc.
- **Arith specialization + frame pooling are effectively closed.** P1 superinstructions already
  inline the monomorphic int path (`loop`/`primes`); `CallFrame`'s `Vec`s are alloc-free, so there's
  no per-call frame alloc to pool. Both ✅/✗ in `future.md §4`.

**All three "real next levers" have since landed** (struct **type-id guard** Phase 5b `bbdcb38`,
measured neutral, kept; **small-string optimization** SSO; faster `usize` hasher → in-tree FxHash
Phase 5a `2603fef`). The contained, parity-safe interpreter backlog is **spent**. What remains is
Tier-3 (`future.md §4`): **#6 Cranelift JIT** (end-game, runs on the current 16 B `Value`), **#7
NaN-box** (BLOCKED — full i64, see above), **#8 register VM / gen-GC** (low ROI). Per the diagnosis,
interpreter levers can only *narrow* the gap — JIT is the only path to *match/beat* CPython on compute.

## Hoist the per-entry `Arc::clone` out of `run_until` (2026-06-12)

The call-flatten (`634c6f5`) killed the *per-call* `run_until` recursion, leaving one
`Arc::clone(&self.program)` at **`run_until` entry** (`mod.rs:2095`) — an atomic refcount
bump+drop that was pure borrow-checker tax (a second owner so `op = &program.protos[…]` doesn't
alias `&mut self` in `step`). Replaced with a raw `*const Program` borrow: `self.program` is an
immutable `Arc` never reassigned after `Vm::new` (verified — zero `self.program =` sites;
`swap_ctx` swaps heap/frames/stack, not `program`), so the pointee outlives the loop. VM-only;
frozen interp untouched.

**Why the standard suite is the wrong place to measure it:** post-flatten, `run_until` is entered
per *top-level run* + per *native re-entry* (HOF callbacks, operator-overload `compare`, deferred
calls) + per *fiber resume* — **not** per call. The 8 standard benches use **no HOFs**, so they
enter `run_until` ~once → the lever is invisible there (as predicted). Added `benches/chz/hof.chz`
(1000-elem list × 2000 passes of `map`+`filter`+`fold`, ~6M `run_until` entries via per-element
`invoke_value`→`run_proto`→`run_until`) to measure where it actually pays.

| bench   | before (ms) | after (ms) | result |
|---------|------------:|-----------:|--------|
| `hof`   | 383.0 ±13.4 | 363.1 ±11.6 | **1.05× faster** (A/B 30 runs) |
| `fib`   |           — |          — | within noise (1.02 ±0.05) |
| `struct`|           — |          — | within noise (1.00 ±0.04) |
| `list`  |           — |          — | within noise (1.02 ±0.05) |
| `loop`  |           — |          — | within noise (1.01 ±0.04) |
| `primes`|           — |          — | within noise (1.00 ±0.04) |

**Verdict:** ~5% on callback-heavy code, neutral (non-regressing) on the standard suite. Kept.
Guarded by `native_reentry_hof_compare_defer_parity` (HOF + operator-overload + defer-in-recursion,
VM == interp). 1552 tests green, conformance 7/7, clippy clean.

## Small-string optimization (SSO) — 2026-06-12

`Obj::Str(Box<str>)` → `Obj::Str(ChzStr)` (`src/vm/chzstr.rs`). `ChzStr` stores strings ≤
`INLINE_CAP` (22 UTF-8 bytes) **inline** in the enum variant (no per-value `Box<str>` heap alloc);
longer strings spill to a `Box<str>`. The `str` bench builds 500k `"item-N"` parts (all ≤11 bytes)
into a list before `,".join` — pre-SSO that was 500k inner `Box` allocs (and 500k retained heap
objects for the GC to mark/sweep); now zero on both counts, so the win is fewer allocs **and** less
GC pressure. (SSO therefore helps any string-*retaining* loop most; a string-churning loop that
frees immediately benefits less.) `Deref<str>` +
`From<&str>/<String>/<Box<str>>` kept all ~100 `Obj::Str` match arms + `"x".into()` test
constructors compiling unchanged; `size_of::<Obj>()` stayed **88 B** (pinned by a guard test —
`Module` dominates, the `Str` variant went 16→24 B, well under; `Closure` later shrank to 64 B under
the positional-captures lever below, so `Module` is now the sole cap).

| bench  | before (ms) | after (ms) | vs CPython | result |
|--------|------------:|-----------:|-----------:|--------|
| `str`  | 217.3 ±3.7  | 173.7 ±2.1 | 2.62×→**2.10×** | **−20%** (output identical: `5888889`) |
| `list` |           — | 460.4 ±10.5 | — | neutral (ints are unboxed `Value` — no per-element box) |
| `loop` |           — | 1107 ±34   | — | neutral |
| `fib`  |           — | 278.3 ±7.7  | — | neutral |

**Why `list` (int) doesn't move — it's at the parity floor.** `for x in xs` snapshots the iterand
(`Op::ListClone`, `mod.rs`) so a body mutating the list doesn't disturb iteration. The **frozen
interpreter snapshots identically** (`exec_for` → `iter_rows_from_value`, "clone out so a body that
mutates the collection doesn't disturb iteration"), so dropping the clone would diverge — it's
mandated by parity, not an oversight. Ints already ride unboxed in the 16 B `Value`, so there's no
per-element box to kill. SSO instead helps any string-heavy list workload (incl. the `str` bench's
`parts` list of 500k now-inline strings).

**Guarded by:** `ChzStr` unit tests (inline/heap selection at the 22-byte boundary, multi-byte
UTF-8 by bytes-not-chars, `Eq`/`Hash` content-equal to `Box<str>`), `obj_size_unchanged_by_sso`,
`vm_alloc_str_inlines_short_spills_long` (production-path wiring), and
`sso_boundary_string_ops_parity` (concat/split/join/index/iterate/`==`/map-key across the boundary,
VM == interp). 1565 tests green, conformance 7/7, `clippy --all-targets` clean. VM-only; frozen
interp untouched.

## M19 Phase 6 — method-call inline cache + flatten `do_method_call` — 2026-06-12

`Op::CallMethod` carries a per-call-site `ic` id (dense `0..method_ic_sites`, allocated in the
compiler like `field_ic`). On a struct receiver the VM caches `(tid → proto, module_idx)` in a
per-`Vm` `method_ic` vector: a hit on a matching layout `tid` skips the `program.structs.get(name)`
clone **and** the name-keyed `def.methods.get(method)` probe — collapsing dispatch to one int
compare — **and** flattens the call (pushes the method frame in place, like the `Op::Call`
flatten, so the running `run_until` executes the body instead of a re-entrant `run_proto`). The cell
holds proto id + module index, **no `GcRef`**, so it is invisible to GC / snapshots / `swap_ctx` —
the same heap-independence trick the field IC uses. Native-re-entry callers (`spawn`/`defer` method
tasks) pass `NO_IC` and keep the synchronous `run_proto` path; a real `ic` is exactly the
"flatten-safe, called-from-the-dispatch-loop" signal.

Measured as **vs-CPython ratio** (lower = faster Chezzi; 3-run median, the ms are stable to ±noise):

| bench    | before | after | result |
|----------|-------:|------:|--------|
| `struct` | 2.90×  | **2.63×** | **−9%** (the predicted target moved) |
| `map`    | 2.78×  | 2.79× | neutral (its `.values()`/`.next()` sites are minor vs the hash work) |
| `fib`/`str`/`primes`/`loop`/`list` | — | — | within noise (no struct-method dispatch) |

Only `struct` moved, as predicted — it is the only bench dominated by struct-method calls. Honest
no-mover note: `map`'s synthetic iterator-protocol method sites (`.values()`/`.next()`) are a tiny
fraction of its FxHash-bound cost, so caching them is invisible. **Guarded by:**
`method_ic_sites_allocated_and_vm_presized` (white-box wiring), `method_ic_monomorphic_hot_loop`,
`method_ic_polymorphic_one_site_via_protocol` (a type-erased protocol-generic site hit by two struct
types — the `tid` guard prevents a stale-proto dispatch), `method_ic_under_parallel_engine`,
`method_ic_gc_stress`, `method_ic_function_typed_field_not_cached` (a `fn`-typed field stays on
`invoke_value`, never cached), and `method_call_flatten_deep_recursion_on_small_stack` (recursive
method no longer consumes host stack — the flatten's robustness bonus). VM-only; interp untouched.

## M19 Phase 7 — inline hot opcodes in `run_until` — 2026-06-12

The dispatch loop now handles the hottest opcodes **inline** before delegating the long tail to
`self.step(op, span)`: `GetLocal`/`SetLocal`, the three superinstructions
(`BinLocalLocal`/`BinLocalConst`/`IncLocal`), `Jump`/`JumpIfFalse`, and `Call`/`Return`. Each inlined
arm calls the **same** helper as `step` (`op_bin_local_local`, `do_call`, `do_return`, …) or copies
its 1–3-line body verbatim — one source of truth per op — so the change is purely *where* the op is
dispatched, not *what* it does. The win: the common op skips a function call + the giant `step` match
jump-table on every instruction.

This was the single biggest lever of the session — it moved **every** op-bound bench:

| bench    | before | after | result |
|----------|-------:|------:|--------|
| `loop`   | 1.30×  | **~1.10×** | **−15%** (was "the dispatch floor" — now near CPython parity) |
| `list`   | 3.06×  | **~2.55×** | **−17%** |
| `struct` | 2.63×  | ~2.60× | small further gain (stacks with Phase 6) |
| `primes` | 2.47×  | **~2.27×** | **−8%** |
| `fib`    | 3.43×  | **~3.24×** | **−6%** |
| `str`    | 2.13×  | ~2.03× | −5% |
| `map`    | 2.78×  | ~2.68× | −4% |
| `empty`/startup | 0.09× | 0.09× | unchanged (11× win) |

The improvement scales with op density per unit work — `loop`/`list` (tight per-op loops) move most;
allocation/hash-bound benches move least. Consistent across 3 runs, all beyond the per-bench σ.
**Behavior-preserving:** the inline arms are byte-identical to `step`'s; the full suite + every
two-engine golden + `inlined_hot_ops_path_matches_step` (hammers all inlined ops in one program, VM
== interp) stay green. VM-only; interp untouched. 1573 tests green, conformance 7/7, clippy clean.

## M19 Phase 8 — call-site specialization for `Op::Call` — analyzed, DEFERRED (no-gain) — 2026-06-12

Ranked Tier-1 #3 (target `fib`), but analysis after Phase 7 shows the premise is largely spent:
- **Call-flatten (Phase 7 inline) already made `do_call`'s happy path lean** — the deref a call-IC
  would skip is one `heap.get(h)` Vec-index + a 3-way match (~2–3 instructions); `check_arity` is a
  single compare on success. fib's dominant residual is **frame construction** in `finish_frame` (the
  Nil-slot fill + `CallFrame` push), which a *dispatch* cache does not touch.
- **A correct call-IC cannot avoid `GcRef`s the way the method-IC did.** The method-IC keys on a
  heap-independent `tid` read cheaply off the receiver; but the callee *is* a heap object, so any
  call-site cache key is a heap-specific handle → a `swap_ctx` invalidation hazard for cooperative
  fibers. That is real complexity/risk for ~0 measurable gain — below the M19 bar.

fib's genuine next lever is **Tier 2 (PEP 659 adaptive quickening)** or **Tier 3 (Cranelift JIT)** —
separate milestones, not an in-loop tweak. Deferred deliberately, not forgotten.

## M19 Tier-2 — index-access specialization (`GetIndex`/`SetIndex`) — 2026-06-12

Two composing, behavior-preserving levers on the index ops:

- **Int-key fast path** inside `get_index`/`set_index` (`src/vm/mod.rs`): a `List` or `Map` indexed by
  an `Int` skips `hash_key_rooted`'s operand-stack push/pop. That rooting exists to survive a *struct*
  key's re-entrant `hash()` + GC; for an `Int` key, `scalar_hash` is allocation-free, infallible, and
  can't re-enter, so the rooting is pure waste. The `candidates` + `values_equal` probe is unchanged —
  only the *hashing* is shortcut — so an `Int` key still matches a `values_equal` `Float` key.
- **Inline dispatch** (Phase-7 style): `Op::GetIndex`/`Op::SetIndex` are handled directly in the
  `run_until` hot-op match (calling the same helpers), skipping the `step` call + giant match jump.

**Measured (absolute chezzi time — the reliable signal; CPython ratios are noisy run-to-run):**

| bench  | before | after | result |
|--------|-------:|------:|--------|
| `list` | 393.7 ms | **378.5 ms** | **−4%** (consistent across 3+ runs, beyond σ) |
| `map`  | 226.4 ms | 225.7 ms | **neutral** |

**The lever moved a *different* bench than predicted** — the project's recurring lesson (measure, don't
trust the a-priori guess). `map` was the target (its `m[i]=i*2` + `total += m[j]` are index-bound) but
came out neutral: `map` is **FxHashMap-probe-bound**, not rooting/dispatch-bound — the push/pop the
fast path removes is a rounding error next to the hash-table lookup. `list`, *not* expected to move
(its bench is `push` + `for`), gained ~4% instead: `for x in xs` lowers to a per-element `Op::GetIndex`
(`src/compiler/mod.rs:823`), so the 2M-element loop runs through the inline Int-`List` fast path 2M
times — the inline-dispatch + no-rooting win lands on iteration, not on map indexing.

**Behavior-preserving:** 7 new VM==interp regression guards (`idxspec_*`, incl. the `Int(3)`/`Float(3.0)`
key-collision trap and the struct-`Index`-protocol fallback) pin every result + error string before and
after; full suite 1607 green, clippy clean. Kept as a principled cleanup (removes genuine waste + a real
list-iteration win) in the spirit of Phase 5b's neutral-but-principled `tid` guard. To actually move
`map` needs a bigger lever (map-shape specialization / a denser int-keyed representation) — out of scope
for a behavior-preserving in-place tweak.

## M19 Tier-2 — adaptive opcode quickening (PEP 659), v1: binops — 2026-06-13

The single most CPython-3.14-like lever, scoped to its highest-probability first target. The static
superinstructions (`BinLocalLocal`/`BinLocalConst`) already fuse arith **and ordered compare** for
`local⊕local` / `local⊕const` operands, so the **generic** `Add..GtEq` arms are reached only by
**stack-operand** binops (intermediate expression results), and `Eq`/`NotEq` are excluded from
`BinKind` so they are **never** fused. Those un-fused arms are what quickening specializes.

**Mechanism (mirrors `field_ic`/`method_ic`):** a per-`Vm` side table `quicken: Vec<u8>`, one state
byte per program instruction, keyed by site `quicken_base[pid] + ip` (`quicken_base` = prefix sum of
per-proto `code.len()`). No `Op`/compiler/interpreter change — the bytecode stays a shared read-only
`Arc<Program>`; the table holds only state bytes (no `GcRef`), so it is heap-independent and never
swapped by `swap_ctx`. State machine: `Q_COLD` → observe operand types once → `Q_INT` (int/int fast
path via the already-proven `fast_int_bin`) or `Q_GENERIC` (sticky; a polymorphic site never
thrashes). A non-int operand at a specialized site deopts to `Q_GENERIC`. Handled inline in
`run_until` (not `step`) because the site id needs `pid`+`ip` — which also buys the Phase-7 dispatch
win for these ops for free.

**Measured (absolute chezzi time — the reliable signal; CPython ratios noisy run-to-run):**

| bench    | before  | after   | result |
|----------|--------:|--------:|--------|
| `primes` | ~665 ms | **~614 ms** | **−7–8%** (consistent across runs, beyond σ — the target) |
| `fib`    | ~258 ms | ~254 ms | marginal (~−1.5%, directionally consistent, near σ) |
| `list`   | ~390 ms | ~387 ms | flat (within σ) |
| `struct` | ~437 ms | ~441 ms | flat (within σ) |
| `str` / `loop` / `map` | — | — | flat (alloc- / fully-fused- / hash-probe-bound) |
| `empty` (startup) | ~0.85 ms | ~0.86 ms | unchanged (prefix-sum + table alloc is free) |

**`primes` was the predicted target and it landed** — its inner loop is `n % i == 0` (a never-fused
int `Eq`, previously routed through the heavyweight `values_equal_guarded`) plus stack-operand arith.
Quickening the `Eq` to a direct `as_f64(x)==as_f64(y)` (skipping the depth guard / `is_numeric` /
recursive match) is the bulk of the win. The fused hot loops (`loop`) and alloc/hash-bound benches
(`str`/`map`) don't touch the generic arms, so they stay flat — exactly as scoped.

**The Eq parity gotcha (pinned by test):** the generic numeric `Eq` is **lossy f64**
(`as_f64(a)==as_f64(b)`), so `2^53 == 2^53+1` is **true**. The quickened int path deliberately
replicates that loss rather than doing exact `x==y` — exactness would change observable output and
break two-engine parity. `quicken_eq_preserves_lossy_f64_semantics` locks the behavior; any future
"fix" to int equality must change the generic path as a separate non-perf change.

**Behavior-preserving:** 6 new VM/parity guards (`quicken_*`: table presizing + prefix-sum,
lossy-f64 Eq, small-int Eq, int→float→str deopt, stack arith/compare fast path, overflow/div-zero
error parity). Full suite 1613 green, conformance green, clippy clean. v1 also lays the reusable
side-table machinery the docs call the unifying base for later index/call quickening (out of scope
here — `GetIndex` already has an Int-key fast path; `Op::Call` spec was deferred for ~0 gain).

## M19 — denser int-keyed map/set index representation — 2026-06-13

The `map` bench (`benches/chz/map.chz`: 200k distinct int keys, 1M lookups) was the last
**hash-probe-bound** bench — the Tier-2 Int-key fast path came out **neutral** on it because it only
skipped operand rooting, not the probe (logged above). Root cause (`src/vm/heap.rs`): the index was
`FxHashMap<u64, Vec<usize>>`, so each distinct key paid one tiny `Vec<usize>` heap allocation (200k of
them) plus a pointer-chase per lookup. But numeric keys hash injectively (`(n as f64).to_bits()`), so
**every candidate list is length 1** — the `Vec` is pure overhead.

**Lever:** collapse the per-key `Vec` to an inline single position. Added
`enum Pos { One(usize), Many(Box<Vec<usize>>) }` and extracted the (formerly duplicated, identical)
index logic from `MapData`/`SetData` into one shared `HashIndex(FxHashMap<u64, Pos>)`:
`candidates` (One → `slice::from_ref`, Many → `as_slice`, absent → `&[]`), `insert`
(absent → `One`; first collision → upgrade to `Many` carrying both positions; further → push), `clear`.
`Pos::One` is **zero-alloc** and inline; `Pos::Many` (a real hash collision only — string
`DefaultHasher`, or a user `hash()` returning a constant) is `Box`ed to keep `Pos` at 2 words so
`MapData`/`SetData` size is unchanged. `MapData::candidates`/`push` keep their exact signatures, so the
VM hot paths in `src/vm/mod.rs` needed **zero change**. `remove_at`→`rebuild_index` re-inserts through
the same `HashIndex::insert`, so the One→Many upgrade is identical on rebuild and initial push.

**Measured** (same machine; `cargo run --release -- run benches/run.chz`, ≥2 runs):

| bench  | before (~) | after | result |
|--------|-----------:|------:|--------|
| `map`  | 2.68× CPython / ~225.7 ms | **1.68–1.72× / ~144.7 ms** | **−36% absolute** (the predicted target landed) |
| `fib` / `loop` / `list` / `struct` / `str` / `primes` | — | — | flat within σ (touch no map/set) |

**The lever moved its predicted target.** Killing 200k tiny allocs + the per-lookup indirection is
the bulk of the win: the probe is now `slice::from_ref` over an inline `usize` (no heap deref) plus the
unchanged `values_equal` confirm. The two-engine parity bar is preserved by construction — the serial
(`parallel=false`) and M:N (`parallel=true`) engines are the same `Vm` and confirm every hash hit with
the same `values_equal` probe, so output is byte-identical.

**Behavior-preserving:** new `dense_index_collision_upgrade_parity` guard (two distinct constant-`hash()`
struct keys land in one bucket, both read back distinctly via the `Many` upgrade) plus the borrowed
`fxhash_constant_hash_collision_still_resolves` (30 constant-hash keys) — both go RED on a `One`-only
stub (key dropped → "key not found"), GREEN with `Many`. `fxhash_map_int_keys_insert_lookup_remove`
(remove→rebuild parity), `idxspec_int_float_key_collision_resolves`, and `fxhash_set_dedup_and_ops`
stay green. Full suite 1712 green, conformance green, clippy clean. **Next suspect for `map`:** the
remaining gap is `values_equal` per-probe cost + `FxHashMap` lookup/rehash (no longer the `Vec` alloc).

**Merge remeasure (2026-06-13, integrated onto current `main` e405d32 — generators + C-ABI FFI now
present).** The lever was developed on an older base (`037f1b6`); re-benched on the merged HEAD:
`map` **166.6 ms ± 9.3 / 1.94× CPython** vs the documented **~225.7 ms / 2.68×** baseline — still a
clear win (**~−26% absolute**). The slightly higher ratio than the branch's `~1.7×` is run-to-run
variance (σ ≈ 6%) plus the heavier merged base; the conclusion (Vec-alloc elimination is the bulk of
the win, `values_equal`/probe is the next suspect) is unchanged. Merged tree: 1832 tests green,
conformance 7/7, clippy `--all-targets -D warnings` clean.

## M19 — N-way polymorphic method-call IC (CALLMETHOD ADAPTIVE) — 2026-06-13

New **`poly_method`** bench (`benches/chz/poly_method.chz`: a protocol `Shape.area()` implemented by
four distinct struct types, a heterogeneous `List[Shape]` walked at ONE `.area()` call site, ~4M
method calls) — a genuinely **megamorphic** `CallMethod` site. Measured baseline on the merged HEAD
(`2a934a8`): **1.886 s ± 0.003 / 6.0× CPython** — *worse* than the monomorphic `struct` bench, because
the existing single-cell `MethodIcCell` holds ONE `tid`. A rotating-4-type site misses on **every**
call, falling to the slow path which (a) re-resolved the dispatch and (b) cloned the whole `StructDef`
(its `fields` Vec + `methods` HashMap) per miss.

**Lever (two complementary fixes, landed together).**
- **Fix B — N-way polymorphic IC.** The single `MethodIcCell` per site becomes an N-way
  `MethodIcSite` (`[MethodIcCell; 4]` + a one-way `sticky` latch). On a miss, fill the next free way;
  once all 4 ways are occupied AND a 5th distinct `tid` arrives, latch `sticky` so the site stops
  probing the ways and goes straight to the slow path — exactly the binop quickening's `Q_GENERIC`
  one-way deopt (a megamorphic site never thrashes). A bounded-megamorphic site (≤4 types) now HITS a
  way for every receiver type and **flattens** (`push_frame_in_place`, no clone, no re-entrant
  `run_proto`). Each way is `tid`- + arity-re-guarded on every hit, so a wrong body can never dispatch.
- **Fix A — clone-free megamorphic slow path.** The slow path resolves `(proto, module_idx)` by
  borrowing `prog.structs.get(name)` (bumping the cheap read-only `Arc<Program>` refcount to release
  the `&mut self` borrow) instead of `.cloned()`-ing the whole `StructDef`. Helps the truly-megamorphic
  / sticky-generic (>4 types) tail that Fix B still sends slow.

The side table holds only ints (tids / proto ids / u32 module indices) — no `GcRef` — so it stays
heap-independent like `field_ic`/`method_ic`/`quicken`: never snapshotted, never swapped in `swap_ctx`.

**Measured** (same machine; `hyperfine --warmup 2 -N -r 3`, serial):

| bench          | before        | after          | result |
|----------------|--------------:|---------------:|--------|
| `poly_method`  | 1.886 s / 6.0× CPython | **1.268 s / 4.28×** | **−33% absolute** |
| `struct` (monomorphic) | ~456 ms | ~456–467 ms (min touches baseline) | flat / noise (has **no** method calls — field-IC bound, untouched) |
| `fib` / `loop` / `list` / `map` / `str` / `primes` | — | — | unaffected (the IC only changes struct-method dispatch) |

**The lever moved its predicted target.** The 33% win is overwhelmingly **Fix B**: the rotating-4
site now flatten-HITS instead of refill-thrashing through the per-miss `StructDef` clone. Fix A only
trims the 1-in-5 sticky tail (the bench's 5th type is not in this 4-type-dominant workload; the golden
`examples/poly_method.chz` adds a 5th `Line` type to exercise the sticky slow path under parity). The
`struct` bench is monomorphic with **zero method calls** (pure field read/write), so it cannot move —
its ±10 ms is pure run-to-run noise (min = baseline mean). Two-engine parity is preserved by
construction (the frozen interpreter has no IC; the IC only changes *which path* computes an identical
result) — `diff` of VM vs `--interp` on the bench workload is byte-identical.

**Behavior-preserving:** five new VM/parity guards — `mega_dispatch_correctness_parity` (4 types, right
body per type across repeated calls, VM==interp), `poly_ic_all_ways_distinct_bodies` (every way a
distinct body; RED on a way-0-only match), `poly_ic_overflow_goes_sticky_generic` (5+ types, correct
dispatch through overflow), `poly_ic_site_latches_sticky_on_5th_type` (white-box: the 5th type latches
`sticky` with all 4 ways filled; RED if sticky never sets), `structdef_clone_free_slow_path_parity`
(megamorphic + a function-typed FIELD call, guards the clone-removal), plus the golden
`examples/poly_method.chz`/`.expected`. Full suite **1838 green**, conformance **7/7**, clippy
`--all-targets -D warnings` clean. **Next suspect for `poly_method`:** the residual ~4.3× is per-op
dispatch + the `for`-loop iterator protocol overhead around the now-fast dispatch, not the IC.

## Cross-nursery flat scheduler (M:N) — 2026-06-14 (behavior-preserving concurrency change)

The cross-nursery circular deadlock fix (M:N `--parallel` only — `examples/parallel_cross_nursery_circular.chz`)
is **not a perf lever**; it touches only the `--parallel` scheduler (`SchedCore` scalar
`{done,total,body_open}` → `Vec<JoinScope>` + flat `slots`, scope-scoped owner stop, global deadlock
predicate, early-enlist-with-deferred-reduce). The serial benches (`benches/run.chz`) do **not** use
`--parallel`, and the new `Vec<JoinScope>` scans are behind a `scopes.len() == 1` fast path, so no
regression was expected or measured:

| bench | before (CPython×, baseline doc) | after (this change) | delta |
|-------|----------------------------------|---------------------|-------|
| fib   | 3.54× | 3.28× | within run-to-run noise (faster, not slower) |
| loop  | 1.32× | 1.13× | within noise |
| primes| ~2.3× | 2.26× | flat |
| list  | ~2.7× | 2.67× | flat |
| struct| ~2.85×| 2.85× | flat |
| poly_method | 4.3× | 4.37× | flat |
| map   | 1.94× | 1.82× | within noise (faster) |
| str   | ~2.1× | 2.10× | flat |
| empty | ~11× faster | 10.82× faster | flat |

The per-fiber `Fiber::scope_id` (one `usize`) and the per-VM `mn_scopes`/`mn_enlisted`/`mn_enlist_sched`
fields add nothing to the hot dispatch loop (the inline body runs with `mn == None` — unchanged serial
path). Full suite **1838 → 1845 green** (4 new MnSched unit tests + the case-A golden + a multi-task
inner-nursery golden + the deadlock guard), conformance **7/7**, clippy `--all-targets -D warnings`
clean. Cross-nursery goldens looped 10–12× under a 30s watchdog (no lost-wakeup from the now-global
parked set; the multi-task case shook out an early-enlist-vs-deadlock-predicate race, since fixed by
enlisting outer scopes BEFORE farming any helper — see
`examples/parallel_cross_nursery_fanout.chz`).

## After M19 memory-layout lever #1 — positional struct layout — 2026-06-16 (same machine)

`Obj::Struct` instance fields changed from `Vec<(Box<str>, Value)>` to a flat positional `Vec<Value>`
(hidden-class / `__slots__` layout). Field names now live only in `StructDef` (resolved on the cold
Display/probe-miss/wire/snap path); the hot field read/write (IC-guarded on `tid`) is a pure
`fields[idx]`. This kills the **N per-field `Box<str>` allocations per struct instantiation** plus the
per-field name-clone on `==`. The synthetic native structs `Match`/`Response` are now registered in
`Program.structs` so the runtime can recover their declaration-order field names.

**Caveat (predicted in `gaps.md`):** the bench suite is **dispatch/call/alloc-bound, NOT layout-bound**,
and the `struct` bench reuses a small fixed set of instances rather than constructing in a hot loop, so
the suite reads perf-neutral — as expected. The value is the alloc reduction + JIT groundwork
(positional storage → constant field offsets the Cranelift codegen needs).

| bench    | before (CPython×) | after (CPython×) | delta |
|----------|-------------------|------------------|-------|
| fib      | 3.14× | 3.23× | within noise |
| str      | 2.07× | 2.12× | within noise |
| primes   | 2.24× | 2.33× | within noise |
| loop     | 1.11× | 1.16× | within noise |
| list     | 2.67× | 2.60× | within noise |
| struct   | 2.80× | 2.65× | within noise (slightly closer to CPython) |
| poly_method | 4.29× | 4.42× | within noise |
| map      | 1.83× | 1.83× | flat |
| empty    | 10.37× faster | 10.26× faster | flat |

**Alloc win (the real payoff), measured directly.** A struct-construction-heavy micro
(`P(a,b,c,d)` built 2,000,000× in a `while` loop, 4 fields) was timed before vs after with `hyperfine`
(8 runs, 2 warmup):

| micro                         | before (Vec of tuples) | after (positional) | delta |
|-------------------------------|------------------------|--------------------|-------|
| 2M × 4-field struct construct | 826.9 ms ± 21.0 ms     | 510.2 ms ± 16.3 ms | **−38%** |

That's the 4 fewer `Box<str>` allocs per instantiation (one per field) made visible. Full suite
**1968 green** (+2: the positional-layout type guard + the two-engine `struct_layout.chz` golden),
conformance **7/7**, clippy `--all-targets -D warnings` clean. Two-engine parity preserved (the interp
is the frozen oracle and was left untouched — both engines iterate fields in declaration order, so
Display/`==`/interpolation stay byte-identical); wire (default) + snap (`--parallel`) struct
round-trips verified to preserve field names + Display.

## M19 memory layout #3 — positional closure captures — 2026-06-16

`Obj::Closure { captured: HashMap<String, Value>, .. }` → `captured: Vec<Value>` indexed by a
compile-time slot. Was: a `HashMap` (~48 B + interned string keys) allocated **per closure
instantiation**, plus a **string hash + probe on every `GetCaptured`**. Now: `Op::GetCaptured(String)`
→ `GetCaptured(u32)` is a pure `captured[slot]` index (no hashing); the capture set is static per proto
(the compiler already knew the `CapSrc` order at `MakeClosure`), so slots are assigned in snapshot
order in the compiler. Capture names moved off every instance into `Proto.capture_names` (cold-path
only: the home-global fallback, error messages, and the wire/snap name carrying that crosses
`spawn`/`Channel`). Nested captures (an inner closure capturing an enclosing closure's capture) map by
`CapSrc::Captured(parent_slot)` stamped at compile time, so `MakeClosure`/`do_spawn_block` read the
parent's `captured[parent_slot]` with no name. This also speeds the per-`spawn` deep-clone (a `Vec`
walk, no `HashMap` rebuild). JIT groundwork: positional captures → constant capture offsets for the
future Cranelift codegen.

**Standard suite reads NEUTRAL** (it has no closure-construction-heavy bench; `hof`'s callbacks are
top-level fns that capture nothing). Re-measured the hot benches before/after to confirm no regression:

| bench  | before/after | result |
|--------|-------------:|--------|
| `fib`  | 1.00× | within noise |
| `loop` | 1.02× | within noise |
| `hof`  | 1.01× | within noise |
| `struct` | 1.04× | within noise (faster) |

**Micro (the real delta).** Like the lever-#1 struct micro (flat suite, −38% on a construction micro),
the win shows on a closure-construction + capture-read micro — `benches/chz/closure.chz`: 3M iterations
each building a closure that captures 3 vars (`MakeClosure` → a 3-slot `Vec`) then calling it
(`GetCaptured` ×3). `hyperfine -N --warmup 3 --min-runs 12`, serial, same machine:

| micro | before (HashMap) | after (Vec slot) | delta |
|-------|-----------------:|-----------------:|-------|
| `closure.chz` | 1.671 s ±0.057 | 0.914 s ±0.034 | **−45% (1.83× faster)** (output identical: `9000006000000`) |

`size_of::<Obj>()` stayed **88 B** (guard `chzstr.rs:205`): `Closure` shrank from 88 B to **64 B**
(HashMap 48 B → Vec 24 B), and `Module` (Box<str> 16 + Vec 24 + HashMap 48 = 88) is now the sole cap.

**Guarded by:** characterization parity tests (1-var, multi-var, nested, deep 3-level, shared-mutable-
box, HOF callbacks, hot-loop read, closure-across-`spawn`) + the `examples/closure_capture.chz` golden
(byte-identical on VM / interp / `--parallel`) + compiler op-shape tests (`GetCaptured(u32)`, distinct
slots, `Proto.capture_names` in slot order). Full suite **1979 green**, conformance **7/7**, clippy
`--all-targets -D warnings` clean. VM-only; the frozen interp (whose `Closure.captured` stays a
`Vec<HashMap<…>>`) is untouched — parity holds because `GetCaptured` is pure compute and iteration
order (which determines observable output) is unchanged.

## M19 memory layout #2 — enum variant_id — 2026-06-16 (completes the #1→#3→#2 sequence)

`Obj::Enum { ty: Box<str>, variant: Box<str>, payload }` → `Obj::Enum { variant_id: u32, payload }`,
the enum analogue of struct `tid` (lever #1). Was: **two `Box<str>` allocated per enum instantiation**
(the type name + variant name, both program-global static) plus, on every `match` arm, a **variant-name
string compare**, and on `==` an `ty==ty && variant==variant` string compare. Now: a single dense
`variant_id: u32` stamped at construction; match-arm dispatch, equality, and `?` are **pure-int
compares**; the type + variant names resolve from a new `Program::variants_by_id` table on the **cold
path only** (Display/stringify/error/wire/snap). Native `Ok`/`Err`/`Some`/`None` get the **reserved**
fixed ids `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3) (registered first; user variants
follow at `4..`, so the native range is **disjoint** from every user id), so `?` (`do_try`) and
top-level-error gating compare against compile-time constants. A user enum is allowed to **shadow** a
native name (`enum Foo: Some(int)` — `main` permits this); the native construction path
(`alloc_enum` — list `pop`, regex/json/fs, `?`-desugar) stamps the **fixed `VID_*` constant directly**,
never a `variants[name]` lookup (which the user variant shadows), so a genuine native Option/Result is
never given the user's id — equality and `?` stay correct. `Op::NewEnum` and `Op::MatchArm` carry the
compile-time id; wire/snap carry the dense `variant_id` **directly** (single shared `Arc<Program>` ⇒
meaningful on both sides — carrying the id, not the name, preserves native-vs-user identity under name
shadowing). JIT groundwork: numeric variant id → constant / jump-table dispatch for the future
Cranelift codegen + match-on-enum.

> **Parity fix (2026-06-16).** The first cut of this lever resolved native Option/Result construction
> through `Vm::variant_id("Some")`, a name lookup the `variants` map shadows. A user enum declaring
> `Some`/`None`/`Ok`/`Err` therefore stamped its own id onto genuine native values, collapsing
> native-vs-user identity (`==` wrongly `true`) and missing `?`'s `variant_id == VID_SOME` gate (`'?'
> expects Result or Option, found enum`) — a VM-vs-interp divergence on well-typed programs. Fixed by
> stamping the reserved `VID_*` constant directly in `alloc_enum` and carrying the `variant_id` (not the
> name) on the wire/snap paths. No perf change (the enum micro doesn't touch these paths; re-measured
> A/B = 1.04× ± 0.05, within noise). Guarded by `user_variant_shadow_does_not_collapse_native_option_equality`,
> `try_operator_works_on_native_option_under_variant_shadow`, and a shadowing section in
> `examples/enum_layout.chz`.

**Standard suite reads NEUTRAL** (it has no enum-construction-heavy bench; the alloc/dispatch saving
isn't on the hot suite paths). Re-measured `fib` before/after to confirm no regression: **1.00×** (within
noise).

**Micro (the real delta).** `benches/chz/enum.chz`: 3M iterations each building three enum instances
(`Circle(i)`, `Rect(i,2)`, `Dot` — `NewEnum`) and matching each (`MatchArm` dispatch).
`hyperfine -w 2 -r 8`, serial, same machine:

| micro | before (two `Box<str>`) | after (`variant_id: u32`) | delta |
|-------|------------------------:|--------------------------:|-------|
| `enum.chz` | 2.688 s ±0.060 | 2.155 s ±0.094 | **−20% (1.25× faster)** (output identical: `9000004499997500000`) |

`size_of::<Obj>()` stayed **88 B** (guard `chzstr.rs:205`): `Enum` shrank from 56 B to **32 B** (two
`Box<str>` 32 B → one `u32`), and `Module` remains the sole cap.

**Guarded by:** the `enum_variant_id_stamped_at_construction` type-level guard (the new `Obj::Enum`
shape won't destructure the old way), `native_result_option_have_fixed_variant_ids`,
`match_arm_dispatches_by_variant_id` (asserts the emitted op carries the dense id), and the
`examples/enum_layout.chz` golden (nullary + payload + generic `Option`/`Result`, exhaustive match,
match-with-binding, guard, `==` equal/ne-by-variant/ne-by-enum-type, Display + interpolation, nested
enums, `?`, and an enum crossing `spawn`+`Channel`) byte-identical on VM / interp / `--parallel`. Full
suite **1985 green**, conformance **7/7**, clippy `--all-targets -D warnings` clean. VM-only; the frozen
interp (name-keyed enums) is untouched — parity holds by identical observable output.

## Uniform by-reference capture — 2026-07-09 (perf-neutral by construction)

The capture-by-reference milestone boxes a local into a heap `Obj::Cell` **only when it is captured**
by a nested closure/`spawn:`/`defer:` — a local no capturing construct closes over stays a plain
positional slot with zero added cost. **No tracked bench captures anything**, so none box and the
headline numbers are unmoved: `fib`/`str`/`list`/`primes`/`loop` are recursion / f-strings / list
push / integer arithmetic (no capturing closures), and the current `run.chz` set (`poly_method` method
dispatch, `map` = **hashmap** ops not `.map`, `empty` startup) likewise captures nothing. Spot re-run
this milestone (same machine, `hyperfine`): `poly_method` 1.371 s (4.52× CPython), `map` 168 ms
(1.98×), `empty`/startup 1.8 ms (**4.95× faster** than CPython) — all consistent with the standing
gaps, no regression. The cell cost lands **only on captured locals** (one heap alloc + one indirection
per captured local); a capture-heavy workload would pay it, but the suite has no such bench to isolate
the delta. A later escape-analysis lever could unbox a cell that provably never escapes its frame.
Full `--lib` **3221 green**, both-engine parity clean, conformance **7/7**, clippy `-D warnings` clean.

## Interactive CLI — streaming stdout — 2026-07-13 (bench-neutral; print-in-a-loop pays a real cost)

`chezzi run` now hands each `print` to a background writer thread (one per stream) that does the real
`write_all` on the process's stdout/stderr, instead of accumulating the run into a `String` flushed once
at exit. Rust's `Stdout` is a `LineWriter`, so that is **one `write` syscall per line** where it used to
be a `push_str` — plus a channel send. (The writer thread is not decoration: an inline `write(2)` on a
fiber blocks a core worker in the kernel, so one stalled reader would starve every other task — the D5
`Kind::Blocking` invariant. See `tests/interactive.rs::stalled_reader_does_not_starve_other_tasks`.)

Tracked suite (same machine, `hyperfine`, `benches/run.chz`, before → after, Chezzi ms):
`fib` 262.6 → 266.7 · `str` 176.4 → 177.7 · `primes` 658.6 → 649.1 · `loop` 979.8 → 992.1 ·
`list` 398.5 → 407.9 · `struct` 455.8 → 465.2 · `poly_method` 1335 → 1347 · `map` 153.0 → 156.0 ·
`empty` 1.9 → 1.9. **All within run-to-run noise** — every bench prints exactly once, so the suite
cannot see the change.

The cost is real where it exists, and it is not hidden: an ad-hoc **200 000-line print loop**
(`for i in range(200000): print("line", i)`, stdout → `/dev/null`) goes **0.048 s → 0.102 s (~2.1×
slower)** — ~270 ns per line for the syscall + the queue handoff. That is the price of output that
actually appears when it happens, from a task that cannot stall the engine. A `BufWriter` would erase
the syscall cost — and would also break "a killed/hung program retains the output it already produced",
which is one of the milestone's acceptance tests. The captured (test/embedder) sink is untouched: it
still `push_str`s, so the parity suite's cost is unchanged.

**Follow-up fix (same milestone), re-measured.** The writer thread now `flush`es every message (the
streamed handles are unbuffered, so a `print(x, end="")` partial line appears immediately instead of
sitting in `Stdout`'s `LineWriter`), and the VM no longer waits on the writer at any seam. Cost: **nil**
— a newline-terminated `print` already forced a `write` through the `LineWriter`, so the extra `flush`
is a no-op memcheck. Re-measured on the same machine: print loop **0.101 s** (was 0.102 s), tracked
suite `fib` 262.0 · `str` 178.7 · `primes` 653.2 · `loop` 992.8 · `list` 406.5 · `struct` 472.6 ·
`poly_method` 1374 · `map` 156.1 · `empty` 1.9 — all within noise of the numbers above.


## Cancellation points — the per-instruction cancel check leaves the hot path — 2026-07-14

Cancel is now observed only at **checkpoints** (loop back-edges + blocking/park ops), so `run_until`'s
dispatch loop no longer does an `Option` deref + relaxed atomic load + branch **per instruction**
(`src/vm/exec.rs`; the check moved to `Vm::jump_checked`, i.e. backward `Op::Jump` only). Correctness
rationale in `docs/gaps.md` §N6 — the speedup is a side-effect, not the goal.

Same machine, `benches/run.chz` (hyperfine, CPython ratio; lower is better), before → after:
`loop` 1.32× → **1.12×** · `fib` 3.54× → **3.16×** · `map` 1.98× → **1.88×** · `poly_method` 4.52× →
**4.49×** · this run's others: `str` 2.12× · `primes` 2.20× · `list` 2.55× · `struct` 2.76×. The dispatch-bound
benches (`loop`, `fib`) move the most, as expected for a removed per-op load+branch; the rest are within
run-to-run noise. Single run, not a median-of-N — treat the small deltas as noise and the `loop`/`fib`
direction as real.

**Re-measured after the adversarial-review fixes** (the cancellation checkpoint added at `Vm::guarded`,
i.e. once per native→user-code re-entry — `map`/`filter`/`fold`/`sort` callbacks; see `docs/gaps.md` N6c),
same machine, quiescent: `loop` **1.15×** · `fib` **3.29×** · `map` **1.86×** · `poly_method` 4.34× ·
`str` 2.10× · `primes` 2.08× · `list` 2.49× · `struct` 2.75× · `empty` **4.55× faster**. `map` is the bench
that pays for the new checkpoint (one relaxed atomic load per element) and it did **not** move (1.88× →
1.86×, within noise) — the check is off the bytecode dispatch path entirely.

**Re-measured after adversarial-review round 2** (`Vm::cancel_requested` — the one predicate every
checkpoint calls; it adds an `is_empty`-style read of the enclosing-scope flags, only at checkpoints,
never on the dispatch path): `loop` **1.13×** · `fib` **3.29×** · `str` 2.15× · `primes` 2.28× · `list`
2.51× · `struct` 2.70× · `poly_method` 4.53× · `map` **1.88×** · `empty` **4.63× faster**. Unmoved from
the numbers above (single run, treat sub-0.1× deltas as noise).

## 8-byte `Value` (int-favoring pointer-tag) — M19 memory-layout lever (2026-07-18)

`Value` shrank 16 B → 8 B: `struct Value(u64)` with a low-bit tag (`bit0=1` → inline `Int` `(n<<1)|1`,
±2^62; low3 `000`=`Obj`, `010`=`Float`→`Obj::FloatBox`, `100`=`Nil`/`False`/`True`). Wide ints and every
`f64` box on the heap (`Obj::BigInt`/`Obj::FloatBox`). Behavior-preserving; int `==`/order exact-i64;
two-engine parity + difftest green. Design/plan: `~/.claude/plans/2026-07-18-8b-value-pointer-tag-*.md`.

**Direct before/after, same machine + session** (hyperfine mean, Chezzi ms; main@`ccbd3c4` 16 B → merged 8 B):
`fib` 270.1→**248.0** (−8.2%) · `loop` 1057→**958** (−9.4%) · `map` 164.6→**152.9** (−7.1%) ·
`list` 412.3→394.5 (−4.3%) · `struct` 447.7→433.3 (−3.2%) · `poly_method` 1340→1285 (−4.1%) ·
`str` 178.2→179.7 (+0.8%) · `primes` 637.7→654.6 (+2.7%, within its ±77 ms band). The dispatch-floor
benches we feared (`loop`, `fib`) got **faster** — the cache-density win beat the tag decode/encode tax.

**CPython ratios after (lower = better):** `loop` **1.03×** (was 1.13× — near parity) · `fib` **2.95×**
(was 3.29× — first sub-3×) · `map` **1.77×** (was 1.88×) · `primes` 2.26× · `str` 2.19× · `list` 2.49× ·
`struct` 2.60× · `poly_method` **3.94×** (was 4.53×) · `empty` 4.5× faster.

**Memory:** `Heap::live_bytes()` on `benches/run.chz` moved 24277 → 23997 (−1.2%) — small because the heap
metric is dominated by the unchanged 88 B `Obj` slot; the real footprint win is the operand/`CallFrame`
stacks (`Vec<Value>` halved), which don't show in `live_bytes` but surface as the speedups above. Float
boxing adds a heap slot per non-inline float — no measured regression on this (int-heavy) bench set, so
float-constant interning (plan Task 5) is **deferred** until a float-heavy workload shows the cost.

## Unified native-handle dispatch prefix — behavior-preserving refactor (2026-07-22)

Deduped the check-OK/run-fault-prone dispatch prefix on two seams: the VM `do_method_call`'s eight
per-handle `if matches!(Obj::X)` arms collapse into ONE `match self.heap.get(h)` yielding a
`Some("<key>")`, and the checker's reserved-handle method arms share `resolve_native_handle_method`.
Purely structural — zero semantic change, no stdlib change; every existing test + two-engine parity
stays green. Not a perf lever, but the VM fold **removes the eight `if matches!` probes that used to sit
on the hot list/map/set/struct method path** (a `None` from the one match now falls straight to
`core_method`), so if anything the method-heavy benches trend slightly better.

**Direct before/after, same machine + session** (hyperfine mean, Chezzi ms; per-arm to folded):
`loop` 1006 to **1010** (+0.4%) · `list` 404 to **403** (-0.2%) · `struct` 429 to **417** (-2.8%) ·
`poly_method` 1287 to **1334** (+3.6%, within its +/-66 ms band) · `map` 139 to **141** (+1.4%). All
within the +/-2-6 % run-to-run noise on this machine — **flat**, as expected for a cold-arm refactor.
The concurrency/io handle arms (`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`Writer`/
`Reader`) are not exercised by any bench, so no fib/loop/primes movement was possible.

## AtomicInt — lock-free int atomic vs Mutex-backed Atomic (2026-07-22)

Bespoke **contention** microbench (NOT in `benches/run.chz`, which is Chezzi-vs-CPython peers only —
this compares two Chezzi constructs). A `parallel:` nursery, **8 tasks × 2,000,000 `add(1)`** on one
shared box (16M increments total), default M:N engine, `cargo run --release`. Median of 5 (includes
constant process/startup overhead, so the pure-RMW ratio is if anything slightly higher):

| Construct | backing | median | runs (s) |
|-----------|---------|--------|----------|
| `AtomicInt` | lock-free `AtomicI64` (checked CAS-loop) | **1.73 s** | 1.44 / 1.71 / 1.73 / 1.76 / 2.00 |
| `Atomic(0)` | `Mutex<WireValue>` | 4.73 s | 4.72 / 4.73 / 4.73 / 4.74 / 4.82 |

**`AtomicInt` is ~2.73× faster under 8-way contention** — the lock-free `compare_exchange` retry beats
the Mutex's lock/unlock + wire-value round-trip on every increment. This **exceeds** the 1.85× the
discarded generic `AtomicI64`-fast-path attempt measured (it was capped by the type-blind runtime
sniffing it had to do). **Uncontended** (single task) the two are within run-to-run noise — the win is
purely a contention story, exactly as predicted. No M19 bench (`fib`/`loop`/`primes`) is affected;
AtomicInt is a new stdlib construct, not a VM-wide lever.

## Cached wire-core GC summary — gaps.md W6-7 (2026-07-27) — correctness-shaped perf fix

Holding a big container in a `Shared`/`RwShared`/`Channel`/`Atomic`/`Executor` used to make the whole
program **quadratic**: the stored value lives outside the GC heap as one `WireValue` tree, `Heap::children`
re-walked that entire tree on **every** GC pass, and because the GC threshold is object-COUNT based
(`next_gc = 2*live`) while a big wire container is ONE heap slot, `live` stayed tiny → GC ran constantly →
O(allocations × payload). Each core now caches `(approximate owned bytes, can-this-payload-root-a-heap-object)`
at store time; a payload with no `Handle` and no nested core is **skipped**, so the per-pass cost is O(1).
The short-circuit alone restored linearity, so this lever needed no pacing change. GC pacing (`next_gc`)
was later made byte-aware for W6-10's sampling half, but **only when `chezzi test --max-heap` sets a cap**
(`mem_cap != 0`) — with no cap the trigger is bit-for-bit the object count it has always been, which is why
the table below (cap-off, `chezzi run`) is unmoved by that change.

Bespoke microbench (NOT in `benches/run.chz`, which is Chezzi-vs-CPython peers only). Release, `--serial`,
best of 3, same machine + session. A 200k-int container is built, handed to a holder, then a sibling loop
allocates n times (each allocation is a GC-threshold tick):

| n | `RwShared` holder — before | after | plain `List` control |
|---|---|---|---|
| 100 000 | 0.447 s | **0.069 s** (6.5×) | 0.061 s |
| 200 000 | 1.946 s | **0.203 s** (9.6×) | 0.196 s |
| 400 000 | 7.916 s | **1.101 s** (7.2×) | 1.203 s |

Before: **4.35× / 4.07× per 2× n — quadratic**. After: the wire-payload holder **tracks the plain-`List`
control at every n**. (The control's own jump at 400k is a pre-existing heap-growth effect — identical
before and after — not part of this lever.)

Holder isolation at n = 200 000 (same allocation loop, same live 200k-int container, only the holder differs):

| holder | before | after |
|---|---|---|
| plain `List` | 0.181 s | 0.195 s |
| `RwShared` | 1.766 s | **0.218 s** (8.1×) |
| `Shared` | 2.051 s | **0.204 s** (10.0×) |
| `Channel.send` | 2.050 s | **0.220 s** (9.3×) |
| no holder | 0.196 s | 0.201 s |

The holder penalty is **gone** on the GC/read side — holding a big container in a core now costs the same
per GC pass as holding it in a plain `List`. (The *store* side is not free: each `set`/`send`/`store` adds
one `wire_summary` walk of the new payload — quantified below.) Traversals whose payload
elements are themselves heap objects (an `RwShared[List[str]]` fold) were already linear before the fix
(their own `Obj`s keep `live` high, so GC rarely fires) and are unchanged: 0.062/0.116/0.233 → 0.063/0.118/0.242
at n = 100k/200k/400k, within noise.

**The cost this buys: one `wire_summary` walk per channel `send`** (the `recv` side is free — each
message's byte count is stored *with* the message in the queue, so `pop` is O(1)). Both queue-lock
sections stay O(1): the send-side walk is hoisted **before** `MnSched::core` is taken, because that lock
serializes every fiber's park/wake/finish and its hold time must not scale with user payload size.
`benches/run.chz` has no channel bench, so this path was previously unmeasured. Bespoke, release,
best-of-5 (base = `main` @ 8a913d0, both binaries built in their own target dirs):

| channel bench | base | after |
|---|---|---|
| 2 000 round-trips × 2 000-element list, `--serial` | 0.163 s | 0.174 s (+7%) |
| 2 000 round-trips × 2 000-element list, M:N | 0.165 s | 0.172 s (+4%) |
| 200 round-trips × **20 000**-element list, `--serial` | 0.333 s | 0.336 s (+0.8%) |
| 4 producers + 1 consumer, 2 000 × 2 000-element list, M:N | 0.129 s | 0.126 s (flat) |

The overhead does **not** scale with message size the way a second full traversal would — 10× bigger
messages cost +0.8%, not +7% — because `wire_summary` is a pointer-chasing sum next to `to_wire`'s much
more expensive allocate-and-clone walk on the same tree. The M:N fan-out case (the one that would show a
global-lock regression) is flat.

**No regression on the common (no-core) path.** `benches/run.chz` allocates no cores, so nothing should
move. Direct before→after, same machine + session, 7 runs each, min / median seconds:
`map` 0.135/0.181 → **0.119/0.129** · `str` 0.157/0.161 → 0.156/0.160 · `primes` 0.564/0.590 →
0.551/0.601 · `fib` 0.226/0.248 → 0.228/0.237 · `loop` 0.852/0.936 → 0.850/0.876. All min-times within
run-to-run noise — **flat**, as expected (the five new `live_bytes`/`children` arms are on `Obj` variants
the benches never allocate). `CHEZZI_HEAP_STATS` peak is byte-identical before and after.

Re-verified on the final binary (after the review fixes below), n = 200 000, best-of-5: `RwShared` holder
**1.534 s → 0.218 s** (7.0×), plain-`List` control 0.198 → 0.193 s — the holder now matches the control.

Same change also closes gaps.md **W6-10**: the cached byte half of the summary feeds `Heap::live_bytes`, so
`chezzi test --max-heap` finally sees an off-heap channel backlog / `Shared`-parked data (195 MB used to
pass a 200 KB cap). Those bytes are charged **once per core per heap** (by `Arc` identity) — charging once
per `Obj` alias slot multiplied a shared payload by the fan-out and produced spurious OVER-MEMORY verdicts.
The one residual escape this left — a nested core with no surviving alias slot — was closed on
2026-08-06 by a cross-core byte recursion gated on a live cap (gaps.md `W6-10r`); cap-off runs, and so
every bench here, are unaffected.

### Round 4 — the trigger counts BYTES, not events (W7-28, 2026-08-07) — the first cap-off cost this family has had to record

Rounds 1–3 charged bytes only for off-heap wire stores. Every other byte still arrived under an EVENT
counter, and each event class has a shape that raises none of it: `xs.push(i)` × 80 M appends into an
existing `Vec` (**PASS at 617.8 MB against an 8 MB cap — 77×**), `big.extend(chunk)` × 150 does the same in
~1200 instructions (**~240 MB**), and `s = s + s` ×22 / `"x".repeat(20000000)` stay under the 256-object
floor at 41 MB / 20 MB. `should_collect`'s byte term is now fed by all three funnels a heap gains bytes
through — `Heap::alloc`, `Heap::get_mut` (deferred before/after delta) and `Vm::to_wire_crossable`. All
shapes are `OVER-MEMORY` rc=1 at 11–32 MB; generous-cap controls still PASS at full footprint.

**An instruction TICK was tried first and is recorded here because it measured clean and was still wrong.**
Sampling every `cap/8` instructions fixed the `push` repro, cost nothing cap-off, and passed the full
suite on both engines — and still let `extend` put 240 MB past an 8 MB cap, because one instruction can
append N values. No instruction interval bounds a bulk op.

**Cap-off now costs ~1% on ONE bench, and it is real.** The gate is still `mem_cap != 0`, but it now sits
in `alloc` and `get_mut` — both hot — rather than only in `should_collect`. 15-run A/B: `fib` −0.98% ·
`str` −0.53% · `primes` +0.32% · `loop` +0.12% · `list` +0.37% · **`struct` +1.41%** · `poly_method`
+0.27% · `map` +0.73%. `struct` re-run at 40 runs × 2: **+0.85%, +1.11%** — ~1σ each but the same sign
three times on the alloc-heaviest bench, so it is recorded as a real ~1% regression rather than filed as
noise. `list` (the `get_mut`-heaviest bench) re-ran at −0.18% / −0.40% and `map` at +1.12% / −0.18%, i.e.
genuinely unmoved. A branchless form would have to run `obj_bytes_shallow` unconditionally, which is
strictly worse cap-off; the branch stays.

### Round 3 — byte-aware GC pacing under a cap (2026-07-27), the half that was wrongly marked done

Counting the bytes did nothing on the natural runaway, because the cap was never **sampled**: `over_cap` is
assigned only inside `sweep()`, and `sweep()` only ran on a heap-OBJECT count. A program sending a ~1 MB
string 300 times (payload built ONCE, ~2 `Obj`s per iteration) PASSED at **304 MB RSS under an 8 MB cap**;
a 200k-int list sent 100 times PASSED at **3369 MB**. `Heap::should_collect` now also fires on charged
off-heap bytes — `mem_cap != 0 && since_gc_wire_bytes >= (mem_cap/4).max(64*1024)` — charged in
`Vm::to_wire_crossable` and reset in `sweep()`. Both repros are now `OVER-MEMORY` rc=1 at 15 MB / 46 MB.

**Cap-off is untouched, by construction and by measurement.** The byte term short-circuits on
`mem_cap != 0`, so `chezzi run` (and therefore `benches/run.chz` and the serial==M:N parity gate) pays one
load+branch per `should_collect` and never walks. Direct A/B of the two release binaries, cap-off, best-of-5,
two independent rounds: `loop` −2.5% / +1.4% · `fib` +0.6% / +2.9% · `primes` +3.8% / +2.5% · `list`
+5.0% / −0.2% · `struct` +1.0% / +1.6% — sign flips between rounds, i.e. run-to-run noise, no consistent
direction. The W6-7 microbench is likewise unmoved (same session, best-of-3, `--serial`): `RwShared` holder
0.116/0.207/0.647 s before → 0.114/0.214/0.651 s after at n = 100k/200k/400k, still tracking the plain-`List`
control (0.082/0.176/0.631 → 0.082/0.176/0.617) and still linear.

**`chezzi test --max-heap` gets slower — stated, not hidden.** A capped run now sweeps on off-heap growth
(more sweeps, each an O(live slots) `live_bytes`) and walks `wire_summary` a second time per store (the
send path walks again to cache the core's summary). Measured on 100 sends of a 200k-int list under a cap
generous enough to PASS (4 GB), best-of-3: **1.649 s → 1.828 s (+11%)**; the same program with no cap:
1.669 s → 1.676 s. Eliminating the second walk would need a precomputed summary threaded through
`MnSched::send_wake`'s signature — not worth a signature change for a CI/debug guard.

### Round 2 — the fix's own two regressions (2026-07-27)

Adversarial review caught the first cut reintroducing the same *shape* of problem on two different axes.
Both are fixed; measured on release binaries, `--serial`, best-of-7, base = `main` @ 8a913d0 built in its
own target dir.

**(1) `live_bytes` was O(D²) in the number of DISTINCT live cores.** Charging a core's bytes once per
core (not once per alias slot) needs a de-dup; the first cut used a linear `Vec::contains` scan re-run for
every core slot. `Heap::live_bytes` runs on **every** `sweep()` (the `peak_live_bytes` probe — not gated on
`--max-heap`), so a program holding D distinct cores paid ~D²/2 comparisons per GC pass. That is the exact
failure shape W6-7 exists to remove, on the "how many cores" axis instead of the "how big is one payload"
axis — and neither the microbench above (ONE holder core) nor `benches/run.chz` (no cores at all) could see
it. Go-idiomatic mailbox-per-connection / actor-per-entity code hits it. Fixed: `FxHashSet` (already
vendored in `src/vm/fxhash.rs`; `HashSet::default` does not allocate, so the no-core path is untouched).

Repro — `for i in range(K): chs.push(Channel[int]())`, then 500 000 list allocations:

| K distinct cores | base (`main`) | round-1 branch | fixed |
|---|---|---|---|
| 10 000 | 0.100 s | 0.351 s | **0.105 s** |
| 20 000 | 0.100 s | 0.665 s | **0.105 s** |
| 40 000 | 0.102 s | 1.239 s | **0.109 s** |
| 80 000 | 0.118 s | 2.669 s | **0.125 s** |

Round-1 grew linearly in K (quadratic per pass); fixed is **flat in K**, ~+5% over base — the constant
cost of five extra `match` arms + one uncontended core lock per distinct core per sweep. Same with
`Shared(i)` (no queue mutex involved, isolating the scan): K = 40 000 base 0.111 s, round-1 1.224 s,
fixed **0.124 s**. Control (`chs.push([i])`, plain lists, no cores): 0.105 → 0.101 s, flat.

**(2) The `wire_summary` walk sat INSIDE the value lock** for `Shared`/`RwShared`/`Atomic` — for
`RwShared` inside the EXCLUSIVE write lock, so every concurrent reader of the flagship zero-copy read view
stalled for a full payload walk on each `set`/`write` write-back. The channel paths already hoist their
walk off `MnSched::core` for exactly this reason ("lock hold time must not scale with user payload size");
the single-value cores did not. Fixed: `SharedCore::store` / `RwSharedCore::store` / `AtomicCore::store`
summarise the caller-owned value **before** taking the lock; `AtomicCore::store_guarded` now *takes* the
pre-computed summary, so `exchange` hoists too (`cas` builds its value under the lock by necessity — its
`to_wire` is already O(payload) there — and `add`/`sub` are scalars).

Single-thread wall time is unchanged by the hoist (same work, reordered), so the win is invisible to
`--serial` and to the parity gate — it is purely reader-stall time on M:N. The store-side cost of the
summary itself remains and is the design's price: `rw := RwShared(<100 000-int list>)` then 50 × `rw.set`
is **0.221 s base → 0.268 s (+21%)**, one extra read-only traversal next to `to_wire`'s allocate-and-clone
traversal of the same tree. Not eliminated (fusing the count into `to_wire` would thread an out-param
through every `to_wire` call site); documented instead — reads are O(1), each store pays one walk.

## `std.path` byte-exact rewrite — the cost of losing the native `str` ops, and clawing it back (W7-8, 2026-07-31)

W7-8 moved every `std.path` helper from `str -> str` to `PathLike -> Path`, so the algorithms now run
over RAW OS BYTES (that is the whole point — a non-UTF-8 filename must survive `basename`/`join`/
`normalize`). The price is real and was not measured when the rewrite landed: `str.split` / `"/".join(..)`
/ `str + str` are single NATIVE calls, while `bytes` has none of them, so the first cut replaced each
with a per-BYTE interpreted `bytearray.push` loop driven by the VM.

Microbench (not in `benches/run.chz` — this compares two spellings of one std module, not Chezzi vs
CPython): 20 000 × `path.normalize("/usr/local/../share/doc/./readme.txt")` + `path.with_ext("/a/b/c.txt",
"md")`, release binary, `--serial`, median of 3, same binary for every row (`CHEZZI_STD` swap, so only the
std source differs):

| `std.path` source | median | vs main |
|---|---|---|
| `main` — the old `str -> str` module (native `split`/`join`/`+`) | **0.296 s** | 1.00× |
| W7-8 first cut — per-byte `ba.push(x)` loops | 0.799 s | 2.70× slower |
| W7-8 fixed — `ba.extend(b)` + one shared `_last_idx` scan | **0.511 s** | 1.73× slower |

**1.56× faster than the first cut**, and the remaining 1.73× is the honest cost of byte-exactness on
today's `bytes` surface:

* `_cat`/`_join` now use `bytearray.extend` (ONE native memcpy per piece) instead of a VM loop per byte —
  that is the whole 1.56×.
* `basename`/`dirname`/`ext` share one backwards `_last_idx` scan. `basename` used to run a full `_split`
  (allocating one slice per component) purely to take the last piece; `dirname` and `ext` each carried
  their own duplicate forward scan.
* What is LEFT is `_split`'s per-byte `while` loop, which `normalize` runs once per call. `bytes` has no
  native `split`, so this is the floor until one exists. **Upgrade path:** a native `bytes.split(sep)`
  (the natural companion to the `ByteSeq` milestone that retires the five `collect_bytes_arg` branches)
  would close most of the remaining gap; it is new builtin surface and was deliberately out of W7-8's scope.

No M19 bench moves — `std.path` is a lexical helper module that no bench imports.

## M24 — static protocol requirements through a bound (witness passing) — measured, NO delta (2026-08-10)

A language milestone, recorded here only because "we measured it" is the claim, not "it got faster".
`hyperfine`, **20 runs**, the M24 branch against a pre-milestone binary built from `b6c17369` in a
**separate `CARGO_TARGET_DIR`** (same machine; the shared-target trap in `CLAUDE.md` is exactly how a
comparison like this silently measures one binary twice):

| bench | branch vs baseline | noise floor (baseline vs a copy of itself) |
|---|---|---|
| fib | 1.00 ± 0.03 | — |
| loop | 1.02 ± 0.07 | — |
| poly_method | 1.00 ± 0.04 | — |
| primes | 1.03 ± 0.06 | 1.00 ± 0.06 |

Every row is inside the noise floor, so **no headline number changes**. That is the expected shape,
not luck: the new `Op::CallStaticDyn` only appears where a generic body calls `T.static()`, the hidden
witness parameter is charged **only** to a body that uses one (`Checker::witness_params_of`), and no
tracked bench has a static-carrying protocol bound at all. The cost that *would* show up on a
witness-heavy workload is one extra argument push per witnessed call plus one `str` per witness per
nested body (`docs/gaps.md` **M24-2**) — the suite has no such bench to isolate it.

## W7-49 — `Span` grows a `file` id (and shrinks to 12 bytes) — measured, flat-or-faster except `map` (2026-08-11)

A **correctness** change measured because `Span` is hot VM data, not because it was meant to be
faster: `Proto.lines` holds one `Span` **per opcode**, so `sizeof(Span)` sets the cache footprint of
every compiled function, and it is also a cross-half table key (hashed and cloned on the
checker→compiler path). The gap it closes is `docs/gaps.md` **W7-49** — a default-parameter expression
spliced across a module boundary aliased another module's side-table entry.

**Size went 16 → 24 → 12 bytes.** The obvious shape (`usize` line/col **+** `usize`-ish file) is
**24 bytes**, and that intermediate is why this section exists: it regressed the `map` bench 1.07×
AND the extra AST-node width pushed two calibrated constants off their margins (`parser::MAX_DEPTH`
64 → 48, `vm::VM_STACK_BYTES` 384 → 512 MiB). The shipped shape is `{ line: u32, col: u32, file: u32 }`
= **12 bytes** — *smaller* than the 16 it started at. A source file cannot realistically reach 4
billion lines or columns, so `u32` costs nothing real. `lexer::tests::span_stays_twelve_bytes` pins it.

| bench | pre-W7-49 baseline | W7-49 (12-byte `Span`) | delta |
|---|---|---|---|
| fib | 389.6 ms | **381.1 ms** | 1.02× faster |
| struct | 673.1 ms | **647.4 ms** | 1.04× faster |
| primes | 953.1 ms | **926.9 ms** | 1.03× faster |
| loop | 1.465 s | **1.447 s** | 1.01× faster |
| map | **257.1 ms** | 268.6 ms | **1.04× slower** |

**The `map` delta is alignment, not size — and it is measured, not argued.** Padding the same tree's
`Span` back to 16 bytes (one dummy field, everything else identical) gives **263.9 ms**: still above
the 257.1 ms baseline, and the size was restored. So the `map` movement does not track `sizeof(Span)`
and is not paid for by it. Every delta in the table is roughly **1σ**, and hyperfine's ratio confidence
bars straddle 1.00 in both directions.

**Conclusion: nothing here is a perf claim.** The four faster rows are not a win to bank and the `map`
row is not a regression to fix in this commit — chasing a ~1σ alignment artefact belongs in the perf
track (`docs/future.md` §4), behind the levers that move benches by whole multiples. Recorded here so
the next person who measures `map` on this tree knows the movement was seen, controlled for, and
deliberately left alone.
