# Chezzi — Claude Code Guide

Chezzi is a fast, statically-typed, Python-feel scripting language, hand-built in Rust.
Full design + roadmap: **[`docs/spec.md`](docs/spec.md)**. Syntax cheat-sheet: **[`docs/syntax.md`](docs/syntax.md)**. Canonical grammar: **[`docs/grammar.bnf`](docs/grammar.bnf)** (executed + drift-checked by `cargo test conformance`). Progress tracker: **[`PROGRESS.md`](PROGRESS.md)**.

## How we work

Claude implements directly. Ship working, tested code each session.

- Write real implementations, not `todo!()` stubs.
- Every milestone lands with passing tests and a clean `cargo build` / `cargo clippy`.
- Each compiler phase is its own module under `src/`. Keep modules focused.
- Verify before claiming done: run the tests and the relevant `chezzi` subcommand, show real output.
- Keep `PROGRESS.md` current after each session; commit in conventional, single-line messages.
- **Docs are part of the change, not a follow-up.** When a change adds/alters/removes observable
  behavior — syntax, stdlib/builtin surface, engine semantics, a flag, a limit lifted/added, or a perf
  delta — update the affected `.md` in the **same commit**: `PROGRESS.md` always, plus as relevant
  `docs/syntax.md`, `docs/spec.md`, `docs/grammar.bnf`, `docs/concurrency*.md`, `docs/future.md`,
  `docs/benchmarks.md`. Grep the docs for any now-stale claim about what you changed (`deferred`,
  `experimental`, `not yet`, `follow-up`, `mooted`) and fix or delete it. A green build with stale docs
  is not done.
- Match the existing code's style and patterns; reuse before adding new abstractions.

## Workflow per milestone

1. Implement the milestone (types, logic, wiring) in its module.
2. Add unit tests + a golden check against `examples/*.chz`.
3. `cargo test` + run the milestone's `chezzi` subcommand to verify end-to-end.
4. Update `PROGRESS.md`, commit, move on.

## Commands

```sh
cargo build --release    # compile (release; the VM is only fast optimized)
cargo test               # run unit + parity (both engines) + guiding tests
cargo test conformance   # execute docs/grammar.bnf, differential-test vs the parser
cargo clippy -- -D warnings   # lint (must be clean before commit)
cargo run -- help        # CLI usage

cargo run -- init my_proj                # scaffold a new project (chezzi.toml + src/main.chz + a _test.chz)
cargo run -- tokens examples/hello.chz   # token stream (M1)
cargo run -- ast    examples/hello.chz   # parsed AST (M2)
cargo run -- check  examples/hello.chz   # type-check only (M4); --errors=json for machine output
cargo run -- run    examples/hello.chz   # type-check + run on the VM, OS-thread engine (default, M5)
cargo run -- run --serial   examples/hello.chz   # cooperative single-thread VM (frozen parity oracle)
cargo run -- run --interp   examples/hello.chz   # tree-walk interpreter (frozen reference engine)
cargo run -- run --parallel examples/primes_parallel.chz   # accepted no-op alias (engine is now default)
cargo run -- run --threads=4 examples/primes_parallel.chz  # size the OS-thread pool (0/omitted = all cores; env CHEZZI_THREADS)
cargo run -- test examples/              # run every `test fn` in *_test.chz (M20); file or dir, default cwd
cargo run -- repl                        # interactive REPL (NOT IMPLEMENTED — stub errors; see src/main.rs:65)

cargo run -- run benches/run.chz         # Chezzi-vs-CPython bench harness (see docs/benchmarks.md)
```

> Flags go **before** the file path; anything after the file is passed to the program.
> `chezzi run` now defaults to the VM's real-thread OS-thread engine. `--serial` selects the
> cooperative single-thread VM (the frozen byte-identical parity oracle); `--parallel` is kept as a
> no-op alias for the default. `--threads=N` (or env `CHEZZI_THREADS`) sizes the OS-thread engine's
> worker pool — `0`/omitted = all cores, the flag wins over the env, and it errors with
> `--serial`/`--interp` (neither is multi-threaded). `--interp` (the frozen sequential reference
> engine) is mutually exclusive with an explicit `--parallel`, and `--parallel`/`--serial` are
> mutually exclusive.

## Conventions

- Commits: single-line conventional (`feat:`, `fix:`, `chore:`, `docs:`, `test:`). No body.
- Each compiler phase is its own module under `src/`: `lexer` → `parser` → `ast` →
  `desugar` → `checker` → `compiler` → `vm` (default engine) / `interp` (frozen tree-walk
  reference), plus `gc`, `native` + `runtime` (builtins / std), `resolver` (module paths).
- Keep modules small and single-purpose.
- Unit tests live next to the code in `#[cfg(test)] mod tests`.
- **Two engines, asserted equal.** Golden tests run each `examples/*.chz` through both the
  VM and the interpreter and assert identical stdout. The interpreter is frozen (slated for
  eventual removal) but parity is still the discipline — a VM change that diverges is a bug.

## Where things stand

Core language is **feature-complete through M18** (scalars, `list`/`map`/`set`/`tuple`,
generic structs + enums, `Result`/`Option` + `?`, generics + structural protocols,
exhaustive `match` + guards, closures/HOF, modules, GC, interpolation, pipe, `defer`,
`recover:`, `Iterator[T]`, slicing/indexing protocols). **Concurrency** has landed through
**Tier-D** (`spawn` / `parallel:` nursery, `Channel[T]`, `Shared[T]`, `Executor`, real
OS-thread engine via `--parallel`, netpoller + `std.net`). ~1500 tests green.

## Current focus

See **[`PROGRESS.md`](PROGRESS.md)** — single source of truth for "what's next."

Right now: **M19 — Perf track (in progress).** The language is frozen feature-wise; this milestone
is pure optimization, so the bar is **behavior-preserving + two-engine parity** on every change —
a VM speedup that diverges from the interpreter (or changes observable output) is a bug, not a win.
Landed: peephole/const-fold, superinstructions, `invoke_value` clone-kill, in-place call args,
stringify-into-buffer, global-slotting, `ConstStr` interning, struct-field IC, FxHash, call-loop
flatten, small-string optimization (SSO). Current gap to CPython: **~1.3×–3.5×** slower (worst on
call-bound `fib` 3.54×; `loop` 1.32× is at the dispatch floor), startup ~11× **faster**.

**Next perf batch (ranked, not started — start here next session).** The remaining gap is **call
overhead + per-op dispatch + a few alloc paths**, not the value model or GC; target is CPython 3.14
(specializing interpreter + optional JIT). Do **Tier 1 in order**: (1) **method-call IC + flatten
`do_method_call`** (hits `struct`/OO), (2) **trim per-op overhead in `run_until`** (lazy `span`, split
serial-vs-MN loop, inline the hottest ops — hits `loop`/`primes`), (3) **call-site specialization for
`Op::Call`** (hits `fib`). Then Tier 2 (adaptive opcode quickening, PEP 659) → Tier 3 (Cranelift
method-JIT). Full ranked detail + `file:line`s in **[`docs/future.md §4` "Post-M19 next levers"]**
and **[`PROGRESS.md` "Next perf batch"]**. Same discipline: measure (`benches/run.chz`) → failing-then-
green correctness test → keep parity → re-measure → record the delta in `docs/benchmarks.md`.

**How to work a perf task here:** measure first (`cargo run --release -- run benches/run.chz`), land
behind a failing-then-green correctness test, keep parity green, re-measure, record the delta in
`docs/benchmarks.md` + `PROGRESS.md`. Don't trust a lever's a-priori payoff guess — several in the
backlog moved a *different* bench than predicted (logged in `docs/benchmarks.md`).

**Remaining levers** (ranked in **[`docs/future.md`](docs/future.md)** §4): Medium — NaN-boxing
`Value` (16B→8B, the biggest remaining lever; its own milestone), struct-field inline caching, string
concat/`split` builder. Big/separate — register VM, generational/incremental GC, and **Cranelift
AOT/JIT as the stretch end-game** (a whole backend — only once the language has truly stopped moving;
not next). Frame pooling and general arith-specialization are deprioritized (superinstructions already
cover the hot int paths; `CallFrame`'s per-call `Vec`s are alloc-free).
