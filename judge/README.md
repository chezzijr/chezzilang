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
./target/release/chezzi run judge/run.chz --samples-only    # committed samples only (the cargo gate)
```

`tests/judge.rs` gates the harness on every `cargo test` (type-check + the committed samples);
`cargo test --release --test judge -- --ignored` runs the full downloaded suite.

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
  fetch_problem.py              # fetch a problem statement + samples, scaffold problems/<slug>/
  problems/<slug>/
    meta.toml                   # name, CSES id, source URL
    solution.chz                # hand-written Chezzi solution, reads stdin (UNDER TEST)
    reference.py                # independent Python oracle: stdin -> correct stdout
    gen.py                      # emit one random in-domain input, seeded by argv[1]
    edges.py                    # (optional) deterministic boundary cases: no arg -> count, k -> k-th input
    samples/N.in, N.out         # committed public samples (from the statement)
  data/<slug>/N.in, N.out       # gitignored generated/fetched cases
```

## Generated oracle (no download — the main path)

You don't need CSES's hidden data to bug-hunt. Each problem ships a `gen.py` (random input within the
problem's stated range/domain), an optional `edges.py` (deterministic boundary inputs), and a
`reference.py` (an **independent** Python implementation — the oracle). `generate.py` feeds the same
input to both sides and writes `judge/data/<slug>/` (gitignored) — random cases as `g{k}.in/.out`,
edge cases as `e{k}.in/.out`; `run.chz` then diffs the Chezzi solution against the Python oracle. A
self-contained Chezzi-vs-Python differential test.

**Random fuzz vs deterministic corners.** `gen.py` casts a wide net but almost never lands on the
exact boundaries that break programs. `edges.py` pins those corners — min/max sizes, all-equal,
value extremes, exact multiples, empty/full grids (`max`, `min`, `0`, single-element). It follows an
index protocol mirroring `gen.py`'s `argv`: run with **no arg** it prints how many edge cases it has;
run with `argv[1]=k` (0-based) it prints the k-th edge input. Every edge case must stay valid for
whichever branch of `reference.py` it triggers (the brute-force branches have small-input caps).

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

## Official hidden test data (optional, manual)

The generated oracle above is the main path and needs no download. If you separately want to run
against CSES's *own* hidden tests, there's no automating it — the data is the authors' IP, gated
behind logging in and solving the problem first (then the task page's "Tests" tab unlocks). `run.chz`
already globs `judge/data/<slug>/*.in`/`.out` (gitignored), so just unzip the official cases there,
named `N.in`/`N.out`, and the harness picks them up alongside the samples and generated cases.

## Fetching a problem statement

The hidden test data is gated, but the **statement** is public. `fetch_problem.py` pulls it and
scaffolds the directory — `meta.toml`, `statement.md` (the statement as plain text, with
constraints), and the public `samples/` — so you only have to write the four reasoning files. It
never overwrites an existing `solution.chz`/`reference.py`/`gen.py`/`edges.py`.

```sh
python3 judge/fetch_problem.py https://cses.fi/problemset/task/1068            # slug from the title
python3 judge/fetch_problem.py https://codeforces.com/problemset/problem/4/A four_a
```

CSES is fully supported. Codeforces is best-effort (Cloudflare sometimes 403s scripted requests from
datacenter IPs); if it blocks, save the page as `.html` in a browser and pass the file path instead of
the URL. (This fetches the statement + samples only; test *data* comes from the generated oracle above,
or, optionally, official hidden cases dropped into `judge/data/` by hand — see below.)

## Adding a problem

0. `python3 judge/fetch_problem.py <url> [slug]` — scaffold statement + samples + meta (above).
1. `judge/problems/<slug>/solution.chz` — read stdin via `std.io.read_line`, print the answer.
2. `judge/problems/<slug>/meta.toml` — name, `cses_id`/`cf_id`, `url`.
3. `judge/problems/<slug>/samples/1.in` / `1.out` — the statement's sample (vets the solution out-of-the-box).
4. `judge/problems/<slug>/reference.py` + `gen.py` — an independent oracle (different algorithm!) and an
   in-domain generator, so `generate.py` can fuzz the solution without any download.
5. `judge/problems/<slug>/edges.py` (optional but recommended) — deterministic boundary cases (index
   protocol: no arg prints the count, `argv[1]=k` prints the k-th input). Cover `max`, `min`, `0`,
   single-element, all-equal, exact-multiple corners that random `gen.py` misses.
6. `python3 judge/generate.py <slug>` then `./target/release/chezzi run judge/run.chz <slug>` → expect `PASS`.

Pick problems that stress distinct language paths (recursion depth, big-int boundaries,
`List`/`Map`/`Set` churn, grids, slicing) — that's where shared-wrongness bugs hide.
