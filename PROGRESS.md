# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

Core language is feature-complete through **M18** plus several gap-closing passes. Concurrency
**C1 + C2 + C3 + C4** have landed (both engines), plus the **`Executor` escape hatch** (C5's
sequential subset) with **program-exit auto-drain** (C5 / A2) and the C5 checker refinements. So
`spawn` / `parallel:` / `Channel[T]` / `Shared[T]` / `Executor` all run on **both** engines.
**Group B's B1 + B2 (cooperative fibers + blocking `recv`) have now landed on the VM engine**: a
`recv` on an empty channel suspends the running fiber and the scheduler resumes it when a sibling
`send`s, so mid-flight producer/consumer works (`examples/channel_block.chz`). Latest suite:
**1268 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean.

**Next candidate:** finish **Group B**. The remaining work: **B1/B2 for the interpreter** (the
tree-walker needs stackful coroutines or a CPS rewrite — closing the documented parity gap below),
**B3** Tier-C OS-thread multicore (alternative bet), then **B4** real `Shared` and **B5** real
`Executor` background pool (incl. the deferred A3b `submit`-capture gate). The surface of `spawn` /
`parallel:` / `Channel` / `Shared` / `Executor` is **unchanged**. Full A/B breakdown:
**[`docs/concurrency.md`](docs/concurrency.md)** §9.

**Parity gap (intentional, documented):** B1/B2 are **VM-only** for now. Under `--interp`, a blocking
`recv` still faults `deadlock` (the interpreter has no suspendable execution yet). Programs that never
block (C1–C5 goldens) stay byte-identical on both engines; only mid-flight blocking diverges, pinned
by `interp::tests::channel_block_chz_faults_deadlock_on_interp` vs the VM golden. **Known v1 limits
(VM):** a blocking `recv` cannot suspend inside a native callback (list HOFs, `sort`, `compare`/`hash`/
`str` hooks, `Shared.update`, the executor drain, or a `defer`red call) — it faults `deadlock`
instead (the callback's loop/recursion state lives on the host stack, not in a fiber); and a fiber in
an outer nursery cannot be woken by progress in an inner one (structured-concurrency scoping).

**Group A status (sequential refinements, no engine rewrite):** **A2 (`Executor` program-exit
auto-drain) is done** (this session, both engines). **A3a** (reject a non-sendable read smuggled
through a *nested closure* in a `spawn:` block) was found **already enforced** — emergent from the
persistent `capture_floors` + the `infer_ident` read gate — and is now **pinned by a regression
test**. **Dropped:** **A1** (`Channel.try_recv`) — a primitive whose motivating mid-flight-producer
scenario needs the engine; **A3b** (`Executor.submit` capture gate) — `submit` runs the closure
in-heap at the drain, so gating it now would wrongly reject valid programs (lands with Group B).

**Permanent non-goals:** `yield`/generators, variadic args, Level-3 dynamic `cdylib`/C-ABI FFI,
bignum (`i64`-only — every overflow is a recoverable fault; binary work → a future `bytes` *sequence*,
no `byte`/`u8` scalar).

---

## Done (newest → oldest)

Each landed TDD, both engines in lockstep, with a golden + parity `examples/*.chz`. Git has the detail.

- ✅ **Concurrency C5 / Group B — B1 + B2 cooperative fibers + blocking `recv`** (VM engine). The
  bytecode VM gained *suspendable execution*: a `recv` on an empty channel under an active `parallel:`
  scheduler **parks** the running fiber (rewind-and-retry at the instruction boundary — push the
  receiver back, `ip -= 1`, set a suspend flag that breaks `run_until`/the re-entrant call path
  without unwinding defers) and the **nursery-local cooperative scheduler** runs a runnable sibling,
  resuming the parked fiber once its channel has data. A child that never blocks still runs to
  completion FIFO, so non-blocking programs are byte-for-byte unchanged. Each fiber owns its full
  execution context (`frames`/`stack`/`call_depth`/`cur_base`/`handlers`/`nurseries`/`fault_trace`),
  swapped in/out around scheduling; parked fibers are GC-rooted; nested `parallel:` recurses into a
  fresh scheduler level; a child fault or `std.os.exit` aborts its siblings. A wide native-reentry
  guard converts a `recv` that can't be parked (inside a HOF/sort/`compare`/`hash`/`str`/`update`/
  executor-drain/`defer`) into the deadlock fault. `examples/channel_block.chz` golden (VM + GC-stress)
  + ping-pong, deadlock-detection, guard, nested-`parallel:`, recover-in-child, and os.exit-in-child
  tests. **VM-only** — interp parity is a later milestone (gap pinned by an interp test). See the
  Current-focus parity-gap note and [`docs/concurrency.md`](docs/concurrency.md) §9.
- ✅ **Concurrency C5 / A2 — `Executor` program-exit auto-drain** (both engines). An executor
  submitted to but never explicitly `shutdown`/`shutdown_now`-ed is now gracefully drained at a clean
  program exit (FIFO, creation order) instead of silently dropping its queued work — mirrors a
  top-level `defer ex.shutdown()`. A per-engine **executor registry** (interp `Vec<Rc<RefCell<…>>>`;
  VM `Vec<GcRef>` that also joins the GC root set so un-shut work survives to the drain) drives it via
  the shipped `shutdown` path (first-fault-aborts-siblings). Hooked into every driver
  (`run_program_inner` / `run_with` stress / `run_file_inner`, both engines). A hard `std.os.exit`
  skips it (like `defer`); a faulting program is not drained. Also pinned **A3a** with a regression
  test — a non-sendable read smuggled through a *nested closure* in a `spawn:` block is rejected
  (already enforced, emergent from `capture_floors`). `examples/executor_autodrain.chz` golden + VM/
  interp parity + GC-stress + os.exit-suppression + fault-propagation tests. *Dropped: A1, A3b
  (see Current focus).*
- ✅ **Concurrency C5 (sequential subset) — `Executor` escape hatch** (both engines). `Executor()` +
  `submit(fn())` / `shutdown()` / `shutdown_now()`, reaped via `defer ex.shutdown()` (docs
  [`concurrency.md`](docs/concurrency.md) §8). New `Ty::Executor` (non-generic, sendable handle,
  reserved type name); interp `Value::Executor(Rc<RefCell<ExecState>>)`; VM `Obj::Executor { queue,
  shut }` + `Op::NewExecutor` + GC child-tracing. `submit` enqueues by handle (rejected once shut);
  `shutdown` drains the **live** queue FIFO one task at a time via the re-entrant call path (first
  fault aborts the rest + propagates, like a nursery; not-yet-run siblings stay for a later reap);
  `shutdown_now` discards pending. Both engines drain the live queue identically (a re-entrant
  `shutdown_now`/fault mid-drain behaves the same) — parity-pinned. `examples/executor.chz` golden +
  VM/interp parity + GC-stress + re-entrancy/fault-during-drain tests. *Deferred to real-C5:*
  program-exit auto-drain + closure-capture sendability gating (see Current focus).
- ✅ **Concurrency C5 refinement — `spawn:` block read sendability gate** (checker). A non-sendable
  *function-local* capture merely **read** inside a `spawn:` block (e.g. capturing a closure and
  calling it) is now a compile error, not just a *reassignment* (closes the C2-era gap). Module
  imports / top-level bindings are excluded (globals resolvable in every task, like free functions),
  so reading an imported module inside a task stays legal.
- ✅ **Concurrency C5 refinement — `StructInfo` origin flag** (checker). The `Ref[T]` non-sendability
  gate now keys on a `StructOrigin::{Builtin,User}` flag (threaded from `check_graph` via a
  `current_module_is_stdlib` flag set per module) instead of a bare struct-name string — so a *user*
  struct merely named `Ref` is sendable, while the builtin `std.ref` `Ref[T]` stays non-sendable.
- ✅ **Concurrency C4 — VM parity for `spawn`/`parallel:`/`Channel`/`Shared`** (bytecode VM +
  compiler). Ported C1–C3 off `--interp`-only onto the default engine: heap `Obj::Channel(VecDeque)`
  / `Obj::Shared(Value)` with GC child-tracing; ops `EnterNursery`/`JoinNursery`/`SpawnCall`/
  `SpawnMethod`/`SpawnBlock`/`NewChannel`/`NewShared`; a VM `deep_clone` (data deep-copied, str/func/
  closure/module/Channel/Shared by handle — mirrors interp). The `spawn:` block compiles to a
  synthetic zero-arg closure proto captured like any closure. Sequential executor: a `nurseries`
  stack drains FIFO at the join, first error aborts siblings; pending tasks are GC roots; a
  `recover:` boundary reclaims a fault-orphaned nursery via `Handler::nursery_len`; `Shared.update`
  re-roots the box across its re-entrant call. Differential parity goldens for all three examples +
  micro-tests + GC-stress tests. The four staging-error stubs are gone. *No checker changes* (it was
  already engine-agnostic). Reviewed by two parallel S++ reviewers — no Critical/Important findings.
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

- 🟦 **Concurrency C5 — Group B (real engine)** — **B1 + B2 (cooperative fibers + blocking `recv`)
  done on the VM** this session. Remaining: **B1/B2 for the interpreter** (close the VM-only parity
  gap — stackful coroutines or CPS), **B3** OS-thread multicore (alternative bet), **B4** real
  `Shared`, **B5** real `Executor` pool (incl. the deferred `submit`-capture gate A3b). Group A is
  done: C1–C4, the `Executor` sequential subset, **A2 auto-drain**, the C5 checker refinements, and
  **A3a** (pinned). See the A/B breakdown in `docs/concurrency.md` §9.
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
