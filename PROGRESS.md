# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

## Current focus

> **M1 — indentation-aware lexer.** Implement `src/lexer/mod.rs`.
> Goal: `cargo run -- tokens examples/hello.chz` prints a clean token stream incl. INDENT/DEDENT.

## M1 — Lexer  🟦

Sub-steps (do in order — each is one sitting):

- ⬜ **1a. Char cursor** — `peek`, `peek_next`, `advance`, `is_at_end`. *(2 are done as worked examples; finish the rest.)*
- ⬜ **1b. Single-char & operator tokens** — `+ - * / % ( ) [ ] : , .` then multi-char `== != <= >= := -> |> +=`.
- ⬜ **1c. Numbers** — int and float literals.
- ⬜ **1d. Strings** — plain `"..."` literal (interpolation lexing deferred to later).
- ⬜ **1e. Identifiers & keywords** — scan a word, then look it up in the keyword table.
- ⬜ **1f. Comments & whitespace** — skip `# ...` to end of line; skip inline spaces.
- ⬜ **1g. Newlines** — emit `Newline` at end of a logical line; skip blank / comment-only lines.
- ⬜ **1h. Indentation** — the hard one. Indent stack, emit `Indent`/`Dedent`. See the big HINT block in `mod.rs`.
- ⬜ **1i. EOF** — flush remaining `Dedent`s, then emit `Eof`.
- ⬜ **1j. Make the guiding tests green** — `cargo test`.
- ⬜ **1k. Review with Claude**, then commit.

**Done when:** all guiding tests pass AND `tokens examples/hello.chz` looks right.

## Roadmap (later)

- ⬜ **M2** — Parser → AST (`chezzi ast`)
- ⬜ **M3** — Tree-walk interpreter (working language!)
- ⬜ **M4** — Type checker (local inference)
- ⬜ **M4.5** — Modules / imports
- ⬜ **M5** — Bytecode VM + mark-sweep GC
- ⬜ **M6** — Stdlib + pipe `|>`

---

## Learning log

Jot what clicked / what confused you — future-you and Claude both use this.

- _(empty — add notes as you go)_
