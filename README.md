# Chezzi

A fast, statically-typed, **Python-feel** scripting language — hand-built in Rust.

Python-easy to write, Go/Rust-fast to run, and designed to be **LLM-friendly** so models codegen it reliably.

```chezzi
fn greet(name: str) -> str:
    return "hi {name}"

fn main():
    print(greet("chezzi"))
```

## Docs

- [`PROGRESS.md`](PROGRESS.md): current status and what is next
- [`docs/README.md`](docs/README.md): index of every doc
- [Syntax reference](docs/reference/syntax/01-lexical-and-literals.md): every construct, by example
- [Standard library](docs/reference/stdlib/01-builtins.md)
- [Concurrency](docs/reference/concurrency/01-model.md): `spawn`, `parallel:`, channels
- [Lessons](docs/contributing/lessons.md): engineering rules; read them before you contribute

`chezzi docs` prints the full reference.

## Design at a glance

- **Host:** Rust — no GC, max perf, real memory learning.
- **Execution:** a bytecode stack VM — the **sole** engine. `chezzi run` runs it on the real-thread
  multicore (M:N) scheduler; size the worker pool with `--threads=N` / `CHEZZI_THREADS` (`0` = all
  cores).
- **Types:** static with local inference — explicit function signatures, inferred locals (`x := 5`).
- **Syntax:** indentation blocks (Python-feel).
- **Errors:** `T!E`/`T?` + `?` — errors as values, no hidden control flow.

## Build

```sh
cargo build
cargo run -- help
```
