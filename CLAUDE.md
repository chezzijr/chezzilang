# Chezzi — Claude Code Guide

Chezzi is a fast, statically-typed, Python-feel scripting language, hand-built in Rust.
Full design + roadmap: **[`docs/spec.md`](docs/spec.md)**. Syntax cheat-sheet: **[`docs/syntax.md`](docs/syntax.md)**. Stdlib/builtin reference: **[`docs/stdlib.md`](docs/stdlib.md)**. Canonical grammar: **[`docs/grammar.bnf`](docs/grammar.bnf)** (executed + drift-checked by `cargo test conformance`). Progress tracker: **[`PROGRESS.md`](PROGRESS.md)**. Bug-discovery strategy (fuzzing / CPython differential / sanitizers, pre-JIT): **[`docs/bug-discovery.md`](docs/bug-discovery.md)**. Hard-won engineering rules (distilled from working memory — read the section for the area you touch): **[`docs/lessons.md`](docs/lessons.md)**. **Where each fact lives (its one owner) and which file owns each topic: [`docs/owners.md`](docs/owners.md) — read it before you grep.** The docs are also emitted by the CLI: `chezzi docs <topic>` / `chezzi docs` (full LLM bundle).

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
- **Derive, don't mirror.** Every fact has one owner and everything else reads it. Before fixing a
  bug, ask **"which duplicate does this delete, or which single source does it add?"** — never add
  another copy, clause or veto next to the failing site in a family that already has sibling fixes.
  A new "Mirrors the checker's …" / "keep in sync with …" comment, or a drift test with an exemption,
  is a red flag. Family fixes land with one test that enumerates the whole grid. Why and where:
  `docs/lessons.md` §1, `docs/root-causes-w16.md`.

## Workflow per milestone

1. Implement the milestone (types, logic, wiring) in its module.
2. Add tests — **prefer Chezzi** (`tests/chz/`, see the testing-policy convention below); Rust
   `#[cfg(test)]` only for what `assert` can't express; a golden `examples/*.chz` for print-shape demos.
3. `cargo test` + run the milestone's `chezzi` subcommand to verify end-to-end.
4. Update `PROGRESS.md`, commit, move on.

## Commands

```sh
cargo build --release            # compile (the VM is only fast optimized)
cargo test                       # FULL pre-commit suite: lib + goldens + conformance + integration
cargo nextest run                # the same suite, binaries in parallel (~3 min vs ~13; the pipeline's verifying gate uses this)
cargo test --lib                 # inner loop: the lib unit suite
cargo test --lib checker::       # scope to the area you edit -> seconds
cargo test -p chezzi-syntax      # the syntax layer alone (lexer, ast, parser): its own crate
cargo test conformance           # execute docs/grammar.bnf, differential-test vs the parser
cargo clippy -- -D warnings      # lint (must be clean before commit)
cargo run -- check examples/hello.chz   # type-check only; also: run <file>, test <file|dir>
```

Flags go **before** the file path. Every other command (`--errors=json` shapes, `--threads`,
`CHEZZI_SCHED_SEED`, `schedfuzz`, the LSP build, editor assets, benches) is in
**[`docs/dev-commands.md`](docs/dev-commands.md)**.

## Conventions

One line per rule. The full text is in **[`docs/code-conventions.md`](docs/code-conventions.md)**:
before you change an area a rule names (native fns, checker warnings, tests, enums, std modules),
read that rule's entry there.

- Commits: single-line conventional (`feat:`, `fix:`, `chore:`, `docs:`, `test:`). No body.
- Each compiler phase is its own module under `src/`: `lexer` → `parser` → `ast` → `desugar` → `checker` → `compiler` → `vm` (the engine of record), plus `native` (builtins / std), `resolver` (module paths). Each phase is split into one file per topic: `docs/owners.md` maps each area to its files.
- Keep modules small and single-purpose: one topic per file, a first line that names it, at most 2,500 lines (3,500 for a test file). `cargo test --test source_layout` enforces it and checks `docs/owners.md`.
- **No wildcard arm on an enum in library or binary code.**
- **New builtin types/ctors/fns go in their owning `std.*` module (import-gated), NOT the global reserved namespace.**
- **A new native fn's `MEMBERS` entry is `(name, fn, Kind)` — the third element is not paperwork.**
- After merging an auto-task branch (post-gate ships): delete the branch + prune its worktree (`git worktree remove --force <wt>; git worktree prune; git branch -D <branch>`).
- **Two auto-task runs in parallel MUST NOT share a release target dir.**
- Unit tests live next to the code in `#[cfg(test)] mod tests`.
- **Testing is HYBRID — write the test in Chezzi if you can, fall back to Rust only when you can't.**
- **ONE ENGINE.** The bytecode VM on its M:N scheduler is the only engine: a test compares against a literal golden, never against a second run.
- **A CHECKER WARNING'S GATE IS DERIVED FROM THE RUNTIME, ONE PROGRAM PER SHAPE — never from the plausible-looking checker flag.**
- **CORRECTNESS IS JUDGED AGAINST THE ANCESTOR — there is no engine agreement to hide behind.**

## Status and roadmap

In **[`docs/status-and-roadmap.md`](docs/status-and-roadmap.md)**: what is implemented, the pre-JIT bug-hunt,
the JIT entry rule, the open-row ledger and the perf roadmap. Read it before core work.
**[`PROGRESS.md`](PROGRESS.md)** is the single source of truth for what is next.
