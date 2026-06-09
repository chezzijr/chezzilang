# Chezzi — Concurrency & Parallelism (`spawn` / `parallel:`)

> **Status:** design doc for *future* implementation — **not yet built**. This is the canonical
> reference the eventual implementation works against. `PROGRESS.md` + `gaps.md` remain the source of
> truth for what's actually scheduled. Promote a milestone into `gaps.md` when it's committed.
>
> Lifted and expanded from `docs/future.md §2` (which now points here). The syntax is fixed; the
> *engine* is staged (a sequential executor first, real multicore later) so the surface never changes
> when concurrency gets teeth.

Chezzi's concurrency is a **shared-nothing actor model** in the lineage of the Erlang/Elixir **BEAM**
(shared-nothing tasks, per-task heap + GC, message passing) **plus** two borrowings — **Go's
first-class channel** and **Rust's move-on-send**. The surface is the **goroutine feel** (`spawn`,
cheap, no `async` colouring) wrapped in a **structured-concurrency nursery** (`parallel:`) so tasks
can't leak. The whole design is chosen so it **never taxes the single-threaded fast path** (`Rc`,
per-heap stop-the-world GC stays untouched).

---

## 1. Status & lineage

- **Model:** shared-nothing tasks. Each task (eventually) owns its heap + GC; tasks share **nothing
  mutable**, communicating only by **move/copy** through channels.
- **Surface:** `spawn` (a statement, goroutine-feel) inside a `parallel:` **nursery** that joins all
  children at the dedent. No `async`/`await` colouring, no `WaitGroup`.
- **Primitives:** `Channel[T]` (the sync primitive — a mailbox outside every heap) and `Shared[T]`
  (the sanctioned cross-task mutable box — an owner-task serialises writes, Elixir's `Agent` trick).
- **Staging (this doc's central decision):** ship the full *surface + type system + both engines* on
  a **sequential, run-to-completion executor** first (milestones **C1–C4**); add real fibers /
  multicore later (**C5**) behind the same syntax. See [§4](#4-execution-semantics) and
  [§9](#9-implementation-roadmap-c1c5).

---

## 2. Why shared-nothing, and why not the Go memory model

The key principle: **cost scales with shared mutable memory, not with the number of cores.** Forbid
sharing — make tasks copy/move messages — and parallelism gets cheap, because no two threads ever
touch the same object.

| Tier | Model | Multicore? | `Rc`→`Arc`? | GC change? | Cost |
|------|-------|-----------|-------------|-----------|------|
| A | Cooperative fibers (1 thread) | ❌ | no | +scan suspended fiber stacks | cheap |
| B | Worker processes (`std.process`) | ✅ | no | none | ~free, but heavy IPC |
| C | **Shared-nothing threads** + channels | ✅ | no | none (per-thread heap+GC) | medium |
| D | Shared-memory threads (Go/Java) | ✅ | **yes, everywhere** | concurrent collector | huge |

**Chosen = A + C composed**, but *staged*: the **surface and semantics of A+C** ship first on a
sequential executor (no scheduler, no threads), then C5 adds the real Tier-A scheduler and/or Tier-C
threads. Porting the **Go model (Tier D)** would mean `Rc`→`Arc` across every value (atomic refcount
bumps ~10–30× a normal bump, taxing single-threaded code *forever*) plus a thread-safe concurrent GC.
That permanently taxes the common case and hands users the entire data-race bug class. **Skipped.**

### Memory picture — own heaps, channel between

```
 task 1: [own heap] ──send──┐
 task 2: [own heap] ──send──┼──►  main task [own heap]
 task 3: [own heap] ──send──┘        ┌──────────────────┐
                                     │ results: [...]    │
  each heap has its OWN GC           └──────────────────┘
  (no handshake, no barrier)   values MOVE across the channel, never shared
```

### The race you can't write

```chezzi
# Go — compiles, runs, WRONG: 1000 goroutines stomp one int → torn writes, data race
# counter := 0; for i := 0; i < 1000; i++ { go func() { counter++ }() }

# Chezzi — the bug is unrepresentable
counter := 0
parallel:
    spawn fn(): counter += 1     # ✗ checker error: captures mutable `counter`; not sendable.
                                 #    captured bindings are read-only copies — use a Channel,
                                 #    or a Shared[int] for shared mutable state.
```

**The trade in one line:** Tier D lets you share → fast sends, but races are your problem. A+C
forbids sharing → the race literally cannot be expressed. For a scripting language that's a *good*
trade — the #1 concurrency bug class doesn't exist.

---

## 3. Surface syntax

`spawn` is a **statement** in two forms (both sidestep the single-expression closure limit):

```chezzi
spawn worker(1, ch)        # form 1: spawn a named call (Go's `go f(x)`)

spawn:                     # form 2: spawn an anonymous indented block (a statement, not an expr)
    x := heavy(1)
    ch.send(x)
```

Both are only legal **inside a `parallel:` nursery**, which joins all children at the dedent:

```chezzi
fn worker(id: int, prefix: str, out: Channel[str]):
    out.send("{prefix}-{id}")

fn main():
    ch := Channel[str]()
    label := "task"
    parallel:                          # nursery — joins all children at the dedent
        spawn worker(1, label, ch)     # 1, label COPIED in; ch = shared mailbox handle
        spawn worker(2, label, ch)
    # reaching here ⇒ both workers finished. No WaitGroup, no leaks.
    for _ in 0..2:
        print(ch.recv())               # workers' strings move into main's heap here

main()
```

Reaching the dedent guarantees every spawned child has finished — no `WaitGroup`, no `Done()` to
forget, no leak. Child errors propagate to the block (composes with `recover:` + `defer`).

### Why a nursery, not a `WaitGroup`

`WaitGroup` is the thing Go regrets: manual (`Add`/`Done`/`Wait` — forget one and you leak or
deadlock) and **unstructured** (a goroutine's lifetime isn't tied to any scope, so it can outlive the
function that spawned it). The nursery *is* the join — no counter to mismanage — and a task **cannot
outlive its `parallel:` block**, so leaks are structurally impossible and errors have an obvious home.
This is **structured concurrency**, the modern consensus that postdates Go: Python `trio` /
`asyncio.TaskGroup`, Kotlin `coroutineScope`, Swift `TaskGroup`, Java `StructuredTaskScope`.

### Nursery scoping rules

- **`spawn` is only legal inside an enclosing `parallel:`.** A bare `spawn` (top level or in a
  function with no nursery) is a **checker error**: *"spawn must be inside a parallel: block."*
- **Function-boundary rule (the safety invariant).** A `spawn` binds to a nursery **within its own
  function** — it can **never** reach an enclosing function's or the module's nursery. This is what
  stops a task spawned in `fn worker()` from outliving `worker`'s return:

  ```chezzi
  fn worker():
      spawn helper()       # ✗ error: no parallel: in THIS function. (Even if main() has one,
                           #    helper must not outlive worker — open a parallel: here.)
  ```
  Both Java `StructuredTaskScope` and Elixir guarantee a task can't outlive its scope; this rule
  gives Chezzi the same.

- **"Background" = an outer nursery.** A longer-lived task is one spawned into a *higher* `parallel:`
  that spans the lifetime you want — not an unscoped task:

  ```chezzi
  parallel:                        # app-lifetime nursery
      spawn logger(ch)             # "background": lives as long as THIS block
      parallel:                    # short-lived inner work
          spawn worker(1)
          spawn worker(2)
      # inner joins here; logger still alive
      handle_requests()
  # outer dedent: logger joins here
  ```
  Truly *detached* daemons are a separate, later construct — see [§8](#8-daemon--background-tasks).

---

## 4. Execution semantics

### Staged executor

The **C1–C4 executor is sequential, run-to-completion** — no scheduler, no OS threads, no `Send`.
It is chosen so the full surface, channels, `Shared`, type-checking, and both engines ship *now*; the
hard multicore work is isolated behind the executor seam ([§9 C5](#9-implementation-roadmap-c1c5)) and
**does not change the surface** when it lands.

How a `parallel:` block runs:

1. Executing `spawn X` **evaluates the call's callee + arguments eagerly, at the spawn point** (Go's
   arg-evaluation timing), **deep-copies them across the airlock** ([§7](#7-sendability)), and
   **registers** the task on the innermost nursery. **The parent then continues to the next statement
   immediately** — `spawn` does not block.
2. At the nursery **dedent (the barrier)**, the registered tasks **run to completion in FIFO order**.
3. The first task to error **aborts the remaining siblings** and propagates out of the `parallel:`
   statement (composes with `recover:` / `defer`).
4. The parent proceeds past the block.

```chezzi
parallel:
    spawn worker(1, ch)    # registered; parent continues
    spawn worker(2, ch)    # registered; parent continues
    print("both spawned")  # runs NOW — parent didn't block on spawn
# barrier: worker 1 then worker 2 run to completion here
print("all done")
```

### Documented consequence (sequential only)

Because tasks run *at the barrier*, statements after a `spawn` **inside** the block run **before** the
spawned bodies, and tasks **do not interleave** — they run one after another, FIFO. This is the
deterministic sequential approximation of concurrency. It is correct for **fan-out / collect** (spawn
N workers, read their results after the block — the common 80% case). It is **not** enough for tasks
that must communicate *mid-flight* (a producer a live consumer waits on): under run-to-completion the
consumer's `recv` can never be satisfied, so it is a **deadlock-detect error**, not a hang. Real
interleaving and mid-flight communication arrive with **C5** — same syntax, no surface change.

---

## 5. `Channel[T]` — a mailbox outside every heap

A `Channel[T]` is **not** an object in any task's heap. It is a separate runtime structure — a
**mailbox** with its own queue — that sits outside both heaps. What each task *holds* is a lightweight
**handle**; the handle is the only shared thing, and values flowing through are **moved/copied** at
the airlock, never live in two heaps at once.

```chezzi
ch := Channel[str]()       # construct; capitalized like Shared[T] / Option[T]
ch.send(x)                 # x moved/copied OUT of the sender's heap → channel queue
v := ch.recv()             # value reconstructed IN the receiver's heap
n := ch.len()              # current queued count
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `send` | `send(self, v: T) -> nil` | enqueue (move/copy at the airlock); sender can't reuse a moved value |
| `recv` | `recv(self) -> T` | dequeue (FIFO); blocking surface (see below) |
| `len`  | `len(self) -> int` | queued count — use to guard a `recv` |

- **Buffered (unbounded) FIFO** under the sequential executor, so `send` never blocks.
- **`recv` on an empty channel** is a **deadlock-detect RuntimeError** under C1–C4 (*"recv would block
  forever — sequential executor; real blocking arrives in C5"*), preserving the C5 blocking surface.
  In the fan-out pattern (workers `send` during the block, main `recv`s after the dedent) the queue is
  already full, so `recv` succeeds — guard with `len()` if unsure.
- **Move-on-send** = Go's O(1) send cost without Go's sharing (the sender can't touch the value after
  — the checker enforces it, like a Rust channel). Deep-copy is the fallback when the sender wants to
  keep its copy.
- **Channels are themselves sendable** — pass a `Channel` over a `Channel` for reply channels.
- *(Candidate, pin when C2 is built:* a non-blocking `try_recv() -> Option[T]` alongside the blocking
  `recv`. The doc records both; `recv -> T` is primary for C5 fidelity.)*

---

## 6. `Shared[T]` — the cross-task mutable box

Captured values are **copies** ([§7](#7-sendability)), so a task can't mutate the parent's state. When
you genuinely need shared mutable state *across* tasks, the sanctioned answer is **`Shared[T]`** — a
box whose writes are serialised by a single owner (Elixir's `Agent` trick), so the torn-write race is
unrepresentable.

```chezzi
s := Shared(0)             # one owner holds the value
parallel:
    spawn bump(s)          # the HANDLE is copied in — both reach the same owner
    spawn bump(s)
print(s.get())             # 2

fn bump(s: Shared[int]):
    s.update(fn(x): x + 1)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `get`    | `get(self) -> T` | read (a request/reply under real concurrency) |
| `set`    | `set(self, v: T) -> nil` | overwrite |
| `update` | `update(self, f: fn(T) -> T) -> nil` | read-modify-write, serialised by the owner |

**The ladder:** bare value (copied) → `Ref[T]` (in-task box, `std/ref.chz`) → `Shared[T]` (cross-task
box). One `get`/`set`/`update` API across the two boxes; the **type names the scope**. `Ref[T]` is
**not** sendable (copy-on-spawn gives each task its own box); `Shared[T]` **is** (the handle is
copied, the value isn't). Naming: `Ref` (a box *here*) ↔ `Shared` (a box other tasks can reach too).

Under the sequential executor a single thread already serialises every write, so `Shared` is correct
from C3 with no locking; under C5 it becomes a real owner-task + channel, same API.

---

## 7. Sendability

Crossing a task boundary (a `spawn` capture or a `Channel.send`) is gated on **sendability**, and
captured bindings are **read-only inside the task**.

- **Sendable:** scalars (`int`/`float`/`bool`), `str`, containers + structs whose contents are all
  sendable, **`Channel`** itself (reply channels), and a **`Shared[T]`** handle.
- **Not sendable:** closures (bound to a heap), native handles (file/regex/HTTP `Response`/etc.), and
  **`Ref[T]`** (an in-task-only box — copied on spawn, so each task gets its own independent box).
- **Read-only captures:** reassigning a captured binding inside a task body is a **compile error** —
  so the copy semantics are obvious: read captured config freely, but produce output only via a
  `Channel` or a `Shared`. The checker gates capture and `send`, **with the fix in the error message**.

```chezzi
fn worker(id: int, prefix: str, out: Channel[str]):
    out.send("{prefix}-{id}")   # ✓ id/prefix copied & read; result leaves via the Channel
```

> **One-sentence mental model:** a spawned task gets its **own copies** of what it captures
> (read-only), holds **shared handles** to channels and `Shared` boxes, and talks **only** through
> those — which move/copy values between isolated heaps.

---

## 8. Daemon / background tasks

A "background" task in C1–C4 is just one spawned into a **longer-lived outer nursery**
([§3](#3-surface-syntax)). True *detached / fire-and-forget daemons* are a **separate, later (C5)
construct** — deliberately **not** "`spawn` without a nursery."

This is research-backed. Both major structured-concurrency ecosystems **forbid truly-unscoped tasks**:

- **Java (Loom / `StructuredTaskScope`):** a subtask's lifetime must not exceed its scope; all subtask
  threads are guaranteed terminated at scope close. The explicit guidance: if a task **outlives** the
  scope, structured concurrency is the *wrong tool* — use a **separate executor / work queue**.
- **Elixir / BEAM:** `Task.async` (linked + awaited = the nursery analog) is separated from
  `Task.start` / `Task.Supervisor.async_nolink` (unlinked, side-effect-only daemons) — and even those
  daemons live under an explicit **supervision tree** (default restart `:temporary`), not floating
  free.

**Conclusion for Chezzi.** A daemon is a **separate explicit construct attached to a root /
app-lifetime scope** that the runtime owns and reaps at program exit (≈ Java daemon threads / Elixir
supervised tasks) — Go's float-free `go` is the model both ecosystems *rejected*. Two corollaries:

- **The implicit / top-level scope must JOIN, never cancel.** Cancel-at-exit would mean a still-pending
  `spawn`'d task **silently never runs** — a terrible default. Cancel-at-exit is *daemon* semantics and
  belongs only to the explicit daemon construct.
- **Defer to C5.** Under the sequential executor a daemon can't run in the background anyway, and the
  correct daemon design needs the root-scope/supervisor machinery that only exists once C5 brings real
  concurrency. The shape is pinned here so it isn't reinvented as unscoped `spawn`.

### The escape hatch (C5): `Executor` — a separately-owned work queue

> **Status (shipped, sequential subset — both engines):** `Executor()` + `submit` / `shutdown` /
> `shutdown_now` run on the sequential executor today. `submit` enqueues; `shutdown` drains the queue
> FIFO to completion at the reap point (the first task to fault aborts the rest and propagates, like a
> nursery), leaving any not-yet-run siblings in place for a later reap; `shutdown_now` discards
> pending work; `submit` after either is a fault. Reap with `defer ex.shutdown()` as shown.
> **Program-exit auto-drain now ships too (both engines):** an executor never explicitly
> `shutdown`/`shutdown_now`-ed is gracefully drained at a clean program exit (a per-engine executor
> registry that doubles as a GC root reaps each live executor FIFO in creation order — its submitted
> work runs instead of silently vanishing). A hard `std.os.exit` skips it (consistent with how it
> skips `defer`); a faulting program is not auto-drained (it is already erroring).
> **One piece is still deferred to real-C5:** **sendability-gating of the submitted closure's
> captures** — `submit` takes the closure by handle and runs it in-heap at the drain (consistent with
> `Shared.update` / list HOFs), so a non-sendable capture is benign now and the gate lands with real
> parallelism.

The sanctioned tool for "a task that **outlives its scope** / runs in the background" is **not** a
nursery and **not** an unscoped `spawn` — it is a distinct, **explicitly-owned `Executor`**: a
long-lived task pool / work queue you create, submit detached work to, and reap yourself. This is
precisely Java's "use a separate executor / work queue" and Elixir's `Task.Supervisor`. It keeps
`parallel:` pure (always structured, always joins) and confines all "outlives-its-scope" work to one
visibly-owned place.

```chezzi
fn main():
    ex := Executor()                  # a long-lived, explicitly-owned task pool
    defer ex.shutdown()               # lifetime tied to a scope YOU pick — graceful reap on every exit path

    ex.submit(fn(): logger(ch))       # detached, side-effect-only; does NOT join any parallel: dedent

    parallel:                         # structured work runs and joins as normal...
        spawn worker(1, ch)
        spawn worker(2, ch)
    # inner nursery joined here; the submitted logger is still running

    handle_requests()
# ex.shutdown() runs here (via defer): the executor is reaped deterministically
```

| Method | Behaviour |
|--------|-----------|
| `submit(f)` | enqueue a detached, side-effect-only task (results leave via a `Channel`, like `spawn`); returns immediately |
| `shutdown()` | **graceful** — stop accepting new work, **await** submitted work to drain, then reap |
| `shutdown_now()` | **cancel** pending work and reap immediately (Java `shutdownNow`) |

- **`defer` is the lifetime knob.** A task "persists through scopes" because its *owner* — the
  `Executor` — does. Bind that owner's reaping to any scope with `defer ex.shutdown()` (a function, a
  `recover:` block, the module top level); `defer`'s all-exit-paths guarantee then reaps it on
  fall-through, `?`, `break`/`continue`, return, or panic. The task may outlive inner `parallel:`
  blocks, but it is **still deterministically reaped** — the leak becomes *your explicit, scoped
  decision*, never an accident.
- **Program exit ⇒ graceful shutdown** of any `Executor` not already shut down (submitted work
  drains; matches `defer`-at-top-level semantics). `std.os.exit` is still a hard halt and does **not**
  drain (consistent with how it skips `defer`).
- **Still no floating tasks.** Even fire-and-forget work has a definite owner and a definite reap
  point — the safety property the whole model rests on is preserved. Submission is gated on the same
  **sendability** rules as a `spawn` capture ([§7](#7-sendability)).
- **Submitted work is unstructured *by design*** — that's the trade for "outlives its scope." Reach
  for `parallel:` first; use an `Executor` only when a task genuinely must outlive the block that
  starts it. (Restart/supervision policies à la Elixir are explicitly **out of scope** for C5 — an
  `Executor` runs tasks and reaps them; it does not restart them.)

---

## 9. Implementation roadmap (C1–C5)

C1–C4 deliver a complete, shippable, deterministic concurrency feature on the sequential executor; C5
is the deferrable multicore upgrade. Each milestone is **TDD**: failing tests first (unit + corpus),
then implement **lexer → grammar/conformance → AST → parser → checker → engine → tests/examples**;
`cargo test` + `cargo test conformance` + `cargo clippy` green; update `PROGRESS.md`; commit. `chan`
internal Rust identifiers may abbreviate, but the **surface type is `Channel`**.

### C1 — surface + nursery + sequential executor (interp)
Ships join semantics with side-effecting tasks (e.g. `print`). No channels yet.
- **Lexer** `src/lexer/mod.rs`: `Token::Spawn`, `Token::Parallel`; map in `keyword()` (~L134-163).
- **Conformance** `src/conformance.rs`: `symbol()` → `"SPAWN"` / `"PARALLEL"` (exhaustive — required to compile).
- **Grammar** `docs/grammar.bnf`: `<compoundStmt>` += `<parallelStmt>`; `<spawnStmt>` (both forms).
- **AST** `src/ast/mod.rs` (`StmtKind`): `Parallel { body: Block }`, `Spawn(SpawnTarget)` where
  `SpawnTarget = Call(Expr) | Block(Block)`.
- **Parser** `src/parser/mod.rs`: dispatch in `parse_stmt`; `parse_parallel` (reuse `parse_block`);
  `parse_spawn` — `spawn:` → block form, else a call expr (reject a non-call form-1 with a clear
  message, mirror `defer`).
- **Checker** `src/checker/mod.rs` (`check_stmt`): a nursery-depth counter — `Parallel` enters,
  `Spawn` at depth 0 errors *"spawn must be inside a parallel: block"*; form-1 target must be a call.
- **Interp** `src/interp/mod.rs`: `Interp.nurseries: Vec<Vec<Task>>`; `Task = Call { callee, args } |
  Block { body, scope }`. `Parallel` → push a list, run the body, pop, run tasks FIFO (reuse the
  re-entrant call path), first `Err` stops siblings + propagates. `Spawn` → eval callee+args (form 1)
  or snapshot the captured scope (form 2) **through `deep_clone`**, push the task. Add
  `deep_clone(&Value)`: scalars/str trivial; list/map/set/struct/enum recursively cloned (fresh
  `Rc<RefCell>`); `Channel`/`Shared` pass by handle; closures/native → error (not sendable).
- **Tests:** parser unit; checker (spawn-outside-parallel rejected, nested parallel ok);
  `tests/corpus/accept/parallel_basic.chz`; `examples/parallel.chz` + `.expected`.

### C2 — `Channel[T]` + sendability (interp)
Ships the canonical worker/fan-out example.
- **Checker types** `src/checker/ty.rs`: `Ty::Channel(Box<Ty>)` + helper + `compatible()`; map
  `Type::Generic("Channel", [T])` → `Ty::Channel`.
- **Checker methods** `src/checker/mod.rs` (`infer_method_call`): `Ty::Channel` arm +
  `channel_method_sig` (`send(T)->nil`, `recv()->T`, `len()->int`); `Channel()` constructor (builtin
  free fn, mirror `set()`).
- **Sendability:** a `sendable(&Ty)` predicate gating `spawn` captures + `Channel.send`; read-only
  captured bindings (reassign of a captured name inside a task = error).
- **Interp** `src/interp/value.rs`: `Value::Channel(Rc<RefCell<VecDeque<Value>>>)` + `type_name` /
  `Display`. `src/interp/mod.rs`: `eval_channel_method` (send = push_back + `deep_clone`; recv =
  pop_front else deadlock RuntimeError; len). `Channel()` constructor in builtins.
- **Tests:** non-sendable capture rejected; reassign-captured rejected; empty-`recv` errors; worker
  example golden.

### C3 — `Shared[T]` (interp)
- **Checker** `ty.rs`: `Ty::Shared(Box<Ty>)`; methods `get()->T`, `set(T)->nil`,
  `update(fn(T)->T)->nil`; `Shared(v)` constructor → `Ty::Shared(typeof v)`; `Shared` is sendable.
- **Interp** `value.rs`: `Value::Shared(Rc<RefCell<Value>>)`; `eval_shared_method`; `Shared()`
  constructor; passed by handle in `deep_clone`.
- **Tests:** cross-task increment via `Shared`; `Ref` is **not** sendable while `Shared` is.

### C4 — VM parity
Port C1–C3 to the bytecode engine (`src/vm`, `src/compiler`) — the standing parity invariant.
- **Heap** `src/vm/heap.rs`: `Obj::Channel(VecDeque<Value>)`, `Obj::Shared(Value)`; `children()` GC
  tracing.
- **Ops** `src/vm/op.rs`: `EnterNursery`, `SpawnCall(argc)`, `SpawnBlock(ProtoId)`, `JoinNursery`,
  `NewChannel`, `NewShared`.
- **Compiler** `src/compiler/mod.rs`: `Parallel` → `EnterNursery` … body … `JoinNursery`. `Spawn`
  form-1 → eval args + `SpawnCall`; form-2 → compile the block as a synthetic zero-arg proto, emit
  `SpawnBlock(proto)` (sidesteps the single-expr closure limit). `Channel()` / `Shared()` → `NewChannel`
  / `NewShared`.
- **VM** `src/vm/mod.rs`: a nursery stack `Vec<Vec<PendingCall>>`; `SpawnCall` / `SpawnBlock` register
  (deep-copy via heap clone); `JoinNursery` runs each pending task to completion using the **existing
  re-entrant call path** (the same one list HOFs `map`/`filter` use to call back into Chezzi).
  `core_method` arms for `Obj::Channel` / `Obj::Shared`; a VM `deep_clone` over heap objects.
- **Tests:** every C1–C3 example runs identically under the VM (default) and `--interp` — add a
  differential parity assertion to the golden harness.

### C5 — what's left, divided

C5 splits into **Group A** (small refinements that work on today's *sequential* executor — no engine
rewrite) and **Group B** (the real concurrency engine — a multi-session epic). Group A is independent
and shippable now; Group B is gated on **B1**. The surface of `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` is **unchanged** throughout.

**Group A — sequential refinements**

| # | Item | Status |
|---|------|--------|
| **A2** | `Executor` **program-exit auto-drain** — reap any executor never explicitly `shutdown`-ed at a clean exit (per-engine registry that doubles as a GC root; FIFO creation order; `os.exit` skips it; a faulting program is not drained). | ✅ **done, both engines** (see [§8](#the-escape-hatch-c5-executor--a-separately-owned-work-queue)) |
| **A3a** | Reject a non-sendable **read through a nested closure** inside a `spawn:` block. | ✅ **already enforced** — emergent from the persistent `capture_floors` + the `infer_ident` read gate; pinned by a regression test (`read_captured_closure_through_nested_closure_in_spawn_block_rejected`). |

> *Dropped from Group A:* **A1** (`Channel.try_recv() -> T?`) — a non-blocking poll whose motivating
> scenario (a live mid-flight producer) is unreachable under run-to-completion; a primitive awaiting
> the engine. **A3b** (`Executor.submit` capture sendability gate) — `submit` runs the closure
> in-heap at the drain, so a non-sendable capture is *benign today*; gating it now would wrongly
> reject valid programs. Both belong with Group B.

**Group B — the real engine (deferred epic)**

| # | Item |
|---|------|
| **B1** | **Suspendable execution** — rewrite the recursive `eval` / VM loop into a resumable form. The fiber core; **everything else in B gates on it**. |
| **B2** | **Cooperative scheduler** — task interleaving; real **blocking `recv`** (replaces the deadlock-detect fault); mid-flight producer↔consumer. |
| **B3** | **Tier-C OS-thread multicore** — per-thread heap + GC; true parallelism. An *alternative bet* to B1/B2. |
| **B4** | **Real `Shared[T]`** — owner-task + channel (today single-thread-serialised, already correct). |
| **B5** | **Real `Executor` background pool** — actually-backgrounded tasks + graceful exit drain under real concurrency; plus A3b (submit-capture gating). |

**Dependency:** Group A is independent and shippable; Group B is gated on **B1**. A2 is unchanged
after B lands; A3a becomes load-bearing (not merely emergent) once captures truly cross threads.

---

## 10. Future evolution

- **M-C — implicit nurseries (ergonomic sugar, deferred).** Today's model (**M-A**) requires every
  `spawn` to sit inside an explicit `parallel:`, *including* top level — chosen because under the
  sequential executor the dedent is *where tasks run*, so keeping it explicit keeps the run-barrier
  visible. A later evolution (**M-C**) could make **every function body an implicit nursery** that
  joins at its `return`/end (the module top level joins at program exit), demoting `parallel:` to an
  explicit *inner* sub-nursery for earlier joins. Ergonomic ("spawn anywhere in a function"), uniform
  (no top-level/function asymmetry), and still safe via the function-boundary rule. Deferred because an
  invisible "joins at end of function" barrier hides *when* work runs — which matters precisely while
  execution is observable-sequential. Revisit after C5.
- **Real concurrency (C5):** the Tier-A cooperative scheduler and/or Tier-C OS threads — true
  multicore and mid-flight task communication, behind the unchanged surface.
- **The `Executor` escape hatch (C5):** the separately-owned work queue for tasks that must outlive
  their scope — `submit` detached work, reap with `defer ex.shutdown()` (graceful) or `shutdown_now()`
  (cancel); program exit drains. Keeps `parallel:` pure and all background work visibly owned —
  see [§8](#8-daemon--background-tasks).
- **Reuse map for the implementer:** builtin method dispatch (`list_method`/`map_method` —
  `src/interp/builtins.rs`, `src/interp/mod.rs`; VM `core_method` — `src/vm/mod.rs`); parameterized
  types (`Type::Generic` — `src/ast/mod.rs`; `Ty::List/Map/Set` — `src/checker/ty.rs`;
  `infer_method_call` — `src/checker/mod.rs`); the re-entrant call-into-Chezzi path (list HOFs) for
  `JoinNursery`; block parsing/scoping (`parse_block`, `exec_scoped_block`/`exec_block`, defer-scope
  markers); `Option` constructors (`some`/`none`, `alloc_enum`) for `recv`; `Ref[T]` (`std/ref.chz`)
  as the `Shared[T]` template.
