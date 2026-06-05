# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

> **Mode:** build mode (Claude implements directly; learning/scaffold workflow retired — see `CLAUDE.md`).

## Current focus

> **M4 — Type checker (local inference).** Build `src/checker/`.
> Goal: type errors caught pre-run with clear messages; `--errors=json` mode.

## M1 — Lexer  ✅ DONE

All 5 guiding tests green; lexes full `examples/hello.chz` (nested Indent/Dedent, `0..10`→DotDot, `?`).

- ✅ **1a. Char cursor** — *(yours, reviewed)*
- ✅ **1b. Operator tokens** — *(scaffolded)*
- ✅ **1c. Numbers** — int + float; `_` digit separators (`10_000_000`, only between digits).
- ✅ **1d. Strings** — `"..."` with escapes `\n \t \r \\ \" \0` (unknown escape → error).
- ✅ **1e. Identifiers & keywords** — *(yours, reviewed)*
- ✅ **1f. Comments & whitespace** — *(scaffolded)*
- ✅ **1g. Newlines** — *(scaffolded)*
- ✅ **1h. Indentation** — indent stack + pending-Dedent queue in `scan_indentation`. *(scaffolded; study it)*
- ✅ **1i. EOF** — Newline → trailing Dedents → Eof *(scaffolded)*
- ✅ **1j/1k. Tests green + reviewed.**

**Open follow-ups (small, do anytime):** scientific notation in numbers (`1e3`); single-quote strings; unicode `\u{…}` escapes.
**Done post-M1:** string escapes (`\n \t \r \\ \" \0`), numeric underscores (`10_000_000`) — both lexer-only, TDD, conformance still green.

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

## M2.5 — Canonical grammar + conformance  ✅ DONE

Canonical grammar file with an executable drift check; 48 tests total.

- ✅ **`docs/grammar.bnf`** — the canonical grammar, BNF over the lexer **token stream** (terminals are token classes, so `INDENT`/`DEDENT`/`NEWLINE` are expressible). Mirrors the `parse_*` rules incl. the M2 hardening (lvalue restriction, statement terminator, inline-block-no-compound, precedence cascade).
- ✅ **Executable differential test** — `docs/grammar.bnf` is run with the [`bnf`](https://docs.rs/bnf) crate (Earley parser, **dev-dependency only** — not in the shipped binary; release build stays zero-dep). For every corpus file, grammar accept/reject must equal the hand parser's. Fed one private-use char per token (since `bnf` matches char-by-char).
- ✅ **Conformance corpus** — `tests/corpus/{accept,reject}/*.chz` (18 + 7), annotated `# rule:` / `# expect:`, doubling as executable docs.
- ✅ **Cross-checks** — grammar terminals == `Token` enum (only `PIPE` reserved/unused); grammar rules ↔ `parse_*` fns; `symbol()` is an exhaustive match (compiler-enforced completeness); every headline rule has a corpus example; reject messages are specific.
- ✅ **Bite-tested** — verified the harness actually fails on grammar drift, a bad corpus file, and a bogus token.
- Run: `cargo test conformance`. Excluded by design: deep-nesting (a parser depth cap, not a grammar rule).

## M3 — Tree-walk interpreter  ✅ DONE

`cargo run -- run examples/hello.chz` executes the program end-to-end. Built `src/interp/`
(`mod` + `value` + `env` + `builtins`); 70 interp tests, 118 total; clean `cargo clippy`.
Built with **TDD** (red→green per feature; every test targets a real bug class).

- ✅ **Values** (`value.rs`) — `Int/Float/Bool/Str/List/Func/Closure/Struct/Enum/Nil`. Reference
  types share via `Rc<RefCell<…>>`. Deterministic `Display` (struct fields in declaration order).
  Result/Option are plain `Enum`s (`Ok/Err/Some/None` pre-registered).
- ✅ **Env** (`env.rs`) — lexical scoping: `globals: Rc<HashMap>` + a stack of local frames; a call
  `swap_locals` to a fresh frame so a callee never sees the caller's locals. Closures snapshot
  captured frames.
- ✅ **Eval/exec** (`mod.rs`) — full expr + stmt set: arithmetic (int/int→int trunc, float promotion,
  checked overflow, div/mod-by-zero error for **both** int and float), `and`/`or` short-circuit,
  comparisons/equality, list literals + indexing, ranges (lazy in `for`), unary, calls, field
  access, method calls (`self`-bound), closures, `if`/`for`/`while`/`match`, `return` (via `Flow`),
  string interpolation (`{expr}`, `{{`/`}}`).
- ✅ **`?` operator** — value-level early return via a `propagating` channel caught at the call
  boundary; unwraps `Ok`/`Some`, propagates `Err`/`None` from the enclosing fn.
- ✅ **Builtins** (`builtins.rs`) — `print`, `len`, `range` (length-capped), `int`/`float`/`str`
  casts (range-checked), `sqrt`. `sqrt`/casts are temporary builtins until `std.math` (M4.5).
- ✅ **Entry point** — hoist top-level `fn`/`struct`/`enum`, run top-level stmts, auto-call nullary
  `main()`. CLI `chezzi run <file>` wired; prints partial output before a runtime error.
- ✅ **Robustness** (review-panel hardened, warm + cold pass) — interpreter runs on a dedicated
  256 MB-stack thread with a `MAX_CALL_DEPTH` guard (infinite recursion → clean error, not SIGABRT);
  no reachable panics on adversarial input; lazy ranges; accurate error spans.

**Deferred (unchanged):** maps `{...}`, pipe `|>` (M6), break/continue (no AST nodes), core-type
methods (`s.upper()`, `xs.push()` — only user struct methods so far). Exhaustiveness of `match` is
a runtime error now; static check arrives with M4. `?` inside a closure is absorbed at the closure
boundary (a checker rule for M4).

## Roadmap (later)

- ⬜ **M4** — Type checker (local inference) ← NEXT
- ⬜ **M4.5** — Modules / imports
- ⬜ **M5** — Bytecode VM + mark-sweep GC
- ⬜ **M6** — Stdlib + pipe `|>` + **core-type methods** (string/list ergonomics — UX priority).
  Extend `eval_method_call` to dispatch on `Value::Str`/`Value::List`; handlers in `builtins.rs`.
  Starter set: `s.len/upper/lower/trim`, `s.split(sep)`, `sep.join(list)`, `s.starts_with/contains`
  (+ list mirror `xs.push/len`). Pullable earlier if string ergonomics start blocking real programs.

---

## Learning log

Jot what clicked / what confused you — future-you and Claude both use this.

- _(empty — add notes as you go)_
