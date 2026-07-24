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

Heaps are share-nothing, but the **process** is one process: a task inherits the parent's `std.os.args`
and env, and **stdin is ONE source every task shares** (Go's `os.Stdin` / Python's `sys.stdin`, not
entry-task-owned). Any task may `io.read_line()` / `io.input()`; a line goes to **exactly one** task
(never duplicated, never dropped); **which** task gets it is nondeterministic on both engines — order it
yourself (entry task reads, fans out over a `Channel[str]`), exactly as with concurrent `print`. `None`
means stdin is genuinely exhausted. Details + the v1 core-worker-pinning limit: `docs/stdlib.md` §`std.io`.

### The race you can't write

```chezzi
# Go — compiles, runs, WRONG: 1000 goroutines stomp one int → torn writes, data race
# counter := 0; for i := 0; i < 1000; i++ { go func() { counter++ }() }

# Chezzi — the shared-mutation race is unrepresentable
counter := 0
parallel:
    spawn: counter = counter + 1   # compiles, but each task mutates its OWN isolated copy of
                                    # `counter` — a module global is deep-copied per spawned task at
                                    # the spawn boundary on BOTH engines, so the parent's `counter`
                                    # stays 0 — no shared write, no race.
print(counter)                     # 0  — to actually share, use a Shared[int] (below)
```

**Module globals isolate per task on both engines.** A `spawn`ed task gets its own deep copy of every
module global (and of every captured local) — mutating one inside a task never propagates out, on
`--serial` or the default M:N engine alike (they snapshot identically; `serial == M:N` by construction).
The snapshot is taken **once, at the first nursery**, and reused for every later task and nested nursery
(module globals are effectively frozen thereafter): a mutation by ordinary sequential code *between* two
nurseries, or by a task *before* it opens a nested `parallel:`, is NOT seen by tasks that read the global
afterward — again identical on both engines. (To thread a fresh value into a task, pass it as a spawn
argument or through a `Channel`.)
To actually **share** mutable cross-task state, use `Shared[T]` / `RwShared[T]` / `Atomic[T]` (below) or a
`Channel[T]` — those cross by shared handle, not by copy, so a task-side write IS visible to the parent.

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
ch := Channel[str]()       # construct; capitalized like Shared[T] / Option[T]. Unbounded FIFO.
bch := Channel[int](2)     # BOUNDED: holds ≤2 queued messages; a 3rd `send` blocks until a `recv` frees a slot
ch.send(x)                 # x moved/copied OUT of the sender's heap → channel queue
v := ch.recv()             # value reconstructed IN the receiver's heap
opt := ch.try_recv()       # non-blocking poll: Some(v) if queued, None if empty
n := ch.len()              # current queued count
c := bch.cap()             # capacity: 2 here; 0 for an unbounded Channel[T]()
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `send` | `send(self, v: T) -> nil` | enqueue (move/copy at the airlock); sender can't reuse a moved value. On a **bounded** channel a `send` **blocks/parks** while the queue is at capacity (backpressure), resuming once a `recv` frees a slot — the send-side mirror of a blocking `recv` |
| `try_send` | `try_send(self, v: T) -> bool` | **non-blocking** send: `true` once queued, `false` if the send can't proceed — the channel is **closed**, or a **bounded** channel is **full**. Never blocks/parks |
| `recv` | `recv(self) -> T` | dequeue (FIFO); blocking surface (see below) |
| `try_recv` | `try_recv(self) -> T?` | **non-blocking** poll (A1): `Some(v)` if queued, `None` if empty — never blocks, never faults, never suspends a fiber. Drain a mailbox without guarding on `len()` |
| `len`  | `len(self) -> int` | queued count — use to guard a `recv` |
| `cap`  | `cap(self) -> int` | capacity: the bound passed to `Channel[T](cap)`, or `0` for an unbounded `Channel[T]()` |

- **`Channel[T]()` is an unbounded FIFO** — `send` never blocks. **`Channel[T](cap)`** (`cap > 0`; a
  `cap <= 0` is a runtime fault) is a **bounded** FIFO: a `send` blocks/parks once `cap` messages are
  queued and resumes when a `recv` frees a slot (Go's buffered channel). Backpressure changes *which*
  task runs *when*, never the value sequence a consumer sees, so a bounded channel is byte-identical
  serial vs M:N by the same argument as a blocking `recv`. A full `send` with no possible consumer
  (top level, no nursery, or inside a native callback) is a **deadlock fault**, not a silent over-fill.
  As with `try_recv`, `try_send`'s full-vs-not decision under multi-sender contention is nondeterministic
  — the same class as `try_recv`'s `None`-vs-`Some` under contention; it is not "fixed".
- **`recv` on an empty channel** is a **deadlock-detect RuntimeError** under C1–C4 (*"recv would block
  forever — sequential executor; real blocking arrives in C5"*), preserving the C5 blocking surface.
  In the fan-out pattern (workers `send` during the block, main `recv`s after the dedent) the queue is
  already full, so `recv` succeeds — guard with `len()` if unsure.
- **Move-on-send** = Go's O(1) send cost without Go's sharing (the sender can't touch the value after
  — the checker enforces it, like a Rust channel). Deep-copy is the fallback when the sender wants to
  keep its copy.
- **Channels are themselves sendable** — pass a `Channel` over a `Channel` for reply channels.
- **`try_recv() -> T?` is shipped (A1, both engines).** The non-blocking sibling of `recv`: it
  pops-or-returns-`None` and never blocks, faults, or suspends a fiber — so it is identical under
  both engines (serial `--serial` and default M:N) (parity-tested). With B1/B2's blocking `recv`, a fiber
  can also drain a mailbox's residue after a blocking `recv` resumes. `recv -> T` stays primary; reach
  for `try_recv` to poll without guarding on `len()`.

### 5a. `std.concurrency.pmap` — scoped parallel map (the ergonomic wrapper)

The report-channel + one-`spawn`-per-element + join + reassemble pattern is common enough that
`std.concurrency.pmap` bakes it in (pure Chezzi over a `parallel:` nursery + `Channel`):

- `pmap[T, U](xs, f) -> List[U]` — spawn a task per element, run `f` in parallel, results in
  **submission order**.
- `pmap_limited[T, U](xs, f, limit) -> List[U]` — same, capping in-flight `f`-executions at `limit`
  via a channel-as-semaphore token bucket (also the standard **concurrency limiter**; `limit > 0`).

Determinism/parity comes from reassembling by submission INDEX (`sort_by_key`), never completion
order — so serial `--serial` == M:N byte-for-byte. The nursery lives inside the helper and joins
before the collect (structured concurrency — a task can't outlive the call); `f` crosses the airlock
into each task by value. See `docs/stdlib.md` for signatures.

### 5b. `std.concurrency.task` — result handles for `Executor` work

Bare `Executor.submit(f)` is fire-and-forget — nothing comes back. The result-returning primitive is
`Executor.submit_result[T](f: fn() -> T) -> Channel[T]`: submit `f` and get a cap-1 `Channel[T]` you
`.recv()` for its result after the pool drains. `std.concurrency.task` wraps that channel in a
future-style handle (memoization + readiness poll):

- `submit_task[T](ex, f) -> Task[T]` — submit `f` detached, get a handle (builds over
  `ex.submit_result(f)`). The work runs when `ex` drains (`shutdown()` or exit).
- `Task.get() -> T` — block until the result lands, then return it; **memoized** (idempotent).
- `Task.done() -> bool` — non-blocking readiness poll.

Canonical shape: submit all → `shutdown()` → `.get()` each. **Parity rule:** a task's value is
deterministic (`f()`); only its *timing* varies by engine, so `.get()` is byte-identical serial vs
M:N **as long as you await in a fixed (submission) order**. There is deliberately **no**
`join_next()`/select-on-completion — completion order is nondeterministic and would break parity.

---

## 6. `Shared[T]` — the cross-task mutable box

> **`import std.concurrency`.** `Shared`, `RwShared`, `Atomic`, `AtomicInt`, and `Executor` are **not**
> global builtins — a module must `import std.concurrency` (whole-module licenses all of them) or
> `import Shared from std.concurrency` (per-name) before using them; bare use is an
> `unknown type 'Shared' (import it from std.concurrency: \`import std.concurrency\`)` error. They also
> stay **reserved names** (no user `struct Shared`/`struct Executor`). `Channel` stays global;
> `timer(ms)` now requires **`import std.time`** (whole-module, or `import timer from std.time`) — see
> §6c. (`std.concurrency` is a file-less native module that exists only to license these four names;
> the constructors are lowered by the compiler, so there is zero runtime cost to the import.)

Captured locals **cross into a task as copies** ([§7](#7-sendability)) — same-task capture is by
reference, but the task boundary deep-copies — so a task can't mutate the parent's state. When
you genuinely need shared mutable state *across* tasks, the sanctioned answer is **`Shared[T]`** — a
box whose writes are serialised by a single owner (Elixir's `Agent` trick), so the torn-write race is
unrepresentable.

```chezzi
import std.concurrency

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
| `get`    | `get(self) -> T` | **snapshot copy out** (a request/reply under real concurrency) |
| `set`    | `set(self, v: T) -> nil` | overwrite |
| `update` | `update(self, f: fn(T) -> T) -> nil` | read-modify-write, serialised by the owner |

> **Gotcha — `get()` returns a copy, not the box.** The value lives **off the GC heap** (a
> lock-guarded wire form so it can cross OS threads safely), so `get` deep-copies it *out* into a
> fresh value each call. Mutating that value — `s.get().push(x)`, `s.get().value = 1` — changes a
> throwaway, **not** the box: the write is silently lost. Mutate only via `update` (or `set` a whole
> new value). Same for `RwShared` (`read`/`write`) and `Atomic` (`load`/`store`). This is *unlike* a
> plain in-task `struct`/collection, whose reads alias the live value (push-through works) but which
> can't cross the airlock.

**The ladder:** bare value (copied) → a mutable in-task `struct`/collection → `Shared[T]` (cross-task
box). An in-task mutable value (a one-field `struct`, a `List`, …) is a shared reference *within one
task* but is **not** sendable (copy-on-spawn gives each task its own copy); `Shared[T]` **is** sendable
(the handle is copied, the value isn't) — reach for it for genuine cross-task mutation.

Under the sequential executor a single thread already serialises every write, so `Shared` is correct
from C3 with no locking; under C5 it becomes a real owner-task + channel, same API.

---

## 6b. `Atomic[T]` — the cross-task atomic box

`Atomic[T]` is `Shared[T]`'s sibling for when you want **atomic operation primitives** (compare-and-swap,
exchange, fetch-add) rather than `Shared`'s closure-based `update`. Same shape: one box, many tasks; the
**handle is sendable**, the value is copied in/out under a lock; constructed value-first (`Atomic(v)`,
`T` inferred — an optional `Atomic[T](v)` turbofish pins it and is checked against the value). Generic
over any `T`; the arithmetic methods are restricted to numeric `T`.

```chezzi
a := Atomic(0)
parallel:
    for _ in 0..100:
        spawn bump(a)
print(a.load())            # 100 — every add is atomic, no lost update

fn bump(a: Atomic[int]):
    a.add(1)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `load`     | `load(self) -> T` | read out |
| `store`    | `store(self, v: T) -> nil` | overwrite |
| `exchange` | `exchange(self, v: T) -> T` | swap; returns the **old** value |
| `cas`      | `cas(self, expected: T, new: T) -> bool` | compare-and-swap; swaps iff the box equals `expected`; returns whether it did |
| `add`      | `add(self, x: T) -> T` | numeric `T` only; returns the **new** value |
| `sub`      | `sub(self, x: T) -> T` | numeric `T` only; returns the **new** value |

`add`/`sub` use the language's checked integer arithmetic (an overflow faults, exactly like `+`/`-`) and
plain float arithmetic. `cas` compares with the same structural equality as `==`. Each method is a single
lock-op-unlock, so the read-modify-write is atomic across threads with no separate update lock. `Atomic`
vs `Shared`: reach for `Atomic` when a lock-free-style counter/flag/CAS-loop is clearer than
`update(closure)`; reach for `Shared` when the update is an arbitrary transformation.

### `AtomicInt` — the monomorphic **lock-free** int atomic

`AtomicInt` is `Atomic[T]`'s monomorphic-int sibling. Because it is statically `int` (no `[T]`, nothing to
widen), it is backed by a genuine lock-free `std::sync::atomic::AtomicI64` — no `Mutex`, no runtime
type-sniffing — exactly like Rust's `AtomicI64`, Java's `AtomicInteger`, or Go's `atomic.Int64` (each also
distinct from the generic reference cell). Same import gate and reserved name as `Atomic`; constructed
`AtomicInt(v)` with one int arg. The method surface is identical to `Atomic`'s but all int-typed, and
`add`/`sub` are **always** available (int is always numeric — no residual numeric gate):

```chezzi
import std.concurrency
a := AtomicInt(0)
fn main():
    parallel:
        for _ in 0..8:
            spawn:
                for _ in 0..100000:
                    a.add(1)
    print(a.load())            # 800000 — lock-free, no lost update
main()
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `load`     | `load(self) -> int` | read out |
| `store`    | `store(self, v: int) -> nil` | overwrite |
| `exchange` | `exchange(self, v: int) -> int` | swap; returns the **old** value |
| `cas`      | `cas(self, expected: int, new: int) -> bool` | compare-and-swap; swaps iff the box equals `expected` |
| `add`      | `add(self, x: int) -> int` | returns the **new** value; overflow **faults** |
| `sub`      | `sub(self, x: int) -> int` | returns the **new** value; overflow **faults** |

Every op uses `SeqCst` ordering (the same sequential consistency `Atomic`'s Mutex gave, so the serial and
M:N engines stay byte-identical). `add`/`sub` keep the i64-overflow fault via a checked `compare_exchange`
CAS-loop (not a silently-wrapping `fetch_add`). Reach for `AtomicInt` over `Atomic(0)` for a hot int
counter/flag under contention — it measured **~2.7× faster** than the Mutex-backed `Atomic` on an 8-way
counter (see [`benchmarks.md`](benchmarks.md)); uncontended it is a wash.

---

## 6f. `RwShared[T]` — the cross-task read-write box

`RwShared[T]` is `Shared[T]`'s read-write counterpart: **MANY concurrent readers OR one exclusive
writer**. Same shape as `Shared` — one box, many tasks; the **handle is sendable**, the value is
copied in/out under a lock; constructed value-first (`RwShared(v)`, `T` inferred — an optional
`RwShared[T](v)` turbofish pins it and is checked against the value) — but the lock is a
`RwLock` instead of a `Mutex`. **Reach for `RwShared` over `Shared` when reads dominate**: read-heavy
workloads (a shared config/registry/map read on every request) scale because read guards don't exclude
each other; `Shared`'s `get`/`update` serialise *every* access.

```chezzi
fn put(r: RwShared[Map[str, int]], k: str, v: int):
    r.write(fn(m): insert(m, k, v))      # exclusive write lock

fn total(r: RwShared[Map[str, int]]) -> int:
    return r.read(fn(m): sum_values(m))  # shared read lock (concurrent with other readers)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `get`   | `get(self) -> T` | shared read guard; **snapshot copy out** (== `read(identity)`) — mutating it is a no-op (see the `Shared` `get()` gotcha above); mutate via `write` |
| `set`   | `set(self, v: T) -> nil` | exclusive write guard; overwrite |
| `read`  | `read(self, f: fn(T) -> R) -> R` | **shared** read guard: run `f` against the current value, return `f`'s result; **no** write-back. Many `read`s run concurrently |
| `write` | `write(self, f: fn(T) -> T) -> nil` | **exclusive** write guard: `Shared.update` under the write lock — read-modify-write, store `f`'s return |

`write`'s read-modify-write is atomic across threads (the box's contract, exactly like
`Shared.update`): under `--parallel` the whole `write` is serialised so concurrent writers can't lose
each other's updates. `read` and `write` copy the value out of the lock and **drop the guard before
running the closure** (a `RwLock` guard is not reentrant), so a closure may freely re-enter
`get`/`set`/`read`/`write` on a **different** box.

> **Reentrancy limit (same class as `Shared.update`):** a closure passed to `read`/`write` that
> re-acquires the **same** `RwShared`'s **write** lock — `write` inside `read`/`write`, or `set` inside
> `write` under `--parallel` — **deadlocks/UB**. Don't re-enter the same box's writer from within its
> own `read`/`write` callback. (Re-entering a *different* box, or a same-box `read`/`get`, is fine.)

`RwShared` vs `Shared`: reach for `RwShared` when reads vastly outnumber writes (concurrent readers
matter); reach for `Shared` when access is write-heavy or you don't need concurrent reads (one lock is
simpler).

### Fan out big shared data — reduce in place, don't `get`/`read` it

`get`/`read` copy the **whole** value out of the lock into the caller's heap. Fanning a 1M-element list
out to 8 workers, each calling `get()`/`read()`, materializes the entire list eight times — most of the
memory in the program is redundant copies. When the box element is a **container**, use the **zero-copy
read-view** instead — gated by a constructor-kind `where T: List/Map/Set` bound to the element's HEAD
(Tuple **excluded**): a `RwShared[List[E]]` gains `len`/`at`/`slice`/`for_each`/`fold`, a
`RwShared[Map[K,V]]` gains `len`/`get_key`/`has`/`for_each_entry`/`fold_entries`, and a `RwShared[Set[E]]`
gains `len`/`contains`/`for_each`/`fold`. They walk the stored container **entry-at-a-time** and
materialize one entry per step, so each worker scans/reduces in **O(1) memory**.

```chezzi
# Each spawned worker reduces its own view of the SAME shared container — no per-worker copy of the inner.
fn sum_shard(box: RwShared[List[int]]) -> int:
    return box.fold(0, fn(a, x): a + x)             # O(1) memory; reads one element at a time

fn sum_values(box: RwShared[Map[str, int]]) -> int:
    return box.fold_entries(0, fn(a, k, v): a + v)  # per-entry reduce over a shared map
```

Every walk RE-ACQUIRES the shared read guard **per entry** and drops it before running the callback (and
before any `has`/`get_key`/`contains` hash+eq probe — the guard is never held across user code), so a
nested read OR write of the same box — and a GC pass triggered inside the callback — are all
deadlock-free (a guard held for the whole walk would deadlock against the write-preferring `RwLock`).
Trade-off: the walk is **not one atomic snapshot** — a concurrent (or in-callback) `set`/`write` to the
same box may be observed mid-walk; use `read`/`get` if you need a consistent snapshot. Reduce into a
**different** box — an `AtomicInt` counter or a local accumulator — which is the fan-out pattern anyway.
On a non-container element (or a Tuple) these methods are a checker "no method" error.

**Ergonomic wrappers — `std.concurrency.collection`.** Raw `RwShared[Map[...]]` is the right primitive
for a shared table, but the `read`/`write` closures are verbose and the *compound* mutations
(insert-if-absent, increment a count) must be done inside a **single** `write` lock or they race. The
pure-Chezzi `std.concurrency.collection` module bakes the correct single-lock idiom into two generic
structs — **`ConcurrentMap[K, V]`** and **`ConcurrentCounter[K]`** — over `RwShared[Map[...]]`. A struct
of all-sendable fields crosses the airlock as a **shared handle** (same as `RwShared` itself), so the
wrappers preserve the cross-task sharing; `get`/`len`/`count`/`snapshot` are concurrent reads and
`set`/`increment`/`get_or_insert` are exclusive writes (the compound ones atomic in one lock). See
**[`docs/stdlib.md` §5 → `std.concurrency.collection`]** and `examples/concurrent_collection.chz`.

---

## 6c. `timer(ms)` — the one-shot timeout channel

`timer(ms)` returns a `Channel[bool]` that becomes ready (`recv()` → `true`) once `ms` milliseconds have
elapsed. It is the **composable timeout primitive**: instead of a bespoke timeout argument on `recv`, a
timeout is just another channel you can receive from — and, once `wait` lands (§6d), race against real
channels.

`timer` requires **`import std.time`** (whole-module, or `import timer from std.time`) — it is NOT a
global builtin (it stays a reserved name: no user `struct timer`/`fn timer`). `timer` is opcode-backed,
so the import is checker-only licensing with zero runtime cost; bare use without the import is an
`unknown function 'timer' (import it from std.time: \`import std.time\`)` error.

```chezzi
import std.time
t := timer(500)
print(t.recv())            # true — blocks ~500ms, then delivers
```

It is **level-triggered**: any `recv` at or after the deadline yields `true` (the typical use recvs it
once). Delivery is handled at `recv` time, in the receiver's own engine — so a `timer` created at the top
level can be `recv`'d inside a `--parallel` child. On `--parallel` the receiver parks and a background
job (on the netpoller timer thread) `send`s `true` at the deadline, accounted so it can't trip a false
deadlock; the cooperative `--serial` VM inline-sleeps to the deadline (single-threaded, like its
`sleep_ms`). Observable output is identical across both engines for a *lone* timer `recv` — but a
`timer` arm inside a `wait:` with a **runnable sibling** diverges (serial inline-sleeps instead of
yielding): `docs/gaps.md` **N10**, a pre-freeze known-limit (M:N is correct; the serial oracle is wrong).

> **v1 limitation:** a `timer.recv()` reached *inside a native callback* (a `Shared.update` closure, a
> list-HOF, an `Executor` task) under `--parallel` pins that worker for the timeout rather than demoting a
> replacement the way `sleep_ms` does — sound (the other workers progress), just lower throughput. Reuse
> of the `sleep_ms` demote path is a future improvement.

---

## 6d. `wait` — racing multiple channel receives *(shipped on both engines)*

> **Status:** the surface and semantics below are **locked** (brainstormed 2026-06) and **implemented on
> both engines** (2026-06-13): lexer→parser→checker→VM, with non-blocking arms (`else:`, an
> already-ready arm, a `timer` arm) AND the **blocking multi-channel park** working in **both** engines —
> the serial `--serial` cooperative scheduler (sequential poll/inline-sleep) and the **M:N
> (`--parallel`) blocking park** (landed 2026-06-13, the M:N park notes below). A blocking `wait` under
> `--parallel` now parks one fiber on N channels (woken by the first sender, swept out of the other
> buckets) instead of faulting. **SEND-arms** (`ch.send(v):`, deterministic source-order selection, a
> bounded send-arm parks until a receiver frees a slot) landed 2026-07-22 — see `examples/wait_send.chz`.
> Both examples are byte-identical across serial `--serial` / default M:N.

`wait` is Chezzi's `select`: block until **whichever of several channels is ready first**, run that arm.
Both directions are supported — **recv-arms** (`x := ch.recv():`, bind the received value) and **send-arms**
(`ch.send(v):`, no `:=`/`=`, run the body once the send goes through). A send-arm is ready when the channel
can accept the value: a **bounded** channel with a free slot, an **unbounded** channel (always), or a
**closed** channel — a send-arm on a closed channel is *selected and faults* `"send on a closed channel"`
(Go's panic-on-send-to-closed), never skipped. Combined with `timer`, `wait` subsumes a bounded-wait `recv`
(`ch.recv_timeout(500)` ≡ a `wait` over `ch` and `timer(500)`), which is why no separate `recv_timeout` exists.

```chezzi
wait:
    v := orders.recv():        handle(v)        # recv-arm: arm-local binding `v: T` (the element type)
    result = cancels.recv():   result = "x"     # `=` assigns an existing outer lvalue instead
    outbox.send(next):         sent += 1        # SEND-arm: fires once `outbox` can accept `next`; binds nothing
    _ := timer(500).recv():    on_timeout()      # `_` discards; a timer arm is just a recv
    else:                      poll_miss()       # optional, non-blocking; if no arm is ready, run this
```

**Selection is deterministic SOURCE ORDER** (first ready arm wins, recv OR send), **not** Go's uniform-random
`select` fairness. This is Chezzi's one principled divergence from Go here, and it is *required*: it is what
makes the serial `--serial` oracle and the M:N (`--parallel`) engine byte-identical (`chezzi run --check-parity`)
— with **one** known exception, a live `timer` arm racing a runnable sibling (`docs/gaps.md` **N10**), a
pre-freeze known-limit deferred to the post-freeze serial removal (`docs/future.md` §2b).
All arm channel handles and send values are evaluated **once**, top to bottom, on entry (Go's rule). `else`
runs only if **no** arm is ready — so a `wait` containing an unbounded or closed send-arm never blocks (that arm
is always ready). See `examples/wait_send.chz`.

**Surface & grammar.** A new compound statement (sibling to `match`/`parallel`):
`wait : NEWLINE INDENT <arm>+ DEDENT`. An arm is one of:
- a **recv-arm** `<target> ( ":=" | "=" ) <chanExpr> ".recv()" ":" <block>` — the RHS **must** be a `.recv()`
  on an expression of type `Channel[T]` (a non-`.recv()` RHS like `v := fn():` is a compile error). The
  `<chanExpr>` can be any expression (`ch`, `chans[i]`, `get_chan()`, `timer(500)`), evaluated **once**. The
  target is `:=` (a fresh arm-scoped binding), `=` (assign an existing outer lvalue — arm bodies are lexical
  sub-scopes, not closures, so outer mutation is normal), or `_` (discard).
- a **send-arm** — a *bare* `<chanExpr> ".send(" <value> ")" ":" <block>` (no `:=`/`=`). The grammar accepts
  any bare `<expr> <block>`; the **checker** enforces the exact `chan.send(value)` shape with `chan: Channel[T]`,
  `value: T` (a bare arm that isn't a `.send()` — e.g. `try_send`, an arbitrary call — is rejected with the
  legal-forms error). A send-arm binds nothing.
- `else:` — optional, at most one, and must be **last**.

**Type-check.** A recv-arm's `chanExpr: Channel[T]` → the target binds/assigns `T`; a send-arm's `chan: Channel[T]`
→ its `value` must be assignable to `T` (same check as a plain `ch.send(v)`). `wait` is **not** exhaustive
(it's a runtime race, not a type match); ≥1 arm is required.

**Runtime semantics.**
1. Evaluate each arm's channel handle (and, for a send-arm, its value) once, in source order.
2. Poll arms in **source order** (deterministic priority, not Go's random fairness — required for
   serial == M:N parity): the first **ready** arm wins. A recv-arm is ready with a queued value → pop, bind,
   run its block. A send-arm is ready when the channel can accept the value (bounded-with-space / unbounded /
   closed) → enqueue (or, if closed, *fault* `"send on a closed channel"`), run its block, bind nothing.
3. A **closed + empty** channel's *recv*-arm is **skipped** (option B); a closed *send*-arm is instead
   *selected and faults* (asymmetric — Go's panic-on-send-to-closed). If *every* recv-arm is closed+empty,
   there's no ready send-arm, and there's no `else`, the `wait` faults `"wait: all channels closed"`.
4. If no arm is ready: with an `else`, run it (non-blocking); otherwise **block** — park the fiber on *all*
   live arm channels and re-poll on the first wake. A recv-arm wakes on a **sender**; a bounded send-arm wakes
   on a **receiver** freeing a slot (reusing the bounded-`send` `wake_senders` / `recv_wake` path).

**Implementation notes.** *(Done on both engines. A new `Op::WaitPoll` holds the arm operands on the
operand stack — one slot per recv-arm (the channel), TWO per send-arm (channel THEN value, walked via a
per-arm slot cursor keyed on `WaitMeta.is_send`) — polls source order, and jumps to the chosen arm's body /
`else`, handles a live `timer` arm (see below), faults all-closed / send-to-closed, or parks. The cooperative
multi-channel park files the fiber under every arm key (`run_child` reads `wait_suspend`, a
`Vec<(handle, is_send)>`) and sweeps the index out of the other buckets on resume; the M:N park (below) does
the same with an `Arc<WaitPark>` token. The park-gap re-check is **kind-aware**: a recv-arm is ready with a
queued value / on close, a send-arm with a **free slot** (`queue.len() < cap`, or unbounded) / on close — using
the recv predicate for a full send-arm would spin requeue→re-poll→re-park.)*

> **v1 limitation — send-arm inside a native callback.** A **full bounded** send-arm reached *inside a
> native callback* (a `Shared.update` closure, a list-HOF, an `Executor` task) can only block, and neither
> engine can carry it: the M:N engine can't snapshot-park there and its in-callback demote path pops arm
> queues (recv semantics), while the cooperative `--serial` VM has no yield point inside a callback. So a
> `wait` with a live send-arm on that path **faults** — with the **byte-identical** full-send-in-callback
> message `chan_send_step` already raises, on **both** engines (the fault is decided before the engine
> split, preserving serial == M:N parity), rather than blocking. Same class as the existing in-callback
> full-`send` / `timer.recv()` v1 limits; the upgrade path is a demote-in-place send block.

> **Timer arm under `--parallel` — timed-park, not inline-sleep.** A live `timer(ms)` arm is handled
> differently per engine. The cooperative `--serial` VM is single-threaded, so it **inline-sleeps** to the
> soonest deadline then takes the timer arm. **Known-limit (`docs/gaps.md` N10):** the inline-sleep fires
> *before* the cooperative park, so if a **runnable sibling** could satisfy a non-timer arm, serial strands it
> and takes the timer where M:N takes the sibling's `send` — a serial ≠ M:N divergence (M:N is correct). Fix
> deferred to the post-freeze serial removal (`docs/future.md` §2b). The M:N engine (`--parallel`) must **not** inline-sleep: that would
> pin the OS worker and strand a sibling `send` that lands mid-window. Instead it arms **one** background
> `timer::submit_at(deadline, send_wake(true))` on the soonest timer arm's own channel (guarded by an
> arm-once `ChannelCore.timer_armed` CAS so a re-park can't re-arm) and falls through to the normal
> snapshot-park, so the timer is just another bucket. The `WaitPark` claimed-CAS sweep then picks **exactly
> one** of {a sibling `send`/`close` on any arm, the timer's own deadline `send_wake`} — a value arriving
> before the deadline wins the wait (the value is **not** stranded), the deadline wins only if nothing else
> did. The `native_reentry > 0` demote path threads the deadline into its bounded poll (channel scan first,
> so a real send still beats the timer).
- *Non-blocking* (`else` present) and the *poll* step reuse the existing `try_recv` path (a timer arm's
  `try_recv` is already deadline-aware) — straightforward in all engines. **(Done.)**
- *Blocking* (no `else`) needs a **multi-channel park** — **done in both schedulers.** A fiber parks in one
  `parked[key]` bucket (`MnSched::park`/`send_wake`/`close_wake`) and is woken by a send to that key. `wait`
  needs one fiber parked on N keys, woken by the first sender, and **swept out of the other N-1 buckets** —
  otherwise a later send wakes a fiber that already moved on. **M:N implementation (landed):** a
  `WaitPark { fiber: Mutex<Option<Fiber>>, keys, claimed: AtomicBool }` held once behind an `Arc`, with a
  `ParkedEntry::Wait(token)` filed in every `parked[key]` (the bucket is now
  `HashMap<usize, Vec<ParkedEntry>>` where `ParkedEntry` is `Recv(Fiber)` or `Wait(Arc<WaitPark>)`).
  `MnSched::park_wait` does the N-key gap re-check (any arm ready/closed/cancel → requeue, not park) and
  files all N tokens + `parked_n += 1` (ONE fiber) under one core-lock hold. The first waker (in
  `send_wake`/`close_wake`/`cancel_drain`/`flag_deadlock`) CASes `claimed`, `take()`s the fiber, and
  removes its token from every other bucket by `Arc::ptr_eq` — all under the one lock, serialized with
  `park_wait`'s gap re-check (lost-wakeup-safe). Routed via `Disp::WaitPark(Vec<(key, core)>)` captured
  while the fiber heap is live (mirrors `Disp::Park`). The single-channel `recv` park stays the **1-key
  `ParkedEntry::Recv` special case** (alloc-free, provably unchanged — regression test
  `vm_wait_single_arm_recv_park_unchanged_under_parallel`).
- *Cooperative `--serial` VM* (sequential): poll arms once in source order; first ready wins; else if `else`,
  run it; else if any arm is timer-backed, inline-sleep to the soonest deadline and take that arm; else
  fault (all-closed or the existing deadlock fault). Deterministic → golden parity with the M:N engine holds
  **except** when a timer arm races a runnable sibling (`docs/gaps.md` N10): the inline-sleep runs before the
  cooperative park, so serial takes the timer where M:N takes the sibling — a pre-freeze known-limit (M:N
  correct). Proper fix = park first, inline-sleep the timer only when the quiesce path would idle-deadlock.
- *`native_reentry > 0`* (inside a native callback) on `--parallel`: snapshot-park is impossible — mirror
  `demote_recv_block` with a **multi-channel demote-poll** (`demote_wait_block`: register all N arm
  channels in `demoted_chans`, poll all N queues source-order under the core lock on a bounded
  `DEMOTE_POLL_BACKOFF`). **v1 limitation (sound, lower-throughput):** there are N channel condvars and no
  single one to block on, so the demote loop polls on a backoff timer rather than waiting on a targeted
  condvar — same shape as the timer-in-callback note in §6c. The snapshot-park (reentry == 0) is the fast
  path; the demote is only reached when a `wait` is run from inside a host-stack native callback.

---

## 6e. `std.cancel` — cooperative cancellation & timeouts

`std.cancel` is a Go-`context`-inspired **cancellation token**, adapted to Chezzi. A `Token` is an
explicit, sendable handle you thread down a call tree (like `Channel`/`Shared`): poll `cancelled()` in
CPU loops, race `done()` in a `wait:` for IO loops, and `cancel()` it from anywhere. It is written
entirely in Chezzi (`std/cancel.chz`) over existing primitives, plus the one native `Channel.trip()`
latch (§6c').

```chezzi
import std.cancel

c := cancel.manual()             # manually-cancellable token, no deadline
t := cancel.timeout(500)         # auto-cancels 500ms after creation; also manually cancellable
child := c.derive()              # a CHILD token — cancelled when c (or any ancestor) is
```

> **Tree-structured (Go `context.WithCancel`).** `derive()` builds a **child** token linked to its
> parent: cancelling or timing out the parent cancels every transitively-derived child, recursively
> root-to-leaves, while cancelling a child **never** touches the parent (one-directional). The link is
> **live** — a parent flip is observed by an already-derived child, *including a child that crossed the
> `spawn`/`parallel:`/`Channel` airlock* — because the link is the parent's `Shared` flag plus a
> `Shared` registry of descendant `done()` channels, which cross as live cores exactly like the flat
> token's `flag`. **`done()` cascades transitively too:** `derive()` registers a child's `done()`
> channel into **every ancestor's** registry (walking the parent chain to the root, each insert an
> atomic `update()` so concurrent siblings don't race-lose), so a manual `cancel()` at *any* depth
> above trips the child's `done()` directly — a grandchild parked in `wait: leaf.done()` wakes on a
> grandparent cancel, not just on its immediate parent. A child inherits the **tightest** deadline
> (the soonest absolute deadline of itself and its ancestors); a derived child of an already-elapsed
> timeout is cancelled at once with reason `"timeout"`. `reason()` is **nearest-cause-wins**: the
> child's own cause if it has one, else the inherited ancestor's. (Known v1 limit: the per-ancestor
> registry only **grows** — there is no token-drop hook, so a long-lived ancestor that has many
> short-lived descendants derived under it retains their `done()`-channel handles until it is itself
> dropped. Tokens are request-scoped and short-lived in practice; a future prune-on-cancel could clear
> the list.)

| Method | Returns | Notes |
|--------|---------|-------|
| `cancelled()` | `bool` | `flag` OR (deadline passed) OR any ancestor is. **Polls; never blocks.** |
| `reason()` | `str?` | `"cancelled"` (manual) \| `"timeout"` (deadline) \| `None` (live). Nearest cause wins, else inherited. |
| `done()` | `Channel[bool]` | ready (recv → `true`) when done — for a `wait:` arm. Same handle every call. |
| `cancel()` | `nil` | manual cancel, anytime, any task; idempotent; wakes `done()` waiters and fans out to derived children. |
| `derive()` | `Token` | a child token: cancelled when self (or an ancestor) is; tightest deadline; one-directional. Also `cancel.derive(parent)`. |
| `deadline_at()` | `float` | absolute monotonic secs, or `0.0` if none. |

```chezzi
# CPU-bound: poll cancelled() at the loop back-edge.
fn crunch(tok: Token, out: Shared[int]):
    i := 0
    while i < 100000000:
        if tok.cancelled(): return     # ordinary return → defer/recover still run
        i = i + 1
    out.set(i)

# IO-bound: race done() against the work channel in a wait:.
fn serve(tok: Token, io: Channel[str]):
    wait:
        v := io.recv():        handle(v)
        _ := tok.done().recv(): cleanup()   # cancelled (timeout or manual) → take this arm
```

> **Cancellation is delivered at CHECKPOINTS — and a registered `defer` ALWAYS runs (BOTH engines).**
> A cancel (a sibling's fault, an `os.exit`, a scope teardown) is observed only at **cancellation
> points**: **loop back-edges** and **blocking / park ops** (`recv`, `wait:`, a socket op, a blocking
> native like `sleep_ms`). It is *not* observed at every instruction. Two consequences, both intended
> (this is Trio-style structured concurrency; Go never preemptively kills a goroutine at all):
>
> - **A STARTED task always runs its straight-line prologue**, so a `defer` it registers is registered
>   *before* anything can kill it. "Does my cleanup run?" no longer depends on scheduler timing.
> - **A long-running CPU loop is still cancelled promptly** — the loop back-edge is the checkpoint.
>
> A cancelled task then unwinds through its `defer`s — cancelled while running (back-edge), while parked
> on a `recv`/`wait:`, while parked on a socket, or while parked when a *sibling*'s fault tore the
> nursery down — **on the M:N engine and on `--serial` alike** (serial's scheduler cancels and re-drives
> its still-parked children before propagating the fault). `defer` is the language's only cleanup
> mechanism (no destructors, no `with`), so this is the guarantee cleanup rests on. At a `recv`/`wait:`
> checkpoint **cancel wins** over a queued value, a tripped `done()` latch and a fired timer.
>
> Exactly one thing deliberately skips a `defer`: **`std.os.exit`**, a hard halt by design. (An
> `os.exit` executed *by* a cancelled task's `defer` is honored — it beats the sibling's fault and sets
> the process exit code, identically on both engines.)
>
> **Every spawned task starts — even into an already-cancelled scope.** A `spawn`ed task is *always*
> run: M:N cannot do otherwise (a scope completes only at `done == total`, so a queued fiber is picked
> up even after a sibling has faulted), and `--serial`'s cancel drain therefore starts its
> never-started children too. So the task runs its prologue, prints what it prints, registers its
> `defer`, and dies at its first checkpoint — on **both** engines, with the same line set. (This is why
> a sibling of a task that calls `std.os.exit` still runs its prologue: the exit is a hard halt for the
> *program*, reduced at the nursery join, not a freeze-frame on already-spawned tasks.)
>
> **A `defer` is never itself cancelled.** No cancellation point fires *inside* a deferred call — a
> `defer` is the cleanup the cancel exists to run. Every registered `defer` of a cancelled task runs, in
> LIFO order (loops, blocking ops and HOF callbacks inside a defer body included), whether the task was
> cancelled at a checkpoint, returned normally, or faulted on its own while a sibling had already
> tripped the scope cancel. The suppression covers the work the cleanup *delegates*, too: a `spawn` /
> `parallel:` opened inside a `defer` gets a **clean slate** — it does not inherit the already-tripped
> enclosing cancel, so its children run to completion (they are still cancellable by their *own*
> nursery's faults). Both engines.
>
> **…so cleanup that blocks, blocks the teardown.** A `defer` that sleeps, waits on a socket or sends a
> last message is *uninterruptible*: it delays the nursery join by exactly as long as it takes, on both
> engines, with no cap (`defer time.sleep_ms(10000)` in a cancelled task = a 10s join). That is
> Go's rule for a deferred function during a panic, and it is the price of "cleanup is never truncated".
>
> **The one thing a `defer` cannot do on `--serial`: PARK.** A defer body runs during frame teardown,
> whose LIFO drain is host-stack state, so it runs *guarded* — like a `list.map` callback, it cannot
> snapshot-park (the **C5** limit). Time/IO blocking is fine (it runs inline / demotes). But a `recv`
> inside a cleanup that needs a value from a **live sibling** cannot yield to that sibling on `--serial`:
> it faults in place with the deadlock error, so that cleanup stops there, while the same cleanup
> *completes* on M:N (the demoted worker blocks in place and the sibling's `send` reaches it). A real,
> recorded **serial ≠ M:N** limit (`docs/gaps.md` **C5/N6g**) — lifting it needs a resumable native
> re-entry, not a cancellation change. Cleanup that only sends, sleeps, closes or computes is unaffected.
>
> **Cleanup that can NEVER complete is REPORTED, never a silent hang.** A `defer` whose body waits for
> something that can never arrive (`ch.recv()` no one will ever answer) leaves the program quiesced —
> and the deadlock detector still fires, on **both** engines (serial faults the `recv` in place; on M:N
> the demoted worker self-detects the quiesce). If a sibling's fault is what cancelled the task, *that*
> fault is what is reported — the stuck cleanup's own error is swallowed with its cancelled task, so
> both engines print the same line set and exit code.
>
> **Cancelling a scope cancels its nested scopes — at their CHECKPOINTS.** A `parallel:` entered from a
> task that is then cancelled dies with it: its children observe the enclosing cancel at their own
> checkpoints (a spinning grandchild cannot wedge the teardown). A nested nursery still keeps its own
> cancel token for its own faults: an inner fault never cancels an *outer sibling*.
> One limit, in the N5 family and identical on both engines: a grandchild that is already **parked**
> (`recv`/`wait:`) when the *outer* scope is cancelled is not re-driven — the cancel drain is scope-
> scoped, and a parked fiber has no checkpoint to observe the inherited flag — so it is torn down by the
> deadlock reap **without running its `defer`s** (`docs/gaps.md` **N5**). A grandchild that is *running*
> (or parks *after* the cancel) unwinds normally.
>
> **Where a cancel is NOT delivered — pure CPU with no back-edge.** A checkpoint is a loop back-edge, a
> blocking op, or a native→user-code re-entry (a `list.map`/`filter`/`fold`/`sort` callback: the native's
> per-element Rust loop *is* the back-edge, and the cancel is delivered between elements). **Deep
> recursion is not a checkpoint** — a recursive function emits only `Call`/`Return`, never a backward
> `Op::Jump` — so a cancelled task sitting in a loop-free recursive computation (`fib(34)`) runs it to
> completion before it dies. This is Trio's model (pure-CPU code is not interrupted); making `Op::Call`
> a checkpoint would put a checkpoint *before the `defer` line* of any prologue that calls a function and
> would give back exactly the bug this design removed. Both engines behave identically, so it is a limit,
> not a divergence. Bound a recursive computation yourself if a task must tear down promptly.
>
> **Cross-task output order is NOT part of the contract.** One `print` = one locked write = line-atomic;
> the *order* of prints from different tasks is nondeterministic on **both** engines (a cancelled task's
> already-printed lines are kept, not retracted). What is identical across engines: the **set of lines**,
> the **exit code**, and **whether the `defer` ran**. Parity tests for concurrent output use the
> order-insensitive comparison, never a byte-equal one.
>
> One known limit remains: a **genuine deadlock** (every fiber parked, nothing cancelled, nothing able
> to arrive) tears the parked fibers down where they stand and does **not** run their `defer`s
> (`docs/gaps.md` **N5**). Both engines agree there, so it is a limit, not a divergence.

**Parity-safe deadline.** A timeout's deadline is checked via `monotonic()` *at poll time* — no
background canceller task — so a self-polling timeout loop stops on time identically on **every**
engine. (`done()`'s deadline delivery rides the proven `timer(ms)` path, §6c.)

**Cooperative contract (by design).** A token *signals* cancellation; it cannot forcibly interrupt.
On the single-threaded `--serial` oracle, a pure-CPU loop that never polls `cancelled()`
and never yields runs to completion — a sibling's manual `cancel()` only lands when that sibling gets
the thread. The default OS-thread engine preempts such a loop. So a *manual* cancel of a non-polling
CPU sibling **diverges by engine** (this is why `examples/cancel_cpu.chz` carries no golden
`.expected`, like `examples/parallel_cancel.chz`); a self-polling *timeout* does not. Guidance: **poll
`cancelled()` in CPU loops; `wait:` on `done()` in IO loops** — exactly Go's `ctx.Done()` contract.

The **same root** covers the *automatic* cancel that structured concurrency issues when a sibling
faults (`docs/gaps.md` **N8/N9**): a `parallel:` with one task in an unbounded CPU loop and one that
`panic`s **hangs on `--serial`** (the spinner never yields, so the faulting sibling never gets the
thread to trip the cancel), and a task cancelled mid-loop emits a different **line set** per engine
(how far it got before yielding is a scheduling fact). This is **not a bug to fix** — it is the
cooperative oracle behaving cooperatively. `--serial` exists only as the byte-identical **parity
oracle** for bug-finding; it is never the recommended runtime for CPU-bound concurrent tasks. For safe
single-thread execution use **`--threads=1`** (still the OS-thread M:N engine — the kernel preempts the
spinner, so it faults promptly, verified 0/15 hangs) or the default engine. Lifting the limit would
require teaching the cooperative scheduler to time-slice a *running* fiber (its own milestone), which
`--threads=1` already makes unnecessary for users.

### 6c'. `Channel.trip()` — the manual level-trigger latch

`trip()` is the one native primitive `std.cancel` needs. It flips a permanent latch on a channel: the
channel then reports ready (`recv`/`try_recv`/`wait` → `true`) on **every** call thereafter, fanning
out to any number of receivers — like a passed `timer` deadline, but flipped on demand. (An ordinary
`Channel[bool]` can't be a fan-out `done()`: it is move-on-send, so a value reaches one receiver
once.) `trip()` is idempotent and reuses `close()`'s wake fan-out (minus the `closed` flag, so a
`wait:` arm stays *ready* rather than *skipped*). See `examples/channel_trip.chz`.

### 6g. Run the parity oracle yourself: `--check-parity`

The test suite proves **serial == M:N** for every example (the in-tree `assert_file_parity` oracle:
run once on `--serial`, once on the default M:N engine, assert byte-identical stdout/stderr/terminal
result). `chezzi run --check-parity <file>` exposes that same oracle as a one-command check on **your
own** program:

```sh
chezzi run --check-parity examples/concurrent_jobs.chz
# … program stdout printed once …
# parity OK (serial == M:N)        (on stderr; exit 0)
```

It type-checks once, then runs the program **twice** — the cooperative serial oracle, then the M:N
OS-thread engine — each into a buffered sink, and diffs the two captures byte-for-byte. Identical →
the captured output is printed once and `parity OK (serial == M:N)` goes to stderr (exit 0, even if
BOTH engines errored *identically* — a held parity is a pass). Divergent → a greppable side-by-side
report headed `parity DIVERGENCE (serial != M:N)` naming the first differing stream and line, and a
non-zero exit.

A divergence is a **signal to investigate**, not automatically a bug: it can be a genuine
order-dependence / airlock / scheduler fault (the real prize), *or* one of the documented
serial-vs-M:N asymmetries above — a task cancelled mid-loop emitting a different **line set** (§6e), a
non-deterministic cross-task **print order** (byte-identical compare flags order too), or an accepted
`--parallel`-only path (`std.net`).

`--check-parity` is mutually exclusive with `--serial`/`--parallel` (it runs both). `--threads=N`
still sizes its M:N leg (the serial leg ignores it). **Limitation:** both legs share the one real
process stdin fd and run sequentially, so a **stdin-reading** program diverges by construction — leg 2
sees EOF after leg 1 drains the input. Don't use `--check-parity` on programs that read stdin.

---

## 7. Sendability

**The model — spawning a task copies its environment (fork-like).** A `spawn`ed task does not share the
parent's heap. It receives its **own isolated copy** of everything it captures — captured locals are
deep-copied, module globals are snapshot-copied once at the first nursery — much like a forked child
copies the parent's address space. Two deliberate differences from a real `fork`:
1. It copies only the **reachable captured environment**, not the whole heap.
2. **Explicit concurrency handles cross by SHARED reference, not by copy** — `Channel`, `Shared`,
   `RwShared`, `Atomic`, `Executor`, and the socket/reader/writer handles carry their one underlying
   `Arc` core across, so all tasks reach the *same* mailbox/box/queue/fd. These handles are the *only*
   way tasks share mutable state; everything else is copied. That is why the shared-mutation data race
   is unrepresentable — a plain captured value mutated in a task changes only that task's copy.

**Almost everything is sendable now** (scalars, `str`, containers/structs of sendable contents,
closures & bare/`fn` and even **recursive** local fns, **protocol existentials**, `.iter()` cursors,
and **live generators** — including one suspended mid-`recover:`). The residual **NOT-sendable** set is
small and each case is rejected cleanly (a recoverable fault or compile error, byte-identical on both
engines, **never UB**):
- **[fundamental]** a value carrying a **live host handle** — a **module namespace** itself, a bound
  **native** handle, or a **raw FFI** resource (`Obj::Module`/`Native`/`Cffi`; a `file`/regex/HTTP
  `Response`/FFI extern — NOT the concurrency handles above, which *do* cross). A foreign OS/library
  resource can't be copied into another heap — rejected at the runtime airlock (`ensure_crossable`). This
  one is intrinsic and stays.
- **[not a real limit]** two **checker-unreachable** suspended-generator shapes (multi-frame,
  pending-`defer`) kept only as defensive guards — no valid program can construct them. (A frame-local
  AND a module-global live generator both cross **by value** now — see the generator note below.)

Crossing a task boundary (a `spawn` capture or a `Channel.send`) is gated on **sendability**. A
captured **local** crosses as an independent per-task **copy** — a task may reassign it (the write
stays on the isolated copy, invisible to the parent); a captured **module global** is **frozen** and
both **reassigning** it AND **mutating it in place** (`.push`/`m[k]=v`/`s.field=x` on a module-global
aggregate) inside a task is a **compile error** (see below).

- **Sendable:** scalars (`int`/`float`/`bool`), `str`, containers + structs whose contents are all
  sendable, **`Channel`** itself (reply channels), an **`Atomic[T]`** handle, an **`AtomicInt`** handle,
  a **`Shared[T]`** handle, a **`RwShared[T]`** handle,
  a **`std.cancel` `Token`** (a struct over the above, so it flows down the call tree), a
  **`.iter()` snapshot cursor** — a frozen data snapshot + read position, so it crosses by deep copy
  exactly like a `list` (the cursor and a generator share the `Iterator[T]` existential, but a cursor
  is plain data), and a **user protocol existential** (Task 2, Go `chan interface` parity — `Channel[P]`
  and protocol-typed spawn args cross; the erased witness rides by deep value copy).
- **Closures / functions cross by value (B3.3).** At runtime the airlock lowers a closure or
  bare `fn` **by value** — its `proto` (shared, read-only) + its captures deep-copied recursively + its
  home module index, never a by-reference heap handle — on **both** engines identically. So a `spawn f()`
  callee whose captured environment contains a nested closure/`fn` (or is itself a bare `fn`) runs
  cleanly, its captured plain data isolated per task exactly like any other sendable. **Checker
  (landed, Task 2a):** the function type is **sendable**, so a closure crosses as data —
  `Channel[fn(int)->int]` type-checks and a closure sent over a channel or returned from a factory runs.
  The rule is: **a closure crosses iff its captures are sendable.** The bare `fn` type cannot carry its
  captures, so that per-closure check runs at the airlock **sites**: a closure/nested-fn value whose
  captures include a **non-sendable local** (a native handle — see below) at a `spawn f()` **callee** or
  `spawn f(g)` **arg** is a **compile error**, matching the `spawn:` block form. A bare **native** handle
  is a different case and stays non-sendable (below). (A protocol existential is **sendable** — Task 2,
  Go `chan interface` parity — so capturing one is fine; a witness that carries a genuinely non-sendable
  handle (a live host resource, or a module handle) is caught at the runtime airlock, not here. A
  native/FFI *fn value* — `math.sqrt`, an `extern` fn — is pure code and now crosses by value, so it is
  NOT caught.)
- **Self-referential DATA IS sendable (identity-preserving airlock).** A struct/list/map/set/tuple/enum/
  newtype/cursor that points back at itself (`a.next = b; b.next = a`, a list holding itself, a map whose
  value refers to the map) crosses **any** airlock (`spawn:` block, `spawn` arg, `Channel.send`, `Shared`,
  module-global snapshot) and **round-trips** — every container `WireValue` arm carries a per-serialization
  `id` + a `WireValue::Backref(id)` for a back-edge, exactly like `Cell`/`Closure`. `from_wire` ties the
  knot on the receiver (placeholder-alloc → register `id` → recurse → patch); `Map`/`Set` reuse the carried
  hash so a cyclic key is never re-hashed. **Byte-identical on both engines.** The identity is **back-edge-
  only** (a node is popped off the serialize DFS stack on exit), so an acyclic **DAG alias** (the same node
  appearing twice off the cycle) is re-serialized as **two independent deep copies**, never collapsed into
  one shared node (mutating one copy in a task leaves the other untouched). The depth cap (`maximum
  structural depth …`) stays **only** as the backstop for a genuinely-unbounded **acyclic** nest.
- **A recursive *local* `fn` IS sendable (identity-preserving airlock).** A nested `fn` that calls itself
  captures its own name for recursion — the compiler's letrec gives it a self-cell, so the closure's
  capture graph is **cyclic** (`Closure → Cell → Closure`). The same `id` + `Backref` machinery (above)
  preserves that identity. So a recursive local `fn` — and a **mutually-recursive closure pair**
  (`Closure_f → Cell_g → Closure_g → Cell_f`) — crosses **any** airlock (`spawn:` block, `spawn f()`
  callee, `spawn f(g)` arg, `Channel[fn].send`) and computes correctly, **byte-identical on both engines**.
  A recursive closure that ALSO reads an outer local carries the self-edge as a `Backref` and the outer
  local as an independent deep copy. A **mixed** struct+closure cycle — a self-capturing closure held
  *inside* a struct/list/map so the cycle passes through a container — now **also round-trips** (every
  container is identity-preserved too, so the old mixed-cycle reject is gone). The **only** value cycle
  that still rejects is one threaded through a live **generator's parked slot** (a generator's frozen
  frame carries no wire id, so it can't back-reference): re-entering the same generator on the serialize
  stack faults cleanly with `a generator cannot be sent across tasks as part of a reference cycle` —
  never a silent duplicate. (The depth cap stays as a *separate* backstop for a genuinely-unbounded
  **acyclic** nest.)
- **Protocol existentials ARE sendable (Task 2, Go `chan interface` parity).** `Channel[Drawable]`,
  a protocol-typed spawn arg / struct field / `Ok`/`Err` payload / return all type-check — the erased
  witness crosses by deep value copy like any other value. The concrete witness's own sendability is
  checked at each widening site; a witness that genuinely can't serialize (one carrying a live host
  resource, or a module handle) is rejected at the **runtime airlock** (`ensure_crossable`), recoverably
  and identically on serial == M:N — not at construction. (A native/FFI *fn value* is pure code and now
  crosses by value / shared `Arc` — `WireValue::Native`/`Cffi` — so it is no longer rejected there.)
- **Not sendable (checker):** native handles (file/regex/HTTP `Response`/etc.) and a **module
  namespace**. Capturing or passing either across the airlock is a **compile error** at a direct
  spawn/`Channel` site — whether captured directly by a `spawn:` block, or by a closure/nested-fn used
  as a `spawn f()` **callee**/**arg** (Task 2a gates the callee/arg sites too). A **module-global** non-sendable value, by contrast, is a **read-only global**
  resolvable in every task (like a free fn), **not** a per-task capture — reading it inside a task is fine
  and it is never gated (only *reassigning* or *in-place mutating* a module global inside a task is the
  error, below).
- **Frame-holding generator (F3 path C — sendable BY VALUE from a frame local):** a live generator held
  in a frame **local** crosses **any** task airlock **as data** (passed/captured into a `spawn`, or stored
  in a `Channel`/`Shared`/`RwShared`/`Atomic`) as an **independent deep copy** — `to_wire`/`from_wire`
  serialize its `proto`, backing closure, and parked operand-stack/args and rebuild a fresh
  `GeneratorCore` on the receiver, so advancing one copy never affects the other (like a cursor, but
  carrying frozen execution state, not a plain snapshot). Every parked slot is wired recursively, so a
  **non-sendable parked slot** still **rejects at the crossing** — a slot is checked at serialize time,
  so there is no under-gate. Two reject shapes: a
  genuinely-unbounded >10000-deep **acyclic** nest held live across a `yield` trips the `maximum
  structural depth …` depth cap; a value **cycle** threaded *through* the generator's own parked frame
  (the generator carries no wire id, so it can't back-reference) is caught by re-entering the same
  generator on the serialize stack — a clean `a generator cannot be sent across tasks as part of a
  reference cycle` fault (never a silent duplicate — the container-back-edge cuts the recursion before
  the depth cap would trip, so the generator arm guards it directly). A parked **recursive local `fn`**
  (or a parked **self-referential struct/list/map**), by contrast, now round-trips like any other
  capture — its cycle back-references cleanly (only a cycle passing through the generator's frame itself
  rejects).
  A suspension **inside a `recover:`** (a live handler stack) is ALSO sendable — a `Handler` is pure
  plain-data (all `usize`, no `GcRef`/`Value`), serialized as-is on the wire and rebuilt so the recover
  boundary resumes intact; the resume path rebases each parked handler/frame `nursery_len` to the resuming
  driver's floor (a generator opens no nursery of its own, so its escape-drain must be a no-op). The two
  remaining rejected shapes are **checker-unreachable** and kept only as defensive guards that reject
  cleanly (a graceful, byte-identical-on-both-engines `... cannot be sent across tasks` error, **never** a
  panic, **never** a silent mishandle): a suspension **with a pending `defer`** (`defer` is banned inside a
  generator) and a **multi-frame** suspension (`yield` fires only in the generator's own body frame). A
  generator held as a **module global** crosses **BY VALUE too** (backlog item B): a task that reaches it
  gets its own independent deep copy through the same `to_wire`/`from_wire` path, via the per-task
  module-global snapshot (`to_snap`) each task already takes. So two tasks reaching the same module-global
  generator each drive their **own** copy (and the parent keeps its own). Memory safety rests on `from_wire`
  rebuilding a fresh `GeneratorCore` on the worker heap (never a shared cross-heap `GcRef`). A **non-sendable**
  module-global generator (a non-sendable parked slot, a value cycle, a parked host handle) differs from the
  frame-local case in ONE way: `snapshot_modules` walks **every** global once at the first `spawn`, reached
  or not, so it must NOT eager-fault on a generator the program merely *holds*. Instead `to_snap`'s slow arm
  snapshots such a generator as an inert **`Nil` placeholder** — a task that never touches it runs **clean**,
  and one that **reaches** it faults recoverably **at the use site** (`cannot iterate over nil`), byte-identical
  on both engines. (Fault only when reached; the frame-local crossing, by contrast, rejects eagerly at the
  `to_wire` serialize point because it crosses only the value actually sent.) (The earlier **Option-B
  reach-gate** model — which scanned each task for a *possible* reach and faulted it — is **retired**:
  by-value crossing removes the "why can a frame-local generator cross but not a module-global one?" drift.)
- **Captured locals are isolated copies; module globals are frozen.** Reassigning (or in-place mutating)
  a captured **local** inside a task is fine — it mutates that task's own copy, invisible to the parent (so
  it can't share state by accident). Both **reassigning** a captured **module global** AND **mutating it in
  place** (`.push`/`.add`/`m[k]=v`/`s.field=x` on a module-global aggregate — B3) inside a task are a
  **compile error** (they would diverge — shared on the serial engine, a worker snapshot on M:N), with the
  fix in the message (`use a Shared or Channel`). Either way, to produce output visible to the parent, use a
  `Channel` or a `Shared`. (Reads of both are always fine.) The in-place-mutation gate covers the direct
  `spawn:` block, `Executor.submit` closures, and closures declared inside a `spawn:` block; a handful of
  fully indirect forms (a top-level-bound closure spawned by name, a closure reached through a captured
  struct field, callee-form *method*-mutation reached transitively through a `spawn f()` free fn, and a
  method-mutation through a task-local alias of the global — `local := xs; local.push(..)`) remain a
  documented v1 gap — see `docs/gaps.md §B3`.
- **Cyclic sendables round-trip (identity-preserving copy).** The airlock copies a sendable by a
  structural deep walk (`spawn` arg / `Channel.send` / `Shared(...)` / worker return / module-global
  snapshot). A value that is sendable-by-type but contains a **reference cycle** (e.g. `a.next = b;
  b.next = a`, a list holding itself) is deep-copied **identity-preservingly**: every container +
  closure/cell node earns a per-serialization `id`, a back-edge becomes a `WireValue::Backref(id)`, and
  the receiver ties the knot — so the copy on the other side is an independent cyclic value with the same
  shape (like Python's `deepcopy`, which memoizes). **Byte-identical on the serial and M:N engines.** The
  depth-guard (`MAX_STRUCTURAL_DEPTH = 10000`, the same bound the display / `==` paths use) now fires
  **only** for a genuinely >10000-deep **acyclic** nest — a **recoverable** `maximum structural depth
  (10000) exceeded (cyclic data structure?)` fault, re-stamped with the real airlock site and catchable
  by `recover:`. (A cycle threaded through a live generator's parked frame — the generator carries no
  wire id — is instead caught by the generator-on-stack guard, a clean `a generator cannot be sent
  across tasks as part of a reference cycle` fault.)
  Large-but-**shallow** data (e.g. a 100k-element list) crosses fine — the counter measures nesting
  depth, not element count.

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
> **Captures cross by value on both engines:** `submit` wires the closure through the same by-value
> airlock (`wire_callable` → `to_wire`) that `spawn` uses, so its captures are deep-copied and isolated
> at submit time and the generator sendability enforcement runs — identically on the
> cooperative default and `--parallel` (serial == M:N for every submitted closure). A mutation of a
> captured collection between `submit` and the drain is NOT observed by the job.

The sanctioned tool for "a task that **outlives its scope** / runs in the background" is **not** a
nursery and **not** an unscoped `spawn` — it is a distinct, **explicitly-owned `Executor`**: a
long-lived task pool / work queue you create, submit detached work to, and reap yourself. This is
precisely Java's "use a separate executor / work queue" and Elixir's `Task.Supervisor`. It keeps
`parallel:` pure (always structured, always joins) and confines all "outlives-its-scope" work to one
visibly-owned place.

```chezzi
import std.concurrency             # Executor (like Shared/RwShared/Atomic) is not a global builtin

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

> **Historical design-log.** The `**Interp** src/interp/*` bullets below record the *original* plan,
> which targeted the since-**removed** tree-walk interpreter. That engine no longer exists — the
> bytecode VM is the sole engine and parity is now serial-VM (`parallel=false`) vs M:N-VM
> (`parallel=true`). Read the `src/interp/*` references as planning history, not current paths.

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
- **Checker methods** `src/checker/expr.rs` (`infer_method_call`): `Ty::Channel` arm resolving via
  `native_handle_method("Channel", …)` against the file-backed `native struct Channel[T]` method table in
  `std/prelude.chz` (`send(T)->nil`, `recv()->T`, `len()->int`, … — the retired bespoke
  `channel_method_sig`'s replacement); `Channel()` constructor (builtin free fn, mirror `Set()`).
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
- **Tests:** cross-task increment via `Shared`; an in-task mutable struct is **not** sendable while `Shared` is.

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
| **A3a** | Reject a non-sendable **read through a nested closure** inside a `spawn:` block. | ✅ **enforced for a non-sendable local** — emergent from the persistent `capture_floors` + the `infer_ident` read gate. **Updated (B3.3 / Task 2a):** a plain **closure** read through a nested closure is now *accepted* (closures cross by value), so the pin is `read_captured_capturefree_closure_through_nested_closure_in_spawn_block_ok`. |
| **A1** | `Channel.try_recv() -> T?` — a **non-blocking poll** (`Some(v)`/`None`, never blocks/faults/suspends). Originally deferred (its motivating mid-flight-producer scenario needed the engine), un-deferred once B1/B2 landed. | ✅ **done, both engines, parity-tested** (it never suspends, so both engines run it identically — see [§5](#5-channelt--a-mailbox-outside-every-heap)). |

> *Dropped from Group A, shipped in B3.6:* **A3b** (`Executor.submit` capture sendability gate). The
> submitted closure now crosses **by value on both engines** (`wire_callable` → `to_wire`), so a
> non-sendable capture (a live generator, a native handle) faults at submit — identically on the
> cooperative default and `--parallel`.

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

**Decision (historical) — interp B1/B2 was a deliberate NON-GOAL.** The tree-walking interpreter (since
removed) was kept frozen at the **sequential concurrency subset** as the differential-testing parity
oracle for the non-blocking language surface — its value was catching VM / GC / compiler bugs, not
running concurrent workloads. Suspendable execution would have required stackful coroutines or a full
CPS rewrite of `eval`, a cost the oracle did not need. **The VM is the sole engine.** Today's parity
contract is between the two VM schedulers (serial `--serial` and default M:N); both run identical
bytecode for the sequential subset and diverge only in scheduling. (Historically, while the interp
existed, a **blocking `recv` was VM-only** — under the old `--interp` it faulted `deadlock`; A1
(`Channel.try_recv`), being non-blocking, ran identically on all engines and stayed parity-tested.)

**Landed (VM):** **B3** OS-thread multicore (the alternative bet, taken — B3.0–B3.6), **B4** real
`Shared`, **B5** real `Executor` pool (+ A3b) — then **Tier-D** rebuilt `--parallel` as an M:N
work-stealing scheduler, **complete through D6** (D0–D6 + owes #1/#2/#3; epoll/`std.net` netpoller
landed). Blocking `recv` inside a native callback (**D5 owe #3**) is **resolved** — see below.

**Cross-nursery wakeups — M:N RESOLVED, cooperative pending.** A fiber in an outer nursery being woken
(and *run*) by an inner one (the circular outer-sibling case — `examples/parallel_cross_nursery_circular.chz`)
is **fixed under `--parallel`** (the M:N engine): one VM-global `MnSched` with a `Vec<JoinScope>` flat
scheduler (each nested nursery is a scope enlisted into the same global run queue, with a scope-scoped
owner stop), plus early-enlisting an outer nursery's siblings so a nested owner — draining the GLOBAL
queue — runs them. The fix also routes the inline outer-body's own `send`/`close` through the held sched
(so they wake an enlisted, parked sibling), runs a `spawn:` issued *after* the enlist, and makes the
enlist atomic — see §11 below and [`docs/cross-nursery-flat-scheduler.md`](cross-nursery-flat-scheduler.md).
The cooperative `--serial` engine still serializes nested nursery levels, so the
same program **still faults `deadlock` on `--serial`**; the cooperative-engine flatten is a
**separate, later commit**. Workaround on the cooperative engine: keep mutually-dependent blocking tasks as
SIBLINGS in ONE nursery (the doc case C pattern).

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
    complete, then cleanup) — and identically for an **explicit** `parallel:` block: a `defer`
    directly inside the block flushes *after* the block's dedent join (its spawned children run to
    completion first, then the block's deferred cleanup), same order as the implicit body nursery.
    The report is emitted **per nursery** (innermost-first — two stacked
    nurseries print two lines), identically on both engines (serial `--serial` and default M:N). The
    **module** top-level nursery is the one exception: an uncaught *top-level* fault leaves it silent
    (it joins only on a clean run to program end). [resolved 2026-06-12 — see PROGRESS.md; previously
    the VM dropped these reports while the interp printed them.]
  - **Zero-overhead gate.** A body gets an implicit nursery only if it lexically contains a bare
    `spawn` (a compile-time pre-scan, `compiler::block_has_bare_spawn`); bodies without one emit
    byte-identical bytecode to pre-M-C. Implemented as a single join site — the compiler emits the
    opening `Op::EnterNursery` and flags the `Proto`; the VM's `do_return` joins for `return`/`?`/end.
    Implementation: `src/{checker,compiler,vm}`; tests in `vm::tests::implicit_nursery_*` +
    `examples/implicit_nursery.chz`.
- **Real concurrency (C5):** the Tier-A cooperative scheduler and/or Tier-C OS threads — true
  multicore and mid-flight task communication, behind the unchanged surface.
- **The `Executor` escape hatch (C5):** the separately-owned work queue for tasks that must outlive
  their scope — `submit` detached work, reap with `defer ex.shutdown()` (graceful) or `shutdown_now()`
  (cancel); program exit drains. Keeps `parallel:` pure and all background work visibly owned —
  see [§8](#8-daemon--background-tasks).
### Tier-D — M:N scheduler + async I/O (shipped — D1/D2/D5; design-log below)

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
  can't be snapshotted. **RESOLVED (D5 owe #3):** **Path A** moved the suspendable list HOFs into chezzi
  (`std/iter.chz` `map`/`filter`/`fold`/`reduce`, like BEAM's `Enum.map`) so a `recv` in their callback
  *parks* normally; **Path C** Go-`handoffp`-demotes the fiber to a thread for the intrinsically-native
  islands (a `recv`/`sleep_ms`/socket op / `wait` reached inside a Rust callback) instead of faulting.
  (Path B stackful was rejected.) Residual: a `recv` with no possible sender still *correctly* deadlocks.
  The same fiddly bit Go's runtime wrestles with for cgo. See [`concurrency-tier-d.md` § "D5 owe #3"].
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
flush-on-join) as the default and demote two-engine (serial-vs-M:N) parity to a *sequential-subset* contract run under
an explicit `--serial` flag. **(Settled for the CLI, 2026-07-13 — Interactive CLI milestone:** the
per-task buffer + task-order flush is a **TEST-harness** property of the captured sink the lib helpers
run, NOT a user-facing guarantee. `chezzi run` **streams**: prints are line-atomic and cross-task order
is nondeterministic on both engines, like Python/Go/Rust. Parity is asserted on the buffered sink,
which every test helper still uses.**)** Serial is **never deleted** — it stays permanently as the deterministic
parity oracle + reproducible-debug engine. Not a date; a checklist.

- **Reuse map for the implementer:** builtin method dispatch (VM `core_method` — `src/vm/mod.rs`); parameterized
  types (`Type::Generic` — `src/ast/mod.rs`; `Ty::List/Map/Set` — `src/checker/ty.rs`;
  `infer_method_call` — `src/checker/mod.rs`); the re-entrant call-into-Chezzi path (list HOFs) for
  `JoinNursery`; block parsing/scoping (`parse_block`, `exec_scoped_block`/`exec_block`, defer-scope
  markers); `Option` constructors (`some`/`none`, `alloc_enum`) for `recv`; a one-field mutable struct
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

- **Cross-nursery wakeups** *(RESOLVED under `--parallel` (M:N) incl. multi-level nesting + late-spawn; cooperative-engine flatten + a few narrow limits still open — see below)*.
  **Resolved (D0):** `wake_on_send` (`src/vm/mod.rs`) drains *every* scheduler level, so cross-level
  **wake-marking** works — a `send` in any nursery marks the blocked fiber ready wherever it parked. The
  **common case** (consumer in an *outer* nursery, producer in an *inner* one that finishes) works end to
  end: the inner nursery completes, the outer resumes, the consumer gets its value.
  **RESOLVED under `--parallel` (M:N):** the circular outer-sibling case — a fiber woken in an *outer*
  nursery being *run* by an inner one — is fixed by the flat scheduler (one VM-global `MnSched` with a
  `Vec<JoinScope>`; each nested nursery is a scope enlisted into the same global run queue; scope-scoped
  owner stop). Early-enlisting an outer nursery's siblings lets a nested owner draining the GLOBAL queue
  run them (`examples/parallel_cross_nursery_circular.chz`, `..._fanout.chz`). The full fix also routes
  the **inline outer-body's** own `send`/`close` through the held sched so they wake an enlisted, parked
  sibling (`..._inline_send.chz`, `..._inline_close.chz`), runs a `spawn:` issued *after* the enlist
  (`..._late_spawn.chz`), and makes the enlist atomic. A genuine no-sender deadlock still faults — the
  global predicate fires unless every still-incomplete scope is merely *awaiting the builder's join*
  (a live external feeder); see `MnSched::all_incomplete_awaiting_builder`. **Independent / normal
  multi-level nesting now RUNS** (the old "2+ enlisting levels" gate is gone): any depth of nested
  `parallel:` with sibling and late `spawn:`s matches the cooperative engine — every pending outer
  nursery enlists as its own scope, and a late `spawn:` into a middle nursery runs on the held flat
  sched as a fresh trailing scope via `register_scope_seeded` (atomic register+seed under one lock,
  un-latches a stale `terminate`) so the inline owner runs it — no clobber, panic, drop, or deadlock-veto
  race. Goldens: `examples/parallel_cross_nursery_multilevel.chz`, `..._late_spawn_parked.chz`.
  **Remaining narrow limits (revisit only if they bite real programs; full brief +
  reproductions in [`docs/cross-nursery-flat-scheduler.md`](cross-nursery-flat-scheduler.md)):**
  - **Contended shared channel across nested nurseries** — 2+ live receivers racing ONE channel across
    nested `parallel:` scopes is concurrent-divergent BY DESIGN: under `--parallel` delivery order may
    differ from the cooperative engine, or it may deadlock-fault. It is NOT gated and NOT special-cased;
    it only must never PANIC and never HANG (completes or faults `deadlock` cleanly — see
    `parallel_cross_nursery_contended_never_panics`). Same gap the cooperative flatten would close.
  - **Cooperative (`--serial`)** still serializes nested nursery levels, so the same program
    **still faults `deadlock`** there — the cooperative-engine flatten is a separate, later commit.
    Workaround: keep mutually-dependent blocking tasks as SIBLINGS in ONE nursery
    (`examples/parallel_cross_nursery_ok.chz`, the doc case C pattern).
  - **Inline outer-body *blocking* recv (case B)** — the cross-nursery fix is **wake-side only**. The
    inline `parallel:` builder body runs with no scheduler frame, so a *blocking* `recv`/`for v in ch:`/
    `wait:` issued directly in the body (not inside a `spawn:`) still faults with a "deadlock — no
    runnable task can send" (the diagnostic points at the `spawn:` fix). Put blocking work in a `spawn:`.
  - **Eager (per-connection) nurseries** run on their OWN private `MnSched` (`activate_eager_nursery`,
    for liveness — no inline worker between Enter/Join). A cross-nursery wake **OUT OF** an eager body
    (child→parent: a `send`/`close` inside the eager body waking a receiver parked in the parent) is now
    routed via `MnSched::parent_wake` (gaps.md B5 — golden
    `parallel_cross_nursery_nested_send_to_outer_recv.chz`). A wake **INTO** an eager body (parent→child:
    receiver parked inside, sender in an ancestor) and sibling-eager→sibling-eager are still a separate
    limit (timing-divergent — complete or deadlock-fault cleanly).

  **(Symbol note:** the old `pick_runnable` linear scan named in earlier drafts is gone — replaced by
  D0's `ready`-set.)

- **`recv` inside a native callback** *(D5 owe #3)* — **RESOLVED (both paths landed).** A blocking
  `recv`/I/O reached inside a Rust callback (list HOFs, `Shared.update`, socket ops, `sleep_ms`, a
  `wait`) once faulted `deadlock`, because that loop/recursion state lives on the host stack and can't
  be snapshotted into a fiber. (Backstory: "mooted by B3" under the B3.3 thread-per-task model, then
  **un-mooted by D2's M:N transition** — fibers park by snapshot, so a native frame breaks the chain.)
  Fixed two ways: **Path A** moved the suspendable list HOFs into chezzi (`std/iter.chz`
  `map`/`filter`/`fold`/`reduce`, like BEAM's `Enum.map`) so their callback `recv` *parks* normally
  (`d5_owe3_recv_in_iter_map_callback_parks`); **Path C** Go-`handoffp`-demotes the fiber to a thread
  for the intrinsically-native islands (`d5_owe3_path_c_*_demotes`). Path B (stackful) rejected.
  Residual: a `recv` with no possible sender still *correctly* deadlocks
  (`d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks`). The lone surviving same-box hazard is
  `Shared.update`'s hold-and-wait (won't-fix by design, separate). Details in
  [`concurrency-tier-d.md` § "D5 owe #3"](concurrency-tier-d.md).

- **`Channel.close()` + closed-channel semantics** — **LANDED** (both engines, branch
  `feat/channel-close`). The natural complement to B1/B2's blocking `recv`: a consumer looping past the
  producer's last value used to deadlock-fault. Resolved surface (decided with the user):
  - **`for v in ch:`** — blocking iteration; drains buffered + future values, ends cleanly when
    closed-and-drained (Go's `for v := range ch`). The headline consumer form.
  - **`close()`** — idempotent, wakes every parked/demoted receiver.
  - **`send` after close → faults** `"send on a closed channel"`; **`recv` on closed-and-empty →
    faults** `"receive on a closed channel"` (drains buffered first).
  - **`try_send(v) -> bool`** — the non-blocking partner of `send` (mirrors `try_recv` vs `recv`);
    `false` = the send can't proceed: the channel is **closed** OR a **bounded** channel is **full**.
    `true` once queued. (An unbounded channel is never full, so there `false` means only closed.)
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
