# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

## Current focus

> **M2 — Parser → AST.** Build `src/parser/` + `src/ast/`.
> Goal: `cargo run -- ast examples/hello.chz` prints a structured AST.
> Use the operator-precedence table in [`docs/syntax.md`](docs/syntax.md) §4 for the Pratt parser.

## M1 — Lexer  ✅ DONE

All 5 guiding tests green; lexes full `examples/hello.chz` (nested Indent/Dedent, `0..10`→DotDot, `?`).

- ✅ **1a. Char cursor** — *(yours, reviewed)*
- ✅ **1b. Operator tokens** — *(scaffolded)*
- ✅ **1c. Numbers** — int + float. *(yours, reviewed: fixed greedy-dot via peek_next lookahead)*
- ✅ **1d. Strings** — plain `"..."`. *(scaffolded)*
- ✅ **1e. Identifiers & keywords** — *(yours, reviewed)*
- ✅ **1f. Comments & whitespace** — *(scaffolded)*
- ✅ **1g. Newlines** — *(scaffolded)*
- ✅ **1h. Indentation** — indent stack + pending-Dedent queue in `scan_indentation`. *(scaffolded; study it)*
- ✅ **1i. EOF** — Newline → trailing Dedents → Eof *(scaffolded)*
- ✅ **1j/1k. Tests green + reviewed.**

**Open follow-ups (small, do anytime):** scientific notation in numbers (`1e3`); string escapes (`\n`, `\"`); string interpolation lexing.

## Roadmap (later)

- 🟦 **M2** — Parser → AST (`chezzi ast`) ← NEXT
- ⬜ **M3** — Tree-walk interpreter (working language!)
- ⬜ **M4** — Type checker (local inference)
- ⬜ **M4.5** — Modules / imports
- ⬜ **M5** — Bytecode VM + mark-sweep GC
- ⬜ **M6** — Stdlib + pipe `|>`

---

## Learning log

Jot what clicked / what confused you — future-you and Claude both use this.

- _(empty — add notes as you go)_
