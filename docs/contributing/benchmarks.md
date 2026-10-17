# Chezzi — Benchmarks (vs CPython)

This page holds the current numbers and the rules to measure them. Every dated measurement is in
[`docs/history/benchmarks/`](../history/benchmarks/), one file per month. The harness is
[`benches/`](../../benches) (see [`benches/README.md`](../../benches/README.md)). The optimization
backlog that these numbers justify is in [`roadmap.md §4`](../design/roadmap.md) (roadmap **M19**).

## Where new entries go

Put each new perf record in `docs/history/benchmarks/<YYYY-MM>.md` for the month of the measurement.
Add it at the top of that file (newest first). Start a new month file when the month has none.
Use the heading form `## TICKET-NNN — <what changed> (YYYY-MM-DD)`. Update the tables on this page
only when the record changes a current number or a bottleneck below.

## Run the benches

```sh
cargo run -- run benches/run.chz      # builds the release binary, then runs every chz/py pair
```

`benches/run.chz` needs `hyperfine` and `python3` on `PATH`. It runs `hyperfine --warmup 2 -N` for
each pair and writes `benches/last-<name>.md` (gitignored). Its list: `fib`, `str`, `primes`, `loop`,
`list`, `struct`, `poly_method`, `map`, `map_str`, `unique`, `empty`.

The other files in `benches/chz/` have no CPython twin, so you time them by hand:

```sh
./target/release/chezzi run benches/chz/closure.chz
```

| directory | what it holds | how to run |
|---|---|---|
| `benches/chz/`, `benches/py/` | one bench per file; each pair prints the same stdout | `run.chz`, or by hand |
| `benches/go/`, `benches/maxrss.py` | Go twins for peak RSS | `RUNS=3 python3 benches/maxrss.py <cmd>` |
| `benches/go/`, `benches/rust/`, `benches/py/` `nursery_fanout` | ancestor twins of `examples/primes_parallel.chz` | sweep against `chezzi run --threads=N` |
| `benches/sched/` | scheduler cost shapes (no CPython twin) | `CHEZZI_THREADS=4 ./target/release/chezzi run benches/sched/<x>.chz` |
| `benches/box/`, `benches/shared_map/` | `RwShared` / shared-map costs | by hand |

`tests/bench_coverage.rs` and `tests/difftest.rs` scan `benches/chz/` and `benches/py/` by name.

### Measurement rules

These rules come from mistakes recorded in the history files.

1. Measure release binaries only. The VM is only fast when optimized.
2. Build the base binary into its own `CARGO_TARGET_DIR`. Call both binaries by absolute path.
   `benches/run.chz` always rebuilds `./target/release/chezzi` in the current directory, so it
   cannot time a saved base binary.
3. Interleave base and branch in ONE `hyperfine` invocation that names both commands. Two
   sessions at two load levels gave a false 5-9% "regression" (TICKET-167, 2026-09).
4. Record the commit of each side, the run count and the `uptime` load average.
5. Take at least 5 runs per side before you claim a difference. Report median and spread.
6. When a bench moves by more than its spread, run it again with the command order reversed.
7. Without `hyperfine`, time whole processes with a Python timer and say so in the record.
8. `fib` and `loop` move 4-5% from code layout alone (one unrelated `#[inline(never)]` moved `fib`
   4%). Do not attribute a change of that size to an edit without a second check.

## Current numbers

### Wall time per bench

Release binary, `hyperfine -N --warmup 2 --runs 7`, 2026-10-08 (base `1ca43817` + TICKET-227),
load average about 4, wall ms mean ± σ. Source: [2026-10](../history/benchmarks/2026-10.md),
"TICKET-227".

| bench | what it stresses | ms |
|---|---|---|
| `fib` | recursive calls, fib(30) | 585 ± 50 |
| `str` | f-string + join, 500k parts | 267 ± 20 |
| `primes` | `while` + `%`, primes below 200k | 1263 ± 12 |
| `loop` | int add, 20M iterations | 1830 ± 41 |
| `list` | push + sum, 2M elements | 732 ± 28 |
| `struct` | 8-field struct, field reads and writes, ~1M steps | 873 ± 10 |
| `poly_method` | megamorphic method call, ~4M calls | 2774 ± 106 |
| `map` | int-keyed map, 200k inserts + 1M lookups | 265 ± 15 |
| `map_str` | string-keyed map, 1M get-then-increment | 385 ± 15 |
| `unique` | `List.unique()`, 500k ints | 130 ± 11 |
| `closure` | build and call a 3-capture closure per step | 2517 ± 100 |
| `enum` | build an enum value and `match` it per step | 4002 ± 146 |
| `hof` | `map` + `filter` + `fold` over 1000 elements, 2000 times | 712 ± 58 |
| `many_list` | 2M `[i, i]` lists kept alive | 686 ± 11 |
| `many_map` | 1M int -> struct entries | 495 ± 30 |
| `many_struct` | 2M `P{x, y}` kept alive | 832 ± 48 |
| `empty` | startup, empty program | 5 ± 0 |
| `print_loop` | 600000 direct prints, three shapes (`sep=` included), stdout to `/dev/null` | 639 ± 23 |

Later records changed none of these beyond noise: TICKET-255 (2026-10-14) measured `loop` and `list`
flat, builtin dispatch by id (2026-10-16) measured eight benches between -0.7% and +1.4%, and
TICKET-264 (2026-10-17) measured `hof` and `closure` flat. The `print_loop` row is from TICKET-264:
the shell `time` keyword, 7 runs, load average about 3 (`hyperfine` was not installed).

### Ratio to CPython

The last sweep with a CPython column ran on 2026-08-14 (§2c1 "eager `spawn` start", source:
[2026-08](../history/benchmarks/2026-08.md)). Lower is better.

| bench | Chezzi ms | CPython ms | Chezzi ÷ CPython |
|---|---:|---:|---:|
| `fib` | 381.0 | 134.3 | 2.84× |
| `str` | 243.4 | 131.6 | 1.85× |
| `primes` | 942.0 | 445.7 | 2.11× |
| `loop` | 1481.1 | 1394.8 | 1.06× |
| `list` | 574.9 | 255.3 | 2.25× |
| `struct` | 639.9 | 264.9 | 2.42× |
| `poly_method` | 1970.6 | 492.6 | 4.00× |
| `map` | 232.3 | 134.9 | 1.72× |
| `empty` | 3.8 | 14.9 | 3.9× faster |

Two later ratios: `map_str` 2.54× whole program (TICKET-085, 2026-09-08; the isolated map phase is
5.48×), and `unique` 105.4 ms vs 81.7 ms, 1.29× (W8-34, 2026-08-29).

The absolute times on 2026-08-14 are lower than the 2026-10-08 table because the machine and the load
differ. Compare ratios, not absolutes, across records.

### Startup of `chezzi run`

`chezzi run one.chz` with `print(1)`, as a process (check, graph build, compile, VM start). Release,
300 interleaved runs, median: **6504 us** (p10..p90 5929..7423), 2026-10-14 (TICKET-255). The `empty`
row above is `hyperfine`'s figure for an empty file.

### Peak memory vs Go

Peak process RSS, `benches/maxrss.py`, median of 3, 2026-07-25 after the four memory levers
(source: [2026-07](../history/benchmarks/2026-07.md), "Memory baseline").

| bench | Go MB | Chezzi MB | Chezzi ÷ Go |
|---|---:|---:|---:|
| `empty` | 8.8 | 8.9 | 1.01× |
| `loop` | 8.8 | 8.8 | 1.00× |
| `struct` | 8.8 | 8.8 | 1.00× |
| `fib` | 8.9 | 8.8 | 0.99× |
| `list` | 33.3 | 37.8 | 1.14× |
| `map` | 8.8 | 19.6 | 2.23× |
| `many_struct` | 67.4 | 148.9 | 2.2× |
| `many_map` | 66.9 | 163.5 | 2.4× |
| `many_list` | 163.1 | 205.8 | 1.3× |

Use RSS. The `CHEZZI_HEAP_STATS=1 peak_live_bytes` probe ignores off-heap `Vec`/`HashMap` capacity and
the operand and frame stacks, so it under-measures.

### GC share of run time

Release, median of 3, 2026-09-28 at `8b1d2331` (source: [2026-09](../history/benchmarks/2026-09.md)).

| bench | GC share | collections |
|---|---:|---:|
| `many_list` | 11.0% | 9 |
| `closure` | 10.8% | 46 875 |
| `many_struct` | 9.4% | 9 |
| `str` | 8.4% | 12 |
| `enum` | 7.3% | 42 180 |
| `many_map` | 5.1% | 8 |
| `poly_method` | 1.4% | 3 906 |
| `hof` / `hof_nursery` | 0.1% | 62 |
| `map_str` | 0.0% | 3 |
| `fib`, `loop`, `primes`, `list`, `struct`, `map`, `unique`, `empty` | 0.0% | 0 |

The GC cost follows the garbage swept, not the pause count. A `MIN_GC_THRESHOLD` of 4 096 or 65 536
(current: 256) cut collections by up to 250× and left GC time flat.

## Where the time goes

The gap to CPython is dispatch count and call overhead. The value model and the GC are not the gap:
GC is 0% on `fib`, `loop` and `primes`, and scalars are unboxed.

### Per-bench bottleneck

| bench | dominant cost now | levers that landed | next lever |
|---|---|---|---|
| `fib` | per-call work: frame setup and ~7 ops per call body | per-call clone kill, in-place call args, call-loop flattening (no Rust recursion per call), 8-byte `Value` | JIT |
| `loop` | raw dispatch count | superinstructions (`BinLocalLocal`, `BinLocalConst`, `IncLocal`), opcode quickening, hot opcodes inlined in `run_until`, 8-byte `Value` | JIT (near parity already) |
| `primes` | int arithmetic and `%` in the inner loop | superinstructions, binop quickening | JIT |
| `str` | allocation of each part | small-string optimization (inline strings up to 22 bytes), `stringify` into the build buffer, `ConstStr` interning | none open; `join` already buffers |
| `list` | per-element push and dispatch | in-place call args, index-access specialization | `Value`-density work; JIT |
| `struct` | field access | struct-field inline cache, positional struct layout, struct type-id guard | JIT |
| `poly_method` | megamorphic method dispatch (worst ratio, ~4×) | method-call inline cache, N-way polymorphic method IC, flattened `do_method_call` | JIT |
| `map` | hashing and index probes | FxHash index, denser int-keyed index | none open |
| `map_str` | string-content hashing (isolated map phase 5.48×) | none | string-key hashing (open follow-up of TICKET-085) |
| `closure`, `enum` | per-instance allocation and GC sweep | positional captures, enum `variant_id` | generational GC per heap (at most ~5-10%) |
| `many_*` | one GC `Slot` per struct (64 B) | boxed `Module`, inline ≤3 fields, mark bitset, struct name from `tid` | value structs (inline in the container), see [`value-structs.md`](../design/value-structs.md) |
| `empty` | startup | prelude `Range` bodies skipped when unused | keep it a win |

### What is spent and what is left

- The contained interpreter backlog is spent. The last three levers (struct type-id guard, SSO, FxHash)
  landed in 2026-06.
- NaN-boxing stays blocked by the full 64-bit int. The 8-byte `Value` uses an int-favoring pointer tag
  instead: inline ints up to ±2^62, wide ints and every `f64` boxed on the heap.
- Interpreter levers can only narrow the gap. The JIT (Cranelift, roadmap Tier 3) is the path to match
  CPython on compute.
- The only GC lever the data supports is generational collection per heap. It is not a JIT
  prerequisite. The JIT needs safepoints and stack maps for JIT frames.
- A lever's predicted payoff is often wrong: several levers moved a different bench than predicted.
  Measure before and after, and record "no gain" when there is none.

### Known costs that are accepted

| cost | size | record |
|---|---|---|
| `xs += ys` on a `List` builds a new list (`x OP= v` is `x = x OP v`) | 20000 appends: 0.67 s vs 0.008 s for `push` | [2026-10](../history/benchmarks/2026-10.md) "TICKET-258" |
| iterating a `Range` value (not the literal `a..b` loop) | about 3.3× the literal loop | [2026-10](../history/benchmarks/2026-10.md) "TICKET-253" |
| T=1 channel hand-off through the runner gate | at most 1.3× (ping-pong), 1.05× (`primes_parallel`) | [2026-10](../history/benchmarks/2026-10.md) "TICKET-205" |
| one OS drainer thread per nursery | a loop of 20 000 single-task nurseries +26% | [2026-08](../history/benchmarks/2026-08.md) "§2c1" |
| `std.cancel` on a deep `derive()` chain (build + cancel) | superlinear in depth | [2026-08](../history/benchmarks/2026-08.md) "`std.cancel`" |
