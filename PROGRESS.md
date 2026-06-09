# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

Core language is feature-complete through **M18** plus several gap-closing passes. Concurrency
**C1 + C2 + C3** have landed on the tree-walk interpreter (see below). Latest suite: **1211 tests**
green (unit + parity + `cargo test conformance`), both engines parity-tested, `cargo clippy` clean.

**Next candidate:** concurrency **C4** — port C1–C3 to the bytecode VM (`src/vm`, `src/compiler`)
for the standing parity invariant: `Obj::Channel`/`Obj::Shared` heap objects, nursery/spawn ops, a VM
`deep_clone`, and a differential parity assertion in the golden harness. Designed in
**[`docs/concurrency.md`](docs/concurrency.md)** (shared-nothing BEAM-style
`spawn`/`parallel:`/`Channel[T]`/`Shared[T]`, sequential-first C1–C5 staging).

**Concurrency staging note:** C1–C3 ship on `--interp` only; the bytecode VM (default engine) and
compiler emit a clean *"runs on `--interp` until VM parity lands (C4)"* error for `parallel:` /
`spawn` / `Channel` / `Shared`, so the default engine never panics on a concurrency program. VM
parity is C4.

**Deferred refinement (post-C2):** read-only-capture and sendability are enforced for `spawn`
arguments (incl. closures smuggled inside struct/enum fields — deep field inspection) and for
`spawn:` block *reassignment* of captures. Still open: **rejecting a non-sendable value merely
*read* (not reassigned) inside a `spawn:` block** (e.g. capturing a closure and calling it) — benign
under the sequential executor; tighten when C5 brings real parallelism.

**Permanent non-goals:** `yield`/generators, variadic args, Level-3 dynamic `cdylib`/C-ABI FFI,
bignum (`i64`-only — every overflow is a recoverable fault; binary work → a future `bytes` *sequence*,
no `byte`/`u8` scalar).

---

## Done (newest → oldest)

Each landed TDD, both engines in lockstep, with a golden + parity `examples/*.chz`. Git has the detail.
(Concurrency C1–C3 are the documented exception: interp-only until VM parity in C4.)

- ✅ **Concurrency C3 — `Shared[T]` cross-task mutable box** (interp). `Shared(v)` (value-first — the
  element type is inferred from `v`, unlike `Channel[T]()`); methods `get()->T` (copies out), `set(T)`
  (copies in), `update(fn(T)->T)` (read-modify-write; releases the box borrow before calling the user
  fn so a re-entrant `get`/`set` can't panic). The handle is sendable and copied across the airlock —
  every task reaches the one box, whose single owner serialises writes (no locking under the sequential
  executor). The element type is *not* sendability-gated (only the handle crosses — the surprising
  asymmetry vs `Channel`, locked by a test). `Ref[T]` (the in-task box, `std/ref.chz`) is now forced
  **non-sendable** so passing it across a `spawn` is a compile error pointing at `Shared` (spec §7).
  *Known limit:* the `Ref` gate is a struct-name check (a user struct named `Ref` would also be
  non-sendable) — a `StructInfo` origin flag is the principled fix, deferred. `examples/shared.chz`.
- ✅ **Concurrency C2 — `Channel[T]` + sendability** (interp). `Channel[T]()` buffered/unbounded
  FIFO mailbox; methods `send` (move-on-send, deep-copied across the airlock), `recv` (FIFO; empty =
  deadlock-detect fault, not a hang), `len`. A `sendable(Ty)` predicate gates channel element types,
  `spawn` arguments, and `spawn:` capture reassignment — recursing into struct/enum fields (a closure
  smuggled inside a struct field is caught) with a cycle guard. `spawn`'s call target is restricted to
  a function/method like `defer`. `examples/channel.chz` (the canonical fan-out worker).
- ✅ **Concurrency C1 — `spawn` / `parallel:` nursery** (interp, sequential executor). `parallel:` is a
  structured-concurrency nursery; `spawn f(x)` (form 1) and `spawn:` block (form 2) register tasks that
  run to completion FIFO at the dedent (first error aborts siblings + propagates, composing with
  `recover:`/`defer`). `spawn` legal only inside a `parallel:` (checker `nursery_depth`, reset across fn
  boundaries). `deep_clone` isolates task data across the airlock; channels/functions pass by handle.
  Grammar + conformance updated. `examples/parallel.chz`.
- ✅ **Integer overflow policy** — every `i64` overflow is a recoverable fault (never wrap/crash);
  closed the last leak (`std.math.abs(i64::MIN)` → `checked_abs`). `examples/overflow.chz`.
- ✅ **Gaps pass II** — `Ref[T]` mutable box (pure-Chezzi `std/ref.chz`); `sort_by_key`; call fn-typed
  field `self.f(x)`; relax non-const defaults (no param/field refs); runtime stack traces (error line
  + call chain, identical on both engines).
- ✅ **Scripting-ergonomics gap pass** — hex/bin/oct literals; list `.concat`/`.extend` + map
  `.merge`/`.update`; tuple-destructuring `for` + `std/iter.chz` `enumerate`/`zip`; optional chaining
  `?.` + null-coalescing `??`; general tuple destructuring + match-on-tuple + guards.
- ✅ **Fix — loop variable is immutable** — checker rejects assignment to a `for`-loop var (was a
  VM/interp divergence); inner `:=` shadow stays mutable.
- ✅ **M18 — `defer` → block/lexical scope** — runs when its enclosing block exits on every path
  (fall-through / break / continue / return / `?` / panic), LIFO, inner-block-first. Supersedes M17.
- ✅ **M17 — `defer` (Go-style, frame-scoped)** — runs at frame exit, LIFO; receiver+args evaluated
  at the `defer` statement.
- ✅ **M16 — comprehensions + `std.os.exit(code)`** — `[e for x in it if g]` (+ set/map forms),
  first-class AST node; hard uncatchable cooperative exit threaded through both run drivers + CLI.
- ✅ **M15 — slicing + `Index`/`IndexSet`/`Slice` protocols** — `xs[1..3]` half-open/clamped;
  list/map/str conform intrinsically, user structs structurally.
- ✅ **M14 — method-level type params** · **user-defined parameterized protocols** (concrete-arg
  bounds, generalizing `Iterator[T]`) · **default + named args on methods** (desugar-pass).
- ✅ **Default + named arguments** — free fns + struct ctors; scope-aware desugar pass, both engines
  consume an already-normalized AST.
- ✅ **Tech-debt sweep** — reject dup generic param `[T, T]`; nested `set` equality parity; explicit
  call-site type args `name[T,…](…)`.
- ✅ **M11 — panic recovery + Go-style errors** — 2-param `Result[T, E]` (`T!`/`T!E`), `Error`
  protocol (`str` conforms), `recover:` boundary catching any transitive runtime fault.
- ✅ **M10 — type-system depth** — `Stringable`, `Hashable`, per-operator `Add`/`Sub`/`Mul` protocols,
  multi-bound `T: A + B`, transparent type aliases, generic enums; `map`/`set` reworked into real
  insertion-ordered hash tables (any `Hashable` key/element).
- ✅ **M9 — Tier-2 stdlib** — `std.regex` (the `regex` crate) + `std.request` (`ureq`+rustls, blocking).
  First runtime deps; language stays single-threaded/sync.
- ✅ **M8 — Tier-1 stdlib** — `s.chars()` + iterable strings; `std.json` (pure-Chezzi parse/stringify
  + type-directed `decode[T]`); native `std.process`/`std.fs`/`std.time`; `set` type.
- ✅ **M7 — generics + structural protocols** — type-erased generic fns/structs, Go-style `protocol`s,
  `Comparable`; stdlib `min`/`max`/`clamp` unified into pure-Chezzi `std.cmp`; `list.sort()` widened.
- ✅ **Round 2 gaps #10–#15** — `sort_by`, `ord`/`chr`, int+float math, map `for`, nested/tuple
  match, bitwise ops. Plus: iterator protocol (struct `next()`), `Iterator[T]` parameterized bound
  with element recovery + lazy adapters, match guards + half-open range patterns.
- ✅ **Tuples + multiple return + destructuring (gap #8)** — `(e1, e2, …)`, tuple types, `a, b := f()`,
  `.0`/`.1` access; immutable, fixed-arity, GC-traced.
- ✅ **M6a/b/c** — core-type str/list methods; pipe `|>` (parse-time desugar); stdlib via the Level-2
  native FFI seam (`NativeFn` + `Host`): `std.math`/`std.io`/`std.os` native, `std.str` pure Chezzi.
- ✅ **`map[K, V]` dictionary (gap #5)** — literals, keyed read/insert/update, six methods, GC-traced.
- ✅ **Index & field assignment** — `xs[i] = v`, `p.x = v`, `+=`/`-=` mutate in place (both engines).
- ✅ **M5a/b/c** — bytecode compiler + stack VM; hand-built mark-sweep GC; cross-engine parity +
  perf (~6.5× arith / ~4.3× fib over the interp) + CLI default flip to the VM (`--interp` for the
  tree-walker). Documented divergence: VM pre-parses `{expr}` chunks (malformed interpolation in dead
  code is a load error). `std.os.getcwd` not yet injectable via `HostConfig`; `read_file` capped at 64 MiB.
- ✅ **M4.5 — modules / imports + resolver** — multi-file, `chezzi.toml` root, run-once dep order,
  cross-module home-globals, cycle detection. Type names are program-global (collision-detected).
- ✅ **M4 — type checker (local inference)** — bidirectional, no unification; return-type inference,
  `T?`/`T!` sugar, expression-valued `match`/`if`, Go-style error accumulation.
- ✅ **M3 — tree-walk interpreter** — full expr/stmt set, `?` operator, string interpolation,
  256 MB-stack thread + `MAX_CALL_DEPTH` guard.
- ✅ **M2.5 — canonical grammar + conformance** — `docs/grammar.bnf` executed via the `bnf` crate
  (dev-dep only), differential-tested vs the parser over a corpus. Run `cargo test conformance`.
- ✅ **M2 — parser → AST** — recursive descent + Pratt; spans retrofitted; depth-capped.
- ✅ **M1 — lexer** — full `examples/hello.chz` incl. Indent/Dedent; string escapes, numeric underscores.
  Open follow-ups (anytime): scientific notation `1e3`, single-quote strings, unicode `\u{…}` escapes.

---

## Roadmap (later)

- ⬜ **Concurrency (C1–C5)** — see `docs/concurrency.md`. Speculative; schedule via `gaps.md` first.
- VM/GC optimizations (superinstructions, inline caching, NaN-boxing) — written up in
  **[`docs/future.md`](docs/future.md)**.

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in
  `docs/spec.md` → *Standard library* → "Future idea — native FFI". Default build stays zero
  third-party crates; dynamic `cdylib` plugins deferred. Do not start without an explicit decision.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **Collection literals must be single-line** — a newline inside `[`/`{` ends the expression.
- **`match` limits** — no multiple `Some(...)` arms, no nested nullary-variant patterns (nest a
  second `match`).
- **Float division by zero is a runtime fault**, not an IEEE `Inf`/`NaN`.
- **`std.os.getcwd`** not yet injectable via `HostConfig` (parity holds); **`read_file`** capped at 64 MiB.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists
  need only `Node?` child fields + a `match` per step, no special support.
