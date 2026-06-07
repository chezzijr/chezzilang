# Chezzi — Language Gaps

Known limitations discovered by writing real programs and probing the language against what an
everyday app needs. Each **open** entry lists what it **blocks** and a **fix sketch** where the path
is clear. Resolved gaps are kept as a one-line log (so we don't re-flag them) — full fix detail lives
in `PROGRESS.md` + the cited `examples/*.chz`.

> Method: small `.chz` snippets run through both engines (`chezzi run` / `--interp`). "Verified"
> means observed, not inferred from the cheat-sheet (`docs/syntax.md`).

Legend: 🔴 blocks real apps · 🟡 notable friction · 🟢 works (recorded so we don't re-flag it).

Last updated: 2026-06-07. Baseline: post-M13 (`Iterator[T]` parameterized bound; `yield` dropped).

> **Forward-looking brainstorm** (a non-Go concurrency model, VM/GC optimizations, far-out ideas)
> lives in **[`docs/future.md`](docs/future.md)** — speculative, NOT scheduled. Concrete near-term
> scripting features have been **promoted into the "Open gaps" section below**; future.md keeps the
> large/speculative tracks (BEAM concurrency, JIT, register VM).

---

## Open gaps

The language **core** is feature-complete: scalars, `list`/`map`/`set`/`tuple`, structs (generic),
sum types (`enum` with payloads, generic), `Result`/`Option` + `?`, generics + structural protocols
(`Comparable`/`Add`/`Sub`/`Mul`/`Hashable`/`Stringable`/`Error`/`Iterator[T]`), exhaustive `match`
(literals, wildcard, nested/tuple, guards, ranges), closures/HOF, struct methods, modules, GC, two
backends, string interpolation, pipe, panic recovery (`recover:`), default + named args. What remains
is **~70% stdlib/scripting breadth, ~30% type-system + runtime depth**, ordered below by leverage.

### 🔴 Scripting essentials (promoted from future.md)

- **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set forms). A Python-feel language without
  these reads as broken. **Fix:** parse-time desugar to a loop + `push`; no new opcode, no engine
  change. Cheap, large UX win.
- **Sub-ranges — Rust-style `xs[1..3]`** — extracting a sub-list / substring today needs a manual
  `for … push` loop. **Fix (decided):** index with the **existing** `..` range — `xs[start..end]`,
  `s[start..end]` — half-open (end-exclusive), matching the `..` used in `for i in 1..n` and range
  patterns. **No new lexer token** (unlike Python's `[a:b:c]`), no step. **Shape:** the parser emits a
  `Slice { obj, start, end }` when an index expression is a `..` range; the checker types it as the
  container type (`list[T] → list[T]`, `str → str`; bounds must be `int`); both engines do a
  bounds-clamped range-copy (list + str). **Deferred extensions:** omitted bounds (`xs[..n]`/`xs[1..]`
  /`xs[..]` — needs optional-bound ranges), inclusive `..=`, and negative indexing on plain `[i]`.
- ~~**Generators (`yield`) + a formal `Iterator[T]` protocol**~~ — **resolved + descoped.** The
  `Iterator[T]` protocol shipped (M13, see 🟢): `[S: Iterator[T], T]` is a real parameterized bound.
  `yield`/generators are a **permanent non-goal** (see `spec.md` *Non-goals*) — they would have
  needed coroutine/continuation support in *both* engines, and are unnecessary: lazy
  `map`/`filter`/`take` are written as **adapter structs** over `Iterator[T]` (Rust's `std::iter`
  model — `examples/iter_adapters.chz`).
- **`std.os.exit(code)` + real process exit codes** — scripts *must* be able to signal failure.
  **Fix:** thread an exit-code channel through both run drivers + the CLI.

### 🟡 Scripting ergonomics (promoted from future.md)

- **List concat + map merge** — combining two lists / maps today needs a manual `for … push` loop
  (`+` is str/numeric-only; there is no list `+`, `.concat`/`.extend`, or map `.merge`/`.update`).
  **Fix:** add list concatenation (extend `Add` to lists, or a `.concat`/`.extend` method) and a map
  `.merge`/`.update` method — both reuse existing operator/method dispatch, no new syntax.
  (Spread/unpack `[*a, *b]`, `{**m}`, `f(*args)` is **dropped** — variadics are a non-goal, see
  `spec.md`, and concat/merge cover the literal case more cleanly.)
- **Hex / binary / octal literals** — `0xFF`, `0b1010`, `0o17`. Bitwise ops shipped but only decimal
  literals exist — awkward for bit work. **Fix:** lexer-only.
- **`enumerate` / `zip` builtins** — `for i, x in enumerate(xs)`, `for a, b in zip(xs, ys)`. The
  two-var `for` form already exists (maps); wire these builtins to it. Daily-driver scripting.
- **Optional chaining + null-coalescing** — `x?.field`, `a ?? default` on `Option`. Cuts `Option`
  boilerplate; `if/else` expr + `?` already exist.
- **String formatting** — width / precision / radix in interpolation: `"{x:08.2f}"`, `"{n:x}"`.
  Interpolation exists; a format spec does not.
- **`defer` (cleanup on scope exit)** — runs on all three exit paths now that M11 added unwinding:
  normal return, `?` short-circuit, panic. **Fix:** per-frame LIFO deferred-call stack drained at
  every frame exit (interp `Flow`/recover path; VM `Return` + handler-stack unwind); evaluate `defer`
  args at the statement (Go semantics). Composes with `recover:`. (Considered & rejected:
  Python-style `with` — needs a new protocol + block.)

### 🟡 Type-system + runtime depth (already-tracked open)

- **Non-constant default expressions** — defaults must be constant literals today (no `compute()` or
  references to other params). Still deferred.
- **Calling a function-typed field** — `self.f(x)` on a struct whose field `f: fn(T)->U` parses as a
  *method* call (`type X has no method 'f'`). **Workaround:** bind first — `g := self.f` then `g(x)`
  (used in `examples/iter_adapters.chz`). **Fix:** in method-call lowering, if the receiver has a
  field matching the name with a `fn` type, treat it as field-access-then-call. Found during M13.
  **GOTCHA when fixing:** the desugar method-arg pass (`src/desugar/mod.rs::normalize_call`) treats
  every `recv.name(...)` as a possible method — make it field-aware then, or a same-named method's
  default could be injected into a fn-field call.
- **`sort_by_key`** — sugar on `sort_by` (#11). Still open.
- **Mutable closure capture** — captures are snapshot-by-value, so closure counters / accumulators
  don't work (real functional gap). **Decide:** keep intentional (document loudly) or fix with a
  capture cell.
- **Runtime stack traces** — error + call chain + line numbers. Debuggability is a scripting feature.
- **Integers** — `i64` only; no `byte`, no bignum, no configurable overflow policy (overflow → error).

### Tier 4 — ecosystem (toolchain, not the language)
REPL (huge for scripting iteration), formatter, `assert` + built-in test runner, LSP, package
manager / registry (spec defers this), debugger, doc comments + docgen.

---

## 🟢 Verified working (so we don't re-flag)

- **Struct equality** `P(1,2) == P(1,2)` → structural compare.
- **String indexing** `s[i]` → 1-char `str`; `s.len/upper/lower/trim/split/join/contains/starts_with`;
  `s.chars()` + strings iterable (`for c in s`).
- **List-of-structs**, field access `ps[1].y`; **nested-list read** `g[i][j]`; **by-reference
  sharing** — a list passed to a fn and `.push`ed is mutated for the caller.
- **`if` / `match` as expressions**, incl. inside interpolation `"{if a>b: a else: b}"`.
- **`Result` / `Option` + `?`**, exhaustive-match checking, deep recursion, integer overflow → error
  (not wrap), int division truncation, `%` on negatives.
- **`std.math` / `std.io` / `std.os` / `std.str` / `std.cmp` / `std.json` / `std.time` / `std.fs` /
  `std.process` / `std.regex` / `std.request`** on both engines.
- **Recursive / self-referential structs** (BST, linked list) build, walk, GC fine.
- **Mutable `self` across method calls** — `self.pos += 1` persists for the caller (recursive-descent
  parser cursor relies on it).
- **Nested-list DP** — `list[list[int]]` with two-level `dp[i][w] = …` index assignment.
- **Empty map literal infers `K,V` from later use** — `m := {}` then `m["a"] = 1` type-checks.
- **User-struct iterator protocol** — a struct with `next(self) -> Option[T]` is iterable in `for`
  (lazy per-step; infinite + early `break` terminates). Both engines.
- **`Iterator[T]` parameterized bound** (M13) — `[S: Iterator[T], T]` accepts any iterable (built-in
  `list`/`set`/`str`/`map` intrinsically, or a struct via `next`) and recovers element type `T` into
  loop vars + return types. The first protocol that takes type arguments — now generalized to
  **user-defined parameterized protocols** (M14, `protocol Container[T]`). Lazy adapter structs
  (Take/Mapped over an infinite source) compose without `yield`. Both engines parity-tested.
  `examples/iterator_bound.chz`, `examples/iter_adapters.chz`.

---

## Resolved log (one line each — full detail in `PROGRESS.md` + examples)

**Round 1 (#1–#9) ✅** · both engines lockstep, parity + conformance green:
1. **Index assignment** `xs[i] = v` (+ `+=`/`-=`) — `Op::SetIndex`/`Dup2`. `examples/mutate.chz`.
2. **Mutable struct fields** `p.x = v` (+ compound) — `Op::SetField`/`Dup`.
3. **HOF params** `f: fn(int) -> int` — `Type::Func` + `resolve_type` lowering. `examples/hof.chz`.
4. **List methods** `pop`/`reverse`/`contains`/`index_of`/`sum`/`sort` + `map`/`filter`/`fold`
   (re-entrant, GC-rooted). `examples/list_methods.chz`, `list_hof.chz`.
5. **Map type** `{"a":1}`, `m[k]`/`m[k]=v`, `get`/`has`/`keys`/`values`/`remove`/`len`. `examples/map.chz`.
6. **Literal + wildcard `match`** (`0:` / `_:`) — `Pattern::Literal`/`Wildcard`, no new opcode.
7. **`break` / `continue`** — `LoopCtx`; for-`continue` lands on the increment. `examples/loops.chz`.
8. **Tuples + multi-return + destructuring** `(a,b)`, `(int,int)`, `a,b := …`, `.0`. `examples/pair.chz`.
9. **Strict compound assignment** — `+=`/`-=` reject `int <op> float` into an `int` slot.

**Round 2 (#10–#15) ✅** · real DSA/apps probe:
10. **`ord` / `chr` builtins** — char→int / int→char. `examples/cipher.chz`.
11. **`sort_by(fn(T,T)->int)`** — stable merge sort over a re-entrant comparator, GC-rooted.
    `examples/sort_by.chz`.
12. **Int `abs`/`min`/`max`** — later unified into generic `std.cmp` (M7-G3). `examples/knapsack.chz`.
13. **Bitwise ops** `& | ^ << >>` (int-only) — lexer→checker→both engines + grammar. `examples/bits.chz`.
14. **Map iteration** `for k in m` / `for k, v in m`. `examples/word_freq.chz`.
15. **Nested / tuple match patterns** — recursive `Pattern` + `MatchKind::Tuple`. `examples/match_nested.chz`.

**M7 — generics ✅** · generic functions + structs + structural protocols (`Comparable`); explicit
call-site type args `max[int](…)`; generic enums; multi-bound `T: A + B`. `examples/generics.chz`,
`generic_structs.chz`, `generic_enum.chz`.

**M8 — Tier-1 stdlib ✅** · `std.json` (dynamic `Json` enum + typed `decode[T]`), `std.time`,
`std.fs`, `std.process`; `s.chars()` + string iteration; `set` type. `examples/json_*.chz`, `set.chz`.

**M9 — Tier-2 stdlib ✅** · `std.regex` (regex crate), `std.request` (blocking HTTP via ureq+rustls);
seam grew `NativeRet::Struct`/`Map`. `examples/regex_demo.chz`, `request_demo.chz`.

**M10 — type-system depth ✅** · `Hashable` (real hash-table map/set, struct keys), `Stringable`
(custom `str()`/`print`/interp), `Add`/`Sub`/`Mul` operator protocols, multi-bound, type aliases
(`type UserId = int`). `examples/hashmap_keys.chz`, `stringable.chz`, `operators.chz`, `type_alias.chz`.

**M11 — Tier-3 robustness ✅** · panic recovery (`recover:` → `Result[T, Error]`, catches index-OOB
/ div-zero / overflow / missing-key); Go-style errors (`T!` = `Result[T, Error]`, `Error` protocol);
structural iterator protocol; match guards (`pat if cond:`) + range patterns (`1..10:`); default +
named args (functions + struct constructors, desugar pass). `examples/match_guard.chz`,
`match_range.chz`, `default_args.chz`, `named_struct.chz`.

**M14 — generics depth ✅** · two gaps closed (TDD, both engines parity-tested):
- **Method-level type parameters** — a method may introduce its own fresh `[U]` beyond the struct's
  `[T]` (`fn map_to[U](self, f: fn(T) -> U) -> U`); `U` is inferred from the call args, bounds
  enforced, recovered through `Iterator[T]` — the free generic-fn path generalized to method calls
  (`infer_generic_method`). Shadowing the struct's own param is rejected. `examples/method_type_params.chz`.
- **User-defined parameterized protocols** (concrete-arg bounds) — `protocol Container[T]:` plus
  bounds like `[X: Container[int]]`; conformance is structural with `T` substituted, the method's
  return flows into the caller. Generalizes the special-cased `Iterator[T]` (which still recovers its
  arg; user protocols take theirs explicitly). Usable as a bound only, not an existential value type.
  `examples/param_protocol.chz`.
- **Defaults / named args on methods** — now consistent with free fns + struct ctors. Handled in the
  pre-type **desugar pass** (`src/desugar`): a program-wide method registry resolves a call by name
  (the receiver type isn't known pre-check), fills omitted defaults + reorders named args into a
  positional list — so the checker and both engines stay untouched. Same-named methods on different
  structs with different params → a named call is ambiguous (rejected); built-in method names are
  skipped. `examples/method_default_args.chz`.

**Tech debt cleared ✅** · parser `MAX_DEPTH` 128→64 (off the test-stack edge); duplicate type param
`[T, T]` rejected; nested-`set` equality parity; explicit call-site type args; `?`-in-closure checked
against the closure's own return type.
