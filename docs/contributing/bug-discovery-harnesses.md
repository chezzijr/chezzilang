# Chezzi — Bug-Discovery Harnesses

The four mechanical oracles. The strategy, the ranked plan and the manual audit playbook are in
[`bug-discovery.md`](bug-discovery.md). The long incident write-ups behind several rules below are in
[`history/bug-hunts/bug-discovery-records.md`](../history/bug-hunts/bug-discovery-records.md).

## Panic-fuzz harness (`src/panicfuzz/`)

A seeded, dependency-free generator emits adversarial and malformed inputs. The harness feeds each one
to `chezzi check <tmpfile>`, the full front end (`resolver::build_graph` -> `checker::check_graph`:
lexer, parser, checker). Invariant: **malformed input yields a clean diagnostic, never a Rust panic or
a signal crash.** It has the same structure as `src/difftest/`: its own `xoshiro256**` RNG and the
same reader-thread + `try_wait` + kill-on-timeout subprocess code. A `#[path]` include wires it into
the `tests/panicfuzz.rs` CI gate (seeds `0..2000`) and the `src/bin/panicfuzz.rs` long-runner.

**Why a subprocess harness and not `cargo-fuzz`:**

- This environment has no nightly, `rustup` or `cargo-fuzz`.
- A subprocess catches more crash classes than in-process `catch_unwind`. It sees a signal kill
  (SIGSEGV, SIGABRT, and stack overflow, the most likely deep-parser crash) as an exit code of `None`,
  and a Rust panic by the `panicked at` marker on stderr. `catch_unwind` cannot catch a stack overflow.
- `src/lib.rs` makes an in-process target possible. The harness keeps the process boundary for the
  stronger classification.

**Classification** (`src/panicfuzz/run.rs`):

| outcome | meaning | finding? |
|---|---|---|
| `Clean` | exit 0, or non-zero with a clean diagnostic and no panic marker | no |
| `HostPanic` | stderr contains `panicked at` | **yes** |
| `Crash` | exit code `None`: killed by a crash signal | **yes** |
| `Timeout` | killed at the wall-clock budget (a slow input) | no |
| `HarnessError(String)` | the child did not spawn, or the input could not be staged to a temp file | no, but **fatal** |

Both callers (`tests/panicfuzz.rs`'s `fuzz_range_cfg`, `src/bin/panicfuzz.rs`) abort on a
`HarnessError` instead of scoring it. The bin exits **2** for it and **1** for real findings, the same
contract as `difftest` (gaps rows **W7-34**, **W7-35**). Only `HostPanic` and `Crash` count
(`is_finding()`).

**Three generators** (`src/panicfuzz/generate.rs`), each bounded to ≤ 2 KB and deterministic in
`(seed, corpus)`:

1. random UTF-8-ish byte strings;
2. a token-alphabet sampler over keyword, punctuation and operator spellings, plus random identifiers,
   numbers, newlines and indentation (reaches deep parser states);
3. raw-byte mutation of the `examples/*.chz` corpus (byte flips, truncation, duplicated or removed
   lines, inserted braces, colons and indentation).

A finding reports the seed and the raw input (there is no shrink pass). Reproduce it with
`panicfuzz --seed N`. This lever never runs the VM.

```sh
cargo test --test panicfuzz                # CI gate: unit guards + fuzz seeds 0..2000
cargo build --release --bin chezzi --bin panicfuzz
./target/release/panicfuzz --seeds 0..200000   # unattended sweep (~30-50 min of subprocess spawns)
./target/release/panicfuzz --seed 12345        # reproduce one seed (prints the triggering input)
```

**Overflow blind spot in release.** `tests/panicfuzz.rs` runs the debug `chezzi` (overflow checks ON),
so it catches arithmetic-overflow panics. A release `chezzi` has overflow checks OFF, so overflow
wraps without a sign (the bin prints a NOTE). For a full overflow sweep, rebuild with
`RUSTFLAGS="-C overflow-checks=on"`. Signal, segfault and explicit-panic crashes show in both profiles.

### Depth: an axis the byte/token fuzzer does not certify

The generators never nest one grammar production thousands of levels deep, so a crash that needs
deep nesting is out of statistical reach. A manual hunt (2026-07-12) found a deeply nested `match`
pattern that overflowed the host stack (SIGABRT) because `parse_pattern_impl` had no `MAX_DEPTH`
guard. "Zero fuzzer crashes" does not certify the depth axis.

Current guards:

- `parser::tests::max_depth_boundary_accepts_then_rejects` (`src/parser/tests/review_panel.rs`)
  bisects the accepted nesting count for each of the seven guarded productions (parens, lists, calls,
  blocks, unary, generic types, match patterns). It asserts that `k` parses and `k+1` is rejected with
  `"too deeply"` (never a host abort), and that `k >= 200` (CPython 3.14's limit) for the four
  comparable forms. It measures the boundary, so a wider AST node shows as a number in a failure
  message.
- `check_errors_json::worst_accepted_nesting_never_signal_crashes` (`tests/check_errors_json.rs`)
  drives `ast`, `check` and `run` over the worst program the parser accepts.
- `parser::MAX_AST_DEPTH` (16 000, `src/parser/mod.rs`) bounds the folded depth of the tree that the
  iterative binary and postfix loops build. `desugar::Walker::walk_expr` (`src/desugar/mod.rs`)
  enforces the bound on the finished tree, so a re-parsed interpolation fragment (a second `Parser`)
  cannot reset it.
- The whole front end (resolve, desugar, check, and `run`'s build and compile) runs on a large stack:
  `on_frontend_stack` / `FRONTEND_STACK_BYTES` in `crates/chezzi-syntax/src/lib.rs`. A depth cap alone
  is not enough: any cap high enough for real code still overflows the ~2 MiB LSP walker stack.

A generator that reaches these shapes is still missing, so the corpus axis stays uncertified.

Rules from this axis (records: [depth axes](../history/bug-hunts/bug-discovery-records.md#panic-fuzz-the-depth-axes)):

1. Crash safety has a post-parse walker depth axis apart from parse-time recursion. Run each recursive
   AST walker on a stack sized for the worst depth the parser accepts.
2. A limit's "no regression" claim is a measurement. Bisect it on the PRE-change binary against the
   worst shape: the one that spends every loop a level can spend.
3. When a bound lives on an object, ask what happens when the pipeline makes a second one. Grep every
   constructor of the type. Enforce on the finished artifact at a seam that every consumer passes.
4. When you enforce on an artifact, list every writer that can still change it after your check.
5. Check a "gone by construction" claim with a probe on the mechanism plus a negative control. A zero
   from a probe you never saw fire proves nothing.

Status: the `0..2000` gate is green. Sweeps of `0..100000` (release, overflow checks OFF) and
`0..20000` (debug, overflow checks ON) found zero panics or signal crashes.

## Differential oracle (`src/difftest/`)

A seeded generator emits a cross-language safe subset: literals, bounded-int arithmetic, bool and str
ops, `if`/`for`/`while`, non-recursive functions, list/map/index/len, and the families below. One
abstract IR (`src/difftest/ast.rs`) has two renderers, `emit_chezzi` and `emit_python`. `run.rs` runs
both and diffs stdout.

### Verdict rules

- **The diff is on raw bytes.** `Capture.stdout` is `Vec<u8>`. `from_utf8_lossy` is not injective, so a
  decoded compare would call `ff fe` and `fe ff` a `Match`. `Capture::stdout_text()` decodes only for
  the report and for the `panicked at` scan (gaps row W7-30).
- **`allowlist::MATCHERS` is empty** (W7-31). `check()` stays as the extension point. To accept a
  new divergence that is deliberately left unfixed, add a narrow matcher to `allowlist.rs` with a cited
  reason.
- **A host crash is always `HostPanic`, checked first on every arm.** `classify` looks for `panicked
  at` on Chezzi's stderr and for `chz.code.is_none()` (a signal kill), also on the both-exit-0 arm,
  because a panicking worker thread does not change the process exit code. A host crash cannot be
  allow-listed or hidden under `BothError` (W7-33).
- **A harness that never ran is fatal, not "0 findings".** `run_one` returns `RunErr::TimedOut` for a
  real timeout and `RunErr::CouldNotRun(String)` (program name + `io::Error` text) when spawn, a stdio
  pipe or `try_wait` fails. `run_sources` routes `CouldNotRun` and temp-file staging failures to
  `Outcome::HarnessError(String)`: `is_finding()` is `false`, but `tests/difftest.rs::fuzz_range`
  panics on the first one and `difffuzz` exits **2** (W7-34, W7-38).
- **Exit codes:** `0` clean; `1` findings; `2` the harness broke (bad args, an empty `--seeds A..A`, or
  a `HarnessError` such as `chezzi` not on `PATH`). Findings win: a break after a confirmed divergence
  exits `1`. On abort the run leaves the seed loop, so the `done:` line still prints how many seeds
  ran (`done: 1 of 50 seeds 0..50, 0 finding(s) [HarnessError 1]`).
- **Every sweep prints an outcome histogram.** `difffuzz` and `panicfuzz` count `kind_label(&outcome)`
  per seed: `done: 200 seeds 0..200, 0 finding(s) [Match 200]` vs `done: 20 seeds 0..20, 0 finding(s)
  [Timeout 20]` (nothing compared). There is no "too many timeouts" threshold: a verdict that cannot be
  certain must decline (W7-12).
- **The CI gates refuse to pass over zero comparisons.** libtest hides stderr on a passing test, so
  `tests/difftest.rs::fuzz_range_cfg` and `tests/panicfuzz.rs::fuzz_range_cfg` assert that the count of
  real comparisons is non-zero (`Match`/`AllowListed`/`Divergence`/`HostPanic`/`BothError`;
  `Clean`/`HostPanic`/`Crash` for panicfuzz). `Timeout` and `HarnessError` are not comparisons (W7-38).
- **A SIGKILL is not a Chezzi bug.** Both oracles capture the signal number. Only SIGILL, SIGTRAP,
  SIGABRT, SIGBUS, SIGFPE, SIGSEGV and SIGSYS are findings (`HostPanic`/`Crash`). Any other signal
  (SIGKILL from the cgroup OOM killer or `kill -9`, SIGTERM from a CI reaper) or an unknown signal
  becomes a `HarnessError` (W7-12, W7-38). Reports name the signal: `--- chezzi killed by SIGSEGV
  (signal 11) ---`.
- **A Chezzi timeout gets a second look.** `generate.rs` bounds every loop (`LOOP_CAP`, a bounded
  `for`, a mandatory `while` increment on a reserved counter). On a Chezzi timeout, `run_sources` runs
  Python. If Python exits 0, Chezzi runs once more at 3× the timeout (a guard against a loaded CI
  box), and only a second timeout becomes `Outcome::Divergence { kind: DivKind::ChezziHang, .. }`.
  Otherwise the outcome stays the non-finding `Outcome::Timeout { which: "chezzi" }` (W7-36).
- **The both-failed arm compares the shared stdout prefix.** A byte difference inside
  `chz.stdout[..n] == py.stdout[..n]` is `Outcome::Divergence { kind: DivKind::BothErrorStdout, .. }`.
  One side that prints more before its own fault stays `BothError`, because CPython fails at parse
  time with no stdout while Chezzi often fails at run time after it prints (W7-36).

### Construct coverage

Granular `Features` flags, all on in `full()`:

- **String methods** (`string_methods`): `upper`, `lower`, `replace`, `split`, `join`, `starts_with`,
  `ends_with`, `contains`. The renderers map names (`starts_with` -> `startswith`, `ends_with` ->
  `endswith`; `contains` -> `sub in recv`). A `split` `sep` is always a non-empty literal: an empty
  `sep` splits per char in Chezzi and raises `ValueError` in Python. A `replace` `old` is kept non-empty
  by choice only; an empty `old` interleaves the same way in both.
- **Slicing + negative indexing** (`slicing`): `xs[a..b]` / `xs[Range(a, b, c)]` on lists and strings
  (`xs[a:b:c]` in Python) and a negative scalar index `xs[-k]`. Both clamp out-of-range bounds and
  reject step `0`, so the generator never emits step `0` and keeps negative indices in `[-len, -1]`.
  Slice results carry `len: None`, so they are never scalar-indexed.
- **Membership** (`membership`): `x in xs`, `k in m`, `sub in s`.
- **Tuples** (`tuples`): literals `(a, b)`, positional fields (`(t).N` / `(t)[N]`), destructuring
  (`a, b := t` / `a, b = t`). The shim's `_chz_str` spells tuples (`(1, 'two', true)`). Arity is ≥ 2,
  because `(1,)` and `()` print differently in Chezzi.
- **Floats** (`floats`): `Add`/`Sub`/`Mul`/`Div`/`Mod` over in-scope float variables, call and index
  leaves, and literals; floats are assignable (`=`, `+=`, `-=`). Leaves are `n/8` scaled by `2^e`
  (exact), which reaches Python's scientific-notation crossover (`|x| >= 1e16` / `< 1e-4`).
  Magnitudes compose across statements, so `inf`, `NaN` and `-0.0` are reachable. Comparisons may draw
  float operands. Int↔float mixing (`1 + 2.0`, either side, 0.3 chance) needs no IR change; `MAX_BOUND`
  straddles `2^53` and the VM compares int to float exactly. Float `/` is total IEEE-754 (zero divisor
  -> `inf`/`-inf`/`NaN`); the shim's `_chz_fdiv` absorbs CPython's `ZeroDivisionError`. Float `%` is
  `fmod` (sign of the dividend, as in Rust and Go); the shim's `_chz_fmod` absorbs CPython's floored,
  raising `%` (a zero divisor or an `inf` dividend gives `NaN`).
- **Calls and indexes of every scalar type.** `gen_bool`, `gen_str` and `gen_float` each call
  `try_call` and `try_index`. An int argument stays a small literal, because the callee's body and
  `ret_bound` assume `|int param| <= PARAM_BOUND`.

The i64-no-overflow guarantee holds: a tuple-field read inherits the element's `tuple_bounds` and is
never emitted inside an in-loop accumulator RHS. Method, `in` and slice results carry no int value.

### Why it is not a tautology

The Python side prepends a shim that implements Chezzi's **specification**: `_chz_str` (`true`/`false`
and the Chezzi float format), `_chz_div`/`_chz_mod` (truncate toward zero, sign of the dividend),
`_chz_repr` (CPython `repr` for a string nested in a container). The Chezzi side runs the
**implementation**. The shim absorbs only by-design surface differences, never a result, so a stdout
difference means the implementation broke its contract. `oracle_detects_real_divergence` proves the
harness catches a real divergence (raw Python `%` vs Chezzi `%` on negatives).

A shim that encodes a divergence hides it: a detector written to mirror the implementation cannot see
a bug in what it mirrors. When a divergence is fixed, make the shim arm the identity (W7-25).

**Correct by construction.** Each generated program is well-typed and in scope, each divisor is
non-zero, each index is in range, and `generate.rs` bound tracking keeps every int in a safe window. A
Chezzi fault is therefore a real bug. Deliberate overflow testing is the opt-in metamorphic mode (P5):
a Chezzi recoverable panic vs a Python bignum is a separate outcome, not a stdout diff.

```sh
cargo test --test difftest                 # CI gate: P0 probes + fixed seed range (reproducible)
cargo test --test difftest -- --ignored    # heavier sweep (fuzz_full_heavy, seeds 0..3000)

cargo build --release --bin chezzi --bin difffuzz
./target/release/difffuzz --seeds 0..100000          # unattended fuzz (full features)
./target/release/difffuzz --seed 12345               # reproduce one seed (prints both sources)
./target/release/difffuzz --seeds 0..5000 --floats   # floats (on by default; explicit here)
```

A finding prints the seed, both sources and both captures. Reproduce it with `--seed N`, then route it
through `auto-task` -> `post-merge-gate`.

## DSA known-answer harness (`judge/`)

The differential generator avoids overflow, recursion and anything outside a cross-language subset.
Real algorithms live at those edges. The judge runs hand-written competitive-programming solutions
(deep recursion, big-int boundaries, heavy `List`/`Map`/`Set` churn, grids, slicing) against known
answers, so its oracle depends on neither the VM nor CPython.

- **Source of truth:** the CSES Problem Set. Each `judge/problems/<slug>/solution.chz` reads stdin
  (`std.io.read_line`). It is vetted once against the published answer; a later divergence is a
  candidate regression.
- **Test cases:** committed public samples in `judge/problems/<slug>/samples/*.in`/`.out`, plus
  generated or official cases in `judge/data/<slug>/` (gitignored). Official CSES hidden data is the
  authors' IP; drop it into `judge/data/` by hand if you have it. `judge/fetch_problem.py <url>`
  scaffolds a problem from its public statement and samples.
- **The harness is a Chezzi program** (`judge/run.chz`). It runs each case with
  `sh -c "timeout N chezzi run solution.chz < case.in"` (`std.process` cannot pipe stdin yet), so a
  hang (`TIME`), crash or host panic (`PANIC`) stays isolated. It compares output with whitespace
  normalized (CSES checkers ignore whitespace).

**Generated oracle (no download).** Each problem ships `gen.py` (random input within the stated range)
and `reference.py` (an independent Python solution: a brute force for small inputs plus a fast path
for stress inputs). `judge/generate.py` feeds the same input to both and writes `judge/data/<slug>/`.
The reference uses a different algorithm from the solution (union-find vs flood fill, enumeration vs
DP), so an agreeing pair cannot hide a shared bug. 12 problems (11 CSES + 1 Codeforces), 318 committed
and generated cases, all clean.

```sh
cargo build --release
python3 judge/generate.py [--count N]                      # synthesize in-domain cases (no download)
./target/release/chezzi run judge/run.chz                  # all problems (samples + generated cases)
./target/release/chezzi run judge/run.chz weird_algorithm  # one problem
./target/release/chezzi run judge/run.chz --samples-only    # committed samples only (the cargo gate)
python3 judge/fetch_problem.py <url> [slug]                # scaffold a new problem from its statement
```

`tests/judge.rs` gates the harness on every `cargo test` (type check + the committed samples).

Verdicts: `PASS`; `WRONG` (prints the first differing line); `FAULT` (Chezzi runtime error + exit
code); `PANIC` (Rust host panic, a Chezzi bug for certain); `TIME` (timeout). A problem with no cases
is skipped, never failed. For a non-`PASS` verdict on a vetted solution: minimize the failing `.in`,
then land a failing-then-green test before the fix.

## Seeded scheduler oracle (`src/schedfuzz/`)

The other oracles never vary the schedule. The two-worker-count gate (`tests/chezzi_threads_cli.rs`)
is two fixed schedules. Before this oracle, a person found each scheduler and netpoller race (W8-7,
W8-8, W11-4, W13-26/27/28, W15-1) by running a hand-built program against Go.

**How it works.** `CHEZZI_SCHED_SEED=<u64>` (`src/vm/sched_seed.rs`) takes every free scheduler choice
from a seeded PRNG: which fiber a pop returns, the step-0/steal cadence, `handoff_wake`'s `runnext`
coin flip, the reds refill. At `CHEZZI_THREADS=1` the same seed replays the same schedule (see the
replay limit). At `CHEZZI_THREADS>=2` it also injects random yields, spins and sleeps at each sync
point, to widen real OS-thread race windows (the loom/shuttle pattern).

```sh
cargo build --release --bin schedfuzz --bin chezzi
target/release/schedfuzz --seeds 1..33 --threads 1,2,0                   # default corpus sweep (0 = default worker count; W15-1 only reproduces there)
target/release/schedfuzz --program tests/sched_seed/interleave.chz       # one program
CHEZZI_SCHED_SEED=12345 CHEZZI_THREADS=1 cargo run -- run <file>         # replay one seed without the harness
```

A finding prints `FINDING kind=<hang|panic|rc|output> seed=<S> threads=<T> program=<path> replay:
CHEZZI_SCHED_SEED=<S> CHEZZI_THREADS=<T> chezzi <test|run> <path>`. Paste the `replay:` clause to
reproduce.

**Baseline.** Each run is judged against an unseeded baseline of the same binary. `measure_baseline`
runs `BASELINE_REPS = 5` unseeded reps at the worker count of the job it judges.

- Reps that disagree on stdout only turn `check_output` off for that target; hang, panic and rc checks
  stay live.
- Reps that disagree on rc, time out or panic mark the target `UNSTABLE`, and the sweep skips it.
- `target_for` loads each sibling `.expected`; sampling alone decides whether the output check is
  trusted. `corpus()` includes each `examples/*.chz` with a sibling `.expected` and a concurrency
  keyword.
- `KNOWN_TARGETS` (`src/schedfuzz/mod.rs`) skips targets by file name on a default sweep (no
  `--program`), each citing its gaps row: **W15-4** (output crosses a `spawn`/`Executor` boundary, the
  documented contract), **W15-5** (wall-clock-ratio tests that flake under the sweep's own CPU load),
  and the filed bugs **W15-3**, **W15-6**, **W15-7**. `--program <path>` ignores the skip list.
- A `KNOWN_TARGETS` entry skips the whole target, including its hang, panic and rc checks. An
  output-only skip is not built.

A clean-tree sweep (`--seeds 1..17 --threads 1,2,0`, 2026-09-23) exits 0:

```
done: 177 targets, 8496 runs, 0 finding(s), 0 unstable, 11 known
```

The history of the false positives that shaped these rules is in the
[records](../history/bug-hunts/bug-discovery-records.md#seeded-scheduler-oracle-the-harness-false-positive).

**Replay limit at T=1.**

1. Byte-for-byte for the six wait shapes of
   `tests/sched_seed_cli.rs::the_same_seed_replays_byte_for_byte_at_one_worker`: a fiber-owned nested
   fan-out, a flat fan-out, a body `recv` on a rendezvous and on a bounded channel, a body `wait:`, a
   `send` inside a native callback. 80/80 each, idle, under 8 and under 40 CPU hogs (seeds 1-8 × 10
   runs, release). A gated thread picks and draws only while it holds the runner permit
   ([`concurrency-runtime.md`](../design/concurrency-runtime.md#one-runner-budget)).
2. A measured rate for the waits in gaps row **W15-10**: an in-place nursery join, a `Shared` guard
   wait, an `Executor` join and job start, a `recv` and a `wait:` inside a native callback (80/80 idle,
   a rate under 40 hogs). W15-10 carries the numbers.
3. Timers, sockets, blocking natives and eager-nursery programs are outside the RNG stream.

**Owner decision 2026-10-05: the seed is a fuzzer first.** Byte-for-byte T=1 replay is not required for
every wait kind. A wait that replays exactly stays in `FIXTURES`; seeded mode adds no seeded-only gate,
queue or park path. The W15-10 waits stay rate fixtures in `tests/sched_seed/open/`. A fix that makes
one exact ships only if unseeded runs pay nothing: release, interleaved, n >= 5,
`tests/shared_update_contention.rs`, `benches/sched/rendezvous_pingpong.chz` and
`examples/primes_parallel.chz` at T=1/2/0 inside base's spread. TICKET-209 measured both candidate
fixes outside that spread: a direct `Shared` guard hand-off cost 7.6× at T=1 (225 ms vs 29 ms), the
exact nursery-join fix about 2× (67 ms vs 34 ms).

**Mutation testing** (release, `--seeds 1..257 --threads 1,2,0`; 0 = default, 28 cores):

| mutation | fix reverted | T=1 | T=2 | default | note |
|---|---|---|---|---|---|
| W15-1 (TICKET-166, `0ff6a8b9`) | `close()` wakes a parked netpoller op | 0/256 | 0/256 | **152/256** | only the default worker count reproduces it; unseeded is 62/64 at default, 0/64 at T=1 and T=2 |
| TICKET-128 (`owner_scope_done`) | the T=1 hang that the debug smoke test gates | 0/256 | 0/256 | 0/256 | masked on release at every count (the debug smoke gate is red on this mutation) |
| W14-39 (TICKET-135, `a5b87cb5`) | cancel-unwind orphans a nested child | **217/256** | **78/256** | 0/256 | seeded rate is lower than unseeded at T=1 (84.8% vs 100%) and T=2 (30.5% vs 46.9%) |

**Standing limit.** Seeded perturbation does not widen a race window past what unseeded OS-thread
jitter finds at the same worker count. Seeding buys a replay at T=1 only. At T>=2 real OS timing still
decides the outcome, so a seed does not reproduce a T>=2 finding; W14-39's T=2 column is a rate, not a
per-seed guarantee. Use the seed for a bisect or a CI repro. A wider net needs perturbation at more or
other sync points (not attempted).

**Found on `main`:** W15-3. A `write` parked on a `Socket` that another task then `close()`s can return
`?v` instead of TICKET-166's error, at `CHEZZI_THREADS=1`: 12/16 seeds in the first sweep, 0/10
unseeded. TICKET-166's listener-accept fix did not cover the socket write path.
