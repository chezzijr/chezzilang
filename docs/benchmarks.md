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

## After M19 Phase 4 — 2026-06-12 (same machine)

Phase 4 is the **struct-field lever**: `GetField`/`SetField` now carry a per-call-site inline-cache
id into a per-`Vm` `field_ic` vector that caches the field's index. A hit re-verifies the cached
index against the live field name (`fields[idx].0 == name`) and collapses the `O(field-position)`
name-probe to one verify-compare; a miss re-probes and refills. Static slotting (the P2b model) is
impossible here — the compiler is type-erased, so the field's struct type is unknown at emit time —
hence a runtime IC. Sound + thread-safe: the cell holds an index (no `GcRef`), so it is invisible to
GC / snapshots / `swap_ctx`; cooperative fibers run sequentially and `--parallel` workers each own a
`Vm`; every access self-verifies, so a stale/cross-type cell can never return a wrong field. The
frozen interp is tree-walk (never sees the opcode) ⇒ parity is automatic. 1541 tests green.

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
> every struct. A struct **type-id guard** (pure-int compare, no name re-verify) is the logged
> follow-up if this caveat needs closing — see `future.md §4`.

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
not the 256 MiB `VM_STACK_BYTES` thread — a recursion that SIGABRT'd a 1 MiB stack pre-change now
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
| fib     | recursive call overhead | per-call `Vec<Value>` arg alloc (`mod.rs:3181`); full `Obj` enum clone incl. captured `HashMap` in `invoke_value` (`:3198`); `name.clone()` for the arity check (`:3200`); per-call slot pre-fill (`push_frame`) | frame pooling; pass args as a stack slice; match on `&Obj` (no clone); kill `name.clone()` |
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

**Real next levers** (contained, parity-safe, bench-moving): struct **type-id guard** for the field
IC (P4 follow-up), **small-string optimization**, faster `usize` hasher — see `future.md §4`.

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
`Module`/`Closure` still dominate, the `Str` variant went 16→24 B, well under).

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
replicates that loss rather than doing exact `x==y` — exactness would diverge from the interpreter and
break two-engine parity. `quicken_eq_preserves_lossy_f64_semantics` locks the behavior; any future
"fix" to int equality must change the generic path + interp together, as a separate non-perf change.

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
unchanged `values_equal` confirm. The two-engine parity bar is preserved by construction — the frozen
interpreter (`src/interp/value.rs`) keeps its `Vec<usize>` index, and both engines confirm every hash
hit with the same `values_equal` probe, so output is byte-identical.

**Behavior-preserving:** new `dense_index_collision_upgrade_parity` guard (two distinct constant-`hash()`
struct keys land in one bucket, both read back distinctly via the `Many` upgrade) plus the borrowed
`fxhash_constant_hash_collision_still_resolves` (30 constant-hash keys) — both go RED on a `One`-only
stub (key dropped → "key not found"), GREEN with `Many`. `fxhash_map_int_keys_insert_lookup_remove`
(remove→rebuild parity), `idxspec_int_float_key_collision_resolves`, and `fxhash_set_dedup_and_ops`
stay green. Full suite 1712 green, conformance green, clippy clean. **Next suspect for `map`:** the
remaining gap is `values_equal` per-probe cost + `FxHashMap` lookup/rehash (no longer the `Vec` alloc).
