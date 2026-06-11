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
