# Chezzi — Future Directions (brainstorm, NOT scheduled)

> **Status:** speculative design notes (recorded 2026-06-07). Forward-looking and opinionated.
> Nothing here is committed work — `PROGRESS.md` + `gaps.md` remain the source of truth for what's
> actually scheduled. This doc captures *what would make Chezzi an effective scripting language* and
> *how to make it faster*, with verdicts and rough implementation shape. Promote items into `gaps.md`
> when they're scheduled.

The language **core** is feature-complete (scalars, `list`/`map`/`set`/`tuple`, generic structs +
enums, `Result`/`Option` + `?`, generics + structural protocols, exhaustive `match`, closures/HOF,
modules, GC, two engines, interpolation, pipe, panic recovery via `recover:`, the `Iterator[T]`
protocol bound). What follows is the gap between "complete core" and "language you reach for to write
real scripts."

---

> **Promotion status (2026-06-07):** §1 (`defer`) and the §3 scripting features have been **promoted
> into `gaps.md` → "Open gaps"** as tracked, near-term work. They stay documented here for the design
> rationale; `gaps.md` is now the scheduling source of truth for them. §2 (concurrency) and §4
> (optimizations) remain speculative and live only here.

## 1. `defer` (cleanup on scope exit) — ✅ **SHIPPED (M17)**, **block-scoped since M18** — see `gaps.md` resolved log + `examples/defer.chz`

> **M18 update:** shipped frame-scoped in M17, then moved to **block/lexical scope** — a `defer` runs
> when its enclosing indented block exits (loop body, branch, `recover:`, `match` arm, function body,
> module top level), not just at function return. Realises the "cleanup on scope exit" intent below
> more literally. See the M18 entry in `PROGRESS.md` and the `defer` section of `docs/syntax.md`.

Before M11 this was weak: no panic meant nothing to clean up after. **Now there is unwinding** —
the `recover:` boundary, `?` propagation, and runtime faults all unwind. So `defer` earns its keep
by running on **all three exit paths**: normal return, `?` short-circuit, and panic unwind. That is
exactly Go's value proposition.

**Implementation shape**
- Per-frame deferred-call stack, drained LIFO on *every* frame exit including unwind.
  - Interp: drain in the `Flow` / propagating channel path **and** the `recover` snapshot/restore path.
  - VM: drain at `Return` **and** inside the handler-stack unwind (`PushHandler`/`PopHandler` already exist).
- **Arg-evaluation timing:** evaluate `defer` arguments *at the `defer` statement* (Go semantics),
  not at exit. Less surprising; the deferred call closes over already-evaluated values.

**Alternative considered:** Python-style `with` (context-manager protocol `enter`/`exit`). More
Python-feel, but needs a new protocol + an indentation block. `defer` is simpler, adds no protocol,
and composes cleanly with `recover:`. **Recommend `defer`.**

---

## 2. Concurrency + parallelism — the shared-nothing (BEAM) model

> **Moved.** The full design — `spawn`/`parallel:` nursery, `Channel[T]`, `Shared[T]`, sendability,
> and the sequential-first **C1–C5** staging — now lives in its own canonical doc:
> **[`docs/concurrency.md`](concurrency.md)**. It is still speculative (not scheduled); promote a
> milestone into `gaps.md` when committed.

---

## 3. Missing features (ranked by leverage for scripting) → **all promoted to `gaps.md`**

1. **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set). A Python-feel language without
   these feels broken. Pure parse-time desugar to loop + push. Cheap, large UX win.
2. **Sub-ranges — Rust-style `xs[1..3]`** — sub-list / substring via the existing `..` range
   (half-open), no new lexer token, no step. `Slice { obj, start, end }` → container-typed →
   bounds-clamped range-copy. (Omitted bounds / `..=` / negative index are deferred extensions.)
3. ~~**Iterator protocol + generators (`yield`)**~~ — **iterator DONE; generators removed.** The
   `Iterator[T]` parameterized protocol shipped (M13): user structs usable in `for`, generic
   `[S: Iterator[T], T]` bounds, and lazy `map`/`filter`/`take` written as **adapter structs** over it
   (Rust `std::iter` model — `examples/iter_adapters.chz`). **`yield`/generators are a permanent
   non-goal** (see `spec.md` *Non-goals*): they would need coroutine/continuation support in both
   engines, and the adapter-struct pattern covers lazy streaming without it.
4. ~~**List concat + map merge**~~ — **DONE.** Method-based: list `.concat`/`.extend`, map
   `.merge`/`.update` (concat/merge new, extend/update mutate). No new syntax; spread/unpack stays
   dropped. `examples/concat_merge.chz`. (See the `gaps.md` resolved log.)
5. ~~**Hex / binary / octal literals**~~ — **DONE.** `0xFF`/`0b1010`/`0o17`, lexer-only via
   `i64::from_str_radix`, `_` between digits. `examples/hex.chz`.
6. ~~**Optional chaining + null-coalescing**~~ — **DONE.** `x?.field`/`x?.method()` + right-assoc
   `a ?? b` on `Option`, lowered to a `match` by the desugar pass (zero checker/engine code).
   `examples/optchain.chz`.
7. ~~**Tuple-destructuring `for` (+ `enumerate` / `zip`)**~~ — **DONE.** `for a, b in list[(A,B)]`
   (N-var over `list[tupleN]`); VM splits map vs list-of-tuples at runtime on a new `Op::IsMap`.
   `enumerate`/`zip` shipped as pure-Chezzi `std/iter.chz`. `examples/for_tuple.chz`.
8. **Mutable closure capture** — currently snapshot-by-value, so closure counters / accumulators
   don't work. Real functional gap. Decide: keep intentional (document loudly) or fix (capture cell).
9. **Match guards + range patterns** — `n if n>0:`, `1..10:`. Roadmap. Guards subsume the rest.
10. **`std.os.exit(code)` + real exit codes** — currently deferred, but scripts *must* signal
    failure. Needs an exit-code channel threaded through both run drivers + the CLI.
11. **Runtime stack traces** — error + call chain + line numbers. Debuggability is a scripting
    feature.

**Ecosystem (Tier 4, separate track):** REPL (huge for scripting iteration), formatter, `assert` +
built-in test runner, LSP.

---

## 4. Optimizations (ranked effort → payoff)

> **Live numbers:** `docs/benchmarks.md` tracks Chezzi vs CPython (reproducible via
> `benches/run.chz`). Current baseline (2026-06-11): **2.1×–5.9× slower than CPython**, and
> a **standing startup win** (~11× faster cold). The gap scales with call density — `loop`
> (no calls) is 2.1×, `fib` (all calls) is 5.9×. Source hot-spot `file:line`s below come
> from that analysis; the scheduled work is roadmap **M19**.

Current: ~4–6.5× over the tree-walker, near the safe-match-dispatch floor. The two real costs are
**dispatch count** and **name lookup** — with **per-call allocation** a close third on call-heavy code.

**Cheap — do first:**
- ✅ **Peephole + constant folding (compiler)** — *landed M19 Phase 1* (`src/compiler/peephole.rs`):
  a jump-relocating pass that folds `ConstInt`/`ConstFloat` arith + `Neg`/`Not`, replicating the
  VM's checked semantics (overflow / div-by-zero stay unfolded so the runtime raises the same error).
- ✅ **Superinstructions** — *landed M19 Phase 1*: `BinLocalLocal` / `BinLocalConst` / `IncLocal`
  fuse the hot `GetLocal+GetLocal+BinOp`, `GetLocal+Const+BinOp`, and `i += k` windows (Int fast
  path inlined; non-Int falls back to the exact unfused op). Cut `loop` −36%, `primes` −25%.
  Remaining candidates: `GetLocal+GetField`, fuse compare+`AsBool`, the load-store accumulator.
- **Inline caching for name lookup** — globals / builtins / struct fields resolve *by name at
  runtime* today. Cache the resolved slot/index on first hit (monomorphic IC). Field access, method
  dispatch, and global reads all benefit. Moves `primes`. ⚠ must stay correct across the concurrency
  module-fault path (`ensure_module_faulted` / lazy worker module snapshot).
- ✅ **Kill per-call clones in `invoke_value`** — *landed M19 Phase 1*: matches on `&Obj` (no whole-
  `Obj` / closure-`HashMap` clone) and drops the arity-check `name.clone()`. Cut `fib` −17%, `list`
  −22%. Still open: the fresh `Vec<Value>` for args at `mod.rs:3181` (pass a stack slice).

**Medium:**
- **NaN-boxing the `Value`** — pack into 8 bytes (vs the current ~16-byte enum). Better cache density
  across the whole operand stack. Touches every `Value` site. Moves `loop`.
- **Specialize arithmetic** — binary ops re-dispatch on type every iteration; type-guard a hot loop
  to a monomorphic int path. Big on numeric loops (`loop`/`primes`, the current weak cases).
- **Frame pooling** — reuse call frames + the per-call slot pre-fill in `push_frame` instead of
  allocating per call. Helps recursion (`fib`).
- **String interning + cached hash** — intern keys / short strings → pointer-compare equality + free
  map hash. Map hashes are already cached; interning extends it. `ConstStr` (`mod.rs:2228`) boxes a
  fresh string on every push — interning constants kills that.
- **Reduce string-op allocations** — `BuildStr` (`mod.rs:2495`) stringifies each interpolation part
  into a fresh `String`; concat / `split` / `+` build a fresh `Rc<str>` each time. A builder / rope
  helps hot concatenation. Moves `str`. (See also: list clone per `for` iter at `mod.rs:2514`,
  per-char `String` on string iteration at `:2530`.)

**Big (separate milestones):**
- **Register VM** instead of stack — fewer ops, less stack traffic. Effectively a VM rewrite; only
  if dispatch count is still the wall after superinstructions.
- **Generational / incremental GC** — current is stop-the-world full-heap (`next_gc = 2×live`).
  Generational cuts pause + rescan cost on allocation-heavy scripts.
- **Cranelift AOT/JIT** — already the stretch goal. Near-native, but a whole backend. Only after the
  language stops moving.

**Highest payoff-per-effort:** superinstructions + inline caching + peephole/const-fold. They attack
dispatch count and name lookup — the two actual costs — without touching the value model or the GC.

**M19 Phase 1 done (2026-06-11):** peephole/const-fold + superinstructions + `invoke_value` clone
kill — all behavior-preserving (1516 tests + full two-engine parity green). Results in
`docs/benchmarks.md`. Next lever for `str` is string interning / `BuildStr` builder; inline caching
is the next dispatch win.
