# `judge/` — DSA known-answer bug-finding harness

A third bug-discovery oracle for Chezzi (alongside the panic-fuzzer and the CPython differential —
see [`docs/bug-discovery.md`](../docs/bug-discovery.md)). It runs hand-written competitive-programming
solutions against **known-correct answers**, so it catches *shared wrongness* that both co-developed
engines agree on — bugs the differential generator can't reach because it stays in a safe-by-
construction subset (no overflow, no recursion). The harness itself is written in Chezzi (dogfood).

## Run

```sh
cargo build --release
./target/release/chezzi run judge/run.chz                  # all problems
./target/release/chezzi run judge/run.chz weird_algorithm  # one problem
```

Verdicts: `PASS` · `WRONG` (prints first differing line) · `FAULT` (Chezzi runtime error + exit code) ·
`PANIC` (Rust host panic — a Chezzi bug for certain) · `TIME` (timeout). A problem with no cases is
**skipped**, never failed.

A non-`PASS` on a *vetted* solution is a candidate Chezzi bug: minimize the failing `.in`, write a
failing-then-green unit test, then fix (repo TDD flow).

## Layout

```
judge/
  run.chz                       # the harness (Chezzi)
  fetch_data.py                 # install a CSES test ZIP into data/<slug>/
  problems/<slug>/
    meta.toml                   # name, CSES id, source URL
    solution.chz                # hand-written Chezzi solution, reads stdin
    samples/N.in, N.out         # committed public samples (from the statement)
  data/<slug>/N.in, N.out       # gitignored full hidden suite (fetched locally)
```

## Full hidden test suite

CSES test data is the authors' IP — **not committed** (`judge/data/` is gitignored). Download a
problem's test ZIP from its CSES task page (the "Tests" tab, visible once you've solved it), then:

```sh
python3 judge/fetch_data.py <slug> path/to/tests.zip
```

It pairs input/output files by numeric stem and writes `judge/data/<slug>/N.in`/`N.out` (the layout
`run.chz` expects). The harness then runs samples **and** the full suite.

## Adding a problem

1. `judge/problems/<slug>/solution.chz` — read stdin via `std.io.read_line`, print the answer.
2. `judge/problems/<slug>/meta.toml` — name, `cses_id`, `url`.
3. `judge/problems/<slug>/samples/1.in` / `1.out` — the statement's sample (vets the solution in CI-less runs).
4. `./target/release/chezzi run judge/run.chz <slug>` → expect `PASS`.

Pick problems that stress distinct language paths (recursion depth, big-int boundaries,
`List`/`Map`/`Set` churn, grids, slicing) — that's where shared-wrongness bugs hide.
