# `judge/`: known-answer DSA harness

Runs Chezzi solutions to data-structure and algorithm problems against known-correct answers. A
non-`PASS` on a correct solution is a candidate Chezzi bug. The harness is written in Chezzi
(`run.chz`).

## Run

```sh
cargo build --release
./target/release/chezzi run judge/run.chz                   # every problem
./target/release/chezzi run judge/run.chz unionfind         # one problem
./target/release/chezzi run judge/run.chz --samples-only    # judge/problems samples only (the cargo gate)
./target/release/chezzi run judge/run.chz --timeout 30      # per-case limit in seconds (default 10)
```

`tests/judge.rs` runs `--samples-only` on every `cargo test`. The full sweep is
`cargo test --release --test judge -- --ignored`.

Verdicts: `PASS`, `WRONG` (the first difference, or the checker's message), `FAULT` (Chezzi runtime
error and exit code), `PANIC` (a Rust host panic: always a Chezzi bug), `TIME` (killed at the
limit), `CHECK` (the checker itself failed).

## Two problem roots

| Root | Cases | Judged by |
|---|---|---|
| `judge/problems/<slug>/` (12 CSES / Codeforces problems) | committed `samples/N.in`, `N.out` | token compare |
| `judge/lc/<problem>/` | [Library Checker](https://github.com/yosupo06/library-checker-problems) cases, built locally | the problem's own checker |

### Library Checker

Library Checker has about 250 data-structure and algorithm problems (union-find, segment trees,
FFT, strings, graphs). Each problem ships random and edge-case generators, a correct C++ solution
and an output checker. Its license is Apache-2.0.

On the first run, `run.chz` clones the commit pinned in `LC_PIN` into `judge/vendor/` (gitignored).
For each problem it then runs `generate.py -p <problem>`, which needs `g++` (C++17) and `python3`.
The build is skipped when the cases are newer than the sources.

To add a problem:

1. Find its name: `ls judge/vendor/library-checker-problems/*/` (the directory that holds `info.toml`).
   Read its `task.md` for the input and output format.
2. Write `judge/lc/<name>/solution.chz`. It reads stdin and writes stdout.
3. Run `./target/release/chezzi run judge/run.chz --timeout 30 <name>`.

The inputs are large (often 2e5 to 1e6). A `TIME` on a correct solution is a speed finding, not a
wrong answer. Raise `--timeout` to tell the two apart.

## Triage

A non-`PASS` on a correct solution: shrink the failing `.in`, write a failing test (`tests/chz/`),
then fix.
