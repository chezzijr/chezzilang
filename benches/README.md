# benches/ — Chezzi vs CPython microbenchmarks

A small sweep that measures the Chezzi VM against CPython on the same workload, to track
where the runtime spends time and whether the optimization track (`docs/future.md §4`,
roadmap **M19**) is moving the numbers. The baseline lives in **[`docs/benchmarks.md`](../docs/benchmarks.md)**.

## Layout

```
benches/
  chz/   one .chz per bench   # fib str primes loop list empty
  py/    one .py  per bench   # same workload, identical stdout
  run.chz                     # the driver — written in Chezzi, shells out to hyperfine
```

Each `chz/X.chz` and its `py/X.py` twin **print identical stdout** (one result line). That
makes every bench a correctness check too: if the outputs ever diverge, the bench is wrong
before its timing means anything.

| bench   | what it stresses                  | result |
|---------|-----------------------------------|--------|
| `fib`   | recursive calls — fib(30), naive  | `832040` |
| `str`   | f-string + join, 500k parts       | `5888889` |
| `primes`| `while` + `%`, primes below 200k  | `17984` |
| `loop`  | int add, 20M iterations           | `199999990000000` |
| `list`  | push + sum, 2M elements           | `1999999000000` |
| `empty` | startup — empty program           | (no output) |

> `primes` here is the **sequential** single-task variant. The spawned/parallel version
> lives at `examples/primes_parallel.chz` (it's a golden test) — don't conflate them.

## Running

```sh
cargo run -- run benches/run.chz
```

`run.chz` builds the release binary (`cargo build --release`), then runs each pair under
[`hyperfine`](https://github.com/sharkdp/hyperfine) (`--warmup 2 -N`). It needs `hyperfine`
and `python3` on `PATH`. Per-bench markdown tables are exported to `benches/last-*.md`
(gitignored) — copy the figures into `docs/benchmarks.md` when refreshing the baseline.

To check a single pair by hand:

```sh
diff <(./target/release/chezzi run benches/chz/loop.chz) <(python3 benches/py/loop.py)
```

## Notes

- **Ratios, not absolutes.** Wall-clock depends on the machine; the chezzi-÷-python ratio
  is the portable signal. Re-stamp the machine + tool versions when you update the table.
- `run.chz` is itself a dogfood test of `std.process`, string interpolation, and `match`
  on `Result` — if the driver can't drive a real toolchain, that's a finding.
