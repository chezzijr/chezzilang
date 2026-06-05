# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

> **Mode:** build mode (Claude implements directly; learning/scaffold workflow retired — see `CLAUDE.md`).

## Current focus

> **M5 — Bytecode VM + mark-sweep GC.** ✅ **DONE** (M5a compiler+VM, M5b mark-sweep GC, M5c
> parity+perf+CLI flip). `chezzi run` now executes on the VM by default (`--interp` falls back).
> **Next: M6 — stdlib + pipe `|>` + core-type methods.**

## M5a — Bytecode compiler + stack VM (handle values, no collector yet)  ✅ DONE

`cargo run -- run --vm <file>` runs on the bytecode VM (tree-walk interp stays the default).
Golden parity holds: VM stdout == `hello.expected` and == the interpreter's output, and the
multi-file `tests/fixtures/proj/` runs identically. Built `src/compiler/` (AST → `Program`) +
`src/vm/` (`value` handle + `heap`-addressed `Obj` + `op` + exec loop); 48 VM tests, 261 total;
clean `cargo clippy --all-targets`. Built **TDD** (red→green per bug class).

- ✅ **Value model** — `Value` is `Copy` (unboxed `Int/Float/Bool/Nil` + `Obj(GcRef)`); the 6
  reference kinds (Str/List/Struct/Enum/Func/Closure/Module) live in a VM-owned `Heap` of slots +
  free-list (handle copies alias one object → by-reference sharing). No `RefCell`; `alloc` only
  inserts (mark-sweep lands in M5b).
- ✅ **Compiler** (`src/compiler/mod.rs`) — locals → operand-stack slots resolved at compile time;
  globals/struct/variant/builtins resolved by name in the interpreter's order. Two passes (hoist
  types → compile toplevel + fn/method/closure protos). Closures **snapshot all visible locals by
  value** (matches the interp's frame snapshot — reassign-after-capture invisible) via `CapEntry`/
  `GetCaptured`. String interpolation pre-parsed at compile time into literal/expr chunks.
- ✅ **Bytecode** (`src/vm/op.rs`) — flat `Vec<Op>` of typed operands (jumps = absolute indices),
  each op paired with a `Span` in `Proto::lines` so runtime errors recover source locations.
  Covers every AST node: literals, all 14 binary ops (runtime type dispatch matching the interp:
  checked int / float-promote / div-mod-by-zero error / str+str), `and`/`or` short-circuit,
  unary, list/struct/enum construction + arity errors, field/index (+ out-of-bounds, str char
  index), method calls (`self`-bound), closures, `?` (unwrap / propagate-to-caller / top-level
  error), `for`-range (**lazy** counting loop) + `for`-list (cloned), `while`, `if`/`elif`/`else`,
  `match` (variant dispatch + payload binding + no-arm error), `MAX_CALL_DEPTH` guard, builtins.
- ✅ **CLI** — `--vm` flag wired in `cmd_run` after the unchanged type-check gate; `vm::run_file`
  mirrors `interp::run_file` (256MB thread, resolver graph, run-once dep order, home-globals,
  entry-only `main()`, partial-output-before-error).

## M5b — Mark-sweep garbage collector  ✅ DONE

Hand-built tracing GC in `src/vm/heap.rs` + `Vm::collect`; 8 GC tests, 269 total; clean
`cargo clippy --all-targets`. Built **TDD** — each test forces a collection and pins one root
source; the headline operand-stack-root test was **bite-verified** to fail (7 dangling-handle
panics) when the root is removed.

- ✅ **Collector** — worklist mark (no native recursion) + sweep + free-list slot reuse. The heap
  owns slot/mark/sweep primitives + the allocation-driven growth threshold (`next_gc = 2×live`,
  min 256); the VM owns root tracing.
- ✅ **Collect at instruction boundaries** — `run_until` collects at the top of each loop step
  (or before *every* step in stress mode), where the entire live set is reachable from the roots,
  so there are **no mid-opcode off-stack temporaries** to miss (the build-then-alloc sequences in
  `NewList`/`NewStruct`/`NewEnum`/`ListClone`/`MakeClosure` complete within one un-interrupted step).
- ✅ **Root set** — the whole operand stack (covers every frame's local slots **and** in-flight
  expression temporaries), each frame's `home` module + backing `closure`, and the module
  namespace cache (`module_objs`). Children traced: list items, struct fields, enum payloads,
  closure captures + home, func home, module globals.
- ✅ **Guarded bug classes** — value live only on the operand stack / in a frame slot / via module
  globals / via a closure capture / propagated by `?` all survive collection; an allocation-heavy
  loop stays bounded (`<2000` live after 10k allocating iterations) instead of growing
  monotonically; `hello.chz` + a struct/enum/closure/match program are byte-identical under GC
  stress vs. normal.

## M5c — Module parity + perf + CLI default flip  ✅ DONE

`chezzi run` defaults to the VM; `--interp` runs the reference tree-walker. 6 parity tests, 275
total; clean `cargo clippy --all-targets`.

- ✅ **Cross-engine parity** — `parity_full_suite_vm_vs_interp` runs 16 programs (every feature
  class + 5 error cases) through **both** engines and asserts identical `(stdout, error)`. Golden
  `hello.chz` + the multi-file `proj/` run identically via `vm::run_file`; the project is also
  byte-identical under GC stress.
- ✅ **Home-globals on the VM (M4.5 headline bug)** — `imported_fn_uses_home_globals`: a new
  `tests/fixtures/homeglobals/` where `main` defines `MSG := "from-main"` and imports `who` from
  `lib` (which has `MSG := "from-lib"`). `who()` resolves `MSG` against **its own** module
  (`from-lib`) — both engines agree. Multi-file run-once / dep-order / `import as` / `from` all
  carry over.
- ✅ **Perf** — refactored the dispatch loop to **borrow** each instruction (one `Rc` bump per
  `run_until`, no per-op `clone`) — the single biggest win. Measured release speedup over the
  interpreter: **~6.5×** on an arithmetic loop, **~4.3×** on recursive `fib`. (Short of the ~10×
  aspiration: at ~1.7 ns/op the safe match-dispatch VM is near its floor and the tree-walker is
  itself fast; closing the gap needs inline caching / unsafe dispatch — deferred.) `bench_vm_…`
  records the ratio and asserts a debug-safe floor.
- ✅ **CLI flip** — `cmd_run` defaults to `vm::run_file`; `--interp` selects the tree-walker;
  `--vm` still accepted. `USAGE` updated.

**Interpolation parse-error timing (documented divergence):** the VM pre-parses `{expr}` chunks at
compile time, so a *malformed* interpolation in dead code is a load error rather than a
reached-only runtime error. Any program that runs successfully on either engine produces identical
stdout — this only differs on already-broken input.

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

## M4 — Type checker (local inference)  ✅ DONE

`cargo run -- check examples/hello.chz` type-checks; `run` now **gates** on the checker (type
errors block execution — no partial output). Built `src/checker/` (`mod` + `ty`); 73 checker
tests, 191 total; clean `cargo clippy`. Built **TDD** (red→green per error class; every test pins
a real bug class).

- ✅ **Type lattice** (`ty.rs`) — `Ty`: `Int/Float/Bool/Str/Nil`, `List[T]`, `Result[T]`,
  `Option[T]`, `Struct/Enum(name)`, `Func{params,ret}`, and `Unknown` (top/bottom element,
  compatible with everything — suppresses error cascades). `compatible()` is structural; **no**
  implicit int→float (numeric promotion lives only in arithmetic).
- ✅ **Pragmatic local inference** — bidirectional, no unification. `:=` infers from RHS; typed
  `let`/params/fields/returns checked against annotations. `Ok/Some` carry their payload type;
  `Err`/`None` are generic (`Result[?]`/`Option[?]`) so they unify with any declared `Result[T]`.
- ✅ **Two-pass** — pass 1 hoists every top-level decl (forward refs work, like the interp);
  pass 2 walks bodies, **collecting all errors** (Go-style) into a `Vec`.
- ✅ **Error classes** (each with a test) — unknown name/type, call arity, non-callable, arithmetic
  (`+`/`-`/`*`/`/`/`%`, matching interp incl. `str+str`), comparison, bool context, assignment
  mismatch (typed-let, `=`/`+=`/`-=`), return-vs-signature, field access, indexing, match
  exhaustiveness (+ unknown variant, dup arm, binding arity), and the `?` operator (operand must
  be Result/Option; enclosing fn ret must be Result/Option/**Nil** — the last allows `?` in
  `main()`, matching interp's top-level unwind).
- ✅ **CLI** — `chezzi check <file>` + `run` gating; `--errors=json` emits a structured JSON array
  (hand-rolled, zero-dep escaper) and preserves the contract even on fatal lex/parse errors.
- ✅ **Robustness** (review-panel: 4 S++ warm + 1 cold pass) — redeclaration guard
  (dup fn/struct/enum/variant → clear error, no pass-2 panic); field/index assignment rejected to
  match the interpreter (which only assigns bare vars); closure body checked against its explicit
  return annotation; unknown CLI flags fail; no reachable panic on valid parsed input.

**Deferred (note):** `map[K,V]` typing (no map literals yet), all-paths-return analysis, deeper
generic unification, user-defined generics, `?`-inside-closure frame semantics, field/index
assignment (blocked until the interpreter supports it), pipe `|>` (M6).

## M4.5 — Modules / imports + resolver  ✅ DONE

Multi-file programs run; `chezzi.toml` root detection works. Built `src/resolver/`; 22 new tests
(7 resolver + 11 interp + 4 checker), 213 total; clean `cargo clippy --all-targets`. Built **TDD**
(red→green per bug class; the headline cross-module-globals test was bite-verified to fail without
the fix). Imports already lexed/parsed since M2 — M4.5 made them *mean* something.

- ✅ **Resolver** (`src/resolver/mod.rs`) — `find_root` (walk up for `chezzi.toml`, else entry's
  dir), `std_root` (`$CHEZZI_STD` else compile-time `<crate>/std`), `build_graph` (DFS, postorder
  load order = deps before dependents, entry last). Module identity = canonicalized abs path
  (`ModuleId`) → diamonds de-dupe, run-once parse. Cycles → clean `ResolveError` (`a -> b -> a`),
  not a stack overflow. `a.b.c` → `<root>/a/b/c.chz`; `std.*` → `<std_root>/…`. Lex/parse errors in
  an imported file are re-labelled (`in module 'core.db': …`) since `Span` carries no filename.
- ✅ **Interp** (`Value::Module` + `ModEnv`) — `module.fn()` is a plain call on a looked-up member
  (no `self`); `import a as m` / `import f, g from a` bind into the importer's scope. **Run-once**:
  each module's body (incl. top-level statements) evaluates exactly once in dependency order, its
  globals snapshotted as a cached namespace; `main()` auto-runs **only** for the entry file.
- ✅ **Cross-module globals (the subtle bug)** — a fn imported from B that reads B's top-level `K`
  must resolve `K` against **B**, not the caller. Fixed by bundling each callable's home globals
  into the value (`Value::Func(decl, ModEnv)`, `Closure.home`) and `Env::swap_globals` on every
  call — mirrors the existing `swap_locals` idiom. `ModEnv` is a `Rc<RefCell<HashMap>>` newtype
  with pointer-eq / opaque `Debug` (the table is self-referential — a deep compare/print would
  recurse forever).
- ✅ **Checker** (`check_graph`, `Ty::Module`, `ModuleSig`) — type-checks the whole graph,
  accumulating errors across modules (Go-style). `io.read()` resolves member sigs; `from` imports
  validate the member exists; imported-module errors carry the `in module '…'` label. Type names
  are program-global in M4.5 → reuse across modules is an "already defined" collision (also a
  runtime backstop in the interp hoist).
- ✅ **CLI** — `check`/`run` are path-aware (`build_graph` → `check_graph`, gate, then `run_file`);
  resolve/cycle/missing-module failures are `Fatal`, preserving the `--errors=json` contract.
- ✅ **Golden** — `tests/fixtures/proj/` (`chezzi.toml` + whole-module + `from` imports + a
  cross-module constant) runs to `main.expected` via the real entry point.

**Deferred (note):** actual `std/` content (M6 — `std.*` *resolves* but the dir is empty by design,
the std-path test asserts the path not a load); per-module type-name namespacing (program-global +
collision-detected for now); next-to-binary std discovery / install story; re-export / transitive
`from`; VM parity (M5 must mirror run-once + home-globals — golden tests will enforce).

## Roadmap (later)

- ✅ **M5** — Bytecode VM + mark-sweep GC (M5a compiler+VM; M5b GC; M5c parity+perf+CLI flip)
- ⬜ **M6** — Stdlib + pipe `|>` + **core-type methods** (string/list ergonomics — UX priority). ← NEXT
  Extend `eval_method_call` to dispatch on `Value::Str`/`Value::List`; handlers in `builtins.rs`.
  Starter set: `s.len/upper/lower/trim`, `s.split(sep)`, `sep.join(list)`, `s.starts_with/contains`
  (+ list mirror `xs.push/len`). Pullable earlier if string ergonomics start blocking real programs.

---

## Learning log

Jot what clicked / what confused you — future-you and Claude both use this.

- _(empty — add notes as you go)_
