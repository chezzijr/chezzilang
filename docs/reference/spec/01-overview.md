# Chezzi — Language Design Spec

A fast, statically-typed, Python-feel scripting language. Hand-built in Rust.

> **Status:** the core language, generics and structural protocols, modules, iterators and
> generators, `defer` + `recover:`, and concurrency (`spawn` / `parallel:`, `Channel`/`Shared`/
> `Executor`, an OS-thread M:N scheduler + netpoller + `std.net`) are implemented. This spec owns the
> *language design*. Build status and what comes next live in [`PROGRESS.md`](../../../PROGRESS.md).

## Goals (ranked)

1. **Self-contained**: the lexer, parser, type checker, VM and GC are hand-built on Rust `std` only. No transpiling, no codegen shortcuts, minimal dependencies.
2. **Usable tool**: a bytecode VM; real modules so programs split into files.
3. **LLM-friendly**: static types as guardrails, explicit signatures, machine-readable compiler errors, a small orthogonal grammar.

Closest existing cousins (read, don't copy): **Crystal**, **Nim**.

## Locked decisions

| Decision | Choice |
|----------|--------|
| Implementation host | **Rust** |
| Execution model | **Bytecode stack VM** (the only engine) |
| Type system | **Static, local inference** (explicit param and return types; inferred locals; an inline expression body takes its expression's type) |
| Surface syntax | **Indentation blocks** (Python-feel; lexer emits INDENT/DEDENT) |
| Errors | **`T!E` / `T?` + `?`** (errors as values, no hidden control flow) |
| Code organization | **Composition, not inheritance**: structs + methods + interfaces (structural `protocol`s), like Rust/Go. No classes, no inheritance. |
| Memory | **Mark-sweep GC** (hand-built; primitives unboxed) |
| Name / ext / binary | **Chezzi** / `.chz` / `chezzi run foo.chz` |
