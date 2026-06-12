# Chezzi

A fast, statically-typed, **Python-feel** scripting language — hand-built in Rust.

Python-easy to write, Go/Rust-fast to run, and designed to be **LLM-friendly** so models codegen it reliably.

```chezzi
fn greet(name: str) -> str:
    return "hi {name}"

fn main():
    print(greet("thuan"))
```

## Status

**Core language feature-complete through M18; concurrency shipped through Tier-D.** Currently in
**M19** (a perf track — optimization only, language frozen). ~1565 tests green, both engines.
- [`PROGRESS.md`](PROGRESS.md) — live milestone tracker (single source of truth for "what's next")
- [`docs/spec.md`](docs/spec.md) — full language design
- [`docs/syntax.md`](docs/syntax.md) — syntax cheat-sheet (every construct, by example)
- [`docs/concurrency.md`](docs/concurrency.md) — concurrency design (`spawn` / `parallel:` / channels)

## Design at a glance

- **Host:** Rust — no GC, max perf, real memory learning.
- **Execution:** bytecode stack VM (default engine) + a frozen tree-walk interpreter kept as the
  byte-for-byte parity oracle. Real-thread multicore via `--parallel`.
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
