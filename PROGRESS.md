# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

> **Mode:** build mode (Claude implements directly; learning/scaffold workflow retired — see `CLAUDE.md`).

## Current focus

> **M3 — Tree-walk interpreter.** Build `src/interp/`.
> Goal: `cargo run -- run examples/hello.chz` executes the program.

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

## M2 — Parser → AST  ✅ DONE

`cargo run -- ast examples/hello.chz` prints the full `{:#?}` tree; 25 tests green (incl. golden hello.chz); clean `cargo clippy`.

- ✅ **Spans retrofitted into the lexer** — `tokenize` now emits `Tok { kind, span }` with 1-based `Span { line, col }`. `tokens` CLI output unchanged (prints `kind`). AST nodes + `ParseError` carry spans.
- ✅ **AST** (`src/ast/`) — `Module`/`Stmt`/`Expr` (kind+span), decls (`FnDecl`/`Field`/`Variant`/`MatchArm`/`Pattern`/`Import`), `Type`, op enums. All `Debug`.
- ✅ **Parser** (`src/parser/`) — recursive descent (statements) + Pratt (expressions, binding powers per syntax.md §4). Shared block rule handles indented + inline (one-line `match` arms). Covers fn/struct/enum/match, if/else-if/else, for/while, return, all 4 import forms, closures, ranges, calls/field/index/`?`, list literals.
- ✅ **CLI** — `chezzi ast <file>` wired (lex → parse → pretty-print).

**Hardened after agent-review-panel** (2 passes + cold pass; 42 tests):
- Non-lvalue assignment (`1 = 2`, `f() = 3`) → `ParseError`, not a wrong AST.
- Statement terminator enforced — `x := 5 y := 6` on one line is an error.
- Recursion depth cap (`MAX_DEPTH = 128`) on all 4 recursive entry points (`parse_bp`, `parse_unary`, `parse_type`, `parse_stmt`) → deep nesting returns a `ParseError` instead of SIGABRT.
- Inline-block bodies allow `else` chaining but reject a nested compound statement (`if a: if b: …`) to avoid dangling-`else` ambiguity — nest via indentation.
- Error messages render tokens in source form (`':='`, not `Walrus`).

**Deferred (unchanged):** map literals `{...}` (no brace tokens), pipe `|>` (M6), string-interpolation parsing.
**Deferred nit (rationale):** the comma-separated-list loop recurs in ~6 spots; left inline — a generic `parse_separated` helper adds `FnMut` borrow friction and the call sites differ (some consume the closing delimiter, some don't) for marginal gain.

## Roadmap (later)

- ⬜ **M3** — Tree-walk interpreter (working language!) ← NEXT
- ⬜ **M4** — Type checker (local inference)
- ⬜ **M4.5** — Modules / imports
- ⬜ **M5** — Bytecode VM + mark-sweep GC
- ⬜ **M6** — Stdlib + pipe `|>`

---

## Learning log

Jot what clicked / what confused you — future-you and Claude both use this.

- _(empty — add notes as you go)_
