# Chezzi — Bug Discovery Strategy

> **Status:** strategy doc. Captures *how* we systematically find correctness bugs in the
> implementation before committing to large work (e.g. the Cranelift JIT). The Tier-1 mechanical
> levers are now built — **#1 panic-fuzz (`src/panicfuzz/`)** and **#2 CPython differential
> (`src/difftest/`)**; the rest below is the ranked plan. Live status in
> [`PROGRESS.md`](../PROGRESS.md); design rationale for the language in [`spec.md`](spec.md).

## Why this exists

Manual edge-case probing (hand-writing `1.0/0.0`, `sum()` overflow, etc.) finds a handful of bugs but
does not scale and is biased toward bugs you can already imagine. Mature language implementations
**automate** bug discovery with adversarial input generators + independent oracles. This doc records
the techniques, who uses them, and the ranked plan for Chezzi.

## The blind spot in our current setup

Chezzi has a **two-VM parity oracle**: every golden test asserts the serial `--serial` VM and the
default M:N VM produce byte-identical output. This is a real differential for *concurrent* programs (it
catches scheduler-dependent divergence) — but it has one structural blind spot:

> **Both engines are the same `Vm`; for sequential programs they run identical bytecode.** So for
> non-concurrent code, parity is a determinism check, not a differential — it catches
> *divergence*, never *shared wrongness*.

Every bug found in the June 2026 hunt was invisible to parity because **both** engines had it:
- `List.sum()` silently wrapped i64 overflow on both engines → parity green → undetected.
- `nan < 1.0` faulted with a misleading message on both engines → parity green → undetected.
- Float div-by-zero faulted (non-IEEE) on both engines → parity green → undetected.

The remedy is twofold and is the core of this strategy:
1. an **external oracle** (compare against CPython — Chezzi is Python-feel) to catch *semantic* bugs both engines agree on, and
2. **fuzzing the Rust host** to catch *crashes* (panics on malformed input) the curated test corpus never exercises.

## What we already have

- **Two-VM parity oracle** — `assert_parity` / golden `examples/*.chz` + `.expected` (~2736 unit tests). Catches serial-vs-M:N scheduler divergence (a determinism check for sequential code, which runs identical bytecode on both engines).
- **Grammar conformance** — `docs/grammar.bnf` is executed and differential-tested against the parser (`src/conformance.rs`, `tests/corpus/`, `cargo test conformance`). Syntax-level only — not semantics.
- **CPython bench harness** — `benches/run.chz` runs paired programs in `benches/chz/` (Chezzi) and `benches/py/` (Python) and compares **timing**. The paired programs seeded the output-differential oracle below.
- **CPython differential oracle** — ✅ **built** (lever #2). `src/difftest/` generates random semantically-equivalent programs, renders each as both Chezzi and Python, runs both, and diffs stdout. Wired as the `tests/difftest.rs` CI gate (fixed seed range, reproducible) and the `src/bin/difffuzz` long-runner. See "Differential oracle" below.
- **Front-end panic-fuzzer** — ✅ **built** (lever #1). `src/panicfuzz/` feeds adversarial / malformed inputs to `chezzi check` (lexer + parser + checker) under a wall-clock timeout and flags any Rust panic or signal crash. A stable, dependency-free **subprocess** harness (a stand-in for `cargo-fuzz`, which is unavailable here: no nightly + no `[lib]`). Wired as the `tests/panicfuzz.rs` CI gate (seeds `0..2000`) and the `src/bin/panicfuzz` long-runner. See "Panic-fuzz harness" below.
- **DSA known-answer harness** — ✅ **built**. `judge/` runs hand-written competitive-programming solutions (`judge/problems/<slug>/solution.chz`) against known-correct CSES answers — a third oracle that catches *shared wrongness* both engines agree on, independent of CPython. The harness itself is written in Chezzi (`judge/run.chz`). See "DSA known-answer harness" below.
- **Adversarial review pipeline** — `auto-task` (prosecute→defend→judge) + `post-merge-gate`. Good at vetting a *known* change; not a *discovery* tool.

## How real implementations find bugs

| Technique | Who uses it | What it catches | Fit for Chezzi |
|---|---|---|---|
| **Coverage-guided fuzzing** (libFuzzer/AFL) | Rust `cargo fuzz`; SQLite `dbsqlfuzz`; JS `Fuzzilli` | parser/checker **panics** on malformed input (should be clean errors, not Rust crashes) | ✅ **built** as a subprocess panic-fuzzer (`src/panicfuzz/`) — `cargo-fuzz` unavailable here (no nightly + no `[lib]`) |
| **Differential vs reference impl** | GCC/LLVM (CSmith vs `-O0`/`-O3`); JS engines cross-tested | **shared** semantic bugs — wrong output both engines agree on | ⭐ Python-feel + CPython harness already exists |
| **Metamorphic / EMI** | GCC/LLVM (Equivalence-Modulo-Inputs, 400+ bugs) | optimizer bugs: behavior-preserving transforms that change output | peephole/const-fold on==off, `x`→`(x)`, dead-code inject |
| **Property-based** (proptest / Hypothesis) | CPython, Rust | invariant violations: `parse∘print==id`, idempotence, roundtrips | compiler pipeline |
| **Miri / sanitizers** (UBSan/ASan/TSan) | Rust (Miri); Go (race detector) | UB in `unsafe`; data races | ⭐ GC + FFI + OS-thread engine are fragile (see the FFI SIGSEGV history) |
| **loom** (exhaustive interleavings) | Rust concurrency libs | concurrency races by exploring all schedules | channels / `Shared` / executor / netpoller |
| **Coverage measurement** | SQLite (100% MC/DC) | untested branches, especially error paths | `cargo-llvm-cov` |
| **Dogfooding real programs** | every language | ergonomic + integration + missing-feature + semantic bugs fuzzing can't reach | a real CLI app |

SQLite is the extreme reference point: ~600× more test code than source, every `malloc`/IO failure
path exercised via fault injection. Rust's **crater** runs a candidate change against all of
crates.io. The common thread is *automated oracles + adversarial input generation*, not hand-written
cases.

## Ranked plan for Chezzi

**Tier 1 — would have auto-caught the June 2026 bugs; closes the blind spot:**

1. **Panic-fuzz the front-end (lexer → parser → checker).** ✅ **Built** — `src/panicfuzz/` (see
   "Panic-fuzz harness" below). Feed adversarial / malformed inputs to `chezzi check`. Invariant:
   *the pipeline never crashes — malformed input yields a clean diagnostic, never a Rust panic /
   `unwrap` / index out-of-bounds / arithmetic overflow / stack overflow / signal kill.* Implemented
   as a **stable, dependency-free subprocess harness** (a hand-rolled stand-in for `cargo-fuzz`),
   not `cargo-fuzz` itself, because this environment has **no nightly / rustup / cargo-fuzz** and the
   crate is **binary-only (no `[lib]`)** to link a fuzz target against — and shelling out catches
   **more** crash classes than in-process `catch_unwind` (notably stack overflow, the most likely
   deep-parser crash). The single highest-yield mechanical lever for a Rust-hosted language.
2. **Differential vs CPython.** ✅ **Built** — `src/difftest/` (see "Differential oracle" below). The
   external oracle that defeats the shared-bug blind spot: it would flag `sum()` overflow and `nan <`
   immediately. The documented intentional divergences are handled *structurally* by a Python
   **shim** (mirrors Chezzi's spec — `true`/`false`/`nil` spelling, raw nested strings, truncating
   `/`,`%`) rather than by a big allow-list, so the allow-list stays near-empty and any hit is a
   genuinely new category to triage.
3. **Miri + ASan on the `unsafe` surface** (GC, FFI/libffi, raw pointers, any `transmute`).
   `cargo +nightly miri test` for UB; ASan on the C-ABI paths. Targeted at the most memory-fragile code.

**Tier 2 — structural, medium effort:**

4. **`proptest` properties:** `parse∘print == id`; peephole on == off; const-fold result == unfolded
   result; `--serial` == default over *randomly generated* programs (extends parity beyond
   the curated corpus).
5. **Generative grammar fuzzer** from `grammar.bnf` → random *valid* programs → run both engines +
   CPython. Complements #1 (which targets the error paths) by targeting the *accept* paths.
6. **TSan / loom on the concurrency engine** — channels, `Shared`, `Executor`, netpoller under race
   detection / exhaustive interleaving.

**Tier 3:**

7. **Coverage measurement** (`cargo-llvm-cov`) — find untested branches, especially error paths; write
   tests for the gaps. (The auto-task found bugs in exactly the spots that lacked adversarial tests;
   coverage makes those gaps visible.)
8. **Dogfooding** — write real programs (a JSON CLI, a mini-interpreter, a log analyzer). Finds
   semantic/ergonomic/integration bugs and missing-feature friction that fuzzing structurally cannot.
9. **Manual feature-audit (agent-driven).** ✅ **Playbook below.** A structured adversarial sweep of the
   feature domains the generators *cannot reach* — generics, `match`/enums, closures, protocols,
   modules, namespace/import gating. The June 2026 *automated* hunt found numeric/parity bugs; the
   June 27 *manual* audit found a **`match` exhaustiveness soundness hole** + namespace leaks the
   differential generator can never surface (it emits no generics/enums/match). See the playbook.

## Recommended starting point

**#1 (panic-fuzz front-end) + #2 (CPython differential)** are both ✅ **built** — together they cover
both *crashes* (fuzz) and *wrong answers* (differential), the two techniques that directly close the
parity blind spot. Run them unattended; triage findings through the existing
`auto-task` → `post-merge-gate` pipeline.

Pre-JIT gate: the JIT is a large, late-stage endeavor that assumes the VM semantics are
correct (it must produce byte-identical results to the VM). Standing up Tier 1 first — so the
reference semantics are fuzzed and differentially validated — de-risks the JIT before a line of it is
written.

## Manual feature-audit playbook (agent-driven — finds what the generators can't reach)

> **Run this when** the automated oracles (panic-fuzz, CPython-differential, DSA) have gone quiet but
> the language is still pre-freeze (pre-JIT). "No bugs found by the fuzzers" ≠ "the language is
> correct" — it means *that bug class* is exhausted. The fuzzers only exercise a narrow surface; this
> playbook attacks the rest by hand. **It is the highest-yield way to find correctness bugs in the
> typed feature surface**, and it is repeatable every session.

### Why the generators miss these (the structural gap)

`src/difftest/generate.rs` is **correct-by-construction over a deliberately small surface**: straight-line
code + simple non-recursive functions over `int`/`List`/`Map`/`str`/`tuple`/slicing, with conservative
bounds (it *avoids* overflow, deep nesting, aliasing). It emits **no** generics, structs, enums, `match`,
closures/HOF, protocols, `Result`/`Option`/`?`, recursion, or modules. So every bug in those features is
invisible to it. The two-VM **parity** oracle is also blind here: both engines are the same `Vm`
running identical bytecode for this sequential code, so they share bugs — parity catches *divergence*, never *shared wrongness*. A bug that both engines agree
on **and** lives in an un-generated feature is undiscoverable by automation. That is exactly where the
real bugs have been.

### Bug taxonomy this audit targets

Every finding so far falls into one of these (use as a "what am I hunting" checklist):

- **Feature doesn't work as documented** — the spec says X, the impl does Y (e.g. `match` accepts a
  non-exhaustive set then faults at runtime — a *soundness* hole: `check` passes, the program traps).
- **Namespace pollution / name leak** — a builtin name resolves without its `import` (e.g. `Socket`
  usable with no `import std.net`), or a builtin type name can be *redeclared* (`struct int`) and is
  then silently shadowed → self-contradictory diagnostics (`expected return type Socket, found Socket`).
- **Import / path resolution** — a module resolves to the wrong file, fails for a file that exists, or
  is cwd-sensitive; double-eval of a module's top level; bad error attribution across a transitive chain.
- **Numeric / scalar edge cases** — overflow at a seam the fuzzer skips (`*`, `pow`, shifts, `abs(MIN)`),
  float formatting (`-NaN`), div/mod sign rules, slice steps, unicode indexing, aliasing/copy semantics.
- **Unintended feature from a bug** — an "abnormal feature" that only exists because of an
  implementation accident (an accepted form the spec never sanctioned); it WILL be relied on, then break.
- **Diagnostic-quality bugs** — wrong/misleading error text (`"earlier push"` when there was none),
  even when the accept/reject decision is correct. Low severity, high user-confusion.
- **Hidden / latent** — passes every existing test because the tests never exercise the angle. The
  audit's whole point: surface these before a JIT freezes them into native code.

### The procedure (repeatable)

1. **Measure the blind spot first.** Re-read `src/difftest/generate.rs`'s `Features`/`gen_*` to confirm
   what's *still* un-generated — that list IS your target domains. (Don't re-hunt a domain the generator
   now covers.)
2. **Build, then fan out.** `cargo build --release`. Dispatch one agent **per feature domain** in
   parallel (`superpowers:dispatching-parallel-agents`) — domains are independent, no shared state.
   The five domains below + import-resolution were the June 27 split; reuse or refine.
3. **Per-agent protocol** (bake into every prompt):
   - **Read the spec FIRST** (`docs/syntax.md`, `docs/spec.md`, `docs/stdlib.md`) — distinguish a *bug*
     from *intended design*. Chezzi's intended Python-divergences (i64 not bignum, truncating `/`,`%`,
     static typing) are **not** bugs; silent wrapping / UB / a runtime fault on an accepted program is.
   - **Write tiny focused `.chz` probes**, one angle each.
   - **Run on BOTH engines** (`run` and `run --serial`) **and** `check`. Divergence = bug. Where the
     semantics should match Python, diff against `python3`.
   - **Evidence-gated, reproduced-only.** Each finding: minimal repro + exact command + EXPECTED (cite
     spec) + ACTUAL (quoted) + serial/M:N divergence? + severity. No speculation; "clean" if an angle
     yields nothing, with the angles covered listed.
4. **Triage each confirmed bug** through `auto-task` (research → plan → TDD → prosecute/defend) →
   merge → `post-merge-gate`. Batch by file seam (most checker bugs touch `src/checker/mod.rs` in
   *disjoint functions* — safe to parallelize as separate auto-tasks, merge serially).

### Domain checklist (the reusable target list)

| Domain | High-value angles (start here) |
|---|---|
| **Generics** | turbofish at decl vs use site; nested (`Map[str,List[int]]`); return-only param inference (`empty[T]()`); generic methods/static methods; protocol bounds + calling a bound method on a type param; recursive generic enums; arity/mismatch → clean error not panic |
| **`match`/enums** | exhaustiveness with **guards** (`A if c` must NOT close A) and **refutable payloads** (`Some(0)`, `Pair(0,y)` must NOT close); nested single-variant *is* irrefutable; or-patterns binding consistency; redundant/duplicate arms; range patterns; match-as-expression divergent arms |
| **Closures/defer/recover** | loop-variable capture (**fresh cell per iteration** — must NOT be late-binding); uniform by-reference capture (local/global/`ref` all share the live binding — a closure write is visible outside); a plain capture into a `spawn` is an isolated per-task copy (F1 divergence); escaping closures; `defer` block shares by reference (sees latest) vs `defer f(x)` eager args; `recover` catching overflow/index/div0; re-panic in `recover` |
| **Namespace/import gating** | redeclare a builtin type (`struct int`/`List`/`Socket`/`range`) → must be `reserved (builtin)`, never silently shadowed; bare std type without `import` → import hint; `import X from M` for a pure type runs on BOTH engines (the `bind_import` skip); reserved-name shadowing by var/param/type-param/field |
| **Import/path resolution** | cwd-sensitivity; nested `chezzi.toml` (entry-root vs import-root must agree); dotted paths; dir/file name collision; diamond re-import (top-level runs once?); a local module shadowing a `std.*` name; entrypoint `:fn` suffix forms; transitive-error attribution |
| **Numeric/string/collection** | overflow at `*`/`pow`/shifts/`abs(MIN)`/fold; float `-NaN` via format-spec; div/mod sign + by-zero; slice neg-step/OOB/step-0; unicode index/len; **aliasing** (list-of-list shared ref, `b:=a`, captured-list mutation); NaN/inf in sort/keys/compare |

### Procedure gotchas (these cost real time — internalize them)

- **Verify the CLI via `cargo run --release --bin chezzi -- …`, never a hardcoded `target/` path.**
  auto-task worktrees redirect `target-dir` to `~/.cache/chezzi-target` (a git-excluded `.cargo/config.toml`);
  main builds to `./target`. A hardcoded cache path serves a *stale worktree binary* → a fake "bug".
  `cargo build` can even report "Finished" while the on-disk artifact is missing/stale. Trust
  `cargo run`/`cargo test`, not a path + mtime. (4 bins exist → `cargo run` needs `--bin chezzi`.)
- **An `ok()`/`check_src` unit test passing ≠ the CLI is correct.** `check_src` (test helper) runs
  *no desugar, single-module*; the real CLI runs `build_graph` (desugars) → `check_graph` (module-scoped
  keys). Use the `check_entry`/`entry_ok` helpers (which go through `build_graph` + `check_graph`) to
  regression-guard a finding on the **real** path, and confirm via the CLI binary itself.
- **Adversarially verify every fix — an over-broad fix over-rejects.** Twice this session a fix that
  closed the reported hole *also* rejected a legitimate neighbor (a nested single-variant match; a
  bound-param push). Always add a failing-then-green test for the *boundary* case the fix must NOT break.
- **Parity-green is necessary, not sufficient.** Both engines agreeing only rules out divergence bugs;
  it says nothing about shared wrongness. The external check (spec + CPython) is what catches those.

## Panic-fuzz harness (`src/panicfuzz/`)

Lever #1, built. A seeded, dependency-free generator emits adversarial / malformed inputs and feeds
each to the already-built `chezzi check <tmpfile>` (the full front-end: `resolver::build_graph` →
`checker::check_graph` = lexer + parser + checker). The single invariant it enforces: **malformed
input must yield a clean diagnostic, never a Rust panic or a signal crash.** It structurally mirrors
`src/difftest/` (its own copy of the `xoshiro256**` RNG; the same reader-thread + `try_wait` +
kill-on-timeout subprocess machinery) and is wired via a `#[path]` include into both the
`tests/panicfuzz.rs` CI gate (fixed seeds `0..2000`) and the `src/bin/panicfuzz` long-runner.

**Why a hand-rolled subprocess harness instead of `cargo-fuzz`.** Three constraints decided the
architecture:
- **No nightly / rustup / cargo-fuzz** in this environment — `cargo-fuzz` is unavailable.
- **The crate is binary-only (no `[lib]`)** by design, so there is nothing to link an in-process
  fuzz target / `libFuzzer` harness against.
- **Shelling out catches more crash classes** than in-process `catch_unwind`: a subprocess detects a
  **signal kill** (SIGSEGV / SIGABRT / **stack overflow** — the most likely deep-parser crash, which
  `catch_unwind` cannot intercept) as an exit code of `None`, *and* a Rust panic via the `panicked
  at` marker on stderr.

**Classification** (`run.rs`): `Clean` = exit 0, or non-zero with a clean diagnostic and no panic
marker (non-finding); `HostPanic` = stderr contains `panicked at` (a BUG); `Crash` = exit code is
`None`, killed by a signal (a BUG); `Timeout` = killed at the wall-clock budget (**not** a finding —
a slow input, not a crash). Only `HostPanic`/`Crash` are findings (`is_finding()`).

**Three combined generators** (`generate.rs`), all bounded to ≤ 2 KB and deterministic in `(seed,
corpus)`: (1) random UTF-8-ish byte strings; (2) a token-alphabet sampler over Chezzi keyword /
punctuation / operator spellings + random identifiers / numbers / newlines + indentation (reaches
deep parser states); (3) structure-aware **raw-byte** mutation of the `examples/*.chz` corpus (byte
flips, truncation, duplicated / removed lines, inserted braces / colons / indentation). A finding
reports the **seed + the raw triggering input** (the input *is* the artifact — no shrink pass in v1),
reproducible with `panicfuzz --seed N`.

**Parity is N/A for this lever** — it exercises only the front-end's crash-safety; it never runs the
VM, so the two-engine parity bar does not apply.

How to run:

```sh
cargo test --test panicfuzz                # CI gate: classify/clean/determinism unit guards + fuzz seeds 0..2000
cargo build --release --bin chezzi --bin panicfuzz
./target/release/panicfuzz --seeds 0..200000   # unattended sweep (~30-50 min of subprocess spawns)
./target/release/panicfuzz --seed 12345        # reproduce one seed (prints the triggering input)
```

**Overflow blind spot in release.** The `tests/panicfuzz.rs` gate runs the **debug** `chezzi`
(overflow-checks ON), so arithmetic-overflow panics ARE caught there. A **release** sibling `chezzi`
has overflow-checks OFF, so arithmetic-overflow wraps *silently* and is invisible in a release sweep
(the bin prints this NOTE). For a full overflow sweep, rebuild with `RUSTFLAGS="-C
overflow-checks=on"`. Signal / segfault / explicit-panic crashes are caught regardless of profile.

Status: the `0..2000` gate is green, and unattended sweeps of `0..100000` (release, overflow-checks
OFF) and `0..20000` (debug, overflow-checks ON) found **zero** panics or signal crashes — the
front-end is crash-safe over the inputs explored so far.

## Differential oracle (`src/difftest/`)

Lever #2, built. A seeded generator emits a *cross-language safe subset* (literals, bounded-int
arithmetic, bool/str ops, `if`/`for`/`while`, non-recursive functions, list/map/index/len, plus the
widened families below). One abstract IR (`ast.rs`) is rendered by two backends — `emit_chezzi`
(native ops/`print`) and `emit_python` — and the two are run and their stdout diffed (`run.rs`).

**Widened construct coverage** (granular `Features` flags, all on in `full()`):
- **String methods** (`string_methods`): the eight ASCII-identical methods `upper`/`lower`/`replace`/
  `split`/`join`/`starts_with`/`ends_with`/`contains`. The emitters map names per language
  (`starts_with`→`startswith`, `ends_with`→`endswith`); `contains` has no Python `str` method so it
  renders as `sub in recv`. Two by-design diffs are dodged by generator restriction (no shim): a
  `replace` `old` and a `split` `sep` are always **non-empty** literals (empty `old` is unchanged in
  Chezzi but insert-everywhere in Python; empty `sep` per-char-splits in Chezzi but `ValueError`s in
  Python).
- **Slicing + negative indexing** (`slicing`): Python-style `xs[a:b:c]` on lists/strings and negative
  scalar index `xs[-k]`. Both engines clamp out-of-range bounds identically and step `0` errors on
  both, so no shim — the generator just never emits step `0` and keeps negative scalar indices in
  `[-len, -1]`. Slice results carry `len: None`, so they are never scalar-indexed (no OOB seam).
- **Membership** (`membership`): `x in xs` (list element), `k in m` (map key), `sub in s` (substring) —
  native `in` on both sides, always `bool`.
- **Tuples** (`tuples`): literals `(a, b)`, positional fields (`(t).N` Chezzi / `(t)[N]` Python), and
  destructuring (`a, b := t` / `a, b = t`). The **single** new shim arm is the tuple stringify in
  `_chz_str` (`(1, two, true)` spelling, raw nested strings); single-element `(1,)` and empty `()`
  diverge from Chezzi's spelling, so the generator only emits arity ≥ 2.

The i64-no-overflow guarantee is preserved across these: the only new path where an int value crosses a
seam is a tuple-field read, which inherits the element's tracked `tuple_bounds` and is never emitted
inside an in-loop accumulator RHS; method (`str`/`bool`/`List[str]`), `in` (`bool`), and slice
(collection/`str`) results carry no int value, so they add no seam.

**Why it isn't a tautology.** The Python backend prepends a fixed *shim* that implements Chezzi's
**specification** (`_chz_str` for `true`/`false`/`nil` + raw nested strings + Chezzi float format;
`_chz_div`/`_chz_mod` for truncate-toward-zero / sign-of-dividend). Chezzi source uses the real
**implementation**. The shim absorbs only the by-design surface/semantic differences — never the
actual arithmetic or control-flow *result* — so a stdout divergence means the implementation
deviated from its own contract (a real bug). The `oracle_detects_real_divergence` test proves the
harness catches a genuine divergence (raw Python `%` vs Chezzi `%` on negatives).

**Correctness by construction.** Every generated program is well-typed and in-scope, every divisor
is non-zero, every index is in range, and every integer value is provably within a safe window
(`generate.rs` bound tracking) so it can never hit Chezzi's i64-overflow fault — that makes a Chezzi
fault a real bug, not a generator artifact. (Deliberate overflow testing is the opt-in metamorphic
mode, P5 — a Chezzi recoverable-panic vs Python bignum, handled as a distinct outcome, not stdout.)

How to run:

```sh
cargo test --test difftest                 # CI gate: P0 probes + fixed seed range (reproducible)
cargo test --test difftest -- --ignored    # heavier sweep (fuzz_full_heavy, seeds 0..3000)

cargo build --release --bin chezzi --bin difffuzz
./target/release/difffuzz --seeds 0..100000          # unattended fuzz (full features)
./target/release/difffuzz --seed 12345               # reproduce one seed (prints both sources)
./target/release/difffuzz --seeds 0..5000 --floats   # enable the float backend
```

A finding prints the seed plus both rendered sources and both captures — paste-ready for a bug
report, reproduce with `--seed N`, then route through `auto-task` → `post-merge-gate`. To accept a
newly-discovered, deliberately-unfixed divergence, add a narrow matcher to `allowlist.rs` with a
cited reason.

## DSA known-answer harness (`judge/`)

A third oracle, complementary to the two above. The differential generator (`src/difftest/`) is
correct **by construction** — it deliberately keeps every int in a safe window so it never trips
overflow, never recurses, and only emits a cross-language-safe subset. That safety is exactly its
blind spot: real algorithms live at the edges it avoids. The DSA harness runs **hand-written
competitive-programming solutions** (deep recursion, big-int boundaries, heavy `List`/`Map`/`Set`
churn, grids, slicing) against **known-correct answers** — so it catches *shared wrongness* the
co-developed engines agree on, with an oracle that depends on neither engine nor CPython.

- **Source of truth:** the CSES Problem Set. Each `judge/problems/<slug>/solution.chz` is a
  hand-written Chezzi solution reading stdin competitive-style (`std.io.read_line`). It is **vetted
  once** against the published answer; after that, any divergence on re-run is a candidate Chezzi
  regression.
- **Test cases:** committed **public samples** (from the statement) under
  `judge/problems/<slug>/samples/*.in`/`.out`, plus generated/official cases under
  `judge/data/<slug>/` (gitignored). The main path generates them (see below); official CSES hidden
  data is the authors' IP (gated/solve-first, no API) — drop it into `judge/data/` by hand if you have
  it. `judge/fetch_problem.py <url>` scaffolds a new problem from its public statement + samples.
- **The harness is itself written in Chezzi** (`judge/run.chz`) — dogfooding: the judge is one more
  real program exercising the language, and it mirrors the `benches/run.chz` driver pattern. It shells
  out per case via `sh -c "timeout N chezzi run solution.chz < case.in"` (`std.process` has no stdin
  piping yet, so the case input is fed by shell redirection), so a solution that hangs (`TIME`),
  hard-crashes, or panics the host (`PANIC`) is isolated and reported instead of taking down the run.
  Output is compared whitespace-normalized (CSES checkers are whitespace-insensitive).

**Self-contained generated oracle (no download).** The primary path needs no CSES data at all: each
problem ships `gen.py` (random input within the problem's stated range/domain) and `reference.py` (an
**independent** Python implementation — ideally a brute force that's obviously correct on small inputs,
plus a fast path for large stress inputs). `judge/generate.py` feeds the same input to both and writes
`judge/data/<slug>/` (gitignored); `run.chz` diffs the Chezzi solution against the Python oracle — a
self-contained Chezzi-vs-Python differential. The oracle uses a *different algorithm* than the solution
(e.g. union-find vs flood-fill, sequence-enumeration vs DP) so an agreeing pair can't be hiding a shared
bug. Seeded with 12 problems (11 CSES + 1 Codeforces) spanning loop/bigint, Set, Map, DP+mod, grid
flood-fill, string iteration, modular loops, sort+i64, and 2^n recursion; 300+ generated + edge cases
run clean.

How to run:

```sh
cargo build --release
python3 judge/generate.py [--count N]                      # synthesize in-domain cases (no download)
./target/release/chezzi run judge/run.chz                  # all problems (samples + generated cases)
./target/release/chezzi run judge/run.chz weird_algorithm  # one problem
python3 judge/fetch_problem.py <url> [slug]                # scaffold a new problem from its statement
```

A non-`PASS` verdict on a vetted solution is a candidate bug: minimize the failing `.in`, then land a
failing-then-green unit test per the repo's TDD flow before fixing. Verdicts: `PASS` / `WRONG` (prints
the first differing line) / `FAULT` (Chezzi runtime error + exit code) / `PANIC` (Rust host panic — a
Chezzi bug for certain) / `TIME` (timeout). A problem with no cases is **skipped**, never failed, so
the harness is inert until samples are committed or data is fetched.
