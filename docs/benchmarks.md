# Chezzi — Benchmarks (vs CPython)

Living perf-tracking doc. The harness that produces these numbers is **[`benches/`](../benches/)**
(driver: `benches/run.chz`, run via `cargo run -- run benches/run.chz`). Re-run it and
re-stamp this table when the runtime changes. The optimization backlog these numbers
justify lives in **[`future.md §4`](future.md)**; the scheduled work is roadmap **M19**
(`spec.md` / `PROGRESS.md`).

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
| fib     | recursive call overhead | per-call `Vec<Value>` arg alloc (`mod.rs:3181`); full `Obj` enum clone incl. captured `HashMap` in `invoke_value` (`:3198`); `name.clone()` for the arity check (`:3200`); per-call slot pre-fill (`push_frame`) | frame pooling; pass args as a stack slice; match on `&Obj` (no clone); kill `name.clone()` |
| str     | f-string build + join | `BuildStr` calls `stringify` per part into a fresh `String` (`:2495`); `ConstStr` clones + boxes on every push (`:2228`) | string interning + cached hash; concat builder/rope; intern constant strings |
| primes  | `while` + `%` | binary ops re-dispatch on operand type every iteration; per-access name lookup for globals/locals | specialize arithmetic (monomorphic int guard); inline caching; superinstructions |
| loop    | 20M int adds | raw dispatch count + arith re-dispatch | superinstructions (fuse `GetLocal+GetLocal+BinOp`); arith specialize; NaN-box `Value` (16→8 B) |
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
✅ `stringify`-into-buffer for `BuildStr` (`str` −5%). **Still open:** inline caching (Phase 2b —
needs global-slotting, see `future.md §4`), frame pooling, `ConstStr` interning (the remaining `str`
lever), arithmetic specialization, NaN-boxing, generational GC — see `future.md §4`.
