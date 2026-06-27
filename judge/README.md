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
  generate.py                   # synthesize in-domain cases from gen.py + reference.py (no download)
  fetch_data.py                 # install a CSES test ZIP into data/<slug>/
  problems/<slug>/
    meta.toml                   # name, CSES id, source URL
    solution.chz                # hand-written Chezzi solution, reads stdin (UNDER TEST)
    reference.py                # independent Python oracle: stdin -> correct stdout
    gen.py                      # emit one random in-domain input, seeded by argv[1]
    samples/N.in, N.out         # committed public samples (from the statement)
  data/<slug>/N.in, N.out       # gitignored generated/fetched cases
```

## Generated oracle (no download — the main path)

You don't need CSES's hidden data to bug-hunt. Each problem ships a `gen.py` (random input within the
problem's stated range/domain) and a `reference.py` (an **independent** Python implementation — the
oracle). `generate.py` feeds the same input to both sides and writes `judge/data/<slug>/` (gitignored);
`run.chz` then diffs the Chezzi solution against the Python oracle. A self-contained Chezzi-vs-Python
differential test.

```sh
python3 judge/generate.py                 # all problems, 20 cases each
python3 judge/generate.py playlist --count 200
./target/release/chezzi run judge/run.chz
```

**Trusting the oracle (the crux).** A differential test only catches a bug the two sides don't *share*.
So each `reference.py` uses a **different algorithm** than `solution.chz` — ideally an obviously-correct
brute force on small inputs (it's too dumb to be subtly wrong), with a fast path for large stress
inputs. Examples here: counting_rooms solution = stack flood-fill, oracle = **union-find**;
coin_combinations solution = bottom-up DP, oracle = **sequence enumeration** (the literal definition);
playlist solution = sliding window, oracle = **O(n²) restart-per-index**. `gen.py` mixes small (drives
the brute-force branch → proves the algorithm) and large (drives the fast path → stresses Chezzi's
arithmetic/allocation) inputs. The committed sample pins one author-certified point on top.

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
2. `judge/problems/<slug>/meta.toml` — name, `cses_id`/`cf_id`, `url`.
3. `judge/problems/<slug>/samples/1.in` / `1.out` — the statement's sample (vets the solution out-of-the-box).
4. `judge/problems/<slug>/reference.py` + `gen.py` — an independent oracle (different algorithm!) and an
   in-domain generator, so `generate.py` can fuzz the solution without any download.
5. `python3 judge/generate.py <slug>` then `./target/release/chezzi run judge/run.chz <slug>` → expect `PASS`.

Pick problems that stress distinct language paths (recursion depth, big-int boundaries,
`List`/`Map`/`Set` churn, grids, slicing) — that's where shared-wrongness bugs hide.
