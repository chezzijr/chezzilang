# Chezzi — Concurrency & Parallelism (`spawn` / `parallel:`)

> **Status:** canonical design doc — **implemented through Tier-D**. The surface (`spawn`,
> `parallel:`, `Channel`/`Shared`/`Executor`) and both engines ship; `--parallel` is a real OS-thread
> M:N work-stealing scheduler with reduction-counting preemption, a dirty/blocking pool, and an
> epoll/kqueue netpoller behind non-blocking `std.net`. Phase history: [`concurrency-tier-d.md`](concurrency-tier-d.md)
> (D0–D6c) + [`concurrency-b3.md`](concurrency-b3.md) (the B3 OS-thread foundation). **M-C implicit
> nurseries shipped** (§10) — every function body and the module top level is an implicit nursery that
> joins at its `return`/end, so a bare `spawn` is legal anywhere. `PROGRESS.md` is the live status tracker.
>
> The syntax was fixed up front and the *engine* shipped staged (a sequential executor first, real
> multicore later) so the surface never changed as concurrency got teeth.

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

- **`spawn` is legal anywhere in a function body or at the module top level** (M-C, §10). Every
  function body and the module top level is an *implicit nursery*; a bare `spawn` binds to it and
  joins at the body's `return`/end (the module top level joins at program exit). An explicit
  `parallel:` is an inner sub-nursery that joins earlier, at its dedent.
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
opt := ch.try_recv()       # non-blocking poll: Some(v) if queued, None if empty
n := ch.len()              # current queued count
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `send` | `send(self, v: T) -> nil` | enqueue (move/copy at the airlock); sender can't reuse a moved value |
| `recv` | `recv(self) -> T` | dequeue (FIFO); blocking surface (see below) |
| `try_recv` | `try_recv(self) -> T?` | **non-blocking** poll (A1): `Some(v)` if queued, `None` if empty — never blocks, never faults, never suspends a fiber. Drain a mailbox without guarding on `len()` |
| `recv_timeout` | `recv_timeout(self, ms: int) -> T?` | **bounded wait**: `Some(v)` if a value arrives, `None` on timeout. **Total — never faults** (a timeout, a closed-empty channel, and an unproducible empty channel are all `None`). Honors `ms` as real wall-clock only on `--parallel`; the cooperative VM + interpreter (no clock) poll-or-resolve-to-`None`. `ms <= 0` polls once |
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
- **`recv_timeout(ms) -> T?` is shipped (both engines).** A bounded-wait `recv` that is *total* —
  it never faults. On `--parallel` it demote-blocks with a deadline (the channel condvar +
  queue-pop-under-lock is the atomic send-vs-deadline arbiter): a sibling `send` → `Some(v)`, the
  deadline → `None`, whichever fires first. On the cooperative VM and the frozen interpreter (no
  wall-clock) it polls-or-resolves: a queued value → `Some(v)`, an empty channel with no producer →
  `None` (NOT the deadlock fault `recv` raises). The parity-safe shapes (value already queued; empty
  at top level → timeout) are byte-identical across all three engines (see `examples/recv_timeout.chz`);
  a concurrent mid-flight producer is `--parallel`-only, like a blocking `recv`. **Known v1 cost:** a
  `recv_timeout` that actually blocks on `--parallel` demotes its worker + spins one replacement
  (reaped by the blocking pool) — acceptable for an "I'm willing to wait" op; the full park+timer
  variant is a possible follow-up.
- **`try_recv() -> T?` is shipped (A1, both engines).** The non-blocking sibling of `recv`: it
  pops-or-returns-`None` and never blocks, faults, or suspends a fiber — so it is identical under the
  sequential interpreter and the VM (parity-tested). With B1/B2's blocking `recv` on the VM, a fiber
  can also drain a mailbox's residue after a blocking `recv` resumes. `recv -> T` stays primary; reach
  for `try_recv` to poll without guarding on `len()`.

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

> **B3's detailed execution plan lives in [`concurrency-b3.md`](concurrency-b3.md)** — a phased,
> multi-session breakdown (B3.0…B3.6) with the validated shared-nothing architecture, decisions, risk
> register, and per-phase TDD focus. Items deliberately *not* in B3–B5 are in
> [§11 Deferred / backlog](#11-deferred--backlog-not-b3b5).

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
- **Checker** `src/checker/mod.rs` (`check_stmt`): `Spawn` is legal anywhere (M-C) — the implicit
  function/module nursery always provides a binding target; form-1 target must be a call, and the
  sendability/airlock checks on the receiver + args still apply.
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
| **A1** | `Channel.try_recv() -> T?` — a **non-blocking poll** (`Some(v)`/`None`, never blocks/faults/suspends). Originally deferred (its motivating mid-flight-producer scenario needed the engine), un-deferred once B1/B2 landed. | ✅ **done, both engines, parity-tested** (it never suspends, so the interp runs it identically — see [§5](#5-channelt--a-mailbox-outside-every-heap)). |

> *Dropped from Group A:* **A3b** (`Executor.submit` capture sendability gate) — `submit` runs the
> closure in-heap at the drain, so a non-sendable capture is *benign today*; gating it now would
> wrongly reject valid programs. It belongs with Group B.

**Group B — the real engine (deferred epic)**

| # | Item | Status |
|---|------|--------|
| **B1** | **Suspendable execution** — make the engine loop resumable. The fiber core; **everything else in B gates on it**. | ✅ **VM done** · 🚫 interp (non-goal) |
| **B2** | **Cooperative scheduler** — task interleaving; real **blocking `recv`** (replaces the deadlock-detect fault); mid-flight producer↔consumer. | ✅ **VM done** · 🚫 interp (non-goal) |
| **B3** | **Tier-C OS-thread multicore** — per-thread heap + GC; true parallelism. An *alternative bet* to B1/B2 (the one taken). | ✅ **VM done** (B3.0–B3.6; superseded by Tier-D D1/D2's M:N fibers) |
| **B4** | **Real `Shared[T]`** — owner-task + channel (today single-thread-serialised, already correct). | ✅ **VM done** (folded into B3.1/B3.4) |
| **B5** | **Real `Executor` background pool** — actually-backgrounded tasks + graceful exit drain under real concurrency; plus A3b (submit-capture gating). | ✅ **VM done** (B3.6 + A3b) |

**Dependency:** Group A is independent and shippable; Group B is gated on **B1**. A2 is unchanged
after B lands; A3a becomes load-bearing (not merely emergent) once captures truly cross threads.

#### B1 + B2 as shipped on the VM (cooperative fibers + blocking `recv`)

Rather than the full recursive-`eval` rewrite, the **bytecode VM** got suspendable execution cheaply
because `run_until(base_level)` is **frame-count driven** (`while frames.len() > base_level`), not
host-recursion driven: a fiber's saved frame stack *replays* on resume via ordinary `Return` opcodes —
no host call stack to rebuild. The design (all in `src/vm/mod.rs`):

- **Suspend = rewind-and-retry at an instruction boundary.** A `recv` on an empty channel, under an
  active scheduler and outside any native callback, re-pushes the receiver, does `ip -= 1` so the
  `CallMethod(recv)` re-executes on resume, and sets `self.suspend`. `run_until` and every re-entrant
  call site (`run_proto`, `do_call`, the struct-method / function-field / channel dispatch) break out
  **without** running defers, returning control to the scheduler. No mid-instruction state is saved.
- **Nursery-local cooperative scheduler.** `JoinNursery` parks the joining (parent) fiber's context
  and runs the spawned tasks as child `Fiber`s, each owning a full `FiberCtx`
  (`frames`/`stack`/`call_depth`/`cur_base`/`handlers`/`nurseries`/`fault_trace`) swapped in/out around
  scheduling. A child that never blocks runs to completion FIFO (so non-blocking programs are
  byte-identical to the old sequential drain); a blocked child parks and a runnable sibling runs; a
  sibling's `send` makes it runnable again. All-blocked-none-runnable ⇒ deadlock fault. Nested
  `parallel:` recurses into a fresh scheduler level. Parked fibers are GC roots.
- **Native-reentry guard.** A `recv` reached inside a native callback (list HOFs, `sort`,
  `compare`/`hash`/`str` hooks, `Shared.update`, the executor drain, a `defer`red call) cannot park —
  that loop/recursion state lives on the host stack — so it faults `deadlock` instead (v1 limitation).
- **`std.os.exit` in a child** aborts its siblings and the program (rides the existing `pending_exit`
  hard-halt path, which stays VM-global, not per-fiber).

**Decision — interp B1/B2 is a deliberate NON-GOAL (do not build it).** The tree-walking interpreter
stays frozen at the **sequential concurrency subset** and serves as the **differential-testing parity
oracle** for the non-blocking language surface — its real value is catching VM / GC / compiler bugs,
not running concurrent workloads. Suspendable execution would require stackful coroutines or a full
CPS rewrite of `eval`: a large, risky cost to cover a narrow slice the oracle does not need. **The VM
is the sole concurrent engine.** This makes the parity contract *narrowed by design*: the engines
agree on the sequential subset (including all non-blocking `parallel:`/`spawn`/`Channel`/`Shared`/
`Executor` programs, byte-identical, parity-tested), while a **blocking `recv` is VM-only** — under
`--interp` it faults `deadlock` (pinned: `interp::tests::channel_block_chz_faults_deadlock_on_interp`
vs the VM golden `golden_channel_block_chz_matches_expected`). This is the stated contract, not a bug
to fix. Note: **A1** (`Channel.try_recv`) shipped on **both** engines after all — being *non-blocking*
it never suspends, so the interp runs it identically and it stays parity-tested (it is not gated on the
blocking-`recv` divergence).

**Landed (VM):** **B3** OS-thread multicore (the alternative bet, taken — B3.0–B3.6), **B4** real
`Shared`, **B5** real `Executor` pool (+ A3b) — then **Tier-D** rebuilt `--parallel` as an M:N
work-stealing scheduler (D0–D5 + owes #1/#2). **Still open:** Tier-D **D6** (epoll/`std.net`) and the
VM v1 limits to lift if wanted — blocking `recv` inside a native callback (**D5 owe #3**, resolution
strategy in [`concurrency-tier-d.md`](concurrency-tier-d.md)) and cross-nursery wakeups (a fiber in an
outer nursery is not woken by an inner one).

---

## 10. Future evolution

- **M-C — implicit nurseries (shipped).** The original model (**M-A**) required every `spawn` to sit
  inside an explicit `parallel:`, *including* top level. **M-C** makes **every function body (and the
  module top level) an implicit nursery** that joins at its `return`/end (the module top level joins
  at program exit), demoting `parallel:` to an explicit *inner* sub-nursery for earlier joins. A bare
  `spawn` is now legal anywhere in a function; it is ergonomic ("spawn anywhere"), uniform (no
  top-level/function asymmetry), and still safe via the function-boundary rule (a task can't outlive
  the function that spawned it).
  - **Join semantics.** `return <value>`, fall-through end, and a `?` early-return are all **join
    points**: the function's spawned tasks run to completion FIFO, *then* control leaves. An explicit
    inner `parallel:` joins earlier at its dedent; a `return`/`?` that *escapes* an inner `parallel:`
    still cancels-and-reports that inner nursery (unchanged) while joining the function's implicit one.
    An uncaught **fault** propagating out of a body cancels-and-reports the implicit nursery's
    unstarted tasks (abnormal exit, not a join). `defer`s run *after* the implicit join (tasks
    complete, then cleanup). The report is emitted **per nursery** (innermost-first — two stacked
    nurseries print two lines), identically on the VM, the frozen interp, and `--parallel`. The
    **module** top-level nursery is the one exception: an uncaught *top-level* fault leaves it silent
    (it joins only on a clean run to program end). [resolved 2026-06-12 — see PROGRESS.md; previously
    the VM dropped these reports while the interp printed them.]
  - **Zero-overhead gate.** A body gets an implicit nursery only if it lexically contains a bare
    `spawn` (a compile-time pre-scan, `compiler::block_has_bare_spawn`); bodies without one emit
    byte-identical bytecode to pre-M-C. Implemented as a single join site — the compiler emits the
    opening `Op::EnterNursery` and flags the `Proto`; the VM's `do_return` joins for `return`/`?`/end.
    Implementation: `src/{checker,compiler,vm,interp}`; tests in `vm::tests::implicit_nursery_*` +
    `examples/implicit_nursery.chz`.
- **Real concurrency (C5):** the Tier-A cooperative scheduler and/or Tier-C OS threads — true
  multicore and mid-flight task communication, behind the unchanged surface.
- **The `Executor` escape hatch (C5):** the separately-owned work queue for tasks that must outlive
  their scope — `submit` detached work, reap with `defer ex.shutdown()` (graceful) or `shutdown_now()`
  (cancel); program exit drains. Keeps `parallel:` pure and all background work visibly owned —
  see [§8](#8-daemon--background-tasks).
### Tier-D — M:N scheduler + async I/O (post-B3 frontier, not yet scheduled)

B3 *originally* gave **CPU-bound** multicore (`--parallel`: a bounded OS-thread pool, one worker `Vm`
per task, blocking `recv` parking the whole thread on a condvar). **Tier-D D1/D2 have since superseded
that baseline** — `--parallel` is now an M:N work-stealing scheduler of lightweight fibers (own heap,
park by `FiberCtx` snapshot, not by blocking a thread), and **D5 closed the I/O-bound gap** (blocking
natives offload to a dirty pool; `sleep_ms` rides a timer thread) so a blocking call no longer pins a
worker (the **G3** starvation, fixed). The text below records the design as it was reasoned through.

**Two orthogonal axes — don't conflate them.** *Memory model* (how tasks share data) is independent
of *scheduler* (how tasks map to cores + how blocking is handled). Chezzi already has **Erlang/BEAM's
memory model** (own heap per task, message-copy across the boundary, races unrepresentable — §2); what
it lacks is BEAM/Go's *scheduler*. Share-nothing does **not** imply "ignore I/O" — Erlang proves you can
have both. The I/O gap is engine immaturity, not a model constraint.

**You don't classify tasks as I/O vs CPU.** A real scheduler discovers it dynamically by *where a task
blocks*: a task that hits a blocking point is parked there (thread freed); a CPU task runs until it
yields / is preempted / finishes. (The one exception is opaque native calls — Erlang's *dirty
schedulers* need a `dirty_cpu`/`dirty_io` label precisely because the runtime can't see where a NIF
blocks. The Chezzi analogue is the `native_reentry` sites.)

**Two tiers of solution — pick by goal, do NOT default to full M:N:**

- **Goal A — just don't starve on a handful of blocking-I/O tasks.** *No M:N needed.* Either
  **grow-on-stall** (spawn another pool thread when all are blocked — what Go does for syscalls) or a
  **separate elastic "blocking" pool** (keep the CPU pool core-sized; route blocking I/O ops to a second
  growable pool so they never pin a CPU thread). This is Erlang's *async thread pool* / Tokio's
  `spawn_blocking`. No suspendable tasks, no pollset — just two pools + routing. **Recommended first
  step** if/when I/O matters: small, fits the current share-nothing design, removes G3.

- **Goal B — cheap *massive* I/O concurrency (10k connections) + CPU parallelism at once.** *This is
  where you genuinely need M:N:* suspendable tasks parked on a **pollset** (epoll/kqueue via `mio`/
  `polling`), multiplexed over a core-sized thread pool with per-thread run queues + work-stealing.
  Only build this if the workload actually demands that scale.

**Why Chezzi is unusually well-positioned for M:N (the two worst sub-problems are already gone):**

1. **Bytecode VM ⇒ suspend is a data snapshot, not stack magic.** A task's state is explicit data
   (`FiberCtx`: frames/stack/ip), not the C call stack — so park/resume-on-another-thread is moving
   structs, not stackful-coroutine / split-stack / asm context-switch machinery (the bulk of Go's
   runtime). The cooperative engine *already* suspends/resumes at `recv` — it is effectively an **M:1
   scheduler today**; M:N is "run several of those in parallel + work-steal + pollset."
2. **Share-nothing per-thread GC ⇒ no concurrent-GC nightmare.** Each task's heap is private and
   collected independently — no stop-the-world coordination, no cross-thread write barriers, no tri-color
   marking across threads (Go's hardest runtime engineering). The model removes it for free.

**What is genuinely still hard (the real work):**

- **Task cost** — ✅ **addressed (Tier-D D1/D2).** The old per-task full-`Vm` + heap reconstruction
  (the ~2 s in the prime demo) was replaced by lightweight fibers sharing an `Arc`'d module snapshot
  (D1) with the heap swapped into the fiber context (D2a). "The first thing to fix" — fixed.
- **Suspend at *every* yield point, not just `recv`** — including a `recv`/I/O reached **inside a native
  callback** (HOF, `sort`, `Shared.update`, executor drain), whose loop state is on the Rust stack and
  can't be snapshotted. Today the `native_reentry` guard faults instead. **This is D5 owe #3** — its
  resolution strategy (Path A: move HOFs into chezzi `std/iter.chz`, like BEAM's `Enum.map`; Path C:
  Go-`handoffp` thread-demote for the native-lock/hook islands; Path B stackful rejected) is written up
  in [`concurrency-tier-d.md` § "D5 owe #3"](concurrency-tier-d.md). The same fiddly bit Go's runtime
  wrestles with for cgo.
- **Correct lock-free work-stealing + pollset wake-ups + preemption.** Preemption is the *easy* part
  here: reduction-style cooperative yield — check a "should-yield" flag at back-edges, reusing the
  existing `cancel`/`gc_stress` dispatch check sites (Erlang's model). Signal-based preemption (Go 1.14)
  is not needed. Work-stealing + pollset are medium and copyable from prior art (Tokio/Go/Rayon).

**Effort:** a multi-month subsystem, not a weekend — but *tractable*, because the foundations
(suspendable fibers, dispatch-loop check sites, private-heap GC) already exist.

**When `--parallel` can become the default** (and serial demoted to an explicit flag): gated on
(1) ✅ **B3.4 + B3.5 landed** — a deadlocked default fails loudly (cancellation + nursery-local
deadlock detection), not hangs; (2) ✅ **per-task overhead dropped** (D1/D2); (3) a still-open
determinism-contract decision — accept **task-ordered** output (decision F's
flush-on-join) as the default and demote VM==interp parity to a *sequential-subset* contract run under
an explicit `--serial` flag. Serial is **never deleted** — it stays permanently as the deterministic
parity oracle + reproducible-debug engine. Not a date; a checklist.

- **Reuse map for the implementer:** builtin method dispatch (`list_method`/`map_method` —
  `src/interp/builtins.rs`, `src/interp/mod.rs`; VM `core_method` — `src/vm/mod.rs`); parameterized
  types (`Type::Generic` — `src/ast/mod.rs`; `Ty::List/Map/Set` — `src/checker/ty.rs`;
  `infer_method_call` — `src/checker/mod.rs`); the re-entrant call-into-Chezzi path (list HOFs) for
  `JoinNursery`; block parsing/scoping (`parse_block`, `exec_scoped_block`/`exec_block`, defer-scope
  markers); `Option` constructors (`some`/`none`, `alloc_enum`) for `recv`; `Ref[T]` (`std/ref.chz`)
  as the `Shared[T]` template.

**Go vs BEAM — the borrow decision (settled).** Chezzi *already has BEAM's memory model* (private
heap per task, message-copy across the boundary); what it lacks is the *scheduler*. Memory model and
scheduler are orthogonal axes — so take Go's scheduler *mechanics* but BEAM's *preemption + native-call
handling*, which are downstream of share-nothing and strictly simpler for a bytecode VM:

- **From Go:** the G/M/P split + per-P work-stealing run queues (`runnext` + bounded ring + global
  overflow), the `wakep`/spinning-worker wakeup with its StoreLoad barrier, and the netpoller
  (epoll/kqueue) for sockets.
- **From BEAM:** **reduction-counting preemption** instead of Go's signal-based SIGURG — Go needs
  signals only because it runs native code with a *shared GC heap* (stop at any PC, find live
  pointers); a bytecode VM has a natural safepoint every dispatch and share-nothing GC, so neither
  applies. And a **dirty/blocking pool** for opaque blocking native calls (`fs`/`io`/`sleep`) instead
  of Go's syscall handoff — Go's handoff only covers syscalls the runtime itself wraps, not opaque
  user native code.
- **Rejected from Go:** SIGURG async preemption and P-handoff-as-native-call-story (both solve
  native-code + shared-GC problems Chezzi doesn't have). **Deferred from BEAM:** priority classes.

One line: *Go skeleton, BEAM brain — because Chezzi is a share-nothing bytecode VM, which is BEAM's
world.* The full per-mechanism ledger + the phased breakdown **D0…D6** (each independently TDD-able,
D0 = the O(N²) ready-queue fix from §11) live in
**[`concurrency-tier-d.md`](concurrency-tier-d.md)** — the companion to this section the way
[`concurrency-b3.md`](concurrency-b3.md) is to §9. **Status: D0–D5 + owes #1/#2 landed (the `--parallel`
engine is now a true M:N work-stealing scheduler); D6 (epoll/kqueue pollset + `std.net`) next.**

---

## 11. Deferred / backlog (not B3–B5)

Concurrency work that is real but **outside the B3–B5 multicore epic**. Recorded so it isn't lost or
reinvented; none is scheduled. (B3–B5 itself is planned in [`concurrency-b3.md`](concurrency-b3.md).)

- **Cross-nursery wakeups** *(cooperative-engine limit)*. The cooperative scheduler (`run_scheduler` +
  the per-nursery `ready: BTreeSet`; cross-level wake via `wake_on_send`, `src/vm/mod.rs`) wakes only
  within the **innermost** scheduler level, so a fiber blocked in an *outer* nursery is not woken by an
  *inner* nursery's `send` until the inner scheduler completes. The common case (consumer in the outer
  nursery, producer in an inner one that *does* finish) already works — the gap is only a fiber whose
  filler is an outer sibling the inner scheduler can't run, which then faults `deadlock`.
  **Why deferred:** a true fix needs a flat/global scheduler and partly conflicts with
  structured-concurrency scoping (inner should join before outer proceeds). **(Symbol note:** the old
  `pick_runnable` linear scan named in earlier drafts is gone — replaced by D0's `ready`-set.)
  **Correction:** this was once "mooted by B3" on the theory that real OS-thread `recv` blocks the
  thread (no level-local polling) — but **Tier-D D2's M:N transition un-mooted it**: `--parallel` now
  parks fibers by *snapshot* into the level's park set, not by blocking a thread, so the level scoping
  is back. Now a limit on **both** engines; revisit only if it bites real programs. Acceptable v1 limit
  (also noted in `PROGRESS.md`).

- **`recv` inside a native callback** *(now a `--parallel` M:N limit too — D5 owe #3)*. The
  `native_reentry` guard (`src/vm/mod.rs`, `guarded()`) faults `deadlock` when a blocking `recv` is
  reached inside a Rust callback (list HOFs, `sort`, `compare`/`hash`/`str`, `Shared.update`, the
  executor drain, a `defer`red call), because that loop/recursion state lives on the host stack and
  can't be snapshotted into a fiber. **Note (corrected):** this was once "mooted by B3" — true for the
  **B3.3 thread-per-task** model (a callback `recv` just blocked the thread on a condvar) — but **D2's
  M:N transition un-mooted it**: fibers now park by *snapshot*, not by blocking their thread, so a
  native frame breaks the chain again. The guard is **live under `--parallel`** (pinned by
  `d5_blocking_native_in_callback_runs_inline`). **Resolution strategy (A + C; B rejected)** is written
  up in [`concurrency-tier-d.md` § "D5 owe #3"](concurrency-tier-d.md): move the suspendable HOFs into
  chezzi (`std/iter.chz`, like BEAM's `Enum.map` — proven to park) and Go-`handoffp`-demote the fiber
  to a thread for the intrinsically-native islands (`Shared.update`'s lock, hash/compare hooks). An
  accepted v1 limit until then.

- **`Channel.close()` + closed-channel semantics** — **LANDED** (both engines, branch
  `feat/channel-close`). The natural complement to B1/B2's blocking `recv`: a consumer looping past the
  producer's last value used to deadlock-fault. Resolved surface (decided with the user):
  - **`for v in ch:`** — blocking iteration; drains buffered + future values, ends cleanly when
    closed-and-drained (Go's `for v := range ch`). The headline consumer form.
  - **`close()`** — idempotent, wakes every parked/demoted receiver.
  - **`send` after close → faults** `"send on a closed channel"`; **`recv` on closed-and-empty →
    faults** `"receive on a closed channel"` (drains buffered first).
  - **`try_send(v) -> bool`** — the safe partner of `send` (mirrors `try_recv` vs `recv`); `false` =
    closed. Channels are unbounded, so closed is `send`'s only failure → `bool`, not `Option`.
  - `try_recv` unchanged (closed reads as `None`); comprehension-over-channel rejected (use the `for`
    form). Implementation notes in PROGRESS.md + [`concurrency-tier-d.md`](concurrency-tier-d.md).

- **A3b — `Executor.submit` capture sendability gate** *(✅ shipped in B3.6)*.
  `submit` ran the closure in-heap at the drain (no airlock, unlike `spawn`'s deep-clone), so a
  non-sendable capture was *benign under the cooperative engine* — gating it then would have rejected
  valid programs. It became load-bearing once captures truly cross threads, and the gate landed with
  **B3.6** (the `submit` arm pushes a `capture_floor` like `spawn`). See §9 Group B and
  [`concurrency-b3.md` §4 B3.6](concurrency-b3.md#4-phased-breakdown).

- **Cooperative scheduler O(N²) in the per-nursery task count** — ✅ **LANDED as Tier-D D0.** The old
  `pick_runnable` linear-scan-per-turn (lowest-index runnable, O(N²); measured 1k→1.4 ms, 10k→51 ms,
  20k→246 ms, 50k→2.34 s) was replaced by a per-nursery **`ready: BTreeSet`** of runnable child indices
  (lowest-index pop, O(log N) per turn → whole nursery O(N·log N); `src/vm/mod.rs` — `run_scheduler` +
  `Nursery.ready`). Byte-identical scheduling order to the old scan (lowest-index is the contract), so
  all goldens stayed green. (Note: this was always purely the *cooperative default* engine; `--parallel`
  uses the M:N `mn_worker_loop`, never `run_scheduler`. D0 removed the quadratic wall but is orthogonal
  to the Tier-D per-task-cost work that makes fibers green-thread-cheap.)
