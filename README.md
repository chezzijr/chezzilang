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

**Core language implemented through M24 (still evolving; M19 perf in progress); concurrency shipped through Tier-D.** Currently in
**M19** (a perf track — optimization-focused).
- [`PROGRESS.md`](PROGRESS.md) — live milestone tracker (single source of truth for "what's next")
- [`docs/spec.md`](docs/spec.md) — full language design
- [`docs/syntax.md`](docs/syntax.md) — syntax cheat-sheet (every construct, by example)
- [`docs/concurrency.md`](docs/concurrency.md) — concurrency design (`spawn` / `parallel:` / channels)
- [`docs/lessons.md`](docs/lessons.md) — hard-won engineering rules (what green gates cannot see, checker/airlock/test traps) — read before contributing

## Design at a glance

- **Host:** Rust — no GC, max perf, real memory learning.
- **Execution:** a bytecode stack VM — the **sole** engine. `chezzi run` runs it on the real-thread
  multicore (M:N) scheduler; size the worker pool with `--threads=N` / `CHEZZI_THREADS` (`0` = all
  cores). (The tree-walk interpreter and the cooperative single-thread `--serial` engine have both
  been removed.)
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
