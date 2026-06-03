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

**Pre-M1 scaffold.** Design is locked; implementation hasn't started.
- [`docs/spec.md`](docs/spec.md) — full design + build roadmap
- [`docs/syntax.md`](docs/syntax.md) — syntax cheat-sheet (every construct, by example)

## Design at a glance

- **Host:** Rust — no GC, max perf, real memory learning.
- **Execution:** tree-walk interpreter first (reference semantics), then a bytecode stack VM (~10x).
- **Types:** static with local inference — explicit function signatures, inferred locals (`x := 5`).
- **Syntax:** indentation blocks (Python-feel).
- **Errors:** `Result`/`Option` + `?` — errors as values, no hidden control flow.

## Build

```sh
cargo build
cargo run -- help
```

## Roadmap

| # | Milestone |
|---|-----------|
| M1 | Indent-aware lexer + token REPL |
| M2 | Parser → AST |
| M3 | Tree-walk interpreter (working language) |
| M4 | Type checker (local inference) |
| M4.5 | Modules / imports |
| M5 | Bytecode VM + mark-sweep GC |
| M6 | Stdlib + pipe `\|>` |

Full detail: [`docs/spec.md`](docs/spec.md).
