# Chezzi — Bug Discovery Strategy

This page says how we find correctness bugs in the implementation before large work such as the
Cranelift JIT. The harnesses themselves (panic-fuzz, CPython differential, DSA judge, seeded scheduler)
are in [`bug-discovery-harnesses.md`](bug-discovery-harnesses.md). Live status is in
[`PROGRESS.md`](../../PROGRESS.md). Earlier incident write-ups are in
[`history/bug-hunts/bug-discovery-records.md`](../history/bug-hunts/bug-discovery-records.md).

## Why this exists

Manual edge-case probing (`1.0/0.0`, `sum()` overflow) finds a few bugs. It does not scale, and it
finds only the bugs you can already imagine. Mature language implementations automate bug discovery
with adversarial input generators and independent oracles.

An oracle that runs the same code twice catches divergence, never shared wrongness. Every bug of the
June 2026 hunt was of that kind:

- `List.sum()` wrapped i64 overflow without an error.
- `nan < 1.0` faulted with a misleading message.
- Float division by zero faulted (not IEEE).

The remedy has two parts:

1. An external oracle (CPython, because Chezzi is Python-feel) catches semantic bugs.
2. Fuzzing the Rust host catches crashes (panics on malformed input) that the curated corpus never
   reaches.

## What we have

| oracle | where | what it catches |
|---|---|---|
| Golden suite | `assert_golden_out` / `golden_entry*` (`src/vm/golden_tests/`), golden `examples/*.chz` + `.expected` | a change from a literal expected output |
| Two-worker-count differential | `tests/chezzi_threads_cli.rs` | a verdict on `tests/chz` that changes between the default worker count and `CHEZZI_THREADS=2` |
| Ported Go test/chan | `tests/chz/go/` (`NOTICE`) | channel behavior that drifts from Go on Go's own programs |
| Grammar conformance | `docs/reference/grammar.bnf`, `src/conformance.rs`, `tests/corpus/`, `cargo test conformance` | parser drift from the grammar (syntax only) |
| Panic-fuzz | `src/panicfuzz/`, `tests/panicfuzz.rs`, `src/bin/panicfuzz.rs` | a Rust panic or signal crash in `chezzi check` |
| CPython differential | `src/difftest/`, `tests/difftest.rs`, `src/bin/difffuzz.rs` | stdout that differs from CPython on generated programs |
| DSA known-answer judge | `judge/`, `tests/judge.rs` | wrong answers on hand-written CSES solutions |
| Seeded scheduler | `src/schedfuzz/`, `src/vm/sched_seed.rs`, `src/bin/schedfuzz.rs` | hangs, panics, exit-code or output changes under explored schedules |
| Adversarial review | `/kit:review` (prosecute, defend, judge) | defects in a known change (not a discovery tool) |

`benches/run.chz` also runs paired Chezzi and Python programs, but it compares timing only.

## How real implementations find bugs

| technique | who uses it | what it catches | fit for Chezzi |
|---|---|---|---|
| Coverage-guided fuzzing (libFuzzer, AFL) | Rust `cargo fuzz`; SQLite `dbsqlfuzz`; JS `Fuzzilli` | parser and checker panics on malformed input | built as a subprocess panic-fuzzer (`src/panicfuzz/`) |
| Differential vs a reference implementation | GCC/LLVM (CSmith); JS engines | semantic bugs both runs agree on | built (`src/difftest/`) |
| Metamorphic / EMI | GCC/LLVM (Equivalence Modulo Inputs, 400+ bugs) | optimizer bugs | peephole and const-fold on vs off; `x` -> `(x)`; dead-code insertion |
| Property-based (proptest, Hypothesis) | CPython, Rust | invariant breaks: `parse∘print == id`, round trips | compiler pipeline |
| Miri / sanitizers (UBSan, ASan, TSan) | Rust (Miri); Go (race detector) | UB in `unsafe`; data races | GC, FFI and the OS-thread engine |
| loom (exhaustive interleavings) | Rust concurrency libraries | races, by exploring every schedule | channels, `Shared`, `Executor`, netpoller |
| Coverage measurement | SQLite (100% MC/DC) | untested branches, mainly error paths | `cargo-llvm-cov` |
| Dogfooding real programs | every language | ergonomic, integration and missing-feature bugs | a real CLI app |

SQLite has about 600× more test code than source and injects a failure into every `malloc` and IO
path. Rust's crater runs a change against all of crates.io. The common method is automated oracles plus
adversarial input generation.

## Ranked plan

**Tier 1: closes the shared-wrongness blind spot.**

1. **Panic-fuzz the front end** (lexer, parser, checker). Built. Invariant: malformed input yields a
   clean diagnostic, never a Rust panic, `unwrap`, index out of bounds, arithmetic overflow, stack
   overflow or signal kill.
2. **Differential vs CPython.** Built. A Python shim implements Chezzi's specified divergences, so the
   allow-list stays empty and each hit is a new category to triage.
3. **Miri + ASan on the `unsafe` surface** (GC, FFI/libffi, raw pointers, `transmute`). Not built.
   Use `cargo +nightly miri test` for UB and ASan on the C-ABI paths.

**Tier 2: structural, medium effort.**

4. **`proptest` properties:** `parse∘print == id`; peephole on == off; const-fold result == unfolded
   result. Not built. The only standing schedule differential is `tests/chezzi_threads_cli.rs`: the
   curated `tests/chz` suite at the default worker count and at `CHEZZI_THREADS=2`. A generative
   version over random programs is not built.
5. **Generative grammar fuzzer** from `grammar.bnf`: random valid programs run against CPython. It
   targets the accept paths; #1 targets the error paths.
6. **TSan / loom on the concurrency engine** (channels, `Shared`, `Executor`, netpoller). Not built.
   This box has no nightly toolchain (`rustup` and `cargo +nightly` are missing), so TICKET-246 could
   not run TSan. The parallel-reader path rests on the compile-time `Heap: Sync` assertion and on
   `tests/rwshared_parallel_readers_stress.rs` at `CHEZZI_THREADS` 1, 2 and default. With a nightly,
   run: `RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test -Zbuild-std --target
   x86_64-unknown-linux-gnu --test rwshared_parallel_readers_stress`.

**Tier 3.**

7. **Coverage measurement** (`cargo-llvm-cov`): find untested branches, mainly error paths, and write
   tests for them. Auto-task found bugs in the spots that had no adversarial tests.
8. **Dogfooding:** write real programs (a JSON CLI, a mini-interpreter, a log analyzer). They find
   semantic, ergonomic and missing-feature bugs that fuzzing cannot reach.
9. **Manual feature audit (agent-driven).** See the playbook below. The June 27 2026 audit found a
   `match` exhaustiveness soundness hole and namespace leaks that the differential generator cannot
   reach (it emits no generics, enums or `match`).

## Recommended starting point

Run #1 (panic-fuzz) and #2 (CPython differential) unattended. Together they cover crashes and wrong
answers. Triage a finding with `/kit:debug`, then fix it and run `/kit:review`.

Pre-JIT gate: the JIT must produce byte-identical results to the VM, so it assumes the VM semantics
are correct. Fuzz and differentially validate the reference semantics before you write the JIT.

## Manual feature-audit playbook

Run this when the automated oracles go quiet and the language is still pre-freeze (pre-JIT). "No
fuzzer findings" means that one bug class is exhausted. The fuzzers reach a narrow surface; this
playbook attacks the rest by hand. Repeat it each session.

### The structural gap

`src/difftest/generate.rs` is correct by construction over a small surface: straight-line code and
simple non-recursive functions over `int`/`List`/`Map`/`str`/`tuple`/slicing, with conservative bounds
(no overflow, no deep nesting, no aliasing). It emits no generics, structs, enums, `match`,
closures/HOF, protocols, `T!E`/`T?`/`?`, recursion or modules. A bug in those features is invisible to
the CPython differential, and most real bugs live there.

### Bug taxonomy

Use this list as the "what am I hunting" checklist:

- **Feature does not work as documented.** Example: `match` accepts a non-exhaustive arm set, then
  faults at run time (a soundness hole: `check` passes, the program traps).
- **Namespace pollution / name leak.** A builtin name resolves without its `import` (`Socket` with no
  `import std.net`), or a redeclared builtin type gives self-contradictory diagnostics (`expected
  return type Socket, found Socket`).
- **Import / path resolution.** A module resolves to the wrong file, fails for a file that exists, or
  depends on the cwd; a module's top level runs twice; a transitive chain blames the wrong file.
- **Numeric / scalar edge cases.** Overflow at a seam the fuzzer skips (`*`, `pow`, shifts,
  `abs(MIN)`), float formatting (`-NaN`), div/mod sign rules, slice steps, unicode indexing,
  aliasing and copy semantics.
- **Unintended feature from a bug.** An accepted form the spec never sanctioned. Users will rely on it,
  then it breaks.
- **Diagnostic-quality bugs.** Wrong or misleading error text (`"earlier push"` when there was none),
  even when the accept/reject decision is correct.
- **Hidden / latent.** Passes every test because no test reaches the angle. Surface these before a JIT
  freezes them into native code.

### The procedure

1. **Measure the blind spot first.** Read `src/difftest/generate.rs` (`Features`, `gen_*`). The
   features it still does not generate are your target domains. Do not re-hunt a covered domain.
2. **Build, then fan out.** Run `cargo build --release`. Dispatch one agent per feature domain in
   parallel (`superpowers:dispatching-parallel-agents`). Use the domains below.
3. **Per-agent protocol** (put it in every prompt):
   - Read the spec first (`docs/reference/syntax/`, `docs/reference/spec/`,
     `docs/reference/stdlib/`) to tell a bug from intended design. The intended divergences from
     Python (i64 not bignum, truncating `/` and `%`, static typing) are not bugs. Silent wrapping, UB,
     or a run-time fault on an accepted program is a bug.
   - Write tiny `.chz` probes, one angle each.
   - Run `run` and `check`. For concurrent code, run again under `CHEZZI_THREADS=1` and
     `CHEZZI_THREADS=2`. A verdict that changes with the worker count is a bug. Where the semantics
     should match Python, diff against `python3`. Where they should match Go, write and run the Go
     program.
   - Report only reproduced findings: minimal repro, exact command, EXPECTED (cite the spec), ACTUAL
     (quoted), whether the worker count changes it, severity. Report "clean" with the angles covered
     when an angle finds nothing.
4. **Root-cause pass before any fix.** Group the confirmed findings by the fact they share. For each
   group, find every place that decides that fact, the grid it spans, the broken cells, and how many
   past tickets patched the same class. Method: [`lessons.md` §1](lessons.md). Worked example:
   [`history/bug-hunts/w16.md`](../history/bug-hunts/w16.md) (12 P0/P1 findings -> 5 families; 34 of 51
   earlier fixes were point patches). Show the write-up to the owner before you file.
5. **File one structural ticket per family**: delete the duplicated decision, add the single source,
   land one whole-grid test. The pipeline's planning stages reject a point patch in a systemic family.
   Fix directly only an isolated defect with no sibling copies (a one-site arithmetic guard, a stdlib
   edge). Order the tickets by file seam, so that two tickets that edit the same code never run at the
   same time.

### Domain checklist

| domain | high-value angles |
|---|---|
| **Generics** | turbofish at declaration vs use site; nested (`Map[str,List[int]]`); return-only parameter inference (`empty[T]()`); generic methods and static methods; protocol bounds and a bound method called on a type parameter; recursive generic enums; arity mismatch -> clean error, not panic |
| **`match`/enums** | exhaustiveness with guards (`A if c` must NOT close A) and refutable payloads (`?0`, `Pair(0,y)` must NOT close); a nested single-variant pattern IS irrefutable; or-pattern binding consistency; redundant arms; range patterns; divergent arms of a match expression |
| **Closures/defer/recover** | loop-variable capture (a fresh cell per iteration, never late binding); by-reference capture (a closure write is visible outside); a plain capture into a `spawn` is a per-task copy; escaping closures; a `defer` block sees the latest value, `defer f(x)` evaluates args eagerly; `recover` catching overflow, index and div0; re-panic inside `recover` |
| **Namespace/import gating** | redeclare a builtin type (`struct int`, `List`, `Socket`, `range`); a bare std type without `import` -> import hint; `import X from M` for a pure type; shadowing a builtin name by a variable, parameter, type parameter or field |
| **Import/path resolution** | cwd sensitivity; nested `chezzi.toml` (entry root and import root must agree); dotted paths; a directory and a file with the same name; diamond re-import (the top level runs once); a local module that shadows a `std.*` name; entrypoint `:fn` suffix forms; transitive-error attribution |
| **Numeric/string/collection** | overflow at `*`, `pow`, shifts, `abs(MIN)`, fold; float `-NaN` through a format spec; div/mod sign and by-zero; slice negative step, out of bounds, step 0; unicode index and len; aliasing (`b := a`, list of lists, a captured list mutated); NaN/inf in sort, keys and compare |

### Procedure gotchas

- **Check the CLI with `cargo run --release --bin chezzi -- …`, never a hard-coded `target/` path.**
  Auto-task worktrees redirect `target-dir` to `~/.cache/chezzi-target`; main builds to `./target`. A
  hard-coded path can serve a stale worktree binary and show a fake bug. `cargo build` can print
  "Finished" while the artifact on disk is stale. The repo has four binaries, so `cargo run` needs
  `--bin chezzi`.
- **A passing `ok()`/`check_src` unit test does not prove the CLI correct.** `check_src` runs no
  desugar and one module. The CLI runs `build_graph` (with desugar), then `check_graph` (module-scoped
  keys). Guard a finding with `check_entry`/`entry_ok`, which go through both, and confirm it on the
  CLI binary.
- **Prove each fix against its neighbours: an over-broad fix over-rejects.** Two fixes once closed the
  reported hole and also rejected a legal neighbour (a nested single-variant match; a push on a bound
  parameter). Add a failing-then-green test for the boundary case the fix must NOT break.
- **A green golden suite is necessary, not sufficient.** It proves the VM matches its own recorded
  output. The spec and CPython are the external check for shared wrongness.

## Load hygiene for synthetic-load experiments

Run each synthetic-load generator (a loop that spawns `chezzi` processes, a `yes`, any CPU hog for a
measurement) under one named scope:

```sh
systemd-run --user --scope --unit=<name> -- bash -c 'while true; do ...; done'
```

Tear it down with:

```sh
systemctl --user stop <name>
```

Do not start one as a bare background `while` loop. `pkill -9 -f "<pattern>"` kills the processes the
loop spawns, not the loop, and the loop respawns them. Every later measurement then runs under load
that nobody intended (gaps row **W15-8**; record in
[`bug-discovery-records.md`](../history/bug-hunts/bug-discovery-records.md#load-hygiene-the-w15-8-incident)).
`systemd-run --scope` puts the whole tree, loops included, in one cgroup that `systemctl stop` removes.
