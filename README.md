# Chezzi

A fast, statically-typed, **Python-feel** scripting language — hand-built in Rust.

Python-easy to write, Go/Rust-fast to run, and designed to be **LLM-friendly** so models codegen it reliably.

```chezzi
fn greet(name: str) -> str:
    return "hi {name}"

fn main():
    print(greet("chezzi"))
```

## Status

**Core language implemented through M21 (still evolving; M19 perf in progress); concurrency shipped through Tier-D.** Currently in
**M19** (a perf track — optimization-focused). ~1565 tests green, both engines.
- [`PROGRESS.md`](PROGRESS.md) — live milestone tracker (single source of truth for "what's next")
- [`docs/spec.md`](docs/spec.md) — full language design
- [`docs/syntax.md`](docs/syntax.md) — syntax cheat-sheet (every construct, by example)
- [`docs/concurrency.md`](docs/concurrency.md) — concurrency design (`spawn` / `parallel:` / channels)

## Design at a glance

- **Host:** Rust — no GC, max perf, real memory learning.
- **Execution:** bytecode stack VM (the engine of record) + a **deprecated** tree-walk interpreter
  (slated for removal) kept for now as the byte-for-byte parity oracle. `chezzi run` defaults to the real-thread multicore engine (size its
  worker pool with `--threads=N` / `CHEZZI_THREADS`, `0` = all cores); `--serial` selects the
  cooperative single-thread VM (the parity oracle).
- **Types:** static with local inference — explicit function signatures, inferred locals (`x := 5`).
- **Syntax:** indentation blocks (Python-feel).
- **Errors:** `Result`/`Option` + `?` — errors as values, no hidden control flow.

## Build

```sh
cargo build
cargo run -- help
```

## Roadmap

Shipped: **M1–M18** (lexer → parser → checker → bytecode VM + GC → stdlib → generics + protocols →
exhaustive `match` → closures/HOF → modules → iterators → `defer`/`recover:`) plus **concurrency
Tiers A–D** (`spawn` / `parallel:` nursery, `Channel`/`Shared`/`Executor`, real OS-thread M:N
scheduler + netpoller + `std.net`). **M19** (perf track) is in progress.

Live status + full milestone history: [`PROGRESS.md`](PROGRESS.md). Design: [`docs/spec.md`](docs/spec.md).
