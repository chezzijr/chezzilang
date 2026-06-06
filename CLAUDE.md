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
- Match the existing code's style and patterns; reuse before adding new abstractions.

## Workflow per milestone

1. Implement the milestone (types, logic, wiring) in its module.
2. Add unit tests + a golden check against `examples/*.chz`.
3. `cargo test` + run the milestone's `chezzi` subcommand to verify end-to-end.
4. Update `PROGRESS.md`, commit, move on.

## Commands

```sh
cargo build              # compile
cargo test               # run unit + guiding tests
cargo test conformance   # execute docs/grammar.bnf, differential-test vs the parser
cargo run -- help        # CLI usage
cargo run -- tokens examples/hello.chz   # M1 target
cargo run -- ast    examples/hello.chz   # M2 target
cargo run -- run    examples/hello.chz   # M3 target
cargo clippy             # lint
```

## Conventions

- Commits: single-line conventional (`feat:`, `fix:`, `chore:`, `docs:`, `test:`). No body.
- Each compiler phase is its own module under `src/` (`lexer`, `parser`, `ast`, `checker`, ...).
- Keep modules small and single-purpose.
- Unit tests live next to the code in `#[cfg(test)] mod tests`.

## Current focus

See **[`PROGRESS.md`](PROGRESS.md)** — single source of truth for "what's next."
Right now: **M2 — parser → AST.**
