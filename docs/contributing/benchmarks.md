# Chezzi — Benchmarks (vs CPython)

This page holds the current numbers and the rules to measure them. Older dated measurements are in
git (`git log -- docs/history`). The harness is
[`benches/`](../../benches) (see [`benches/README.md`](../../benches/README.md)). The optimization
backlog that these numbers justify is in [`roadmap.md §4`](../design/roadmap.md) (roadmap **M19**).

## Where new entries go

Put each new perf record in this file, in the section of the bench it measures. Give the date, the
base commit and the command. Replace an older figure of the same bench; git keeps it. Put a one-line
summary of the delta in the month's dated progress entry.

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

These rules come from past measurement mistakes.

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
load average about 4, wall ms mean ± σ (TICKET-227).

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
TICKET-264 (2026-10-17) measured `hof` and `closure` flat. TICKET-268 (2026-10-19) moved the fused int ops onto `Vm::int_op`: release, `hyperfine -N -r 10 --warmup 2`, load average about 4, base `40fe4d7e` against the change, `fib` 543 ± 20 ms against 520 ± 18 ms, `loop` 1769 ± 46 ms against 1781 ± 30 ms. Without `#[inline(always)]` on `int_binop`, `operand_fault` and `Vm::int_op`, `loop` ran 1887 ms against 1786 ms in the reversed order. The `print_loop` row is from TICKET-264:
the shell `time` keyword, 7 runs, load average about 3 (`hyperfine` was not installed).

### Ratio to CPython

The last sweep with a CPython column ran on 2026-08-14 (§2c1 "eager `spawn` start"). Lower is better.

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

Std protocol table (TICKET-265), `print(1)`, release, mean of 100 runs, three rounds interleaved with
a build of main `25e8912d` on the same box: main 8110 / 7840 / 7990 us, branch 7990 / 7870 / 7650 us.
Before the fix the branch took 19-22 ms: `std_decls` parsed all 225 KB of `std/*.chz` (11.3 ms) on
every start to find 28 `protocol` declarations. `std_protocol_decls` now reads only those
blocks (the build script extracts them, TICKET-282); `std_type_owners` reads every file
on the first import-hint diagnostic. Debug `print(1)`: 23.6 ms on the branch, 22 ms on main. Cost of a
std import (release, `print(1)` after the import): `std.ops` 10.7 ms, `std.cmp` 9.3, `std.iter` 14.4,
`std.string` 15.6, against 8.1 with none. A protocol name used to cost nothing; it now costs that
import, because a program must import the module that declares a protocol (owner decision 1). The
checker loop for `std.ops` takes 0.6 ms; the rest of the import cost is outside it and is open.

Std import rows after the reach set (TICKET-265: a file-backed std module checks, desugars and
compiles only the top-level fns the program reaches, `resolver::unreached_std_fns`). Release,
`hyperfine -N -w 5 -r 50`, mean (median), load average 3.1, 2026-10-17:

| Program | main `25e8912d` (owner) | branch |
|---|---|---|
| `print(1)` | 8.0 ms | 8214 us (8086) |
| `import max from std.cmp` | 8.9 ms | 8793 us (8799) |
| `import enumerate from std.iter` | 14.1 ms | 11116 us (11080) |
| `import is_empty from std.string` | 14.9 ms | 10292 us (10189) |
| `import Add from std.ops` | 10.7 ms | 10727 us (10571) |

The target is each row within 1 ms of `print(1)`; `std.cmp` meets it, the other three do not. For
`std.string` the per-phase timers put the rest at: lex 270 us, parse 370 us, checker 380 us over two
passes (hoist and signatures of every fn), compile 120 us, reach and desugar 120 us.
The grid in `tests/hook_conformance_grid.rs` imports a std module in each cell. Its one test
`operator_and_bound_agree_in_every_cell` took 284 s on the branch, 189 s on main (load about 3.5 on
the box). The grid is one test per owner x hook since TICKET-276.

Std parse cache (TICKET-265, `resolver::shared_std_parse`; since TICKET-282 the first load decodes a build-time
AST and these lex and parse figures are the cost it removed): a process lexes and parses each embedded
std module once and copies the AST into each later graph. Debug, one process, first load (fill) vs a
later load (clone): `string.chz` 3.65 vs 0.19 ms, `iter.chz` 3.24 vs 0.14, `prelude.chz` 5.21 vs 0.23,
`ops.chz` 0.60 vs 0.03. A long-lived process (the LSP, the lib tests) saves the difference on each
graph after the first. A one-shot `chezzi run` builds one graph, so it fills and saves nothing; each
`hook_conformance_grid` cell is such a process (265 s after the cache, 271-284 s before).

Std build-time AST (TICKET-282): `crates/chezzi-native/build.rs` parses each `std/**/*.chz` once, at
`cargo build`, and embeds the `borsh` AST; a run decodes it (`std_embed::decode_ast`) and lexes and
parses no std text. This also deletes the per-run protocol scan (0.65 to 1.1 ms in 5 timed runs) and
the `ffi.chz` lex. `$CHEZZI_STD` and a live buffer keep the text path. Release, `hyperfine -N -w 5
-r 40`, base `57dbf708` and the branch each in its own target dir, mean ± σ, load average 2.7:

| Program | base | branch | drop |
|---|---|---|---|
| `print(1)` | 8.0 ± 0.3 ms | 6.5 ± 0.4 ms | 1.5 ms |
| `import Iterator from std.iter` | 10.6 ± 0.5 ms | 9.0 ± 0.4 ms | 1.6 ms |
| `import Eq from std.cmp` | 8.6 ± 0.4 ms | 6.8 ± 0.4 ms | 1.8 ms |
| `import split from std.string` | 11.8 ± 0.8 ms | 10.1 ± 0.5 ms | 1.7 ms |

A first run of the same commands at load average 20 gave drops of 1.8, 1.5, 1.2 and 2.0 ms. The
saving is per run (prelude parse plus protocol scan). A std import still costs about 2.5 ms over
`print(1)` (`std.iter`: 9.0 against 6.5): graph build, check and compile, which are open. The 35
encoded ASTs take 601,653 bytes; the release binary grows from 15,760,960 to 16,875,240 bytes (the
artifact plus the `borsh` code for every AST type).

Std protocol shapes and prelude native bodies (2026-11-06, `TryFrom` + `int.from`): `print(1)`,
release, `perf stat -e instructions:u`, median of 5, base `a3149cda` in its own target dir. `run`:
8,737,303 base, 8,608k branch (-1.5%); `check`: 5,870k base, 6,017k branch (+2.5%). Before the two
fixes the branch was +6.8% on `run`: one 2-line std protocol cost +62k instructions per `run`
(+32k per `check`), because each checker (a `run` builds two, `check_graph_diags` and
`compile_tables_pass`) harvested every std protocol twice (`insert_protocols_in_two_passes`), about
309k per checker. Now one process harvests them once (`seed_std_protocols`, a `OnceLock`) and
`ProtocolInfo::methods` is an `Arc`, so a copy copies no signature. The 4 prelude bodies cost
+500k per `run` (decoded, desugared, checked, compiled); a graph whose sources name none of them as
a member (`int.from`, `T.from`) now skips them (`PreludeUse::uses_native_bodies`). The `check`
remainder is the harvested sigs of the 4 bodied methods (~55k) and the `TryFrom` shape.

One-pass std protocol harvest (2026-11-06, with `Into` + `TryInto`): `print(1)`, release,
`perf stat -e instructions:u`, median of 5, base `6ae90b8d` in its own target dir. `run`: 8,610,999
base, 8,560,578 branch (-0.59%); `check`: 6,015,034 base, 5,940,376 branch (-1.24%). The two new
protocols and the `into` gate alone were `run` +1.12%, `check` +1.22%; `seed_std_protocols` now
seeds every std protocol's name and arity and harvests each once, instead of two full passes.

Protocol slots in scope + per-kind hint table (2026-11-06): `print(1)`, release,
`perf stat -e instructions:u`, median of 5, base `6668505a` in its own target dir. `run`: 8,530,799
base, 8,570,945 branch (+0.47%); `check`: 5,922,610 base, 5,960,819 branch (+0.65%). The first cut
of the scope-aware slot gate was `run` +17.7%: each type body rebuilt the visible-protocol set (in a
std module every std protocol, with a linear `module_sigs` walk per miss) and deep-cloned each
requirement's `FnSig`. Now a slot is `{name, protocol key, slot}` behind an `Arc`, and the std
modules filter one per-process table (`std_slot_table`, a `OnceLock`). The remainder is that table's
one-time derivation (~15k) and the prelude `int`/`float` slots.

### Callbacks as frames (2026-11-07)

Plan `2026-11-07-frame-reentry` (Tasks 2-6: hooks, HOF callbacks, sorts, box closures and
generators run as frames with a `RetAction`). Release, `perf stat -e instructions:u`, median of 5,
after Task 6, base `97012a20` built in its own worktree and target dir. Budget: +3% per bench.

| bench | change vs base |
|---|---|
| `benches/chz/hof.chz` | −2.75% |
| `benches/chz/hook_dispatch.chz` | −4.51% |
| `benches/chz/struct.chz` | −1.19% |
| `benches/chz/list.chz` | −0.49% |
| `benches/chz/many_list.chz` | +0.96% |
| `print(1)` | −0.12% |
| `sort_by` loop (scratch bench) | −0.02% |
| generator loop (scratch bench) | −1.72% |
| `s.update` loop (scratch bench, after Task 5) | −6.73% |

### `defer` drain as frames (2026-11-07)

Plan `2026-11-07-defer-drain-frames`: a `defer` drain is a trampoline frame (`src/vm/defer_drain.rs`).
Release, `perf stat -e instructions:u`, median of 5 interleaved single runs, base `fb8edf37` built
in its own worktree and target dir. Budget: +3% per bench.

| bench | change vs base |
|---|---|
| `benches/chz/hof.chz` | −0.54% |
| `benches/chz/hook_dispatch.chz` | −0.75% |
| `benches/chz/struct.chz` | −1.19% |
| `print(1)` | −0.07% |
| `benches/chz/defer.chz` (new) | +2.71% |
| `benches/sched/rendezvous_pingpong.chz` (cap-0 send) | +0.50% |

2000 tasks that each wait in a `defer` at `--threads=2` (`tests/defer_park_scale.rs`): 5 OS threads
and 0.47 s, `demotes=0`; before, 2005 threads and 38.3 s.

### One global deadlock check (2026-11-18)

Plan `2026-11-17-checkdead`: a run-wide awake count (one atomic add per start, park, wake and end)
replaces the per-scope, per-sched and family deadlock judges; main runs as fiber 1 and an
Executor's jobs run on the root sched's workers. `benches/sched/ab_pair.py`, release, n=5
interleaved, wall ms median (spread = max - min) and max RSS; T `default` is the default worker
count. Base `7f2e03b3` built in its own target dir. Gate (`ab_pair.py`): `ok` when the branch median is at most
the base median plus the base spread; budget +3% per bench.

After Task 1 (the count added, the old judges still deciding), loadavg 3.6:

| bench | T | base ms (spread) / RSS MiB | branch ms (spread) / RSS MiB | ratio | gate |
|---|---|---|---|---|---|
| `benches/sched/body_and_spawn.chz` | 4 | 3145 (77) / 15 | 3026 (55) / 15 | 0.96x | ok |
| `benches/sched/burn8_nursery.chz` | 4 | 1857 (30) / 15 | 1782 (21) / 15 | 0.96x | ok |
| `benches/sched/churn2.chz` | 4 | 2320 (142) / 23 | 2291 (205) / 23 | 0.99x | ok |
| `benches/sched/churn8.chz` | 4 | 3124 (181) / 23 | 3284 (121) / 24 | 1.05x | ok |
| `benches/sched/deep_nurseries.chz` | 4 | 2688 (1271) / 15 | 2758 (808) / 15 | 1.03x | ok |
| `benches/sched/executor_and_parallel.chz` | 4 | 1922 (143) / 15 | 1852 (63) / 15 | 0.96x | ok |
| `benches/sched/extern_abs.chz` | 4 | 87 (30) / 15 | 92 (27) / 15 | 1.07x | ok |
| `benches/sched/fan_open.chz` | 4 | 487 (21) / 15 | 511 (30) / 15 | 1.05x | SLOW |
| `benches/sched/guard_contend.chz` | 4 | 133 (19) / 15 | 127 (4) / 15 | 0.95x | ok |
| `benches/sched/rendezvous_pingpong.chz` | 4 | 3424 (1620) / 15 | 4794 (2805) / 15 | 1.40x | ok |
| `benches/sched/send_one_channel.chz` | 4 | 1261 (449) / 15 | 1214 (66) / 15 | 0.96x | ok |
| `benches/sched/spawn_fresh_graph.chz` | 4 | 205 (44) / 15 | 214 (37) / 15 | 1.04x | ok |
| `benches/sched/spawn_native_call.chz` | 4 | 805 (106) / 26 | 819 (85) / 25 | 1.02x | ok |
| `benches/sched/storm.chz` | 4 | 44 (11) / 15 | 48 (6) / 15 | 1.10x | ok |
| `benches/sched/trips.chz` | 4 | 57 (7) / 15 | 58 (11) / 15 | 1.02x | ok |
| `benches/sched/two_executors.chz` | 4 | 1954 (97) / 15 | 1875 (53) / 15 | 0.96x | ok |

`fan_open.chz` re-sampled n=10: 491 (26) vs 504 (15) ms, 1.03x, ok. `rendezvous_pingpong.chz` n=10:
1.07x, ok (noisy).

After the switch (Task 3e, the judges deleted). **Measured under load, loadavg ~12**, so the
spreads are wide; re-run on a quiet box before using a row as a baseline:

| bench | T | base ms (spread) / RSS MiB | branch ms (spread) / RSS MiB | ratio | gate |
|---|---|---|---|---|---|
| `benches/sched/body_and_spawn.chz` | 4 | 3212 (90) / 15 | 3166 (48) / 15 | 0.99x | ok |
| `benches/sched/burn8_nursery.chz` | 4 | 1869 (14) / 15 | 1863 (11) / 15 | 1.00x | ok |
| `benches/sched/churn2.chz` | 4 | 2408 (126) / 23 | 1049 (319) / 15 | 0.44x | ok |
| `benches/sched/churn8.chz` | 4 | 3246 (151) / 24 | 1277 (153) / 15 | 0.39x | ok |
| `benches/sched/deep_nurseries.chz` | 4 | 2418 (519) / 15 | 599 (125) / 15 | 0.25x | ok |
| `benches/sched/executor_and_parallel.chz` | 4 | 1938 (104) / 15 | 1814 (9) / 15 | 0.94x | ok |
| `benches/sched/extern_abs.chz` | 4 | 99 (24) / 15 | 95 (12) / 15 | 0.96x | ok |
| `benches/sched/fan_open.chz` | 4 | 491 (41) / 15 | 511 (8) / 15 | 1.04x | ok |
| `benches/sched/guard_contend.chz` | 4 | 133 (22) / 15 | 116 (10) / 15 | 0.87x | ok |
| `benches/sched/rendezvous_pingpong.chz` | 4 | 3821 (1857) / 15 | 1834 (574) / 15 | 0.48x | ok |
| `benches/sched/send_one_channel.chz` | 4 | 1223 (330) / 15 | 1110 (242) / 15 | 0.91x | ok |
| `benches/sched/spawn_fresh_graph.chz` | 4 | 215 (29) / 15 | 220 (25) / 15 | 1.02x | ok |
| `benches/sched/spawn_native_call.chz` | 4 | 795 (106) / 25 | 856 (108) / 23 | 1.08x | ok |
| `benches/sched/storm.chz` | 4 | 49 (7) / 15 | 47 (6) / 15 | 0.96x | ok |
| `benches/sched/trips.chz` | 4 | 63 (10) / 15 | 72 (7) / 15 | 1.13x | ok |
| `benches/sched/two_executors.chz` | 4 | 1959 (97) / 15 | 1815 (47) / 15 | 0.93x | ok |
| `benches/chz/closure.chz` | default | 2483 (43) / 15 | 2479 (37) / 15 | 1.00x | ok |
| `benches/chz/defer.chz` | default | 701 (49) / 15 | 684 (27) / 15 | 0.98x | ok |
| `benches/chz/empty.chz` | default | 7 (2) / 15 | 8 (3) / 15 | 1.13x | ok |
| `benches/chz/enum.chz` | default | 3952 (194) / 15 | 3962 (19) / 15 | 1.00x | ok |
| `benches/chz/fib.chz` | default | 529 (27) / 15 | 507 (43) / 15 | 0.96x | ok |
| `benches/chz/hof.chz` | default | 707 (4) / 15 | 696 (48) / 15 | 0.98x | ok |
| `benches/chz/hof_nursery.chz` | default | 711 (25) / 15 | 720 (75) / 15 | 1.01x | ok |
| `benches/chz/hook_dispatch.chz` | default | 311 (33) / 15 | 279 (95) / 15 | 0.90x | ok |
| `benches/chz/list.chz` | default | 816 (4) / 42 | 753 (36) / 42 | 0.92x | ok |
| `benches/chz/loop.chz` | default | 1778 (39) / 15 | 1724 (54) / 15 | 0.97x | ok |
| `benches/chz/many_list.chz` | default | 751 (27) / 195 | 657 (49) / 196 | 0.88x | ok |
| `benches/chz/many_map.chz` | default | 499 (37) / 96 | 493 (37) / 95 | 0.99x | ok |
| `benches/chz/many_struct.chz` | default | 811 (45) / 115 | 783 (49) / 117 | 0.97x | ok |
| `benches/chz/map.chz` | default | 284 (52) / 26 | 264 (35) / 26 | 0.93x | ok |
| `benches/chz/map_str.chz` | default | 423 (35) / 15 | 410 (29) / 15 | 0.97x | ok |
| `benches/chz/poly_method.chz` | default | 2729 (68) / 15 | 2672 (108) / 15 | 0.98x | ok |
| `benches/chz/primes.chz` | default | 1181 (29) / 15 | 1180 (44) / 15 | 1.00x | ok |
| `benches/chz/print_loop.chz` | default | 550 (44) / 34 | 585 (33) / 30 | 1.06x | ok |
| `benches/chz/str.chz` | default | 271 (33) / 60 | 261 (25) / 59 | 0.96x | ok |
| `benches/chz/struct.chz` | default | 918 (35) / 15 | 802 (18) / 15 | 0.87x | ok |
| `benches/chz/unique.chz` | default | 166 (8) / 44 | 164 (36) / 45 | 0.99x | ok |

No row is `SLOW`. The large drops (`churn2`, `churn8`, `deep_nurseries`, `rendezvous_pingpong`)
were not sampled on a quiet box, so no cause is claimed for them here.

### Every wait parks (2026-11-20)

Plan `2026-11-18-every-wait-parks`: the nested `==`/`str`/`<` walks, native callees, iterable
drains, json `default` thunks and hard-halt `defer` drains run as frames; the block table has two
contexts; `pool.rs` and `runner_cache.rs` are gone. Release, `perf stat -e instructions:u`, median
of 3, base `9c0f8f68` (phase 3) built in its own target dir, against the final tree. Budget: +1%
per bench. The 13 rows at +1.00% to +1.63% were accepted by the owner (2026-11-20).

| bench | base M instr | branch M instr | change |
|---|---|---|---|
| `benches/chz/closure.chz` | 21446.9 | 21683.3 | +1.10% |
| `benches/chz/defer.chz` | 5321.3 | 5372.6 | +0.96% |
| `benches/chz/empty.chz` | 9.7 | 9.7 | −0.29% |
| `benches/chz/enum.chz` | 33696.2 | 34059.1 | +1.08% |
| `benches/chz/fib.chz` | 4150.7 | 4201.8 | +1.23% |
| `benches/chz/hof.chz` | 5959.3 | 5987.3 | +0.47% |
| `benches/chz/hof_nursery.chz` | 6036.2 | 6064.2 | +0.46% |
| `benches/chz/hook_dispatch.chz` | 2330.4 | 2365.6 | +1.51% |
| `benches/chz/list.chz` | 7172.4 | 7252.2 | +1.11% |
| `benches/chz/loop.chz` | 17928.8 | 18108.1 | +1.00% |
| `benches/chz/many_list.chz` | 4921.5 | 4966.6 | +0.92% |
| `benches/chz/many_map.chz` | 3539.7 | 3531.8 | −0.22% |
| `benches/chz/many_struct.chz` | 6829.1 | 6859.3 | +0.44% |
| `benches/chz/map.chz` | 2088.0 | 2072.9 | −0.72% |
| `benches/chz/map_str.chz` | 3781.0 | 3756.8 | −0.64% |
| `benches/chz/poly_method.chz` | 22461.6 | 22702.7 | +1.07% |
| `benches/chz/primes.chz` | 11807.8 | 11943.4 | +1.15% |
| `benches/chz/print_loop.chz` | 1621.5 | 1640.5 | +1.17% |
| `benches/chz/str.chz` | 2063.4 | 2005.6 | −2.80% |
| `benches/chz/struct.chz` | 8159.3 | 8248.0 | +1.09% |
| `benches/chz/unique.chz` | 961.3 | 965.8 | +0.48% |
| `benches/sched/body_and_spawn.chz` | 63432.3 | 63910.5 | +0.75% |
| `benches/sched/burn8_nursery.chz` | 67888.2 | 68398.8 | +0.75% |
| `benches/sched/churn2.chz` | 5770.8 | 5865.1 | +1.63% |
| `benches/sched/churn8.chz` | 18709.5 | 18892.2 | +0.98% |
| `benches/sched/deep_nurseries.chz` | 1173.6 | 1170.1 | −0.30% |
| `benches/sched/executor_and_parallel.chz` | 68164.3 | 68676.6 | +0.75% |
| `benches/sched/extern_abs.chz` | 606.1 | 601.2 | −0.80% |
| `benches/sched/fan_open.chz` | 17496.2 | 17651.4 | +0.89% |
| `benches/sched/guard_contend.chz` | 641.6 | 640.0 | −0.24% |
| `benches/sched/rendezvous_pingpong.chz` | 4114.3 | 4089.8 | −0.59% |
| `benches/sched/send_one_channel.chz` | 2203.3 | 2195.3 | −0.36% |
| `benches/sched/spawn_fresh_graph.chz` | 1260.1 | 1243.6 | −1.32% |
| `benches/sched/spawn_native_call.chz` | 6201.9 | 6083.4 | −1.91% |
| `benches/sched/storm.chz` | 276.8 | 277.8 | +0.35% |
| `benches/sched/trips.chz` | 148.0 | 147.5 | −0.34% |
| `benches/sched/two_executors.chz` | 68408.4 | 68918.5 | +0.75% |

Per task (M instructions, one run each; the counts are deterministic):

| binary | `fib` | `loop` | `primes` | `struct` | `closure` |
|---|---|---|---|---|---|
| base (`9c0f8f68`) | 4150.7 | 17928.8 | 11807.8 | 8159.3 | 21449.5 |
| after-1a | 4140.0 | 17928.9 | 11807.1 | 8159.3 | 21421.7 |
| after-1b | 4156.2 | 17989.1 | 11875.4 | 8226.4 | 21537.7 |
| after-2 | 4156.2 | 17989.2 | 11875.4 | 8226.4 | 21535.2 |
| after-3 | 4177.7 | 18109.2 | 11942.3 | 8248.3 | 21641.3 |
| after-4 | 4177.7 | 18109.1 | 11942.3 | 8248.4 | 21621.7 |
| after-5 | 4150.8 | 17969.2 | 11852.7 | 8193.4 | 21495.7 |
| after-6a | 4156.1 | 17968.5 | 11852.7 | 8193.1 | 21507.1 |
| after-6c | 4177.5 | 18108.5 | 11941.9 | 8248.1 | 21612.2 |
| after-6d | 4177.6 | 18108.2 | 11941.6 | 8248.0 | 21614.4 |
| final | 4201.8 | 18108.2 | 11943.4 | 8248.0 | 21693.6 |

No task adds work to the paths of `fib`, `loop` and `primes`. Their +1.0% to +1.2% comes from
where the dispatch loop's code lands: `step`'s frame grew from 0x328 to 0x348 bytes in Task 1b,
and the count moves per task with no feature on the path (after Task 5 it is near base). On this
box (Broadwell E5-2680 v4) the hot VM code also runs at one of two speeds after unrelated edits:
`list.chz` took about 2345M or 2450M cycles with a byte-identical `run_until`, and the
legacy-decoder uops (`idq.mite_uops`, 439M against 658M) track it. So gate a change this small on
`instructions:u`, not on cycles or wall time. The levers (shrink `step`/`run_until`, layout
control) are in [`roadmap.md §4`](../design/roadmap.md#4-optimizations-open-levers).

### A cancel is a wake (2026-11-23)

Plan `2026-11-21-cancel-is-a-wake`: every cancel goes through one `cancel`, a fiber's `sleep_ms`
parks with one timer, and no wait polls for a cancel except a demoted socket. Release, 28-core box,
base `615881a3` built in its own target dir. Program: a `parallel:` of 2000
`spawn: time.sleep_ms(2000)`. n=5, median CPU (user + sys) and wall; `ticks` from
`CHEZZI_SCHED_STATS=1`.

| workers | base CPU, wall | now CPU, wall | now `ticks` |
|---|---|---|---|
| 1 | 0.60 s, 2.03 s | 0.09 s, 2.04 s | 0 |
| 2 | 0.67 s, 2.03 s | 0.12 s, 2.04 s | 0 |
| default (28) | 2.12 s, 2.18 s | 2.41 s, 2.26 s | 0 |

Go 1.27 `time.Sleep`, same shape (2026-11-21, 3 runs): 2.01 s wall, 0.01-0.02 s CPU.

**The limit at the default worker count.** The 2000 simultaneous wakes still cost ~2.4 s of CPU,
because `complete_offload` -> `notify_waiters` wakes every idle worker per wake. The 0.2 s goal is
not met at the default count. Waking one worker per ready task (Go's `wakep`) is the next plan.

**Cancelled sleeps.** A loop that submits a job sleeping 1 h, cancels it and shuts the Executor
down, at `CHEZZI_THREADS=1`, n=3, median CPU and peak RSS (2026-11-24). Control: the same loop
with a `recv` in place of the sleep.

| iterations | base CPU, RSS | now CPU, RSS | control now RSS |
|---|---|---|---|
| 4000 | 0.81 s, 15.1 MB | 0.20 s, 15.1 MB | 15.1 MB |
| 16000 | over 40 s wall (killed) | 0.76 s, 19.5 MB | 19.2 MB |
| 32000 | over 40 s wall (killed) | 1.49 s, 28.6 MB | 27.3 MB |

Base notices each cancel at its next 5 ms re-arm, so 16000 iterations take over 80 s of wall time.
Before the timer stop, a stopped sleep's timer kept its sched alive until its deadline: 7.8 s of
CPU at 4000 iterations, and 68 MB at 32000.

**Idle workers.** A listed idle worker keeps its 5 ms `idle_cv` timeout (TICKET-205: it waits for a
permit ticket, not for a cancel; base has it too), and `ticks` does not count it. Whole-process
voluntary context switches of one task in `sleep_ms(1000)`, n=5 median, startup and teardown
included:

| workers | base | now |
|---|---|---|
| 1 | 401 | 205 |
| 2 | 405 | 209 |
| default | 471 | 278 |

`benches/sched/*` at T=4, `ab_pair.py`, n=5 against base: no SLOW row (`body_and_spawn` 0.99x,
`guard_contend` 1.00x). The two highest medians, resampled at n=21: `deep_nurseries` 1.04x and
`send_one_channel` 1.06x. Their base spread is over 40% of the median, so this run cannot tell them
from noise.

### Method calls through one method table (2026-11-15)

Plan `2026-11-15-error-origin-vm-method-ids`: a method call carries a `MethodId`, and one dense
(receiver row, method id) table (`src/base/method_table.rs`) replaces the 13 name-keyed deciders of
`Vm::do_method_call`; the struct IC still answers first. Also in the change: an executed `!x` stamps
its payload's origin. Release, `perf stat -e instructions:u`, median of 5 interleaved single runs,
base `91acae84` built in its own worktree and target dir. Budget: +1% per bench.

| bench | change vs base |
|---|---|
| `benches/chz/poly_method.chz` | −1.58% |
| `benches/chz/struct.chz` | +0.31% |
| `benches/chz/enum.chz` | −0.00% |
| `benches/chz/hof.chz` | +0.75% |
| `benches/chz/hook_dispatch.chz` | −1.26% |
| `benches/chz/defer.chz` | −2.50% |

### Guard waits and Executor joins park (2026-11-08)

Plan `2026-11-08-park-guard-and-join`: a fiber's guard wait and `Executor.shutdown()` park, and a
guard holder is preempted. Release, `perf stat -e instructions:u`, median of 5 interleaved single
runs at the default worker count, base `96d3d719` built in its own worktree and target dir.
Budget: +3% per bench.

| bench | change vs base |
|---|---|
| `benches/chz/struct.chz` | +2.84% |
| `benches/chz/hof.chz` | +0.95% |
| `benches/chz/hook_dispatch.chz` | +1.32% |
| `benches/chz/defer.chz` | +1.25% |
| `benches/sched/rendezvous_pingpong.chz` | +0.67% |
| `benches/sched/guard_contend.chz` (new: 8 tasks x 20000 `update`s on one box) | −35.73% |

`guard_contend.chz` wall time, median of 5: T=1 117 -> 101 ms, T=2 367 -> 108 ms. 200 guard
waiters at `--threads=2` (`tests/park_scale.rs`): `demotes=0`, base 199; 100 fiber joiners of their
own `Executor`: `demotes=0`, base 100.

### Peak memory vs Go

Peak process RSS, `benches/maxrss.py`, median of 3, 2026-07-25 after the four memory levers.

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

Release, median of 3, 2026-09-28 at `8b1d2331`.

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
| `xs += ys` on a `List` builds a new list (`x OP= v` is `x = x OP v`) | 20000 appends: 0.67 s vs 0.008 s for `push` | 2026-10 "TICKET-258" |
| iterating a `Range` value (not the literal `a..b` loop) | about 3.3× the literal loop | 2026-10 "TICKET-253" |
| T=1 channel hand-off through the runner gate | at most 1.3× (ping-pong), 1.05× (`primes_parallel`) | 2026-10 "TICKET-205" |
| `std.cancel` on a deep `derive()` chain (build + cancel) | superlinear in depth | 2026-08 "`std.cancel`" |
| dispatch-loop code placement after plan `2026-11-18-every-wait-parks` | +1.0% to +1.6% instructions on 13 benches | 2026-11 "Every wait parks" |
