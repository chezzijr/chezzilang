# Chezzi — Concurrency & Parallelism (`spawn` / `parallel:`)

> **Status:** canonical design doc — **implemented through Tier-D**. The surface (`spawn`,
> `parallel:`, `Channel`/`Shared`/`Executor`) ship; the M:N engine is a real OS-thread
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
- **Staging (this doc's central decision):** ship the full *surface + type system + engine* on
  a **sequential, run-to-completion executor** first (milestones **C1–C4**); add real fibers /
  multicore later (**C5**) behind the same syntax. **C5 shipped**: the M:N OS-thread engine is the
  default, tasks start at their `spawn` and interleave, and the surface did not change — exactly as the
  staging bet predicted. See [§4](#4-execution-semantics) and [§9](#9-implementation-roadmap-c1c5).

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
(never duplicated, never dropped); **which** task gets it is nondeterministic — order it
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
                                    # the spawn boundary, so the parent's `counter`
                                    # stays 0 — no shared write, no race.
print(counter)                     # 0  — to actually share, use a Shared[int] (below)
```

**Module globals isolate per task.** A `spawn`ed task gets its own deep copy of every
module global (and of every captured local) — mutating one inside a task never propagates out.

**The checker warns when you read the lost value.** Writing a captured binding inside a `spawn:` body
and reading it again after the join emits a non-fatal warning naming the binding and citing the write's
line (exit code unchanged — the isolation is deliberate). A `Shared`/`RwShared`/`Atomic`/`AtomicInt`/
`Channel` write is silent: those cross by handle, so the write really is visible. So is a parent-side
write that replaces the WHOLE binding (`xs = [...]`) — but not `xs.push(v)` or `n += 1`, which read the
stale copy before writing it and so warn at the write. The rule has **seven** deliberate ceilings, every
one of them under-warning rather than over-warning (per frame, so it neither enters nor leaves a nested
`fn`; lexical, not dataflow; builtin containers only; keyed by bare name, so any fresh binding of the
name clears it — though the taint carries a scope coordinate, so a *block-local* shadow's taint is never
charged to the outer binding; a partial `m[k] = v` / `p.f = v` in the parent untaints silently; a write
made only through a closure or nested `fn` declared *inside* the task is not tainted at all; and a
partial *read* of a partial write declines, so a task-side `p.count = ...` read back as `p.name` is
silent). Full rules and the reasoning for each:
[`syntax.md` §capture](syntax.md).

**The copy is taken FRESH, per task, at its `spawn` — at every depth.** A task sees the values current
when it was spawned (the Go rule: a goroutine reads whatever a package-level var holds when `go` runs).
So a global first initialized *after* an earlier nursery is visible, a mutation by ordinary sequential
code *between* two nurseries is visible to the second nursery's tasks, two spawns straddling a global
assignment see the old and the new value respectively, and a task that mutates its own copy and then
opens a **nested** `parallel:` gives its children the **task's** current view, not the parent module's.
In-place mutation of an aggregate global (`q.push(x)`, `m[k] = v`, `p.x = 1`) is picked up by the next
nursery too. An `Executor` job has no nursery, so it sees the globals as of the instant it **starts**,
which under eager execution is its **`submit`**. Reading a global that is mutated between the `submit`
and the `shutdown()` is racing a running job, not observing a defined state. The view is identical at
every `--threads`.

One sub-statement caveat, because in-place aggregate mutation writes no module slot for the runtime to
notice: **within one nursery**, consecutive `spawn`s share one view, which is refreshed by a global
*assignment* (`g = …`, `q = […]`) but not by an in-place mutation. So if a `spawn` is followed by
`q.push(x)` and then another `spawn` into the SAME nursery — with no assignment in between — the second
task still sees the pre-`push` `q`. Every task's view is a single coherent instant (never a mix of old
and new values), and it is the same instant; only its freshness stops at the last
assignment / nursery open. Open a new `parallel:`, assign the global, pass the value as a spawn argument,
or send it through a `Channel` if a task must see an in-place mutation made mid-nursery.
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
forget, no leak.

### A spawned task's two error channels

They are **not** the same channel, and the difference is Go's, not an accident:

| what the spawned callee does | what the nursery does |
|---|---|
| **returns a value** — including an `Err(e)` from a `-> T!` callee | **discarded.** `spawn f(x)` is a statement; there is no place for a return value to go, exactly like Go's `go f(x)`. |
| **faults** (a runtime error, an uncaught `panic(...)`) | **aborts the nursery** and the program, rc=1 — as a panicking goroutine takes down a Go process. |

Measured Go 1.26.5, the ancestor that owns this seam:

```go
// program 1 — the error channel: the returned value is dropped, main completes.
func main() {
	var wg sync.WaitGroup
	wg.Add(1)
	go func() { defer wg.Done(); _ = errors.New("boom") }()
	wg.Wait()
	fmt.Println("main finished normally")   // prints; rc=0
}

// program 2 — the fault channel: the panicking goroutine kills the process.
func main() {
	go func() { panic("boom") }()
	time.Sleep(200 * time.Millisecond)
	fmt.Println("main continued")           // NEVER prints — "panic: boom" + stack, rc=2
}
```

(rc=2 is the `go build` binary; `go run` wraps it and exits 1 while printing `exit status 2`.)

This is precisely why `errgroup` exists in Go and why `spawn f()` in Chezzi is a **statement**, not an
expression. **To observe what a task produced, collect it explicitly** — send it on a `Channel[T]`, or
reduce it into a `Shared[T]`/`AtomicInt`, then read after the join:

```chezzi
served := AtomicInt(0)
parallel:
    for _ in 0..n:
        spawn client(addr, served)     # a client bumps `served` only when it really succeeded
print(served.load())                   # the nursery has joined — the count is final and real
```

`examples/echo_server.chz` is the worked version. A `Result` returned by a spawned callee and never
collected is silently gone, so **never conclude "it worked" from a nursery that merely finished** —
count what actually happened.

`recover:` + `defer` compose with the fault channel only.

### Why a nursery, not a `WaitGroup`

`WaitGroup` is the thing Go regrets: manual (`Add`/`Done`/`Wait` — forget one and you leak or
deadlock) and **unstructured** (a goroutine's lifetime isn't tied to any scope, so it can outlive the
function that spawned it). The nursery *is* the join — no counter to mismanage — and a task **cannot
outlive its `parallel:` block**, so leaks are structurally impossible and a *fault* has an obvious home
(the join surfaces it). A **returned `Err` still does not** — the nursery discards it, exactly like Go;
see "A spawned task's two error channels" above and collect it yourself.
This is **structured concurrency**, the modern consensus that postdates Go: Python `trio` /
`asyncio.TaskGroup`, Kotlin `coroutineScope`, Swift `TaskGroup`, Java `StructuredTaskScope`.

### Nursery scoping rules

- **`spawn` is legal anywhere in a function body or at the module top level** (M-C, §10). Every
  function body and the module top level is an *implicit nursery*; a bare `spawn` binds to it and
  joins at the body's `return`/end (the module top level joins at program exit). An explicit
  `parallel:` is an inner sub-nursery that joins earlier, at its dedent.
- **A task *starts* at its `spawn`, not at that join** — Go's `go f()` ([§4](#4-execution-semantics)).
  The join is a **completion** barrier and nothing more: it guarantees the task is *done* by the
  dedent/`return`, never that it could not have started (and printed) earlier.
- **Function-boundary rule (the safety invariant).** A `spawn` binds to a nursery **within its own
  function** — it can **never** reach an enclosing function's or the module's nursery. This is what
  stops a task spawned in `fn worker()` from outliving `worker`'s return:

  ```chezzi
  fn worker():
      spawn helper()       # OK since implicit nurseries landed: `worker` gets its OWN implicit
                           # nursery and joins helper at its return. It does NOT reach main()'s.
  ```
  **This used to be an error and no longer is** — the rule it enforces is unchanged, but implicit
  nurseries satisfy it by *giving* `worker` a nursery rather than by rejecting the `spawn`. The
  invariant is the same either way: `helper` cannot outlive `worker`.
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

### When a task starts, and when it is done

Two separate guarantees, and conflating them is the mistake this section exists to prevent:

- **start** — a task starts **at its `spawn`**, running concurrently with the rest of the nursery
  body. This is Go's `go f()`.
- **completion** — the nursery's join (an explicit `parallel:`'s dedent, a function's `return`/end,
  program exit for the module top level) waits until every task of that nursery has finished. It
  guarantees the task is *done* by the barrier; it says nothing about when it *started*.

How a `parallel:` block runs on the M:N engine (`chezzi run` — the default):

1. Executing `spawn X` **evaluates the call's callee + arguments eagerly, at the spawn point** (Go's
   arg-evaluation timing), **deep-copies them across the airlock** ([§7](#7-sendability)), and
   **starts** the task on the innermost nursery. **The parent then continues to the next statement
   immediately** — `spawn` does not block.
2. The task runs **concurrently** with the statements that follow it and with its siblings. There is
   no FIFO order between tasks and no defined order against the parent's own statements.
3. The first task to error **aborts the remaining siblings** and propagates out of the `parallel:`
   statement (composes with `recover:` / `defer`).
4. At the **barrier**, the parent waits for every task, then proceeds past the block.

```chezzi
print("A")
spawn:
    print("SPAWNED")
time.sleep_ms(300)
print("B")
```

`chezzi run` prints `A SPAWNED B` — the same as the paired Go program (`go fmt.Println("SPAWNED")`,
go1.26.5). The task runs *during* the sleep; it is not deferred to the end-of-main join.

Because a task is already running, a **live consumer can wait on a producer mid-flight**:

```chezzi
ch := Channel[int]()
spawn:
    ch.send(1)
print(ch.recv())    # 1 — the spawned sender is running, so the recv is satisfiable
```

### Output ordering: streaming CLI vs buffered sink

A concurrent task's stdout is a **private per-task buffer flushed at the join, in task order**, under
the **buffered** sink — every Rust test helper and every embedder. `chezzi run` **streams** instead:
a `print` reaches the real fd at the moment it runs (line-atomic, never withheld), so on the real CLI
a task's output interleaves with the parent's and with its siblings' in **completion** order.
Cross-task print order is nondeterministic by contract; do not build a program (or a golden) on it.
`examples/implicit_nursery.chz`'s header comment is the worked statement of this.

**This covers `Executor` jobs exactly as it covers `spawn`ed tasks** — the two are not different
here. An `Executor`'s slots are indexed by *submission* order, and that is what decides which fault
wins and what order the **buffered** sink emits; it is **not** a promise about live CLI output. Three
jobs that each `print` once and return will usually come out in submission order simply because they
are too short to overlap, and that is luck, not a guarantee: give them real work and they come out in
completion order. The ancestor is the same — CPython 3.14.6 `ThreadPoolExecutor` with
`shutdown(wait=True)` over three jobs doing a few million loop iterations each held submission order
in **0 of 30** measured runs. If you need ordered output, collect the results (`submit_result` /
`std.concurrency.task`) and print them yourself after the join.

### Early exit from a `parallel:` cancels silently

A `break`/`continue`/`return`/fault that leaves a `parallel:` body early **cancels** the nursery's
tasks, and prints **nothing** about it. There is no "N pending task(s) cancelled" report: a task
starts at its `spawn`, so there are no unstarted tasks to count, and any residual number would be a
race. `trio` and `asyncio.TaskGroup` are silent here too.

---

## 5. `Channel[T]` — a mailbox outside every heap

A `Channel[T]` is **not** an object in any task's heap. It is a separate runtime structure — a
**mailbox** with its own queue — that sits outside both heaps. What each task *holds* is a lightweight
**handle**; the handle is the only shared thing, and values flowing through are **moved/copied** at
the airlock, never live in two heaps at once.

```chezzi
ch := Channel[str]()       # construct; capitalized like Shared[T] / Option[T]. Unbounded FIFO.
rch := Channel[int](0)     # RENDEZVOUS: send blocks until a receiver is already waiting (Go's make(chan T))
bch := Channel[int](2)     # BOUNDED: holds ≤2 queued messages; a 3rd `send` blocks until a `recv` frees a slot
ch.send(x)                 # x moved/copied OUT of the sender's heap → channel queue
v := ch.recv()             # value reconstructed IN the receiver's heap
opt := ch.try_recv()       # non-blocking poll: Some(v) if queued, None if empty
n := ch.len()              # current queued count
c := bch.cap()             # capacity: 2 here; 0 for a rendezvous Channel[T](0); -1 for an unbounded Channel[T]()
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `send` | `send(self, v: T) -> nil` | enqueue (move/copy at the airlock); the sender MAY keep using the value — the crossing copies, so its later writes are simply not seen by the receiver. On a **bounded** channel a `send` **blocks/parks** while the queue is at capacity (backpressure), resuming once a `recv` frees a slot — the send-side mirror of a blocking `recv`. On a **rendezvous** channel (`cap == 0`) a `send` blocks until a receiver is already waiting, exactly like a bounded `send` at capacity 0 conceptually would, except capacity 0 is otherwise inexpressible as `queue.len() < cap` |
| `try_send` | `try_send(self, v: T) -> bool` | **non-blocking** send: `true` once queued, `false` if the send can't proceed — the channel is **closed**, a **bounded** channel is **full**, or a **rendezvous** channel has no receiver already waiting. Never blocks/parks |
| `recv` | `recv(self) -> T` | dequeue (FIFO); blocking surface (see below) |
| `try_recv` | `try_recv(self) -> T?` | **non-blocking** poll (A1): `Some(v)` if queued, `None` if empty — never blocks, never faults, never suspends a fiber. Drain a mailbox without guarding on `len()` |
| `len`  | `len(self) -> int` | queued count — use to guard a `recv` |
| `cap`  | `cap(self) -> int` | capacity: `-1` for an unbounded `Channel[T]()`, `0` for a rendezvous `Channel[T](0)`, or the bound passed to `Channel[T](cap)` |

- **Three shapes: unbounded, rendezvous, bounded.** `Channel[T]()` is an **unbounded FIFO** — `send`
  never blocks. `Channel[T](0)` is a **rendezvous** channel — `send` blocks until a receiver is
  already waiting (Go's `make(chan T)`; `try_send` declines with no waiting receiver). `Channel[T](cap)`
  (`cap > 0`; a negative `cap` is a runtime fault) is a **bounded** FIFO: a `send` blocks/parks once
  `cap` messages are queued and resumes when a `recv` frees a slot (Go's buffered channel).
  Backpressure changes *which* task runs *when*, never the value sequence a consumer sees, so a
  bounded or rendezvous channel gives the same value sequence at every worker count, by the same
  argument as a blocking `recv`. A full/rendezvous `send` with no possible consumer (top level, no
  nursery, or inside a native callback) is a **deadlock fault**, not a silent over-fill or hang.
  As with `try_recv`, `try_send`'s full-vs-not decision under multi-sender contention is nondeterministic
  — the same class as `try_recv`'s `None`-vs-`Some` under contention; it is not "fixed".
- **DIVERGENCE from Go: `Channel[T]()` is NOT `make(chan T)`.** Go's no-argument channel is the
  rendezvous shape; Chezzi's no-argument `Channel[T]()` is UNBOUNDED instead, and this is
  DELIBERATE — see `## Decisions` in TICKET-028. A Go programmer porting `make(chan T)` should write
  `Channel[T](0)`, not `Channel[T]()`. Reach for `Channel[T](cap)` or `Channel[T](0)` by default: an
  unbounded channel is what lets a producer outrun a consumer with no backpressure at all — the bug
  class a bounded/rendezvous default shape exists to prevent.
- **`recv` on an empty channel BLOCKS** until a sibling sends. It faults `recv on an empty channel:
  deadlock` only when the run is provably stuck — every counted party blocked with no satisfiable
  wait — which is Go's own detector (`fatal error: all goroutines are asleep - deadlock!`). Since
  §2c1 a `spawn`ed producer is already running when the body reaches its `recv`, so Go's plainest
  channel idiom works verbatim:

  ```chezzi
  ch := Channel[int]()
  spawn:
      ch.send(1)
  print(ch.recv())     # 1
  ```
- **Move-on-send** = Go's send without Go's sharing. Nothing is *enforced* — there is no Rust-style
  move checker here, and a sender that keeps using the value it sent is legal and safe: the crossing
  deep-copies, so the two sides simply stop being the same object. Measured: `ch.send(xs)` then
  `xs.push(3)` leaves the sender with `[1, 2, 3]` and hands the receiver `[1, 2]`.
- **Channels are themselves sendable** — pass a `Channel` over a `Channel` for reply channels.
- **`try_recv() -> T?` is shipped (A1).** The non-blocking sibling of `recv`: it
  pops-or-returns-`None` and never blocks, faults, or suspends a fiber. With B1/B2's blocking `recv`, a fiber
  can also drain a mailbox's residue after a blocking `recv` resumes. `recv -> T` stays primary; reach
  for `try_recv` to poll without guarding on `len()`.

### 5a. `std.concurrency.pmap` — scoped parallel map (the ergonomic wrapper)

The report-channel + one-`spawn`-per-element + join + reassemble pattern is common enough that
`std.concurrency.pmap` bakes it in (pure Chezzi over a `parallel:` nursery + `Channel`):

- `pmap[T, U](xs, f) -> List[U]` — spawn a task per element, run `f` in parallel, results in
  **submission order**.
- `pmap_limited[T, U](xs, f, limit) -> List[U]` — same, capping in-flight `f`-executions at `limit`
  via a channel-as-semaphore token bucket (also the standard **concurrency limiter**; `limit > 0`).

Determinism comes from reassembling by submission INDEX (`sort_by_key`), never completion order — so
the result is byte-identical at every worker count. The nursery lives inside the helper and joins
before the collect (structured concurrency — a task can't outlive the call); `f` crosses the airlock
into each task by value. See `docs/stdlib.md` for signatures.

### 5b. `std.concurrency.task` — result handles for `Executor` work

Bare `Executor.submit(f)` is fire-and-forget — nothing comes back. The result-returning primitive is
`Executor.submit_result[T](f: fn() -> T) -> Channel[T]`: submit `f` and get a cap-1 `Channel[T]` you
`.recv()` for its result after `shutdown()`. `std.concurrency.task` wraps that channel in a
future-style handle (memoization + readiness poll):

- `submit_task[T](ex, f) -> Task[T]` — submit `f` detached, get a handle (builds over
  `ex.submit_result(f)`). The work starts at the `submit` and is waited for by `shutdown()` (or the
  program-exit join). Read the result AFTER that call.
- `Task.get() -> T` — block until the result lands, then return it; **memoized** (idempotent).
- `Task.done() -> bool` — non-blocking readiness poll.

Canonical shape: submit all → `shutdown()` → `.get()` each. **Determinism rule:** a task's value is
deterministic (`f()`); only its *timing* varies at runtime (the OS-thread workers race), so `.get()` is
byte-identical across runs **as long as you await in a fixed (submission) order**. There is deliberately
**no** `join_next()`/select-on-completion — completion order is nondeterministic and would break that
determinism.

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
plain float arithmetic. **`cas` compares *structurally* — it never calls a user `eq`** (M23): the compare
happens under the value's lock, and re-entering user code there could deadlock. Rather than let
`a == b` and `atom.cas(a, …)` disagree, a payload type that **reaches** a user `eq` is **rejected at
check time** — its own (`Atomic[K] payload defines its own 'eq' …`) or one on any element, entry, tuple
slot, struct field, enum payload or newtype underlying the structural compare would recurse into
(`Atomic[List[K]] payload reaches 'K', which defines its own 'eq' …`); use `Shared[K]`, which has no
`cas`. Because no checker walk can see through a protocol existential payload, the VM ALSO switches the
`eq` hook off for the compare — the no-user-code-under-the-lock property is enforced, not assumed. Each method is a single
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
`Shared.update`): under `--parallel` the whole `write`/`set` is serialised by the box's **update
guard** (a process-wide wait-for graph, not a bare lock) so concurrent writers can't lose each other's
updates — a `set` racing an in-flight `write` **blocks** behind the guard and lands, it is never
silently overwritten. `read` and `write` still copy the value out of the `RwLock` and **drop that
value lock before running the closure** (the `RwLock` guard is not reentrant), so a closure may freely
re-enter `get`/`read` — or `write`/`set` on a **different** box.

> **Reentrancy fault (same class as `Shared.update`):** a closure passed to `write` (or `set`) that
> re-acquires the **same** `RwShared`'s update guard — `write` or `set` inside `write` — is the guard
> registry's length-1 wait-for cycle and **faults** (`"already holds the update guard"`), instead of
> hanging or silently losing the inner write. A genuine cross-box wait-for cycle (e.g. two boxes each
> written from inside the other's closure) faults the same way instead of hanging undetected; two
> tasks that merely contend for different boxes are not a cycle and still complete normally. `write`
> nested in `read` is the one legal same-box reentrancy — `get`/`read` never take the update guard, so
> they still read the pre-guard value and cannot fault or deadlock.

> **A guard wait does not cost an OS thread unless it is long.** A contended `set`/`update`/`write`
> waits in place on its worker for up to 5 ms (`GUARD_DEMOTE_BUDGET`, `src/vm/core.rs`) before it
> demotes the worker and spawns one replacement OS thread — almost every wait is microseconds, so
> almost none pays for a thread. Demoting on every acquire instead made 50 000 one-`update` fibers
> exhaust a 32 768-task ceiling and never finish (TICKET-016).

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

> **GC cost of holding a big container in a box (gaps.md W6-7, fixed 2026-07-27).** The stored value
> lives outside the GC heap as one wire payload, and the collector used to re-walk that whole payload
> on **every** GC pass — so a traversal that allocates per step paid O(payload) per collection and the
> read-view came out **quadratic** in the container's size (a 200k-element box: 1.77 s vs 0.18 s for the
> same work on a plain `List`). Each core now caches whether its payload can root a heap object at all;
> a pure-data payload is **skipped**, so the per-pass cost is O(1) and *holding* a big container in a
> `Shared`/`RwShared`/`Channel` costs the same per GC pass as holding it in a plain `List`. Memory was,
> and stays, O(1) per traversal.
>
> **Reads are O(1); each *store* pays one walk.** `set`/`write`/`send`/`store` summarises the new payload
> once (O(payload), ~+20% on a 100k-element `RwShared.set` — measured in `docs/benchmarks.md`), so a
> rebind into a box is *not* as cheap as rebinding a plain `List`. The walk runs **before** the value lock
> is taken, so it never lengthens the window in which a writer blocks the readers.

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
deadlock. A waiter that owns its OS thread — an eager `Executor` job, or the top-level `main` thread,
**including inside a native callback** — does **not** park: it blocks with the timer as one more arm,
so a sibling value that arrives first wins (`gaps.md` **W7-14**).

**One shape still inline-sleeps.** A `wait:` reached in the INLINE body of an outermost `parallel:`
builder has no worker loop under it to drive a park, so a live timer arm there sleeps to the deadline
and takes it **without re-polling siblings** — a runnable sibling that could satisfy another arm is
stranded until the timer fires. `docs/gaps.md` **N10** records exactly this.

> **v1 limitation:** a `timer.recv()` reached *inside a native callback* (a `Shared.update` closure, a
> list-HOF, an `Executor` task) under `--parallel` pins that worker for the timeout rather than demoting a
> replacement the way `sleep_ms` does — sound (the other workers progress), just lower throughput. Reuse
> of the `sleep_ms` demote path is a future improvement.

---

## 6d. `wait` — racing multiple channel receives *(shipped)*

> **Status:** the surface and semantics below are **locked** (brainstormed 2026-06) and **implemented on
> the VM** (2026-06-13): lexer→parser→checker→VM, with non-blocking arms (`else:`, an already-ready
> arm, a `timer` arm) AND the **blocking multi-channel park** (landed 2026-06-13, the M:N park notes
> below): a blocking `wait` parks one fiber on N channels (woken by the first sender, swept out of the
> other buckets) instead of faulting. **SEND-arms** (`ch.send(v):`, deterministic source-order
> selection, a bounded send-arm parks until a receiver frees a slot) landed 2026-07-22 — see
> `examples/wait_send.chz`. Both examples carry byte-exact goldens.

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
`select` fairness. This is Chezzi's one principled divergence from Go here, and it is deliberate: a
`wait:` over already-ready arms gives the same answer on every run and at every worker count, which is
what makes a `wait:`-using program goldenable at all. The one shape that is still schedule-sensitive is
a live `timer` arm reached in an inline outermost-`parallel:` body (`docs/gaps.md` **N10**).
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
2. Poll arms in **source order** (deterministic priority, not Go's random fairness — so a `wait:` over
   already-ready arms gives the same answer on every run): the first **ready** arm wins. A recv-arm is ready with a queued value → pop, bind,
   run its block. A send-arm is ready when the channel can accept the value (bounded-with-space / unbounded /
   closed) → enqueue (or, if closed, *fault* `"send on a closed channel"`), run its block, bind nothing.
3. A **closed + empty** channel's *recv*-arm is **skipped** (option B); a closed *send*-arm is instead
   *selected and faults* (asymmetric — Go's panic-on-send-to-closed). If *every* recv-arm is closed+empty,
   there's no ready send-arm, and there's no `else`, the `wait` faults `"wait: all channels closed"`.
4. If no arm is ready: with an `else`, run it (non-blocking); otherwise **block** — park the fiber on *all*
   live arm channels and re-poll on the first wake. A recv-arm wakes on a **sender**; a bounded send-arm wakes
   on a **receiver** freeing a slot (reusing the bounded-`send` `wake_senders` / `recv_wake` path).

**Implementation notes.** *(Done. A new `Op::WaitPoll` holds the arm operands on the
operand stack — one slot per recv-arm (the channel), TWO per send-arm (channel THEN value, walked via a
per-arm slot cursor keyed on `WaitMeta.is_send`) — polls source order, and jumps to the chosen arm's body /
`else`, handles a live `timer` arm (see below), faults all-closed / send-to-closed, or parks. The cooperative
multi-channel park files the fiber under every arm key (`run_child` reads `wait_suspend`, a
`Vec<(handle, is_send)>`) and sweeps the index out of the other buckets on resume; the M:N park (below) does
the same with an `Arc<WaitPark>` token. The park-gap re-check is **kind-aware**, classifying each arm
READY / DEAD / LIVE exactly as the poll does: a recv-arm is ready with a queued value (a closed+empty
recv-arm is **DEAD**, not ready — it is skipped by the re-poll), a send-arm with a **free slot**
(`queue.len() < cap`, or unbounded) or on close. Using the recv predicate for a full send-arm, or calling a
dead recv-arm ready, would spin requeue→re-poll→re-park; but an **all-dead** re-check must requeue, or a
`close()` landing in the poll→park window is a lost wakeup (W7-2) — see §6d.)*

> **v1 limitation — send-arm inside a native callback.** A **full bounded** send-arm reached *inside a
> native callback* (a `Shared.update` closure, a list-HOF, an `Executor` task) can only block, and neither
> engine path can carry it: the M:N engine can't snapshot-park there and its in-callback demote path pops
> arm queues (recv semantics). So a `wait` with a live send-arm on that path **faults** — with the
> full-send-in-callback message `chan_send_step` already raises — rather than blocking. Same class as the
> existing in-callback full-`send` / `timer.recv()` v1 limits; the upgrade path is a demote-in-place send
> block.

> **Timer arm — timed-park, not inline-sleep.** A live `timer(ms)` arm is handled differently per
> *waiter*. A waiter with no worker loop under it — the INLINE body of an outermost `parallel:` builder
> — has no thread of its own, so it **inline-sleeps** to the soonest deadline then takes the timer arm.
> **Known-limit (`docs/gaps.md` N10):** the inline-sleep fires
> *before* the park, so if a **runnable sibling** could satisfy a non-timer arm, that waiter strands it
> and takes the timer where a party parked on a real worker thread would take the sibling's `send`
> instead (the worker-thread behavior is correct). N10 was closed for the now-removed cooperative-fiber
> scheduler, but this shape still lives on this surviving inline arm (`docs/future.md` §2b). A party with
> its own worker thread (`--parallel`) must **not** inline-sleep: that would
> pin the OS worker and strand a sibling `send` that lands mid-window. Instead it arms **one** background
> `timer::submit_at(deadline, send_wake(true))` on the soonest timer arm's own channel (guarded by an
> arm-once `ChannelCore.timer_armed` CAS so a re-park can't re-arm) and falls through to the normal
> snapshot-park, so the timer is just another bucket. A waiter that OWNS ITS OS THREAD — an eager
> `Executor` job, or the top-level `main` thread (with or without a native-callback frame under it) —
> needs no injected wake at all: it
> blocks in place with the timer as one more arm and simply **clamps** its wait to the deadline, so a
> sibling value that lands first wins and the timer is taken on the re-poll otherwise. It used to fall
> into the cooperative inline-sleep instead (`mn == None`), which took the timer without re-reading the
> siblings — `timer(300)` beat a value that arrived at 50 ms, where Go's `select` takes the value
> (`docs/gaps.md` **W7-14**, fixed 2026-08-04). Note the in-callback `main` case is admitted **only
> because the wait is timed** and therefore provably finite; an UNtimed `wait:` there still faults
> rather than blocking, because nothing could judge it as deadlocked. The `WaitPark` claimed-CAS sweep then picks **exactly
> one** of {a sibling `send`/`close` on any arm, the timer's own deadline `send_wake`} — a value arriving
> before the deadline wins the wait (the value is **not** stranded), the deadline wins only if nothing else
> did. The `native_reentry > 0` demote path threads the deadline into its bounded poll (channel scan first,
> so a real send still beats the timer).
- *Non-blocking* (`else` present) and the *poll* step reuse the existing `try_recv` path (a timer arm's
  `try_recv` is already deadline-aware) — straightforward in the VM. **(Done.)**
- *Blocking* (no `else`) needs a **multi-channel park** — **done in both schedulers.** A fiber parks in one
  `parked[key]` bucket (`MnSched::park`/`send_wake`/`close_wake`) and is woken by a send to that key. `wait`
  needs one fiber parked on N keys, woken by the first sender, and **swept out of the other N-1 buckets** —
  otherwise a later send wakes a fiber that already moved on. **M:N implementation (landed):** a
  `WaitPark { fiber: Mutex<Option<Fiber>>, keys, claimed: AtomicBool }` held once behind an `Arc`, with a
  `ParkedEntry::Wait(token)` filed in every `parked[key]` (the bucket is now
  `HashMap<usize, Vec<ParkedEntry>>` where `ParkedEntry` is `Recv(Fiber)` or `Wait(Arc<WaitPark>)`).
  `MnSched::park_wait` does the N-key gap re-check and files all N tokens + `parked_n += 1` (ONE fiber)
  under one core-lock hold. The re-check is **kind-aware, with three outcomes per arm** (mirroring
  `op_wait_poll` exactly, or the two disagree — that was **W7-2**): **READY** (a recv arm with a queued
  value / a tripped `done_latch`; a send arm with a free slot or a close) → requeue; **DEAD** (a
  closed+empty non-timer recv arm — the re-poll *skips* it, it only counts toward `all_closed`); else
  **LIVE**. Cancel → requeue. A dead arm is deliberately NOT "ready": requeueing on one dead arm among
  live ones would spin requeue→re-poll(skip)→re-park. But if **every** arm is dead the fiber must be
  requeued, because a `close()` landing in the poll→park window wakes an empty bucket and nothing will
  ever wake that key again — parking there is a stranded fiber the deadlock detector then (correctly)
  reaps as a spurious `deadlock:` fault. The all-dead requeue terminates: the re-run `WaitPoll` hits
  `all_closed` and faults `wait: all channels closed`. The
  first waker (in
  `send_wake`/`close_wake`/`cancel_drain`/`flag_deadlock`) CASes `claimed`, `take()`s the fiber, and
  removes its token from every other bucket by `Arc::ptr_eq` — all under the one lock, serialized with
  `park_wait`'s gap re-check (lost-wakeup-safe). Routed via `Disp::WaitPark(Vec<(key, core)>)` captured
  while the fiber heap is live (mirrors `Disp::Park`). The single-channel `recv` park stays the **1-key
  `ParkedEntry::Recv` special case** (alloc-free, provably unchanged — regression test
  `vm_wait_single_arm_recv_park_unchanged_under_parallel`).
- *A waiter with no worker loop* (the inline outermost-`parallel:` body): poll arms once in source order; first ready wins; else if `else`,
  run it; else if any arm is timer-backed **and the waiter has no worker loop to drive a park**
  (`!can_block_in_place() && !timed_block`, netio.rs:2599 — a party that owns its OS thread blocks in
  place with the timer clamped instead, `timed_block`, W7-14),
  inline-sleep to the soonest deadline and take that arm; else
  fault (all-closed or the existing deadlock fault). Deterministic → matches a worker-thread party's
  behavior **except** when a timer arm races a runnable sibling (`docs/gaps.md` N10, closed for the
  now-removed cooperative-fiber scheduler but still live here): the inline-sleep runs before the park,
  so this no-worker-loop waiter takes the timer where a worker-thread waiter takes the sibling — a
  known limit (the worker-thread behavior is correct). Proper fix = park first, inline-sleep the timer
  only when the quiesce path would idle-deadlock.
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

> **Tree-structured, registered like Go's (`context.WithCancel`).** `derive()` builds a **child** token
> linked to its parent: cancelling or timing out the parent cancels every transitively-derived child,
> root-to-leaves, while cancelling a child **never** touches the parent (one-directional). The
> registration is Go's: a child is sent into its **immediate** parent's `kids: Channel[Token]` registry
> — **O(1)**, no ancestor walk — and `cancel()` cascades **down**, draining each node's registry and
> marking every descendant it reaches (an explicit work-list, so tree depth is not call-stack depth).
> **`done()` cascades transitively** through that same downward walk: a manual `cancel()` at *any*
> depth above trips a grandchild's `done()`, so a task parked in `wait: leaf.done()` wakes on a
> grandparent cancel, not just on its immediate parent. The link is **live** — a parent flip is
> observed by an already-derived child, *including a child that crossed the `spawn`/`parallel:`/
> `Channel` airlock* — because the link is the `kids` channel plus each node's own `Shared` flag, which
> cross as live cores exactly like the flat token's `flag`, in **both** directions (a task on the far
> side of a `spawn` can `derive()` into a registry the canceller drains). A child inherits the
> **tightest** deadline (the soonest absolute deadline of itself and its ancestors), *materialised into
> its own `deadline` field* at derive time — so a timeout needs no cascade at all; a derived child of an
> already-elapsed timeout is cancelled at once with reason `"timeout"`. `reason()` reports the
> **nearest** cause — this token's own state first, else the ancestor's — and the first cause
> **latches** (Go): the cascade does not overwrite a descendant whose own deadline has already
> elapsed, and neither does `cancel()` on the token itself (`cancel.timeout(10)`, sleep past it, then
> `cancel()` → `"timeout"`, matching Go's `context deadline exceeded`).
>
> **A child also keeps a `parent` link, and that is the cascade's safety net — not a duplicate of it.**
> The downward cascade `try_recv()`s children out of the registry, which **removes** them, so a
> cancelling task torn down mid-cascade (a nursery sibling faults) takes the already-popped subtree
> with it, permanently — a later `cancel()` cannot recover it, the entries are gone. Measured on a
> 500-deep chain at `CHEZZI_THREADS=4`, one task calling `root.cancel()` while a sibling faults:
> without the link `leaf.cancelled()` was **false** 3/3 and a second `root.cancel()` still false (387
> of 501 tokens permanently uncancelled); with it, **true** 3/3, 0 of 501 lost. Go is not immune by
> having the same cascade shape — Go has no way to abandon a goroutine mid-function, so
> `cancelCtx.cancel`'s drain always completes; Chezzi's nursery teardown does abandon tasks. The link
> is only read on the fallback path, and it is **not** what the cubic v1 cost came from (that was the
> every-ancestor `Shared.update` registration), so `derive()` stays O(1). What it does cost:
>
> | | with the parent link | without it |
> |---|---|---|
> | `cancelled()` poll, depth 0 / 1 / 3 / 5 | **0.30 / 0.58 / 1.15 / 1.74 µs** (~0.29 µs per ancestor) | 0.30 µs at any depth |
> | deepest token that crosses a `spawn` airlock | **4 999** (5 000 faults `maximum structural depth (10000) exceeded`) | any depth |
> | 1 000-link chain of `derive()` + one root cancel | 85 ms | 19 ms |
> | 8 000-wide fan-out + cancel | 81 ms | 88 ms (unaffected) |
>
> Only what the cascade reads is registered: `derive()` sends a **parent-less twin** of the child into
> `kids`, sharing its `flag` / `dl` / `kids` cores. A channel send crosses the airlock and deep-copies
> the struct, so sending the child itself would copy its whole ancestor chain — O(depth) per derive,
> and a measured 25.6 s for 300 derives.
>
> *(v1 registered each new child into **every** ancestor, each insert a `Shared.update()` that copied
> the whole list across the wire — `derive()` was cubic in chain depth (400 derives = 6.0 s, against
> Go's 0.08 ms) and quadratic in fan-out. See [`benchmarks.md`](benchmarks.md).)*
>
> **Retention:** a `cancel()` **drains** the registries it walks, so a cancelled subtree releases its
> child handles. An **uncancelled** long-lived parent still retains them — there is no token-drop hook,
> so many short-lived children derived under one long-lived parent accumulate in its `kids` channel
> until it is itself cancelled or dropped. Same shape as v1, but heavier entries — v1 retained one
> `Channel[bool]` per descendant, this retains whole `Token`s (flag + `dl` + `kids` cores). Tokens are
> request-scoped in practice.
>
> **Ordering (C5):** marking a descendant sets its cancel bit **before** it trips its `done()`, because
> the trip is what wakes a parked `wait:`. So a task woken by any node's `done()` — its own or a
> cascaded ancestor's — always reads `cancelled() == true`, Go's `ctx.Done()`/`ctx.Err()` contract.
> (On the one arm where the bit is *not* set — the node's own deadline had already elapsed, so the
> latch keeps `"timeout"` — `cancelled()` is already true from the deadline itself, so the contract
> holds there too.) Measured: the reverse order loses the race 141 times in 400 rounds at
> `CHEZZI_THREADS=8`, and 0 at `=2` — which is why the gate is pinned at 8 workers rather than left
> to the suite's default.

| Method | Returns | Notes |
|--------|---------|-------|
| `cancelled()` | `bool` | own `flag` (an ancestor cancel is *pushed* into it) OR own deadline passed OR the `parent` chain says so. **O(depth), 0.30 µs + ~0.29 µs per ancestor; polls, never blocks.** |
| `reason()` | `str?` | `"cancelled"` (manual or cascaded) \| `"timeout"` (own/inherited deadline) \| `None` (live). NEAREST cause wins (own first, else the ancestor's), and the FIRST cause **latches**: a cascaded cancel beats a later own deadline, while an already-elapsed own deadline stays `"timeout"` even under an explicit `cancel()`. |
| `done()` | `Channel[bool]` | ready (recv → `true`) when done — for a `wait:` arm. Same handle every call. |
| `cancel()` | `nil` | manual cancel, anytime, any task; idempotent; sets the cancel bit, then wakes `done()` waiters and **cascades down** — drains each node's immediate-child registry and marks every transitive descendant (work-list, not recursion). |
| `derive()` | `Token` | a child token: cancelled when self (or an ancestor) is; tightest deadline; one-directional. **O(1)** — one send into self's registry. Also `cancel.derive(parent)`. |
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

> **Cancellation is delivered at CHECKPOINTS — and a registered `defer` ALWAYS runs.**
> A cancel (a sibling's fault, an `os.exit`, a scope teardown) is observed only at **cancellation
> points**: **loop back-edges** and **blocking / park ops** (`recv`, `wait:`, a socket op, a blocking
> native like `sleep_ms`). It is *not* observed at every instruction. Two consequences, both intended
> (this is Trio-style structured concurrency; Go never preemptively kills a goroutine at all):
>
> - **A STARTED task always runs its straight-line prologue**, so a `defer` it registers is registered
>   *before* anything can kill it. "Does my cleanup run?" no longer depends on scheduler timing.
> - **A long-running CPU loop is still cancelled promptly** — the loop back-edge is the checkpoint.
>
> **Two kinds of blocking checkpoint — a wait whose DEADLINE WE OWN is CONTINUOUS; a syscall-blocking
> native is ENTRY-only.**
>
> | wait | checkpoint | a cancel arriving *during* it |
> |---|---|---|
> | `time.sleep_ms(ms)`, `time.timer(ms).recv()`, a `wait:` timer arm | **continuous** (~5 ms) | ends the wait |
> | `ch.recv()`, `ch.send()` on a full channel, `wait:`, a socket op | continuous | ends the wait |
> | `fs.*`, `request*`, `process*`, `io.*` file I/O | **entry only** | observed after the syscall returns |
>
> The split is what we can actually do: a timer deadline is ours to cut short, a `read(2)` already in
> the kernel is not. It holds in a `parallel:` nursery, in an eager `Executor` job and on the top-level
> `main` thread, so `shutdown_now()` and a sibling's fault reach a sleeping task the same way, and the
> task still unwinds through its `defer`s (`docs/gaps.md` **W7-16**; before it, both waited out the
> full deadline *and* ran the code after the sleep).
>
> Three precisions, all measured:
>
>   (Historically the cooperative `--serial` engine had this checkpoint but nothing to observe at it:
>   nothing else ran while its one thread slept, so a *sibling's* cancel could not arrive mid-sleep at
>   all. That engine was removed 2026-08-16.)
> - **`chezzi test --timeout` reaches every blocking wait** — `sleep_ms` and `timer(ms).recv()`,
>   top-level, in a nursery and in an `Executor`, blocked in place or PARKED. The timer-park half is
>   **W7-17** (fixed 2026-08-05): a parked fiber observes nothing, so its timer job is armed for the
>   *sooner* of its own deadline and the run's, and the wake re-checks. The **netpoller** half is
>   **W7-18** (fixed 2026-08-05), same recipe: an `accept`/`read`/`write`/`connect` park registers with
>   the sooner of the op's own `timeout_ms` and the run deadline, and the resumed op re-reads the clock
>   to tell the two apart — the op's own deadline stays a catchable `Err("timeout")`, the run's is a
>   hard abort. A socket op's `timeout_ms` is unaffected when the cap is off or unexpired.
> - **`--max-heap` reaches a sleeper only through the cancel arm** — i.e. when the over-allocating task
>   is a nursery/`Executor` sibling sharing its cancel scope (measured 365 ms). A sleeping top-level
>   `main` has no cancel flag and its own heap is not the one growing, so it sleeps out (3005 ms) before
>   the OVER-MEMORY verdict lands. `--max-heap` is a per-heap cap, not a process-wide signal.
>
> A cancelled task then unwinds through its `defer`s — cancelled while running (back-edge), while parked
> on a `recv`/`wait:`, while parked on a socket, or while parked when a *sibling*'s fault tore the
> nursery down. `defer` is the language's only cleanup
> mechanism (no destructors, no `with`), so this is the guarantee cleanup rests on. At a `recv`/`wait:`
> checkpoint **cancel wins** over a queued value, a tripped `done()` latch and a fired timer.
>
> Exactly one thing deliberately skips a `defer`: **`std.os.exit`**, a hard halt by design. (An
> `os.exit` executed *by* a cancelled task's `defer` is honored — it beats the sibling's fault and sets
> the process exit code, identically.)
>
> **Every spawned task starts — even into an already-cancelled scope.** A `spawn`ed task is *always*
> run: M:N cannot do otherwise (a scope completes only at `done == total`, so a queued fiber is picked
> up even after a sibling has faulted). So the task runs its prologue, prints what it prints, registers
> its `defer`, and dies at its first checkpoint. (This is why
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
> nursery's faults).
>
> **A `recover:` INSIDE a defer body catches — even while the task is being torn down.** Since no
> cancellation point fires inside a deferred call, a fault raised *beneath* a `recover:` that the defer
> body itself installed is caught there, and the rest of the cleanup runs: a panic in cleanup step 1
> does not silently skip cleanup step 2 (Go's rule — a deferred function running during a panic
> completes normally and its own `recover()` works). It buys the **defer body**, not the task's life:
> once the body finishes the pending cancel resumes travelling up — the task still dies and the nursery
> still reports the original sibling fault, unchanged. A `recover:` **outside** the defer still cannot
> defeat a cancel. `chezzi test`'s `--timeout` / `--max-heap` hard aborts stay **un-swallowable
> everywhere**, inside a defer included. (`docs/gaps.md` **W7-3**.)
>
> **…so cleanup that blocks, blocks the teardown.** A `defer` that sleeps, waits on a socket or sends a
> last message is *uninterruptible*: it delays the nursery join by exactly as long as it takes, with no
> cap (`defer time.sleep_ms(10000)` in a cancelled task = a 10s join). That is
> Go's rule for a deferred function during a panic, and it is the price of "cleanup is never truncated".
>
> **A `defer` body cannot snapshot-PARK.** It runs during frame teardown, whose LIFO drain is host-stack
> state, so it runs *guarded* — like a `list.map` callback, it cannot snapshot-park (the **C5** limit).
> On the M:N engine this costs nothing observable: a `recv` in a cleanup DEMOTES (the worker blocks in
> place on its own thread, a replacement is spun) and the sibling's `send` reaches it, so the cleanup
> completes. It was the since-removed cooperative engine that could not do this at all — it faulted in
> place with the deadlock error and the cleanup stopped there (`docs/gaps.md` **C5/N6g**, dissolved
> 2026-08-16). Lifting the guard itself would still need a resumable native
> re-entry, not a cancellation change. Cleanup that only sends, sleeps, closes or computes is unaffected.
>
> **Cleanup that can NEVER complete is REPORTED, never a silent hang.** A `defer` whose body waits for
> something that can never arrive (`ch.recv()` no one will ever answer) leaves the program quiesced —
> and the deadlock detector still fires (the demoted worker self-detects the quiesce). If a sibling's
> fault is what cancelled the task, *that* fault is what is reported — the stuck cleanup's own error is
> swallowed with its cancelled task.
>
> **Cancelling a scope cancels its nested scopes — at their CHECKPOINTS.** A `parallel:` entered from a
> task that is then cancelled dies with it: its children observe the enclosing cancel at their own
> checkpoints (a spinning grandchild cannot wedge the teardown). A nested nursery still keeps its own
> cancel token for its own faults: an inner fault never cancels an *outer sibling*.
> One limit, in the N5 family and identical: a grandchild that is already **parked**
> (`recv`/`wait:`) when the *outer* scope is cancelled is not re-driven — the cancel drain is scope-
> scoped, and a parked fiber has no checkpoint to observe the inherited flag — so it is torn down by the
> deadlock reap **without running its `defer`s** (`docs/gaps.md` **N5**, which is the deliberate
> deadlock exception below, not a filed bug). A grandchild that is *running* (or parks *after* the
> cancel) unwinds normally.
>
> **Where a cancel is NOT delivered — pure CPU with no back-edge.** A checkpoint is a loop back-edge, a
> blocking op, or a native→user-code re-entry (a `list.map`/`filter`/`fold`/`sort` callback: the native's
> per-element Rust loop *is* the back-edge, and the cancel is delivered between elements). **Deep
> recursion is not a checkpoint** — a recursive function emits only `Call`/`Return`, never a backward
> `Op::Jump` — so a cancelled task sitting in a loop-free recursive computation (`fib(34)`) runs it to
> completion before it dies. This is Trio's model (pure-CPU code is not interrupted); making `Op::Call`
> a checkpoint would put a checkpoint *before the `defer` line* of any prologue that calls a function and
> would give back exactly the bug this design removed. Behaviour is identical across runs, so it is a limit,
> not a divergence. Bound a recursive computation yourself if a task must tear down promptly.
>
> **Cross-task output order is NOT part of the contract.** One `print` = one locked write = line-atomic;
> the *order* of prints from different tasks is nondeterministic on **both** engines (a cancelled task's
> already-printed lines are kept, not retracted). What is identical across engines: the **set of lines**,
> the **exit code**, and **whether the `defer` ran**. Parity tests for concurrent output use the
> order-insensitive comparison, never a byte-equal one.
>
> **One deliberate exception: a genuine deadlock does not run `defer`s.** When every fiber is parked,
> nothing is cancelled and nothing can arrive, the parked fibers are torn down where they stand and
> their `defer`s do **not** run. This is the contract, not a debt — a deadlock is the runtime declaring
> the program cannot proceed, which is not a cancellation, and the ancestors draw the line in the same
> place: Go's `fatal error: all goroutines are asleep - deadlock!` skips its `defer`s (its `panic` path
> runs them), and CPython does not even reach the question — a `queue.Queue().get()` or an unset
> asyncio `Event` under a `TaskGroup` hangs until something external kills it, `finally` unrun.
> Chezzi is the strictest of the three: it detects and reports in milliseconds. (`docs/gaps.md` **N5**,
> closed as not-a-bug 2026-08-06 with the measured table.)

**Deterministic deadline.** A timeout's deadline is checked via `monotonic()` *at poll time* — no
background canceller task — so a self-polling timeout loop stops on time identically across runs.
(`done()`'s deadline delivery rides the proven `timer(ms)` path, §6c.)

**Cooperative contract (by design).** A token *signals* cancellation; it cannot forcibly interrupt. The
M:N engine runs the sibling on a real OS thread, so the kernel preempts a pure-CPU loop and the cancel
lands — but WHERE it lands is a scheduling fact, so a *manual* cancel of a non-polling CPU sibling is
**timing-dependent** (this is why `examples/cancel_cpu.chz` carries no golden
`.expected`, like `examples/parallel_cancel.chz`); a self-polling *timeout* does not. Guidance: **poll
`cancelled()` in CPU loops; `wait:` on `done()` in IO loops** — exactly Go's `ctx.Done()` contract.

The **same root** covers the *automatic* cancel that structured concurrency issues when a sibling
faults (`docs/gaps.md` **N8/N9**): a task cancelled mid-loop emits a different **line set** from run to
run, because how far it got before the cancel landed is a scheduling fact. This is **not a bug to fix**.
(Historically the same shape *hung* on the since-removed cooperative `--serial` engine — the spinner
never yielded, so the faulting sibling never got the thread to trip the cancel. That engine is gone;
**`--threads=1`** is the single-runner mode — still the OS-thread M:N engine, one CPU runner, where the
kernel preempts the spinner and it faults promptly.) Lifting the limit would
require teaching the cooperative scheduler to time-slice a *running* fiber (its own milestone), which
`--threads=1` already makes unnecessary for users.

**Re-derived 2026-08-18 on the genuinely 1-wide binary** (`docs/gaps.md` **W8-8**, below — until it
landed, `--threads=1` silently ran two runners, so the original "0/15 hangs" was measured two-wide): the
spinner-plus-faulting-sibling repro ran 15× → **15/15 faulted in 4–6 ms, 0 hangs, 0 timeouts**, the
spinner never completing. `CONTEXT_REDS = 4000` reduction-budget preemption fires per dispatched op
regardless of worker count, so one runner is enough — the argument that made deleting `--serial` safe
now rests on a single-runner measurement.

> **`--threads=N` means N CPU runners — including `N=1`.** This corrects an earlier warning here that
> `--threads=1` did **not** serialize: it ran TWO workers (`docs/gaps.md` **W8-8**, 8 CPU-bound tasks at
> **1.98 cores**, byte-identical to `--threads=2`). **Fixed 2026-08-18.** Measured after the fix on
> `examples/primes_parallel.chz` (12-core Linux, cores = user/real): `--threads=1` **1.00** (was 1.91),
> `=2` 1.91, `=4` 3.02 — matching Go `GOMAXPROCS=1` (1.00) and a 1-thread Rust fan-out (0.99) running the
> identical workload on the same box. So `--threads=1` **is** the way to serialize a flaky concurrency
> repro, and the claims elsewhere in these docs justified as "measured at `CHEZZI_THREADS=1`" have each
> been re-derived on the 1-wide binary — all nine held (`docs/gaps.md`, the scheduler section of the W8
> session log). Also fixed the same day: **W8-7** — the default worker count used to be the *slowest*
> setting because every preemption woke every idle worker; `sys` at the default went 10.110 s → 0.009 s
> and the default is now at parity with the best setting. Full tables: `docs/benchmarks.md`
> §"W8-7 / W8-8 idle-worker-policy fix".

### 6c'. `Channel.trip()` — the manual level-trigger latch

`trip()` is the one native primitive `std.cancel` needs. It flips a permanent latch on a channel: the
channel then reports ready (`recv`/`try_recv`/`wait` → `true`) on **every** call thereafter, fanning
out to any number of receivers — like a passed `timer` deadline, but flipped on demand. (An ordinary
`Channel[bool]` can't be a fan-out `done()`: it is move-on-send, so a value reaches one receiver
once.) `trip()` is idempotent and reuses `close()`'s wake fan-out (minus the `closed` flag, so a
`wait:` arm stays *ready* rather than *skipped*). See `examples/channel_trip.chz`.

### 6g. Checking your own program for schedule-dependence

`chezzi run --check-parity` is **gone** — it ran the program on the cooperative `--serial` engine and
on M:N and diffed the two, and `--serial` was removed 2026-08-16 (`docs/future.md` §2b). Both flags are
now `unknown flag` errors.

What replaced it, for a program you want to be schedule-independent, is running it at more than one
worker count and diffing yourself:

```sh
chezzi run examples/concurrent_jobs.chz > a.txt
CHEZZI_THREADS=2 chezzi run examples/concurrent_jobs.chz > b.txt
diff a.txt b.txt
```

That is exactly what the repo's own standing gate does over `tests/chz`
(`tests/chezzi_threads_cli.rs`). **Since 2026-08-18 (`docs/gaps.md` W8-8) this recipe is strictly
stronger than it was:** `CHEZZI_THREADS=1` and the default used to be nearer neighbours than they
looked, because `--threads=1` really ran two runners — the two arms differed only in width, never in
*whether* work could overlap. Now `=1` is genuinely one CPU runner (1.00 cores, vs 1.91 at `=2`), so the
diff compares a serialized run against a concurrent one and a schedule-dependence shows up as a
difference far more readily. A difference is a **signal to investigate**, not automatically a bug:
it can be a genuine order-dependence / airlock / scheduler fault (the real prize), *or* simply a
**non-deterministic cross-task print order**, which `chezzi run` does not promise (§"Output ordering").
Compare a line SET, or make the program deterministic by construction, before calling a diff a defect.

> **`std.net` — exactly where a would-block socket op blocks.** A `spawn`/`parallel:`
> fiber parks on the netpoller (that is the whole D6 design). A socket op reached
> where there is no fiber to park **blocks its thread in place** in exactly two contexts — top-level
> `main` (Go-identical: `ln.Accept()` on the main goroutine blocks until a
> client arrives) and an M:N worker inside a native callback (which spins a replacement worker first).
> Everywhere else — an eager `Executor` job — it returns `Err("<op> would block: an Executor job
> doesn't own its thread — blocking here would starve every other job and `parallel:` nursery
> sharing the pool. Do this socket op inside `spawn:` or a `parallel:` nursery instead, where it
> parks rather than blocking a shared thread.")`, and that is deliberate rather than an unfinished
> corner. **The op set is `accept`/`read`/`read_bytes`/
> `write`; `connect` joins it ONLY inside an eager `Executor` job** — those four wait on a Chezzi peer
> fiber that can only run on the very thread they would block, whereas a `connect` handshake is
> completed by the kernel and starves no chezzi party, so top-level `main` blocks it and succeeds, as
> CPython (0.1 ms) and Go (314 µs) both do (`gaps.md` **W7-59**):
>
> - **On top-level `main`, an untimed op has NO escape but SIGINT.** The escapes are the op's own
>   `timeout_ms`, the run's `--timeout`, and cancellation — and under `chezzi run` two of those three do
>   not exist: `--timeout` is a **`chezzi test` flag only** (`chezzi run --timeout=500 f.chz` →
>   `chezzi run: unknown flag '--timeout=500'`), and `main` has no scope, so there is no cancel flag to
>   trip. `accept()`/`read()`/`write()` on `main` with no `timeout_ms` therefore blocks forever on an fd
>   that never becomes ready — Go-identical, and the reason to pass a `timeout_ms` when you need a bound.
> - **A socket-blocked `main` SUPPRESSES the process-wide deadlock verdict** while its op is outstanding
>   — for a never-ready fd, permanently. `main` in `accept()` plus an `Executor` job blocked on a
>   `Channel` nothing can send: Chezzi prints `listening` and hangs (rc=124). **Not a bug**: Go hangs
>   identically — an open socket is a runnable party, so `all goroutines are asleep` never fires
>   (measured, rc=124).
>

> - **Inside an eager `Executor` job**: a job does not own its thread — it runs on the bounded,
>   process-wide job pool (`CHEZZI_THREADS`, never grown on demand) and has no scheduler under it to
>   spin a replacement, so a blocked job starves every other job and every `parallel:` nursery sharing
>   that pool (measured at `CHEZZI_THREADS=1`, before either op refused here: an `accept` job plus a
>   later `connect` job = hang). **That measurement survives W8-8 unchanged, and structurally so**
>   (re-derived 2026-08-18): the job pool is `vm::pool`, sized straight off `worker_count()`
>   (`src/vm/pool.rs:38-40`), whereas W8-8's phantom second runner lived in the nursery enlist/owner path
>   — so `CHEZZI_THREADS=1` gave this pool exactly one thread before that fix and still does. Re-measured
>   on the 1-wide binary: both ops return their `Err` in 0.006 s, rc=0, no hang. This is the one context where **`connect` refuses too** (`W7-59`) —
>   before that it spun in place for up to 10 s, pinning a pool worker with no cancel or `--timeout`
>   escape (measured: an outer `shutdown_now()` at 200 ms took **10 009 ms** to end the run, now
>   **209 ms**). Use `spawn`/`parallel:` for socket work, which parks instead of blocking.

---

## 6h. M25 — killable subprocesses (`std.process` + cancellation) — **SPEC, not yet implemented**

**Problem.** Cancellation in Chezzi is *cooperative* (§6e): a `Token` signals, and the callee polls.
That works for Chezzi code, and it does not work at all for a thread parked inside a blocking native.
Measured on the release binary, a nursery task running `process.cmd("sleep 5")`:

| halt | what happens |
|---|---|
| sibling task faults (trips the scope cancel) | runs the full 5013 ms |
| `os.exit(3)` from another task | runs the full 5015 ms |
| `chezzi test --timeout=500` | runs the full 5008 ms |

Every `Kind::Blocking` native is offloaded to the dirty pool *precisely so it never pins a core
worker*; the price is that a thread inside the libc call has no cancellation checkpoint until it
returns (`docs/stdlib.md` → *Blocking calls cannot be interrupted*). Nothing in the VM can fix that
from the outside — the fix has to be to **end the thing being waited on**.

For a subprocess we own the PID, so we can. This is exactly Go's answer: `exec.CommandContext` kills
the child when its context is done, and Go 1.20 added `Cmd.Cancel` / `Cmd.WaitDelay` to choose the
signal and a grace period.

### Scope — one module, deliberately

| module | killable? | decision |
|---|---|---|
| **`std.process`** | ✅ we own the child PID | **in scope — this milestone** |
| `std.request` | ❌ a signal is the wrong tool | out of scope; wants a client-side request timeout, same `Token`, separate work |
| `std.fs`, `std.io` file seams | ❌ not signal-interruptible | **out of scope permanently — Go does not cancel these either** (`os.ReadFile` ignores `context`), so today's behaviour already matches the ancestor |

Extending this to `fs`/`io` would be drift *away* from Go, not toward it. Say so at review time when
somebody asks why the milestone stops at one module.

### Three implementation traps, all measured in this tree

**1. Kill the process GROUP, never the PID.** `cmd`/`run`/`run_bytes` shell out through `sh -c`
(`src/native/process.rs:51`), so the real workload is frequently a *grandchild*. Measured:

```
sh -c '<victim> | cat'        sh=1959641   victim=1959643
kill -TERM 1959641
→ victim 1959643 SURVIVED
```

And the survivor is worse than an orphan: it still holds the write end of the stdout pipe, and the
parent is blocked reading that pipe — so killing only the shell **does not even unblock us**. The fix
is `std::os::unix::process::CommandExt::process_group(0)` at spawn plus `kill(-pgid)`, which is
correct whether or not the shell `exec`-optimizes into a single process.

**2. `.output()` has to go.** It consumes the child and only returns on exit, so there is no handle to
kill. `spawn_shell` (`src/native/process.rs:49`) must become `.spawn()` + a retained `Child`, with the
pipe draining moved onto that handle. This is a refactor of the module's core, not a new parameter —
budget it as such.

**3. `TERM` → grace → `KILL`, not bare `KILL`.** Go's default is `Kill`, but `WaitDelay` exists because
that is too blunt; since we shell out to real tools, give them a chance to flush and clean up. Default
grace on the order of a few hundred ms, overridable.

### The one deliberate divergence from Go: ambient by default

Go requires an explicit `ctx` on every call because Go has **no structured concurrency** — there is no
ambient scope to inherit. Chezzi has one: **the nursery IS the context.**

So a `std.process` call is cancelled by, in order of precedence:

1. an **explicit** `Token` passed to the call (Go's model, for the cases that want a narrower scope);
2. otherwise the **ambient scope cancel** of the task that made the call.

Ambient-by-default is the whole point: a sibling fault, `os.exit`, and `chezzi test --timeout` already
trip the scope cancel, and today none of them reach `sh`. Explicit-only would leave the `--timeout`
case — the one that motivated this milestone — still broken.

### Semantics to pin before writing code

- **A killed child surfaces as the CANCELLATION, not as a command failure.** If it returns
  `Err("exited with signal 15")` a `recover:` swallows it and the task keeps running, which defeats the
  feature. It must carry the cancel/exit marker the surrounding halt already uses, so the existing
  precedence (`Exit` > hard fault > fault > deadlock) applies unchanged.
- **`defer` interaction.** A `defer:` body that itself shells out must not be truncated mid-cleanup —
  the same rule `W7-57` settled for the CPU rungs (`deferring > 0` suppresses the halt at the shared
  funnel). Decide explicitly whether a *cleanup* subprocess is killable; the default should be no.
- **Idempotence + reaping.** Kill must be safe to call twice and after natural exit, and must always
  `wait()` the group so no zombie survives — including on the `os.exit` path, where the run is ending
  anyway.
- **Non-unix.** `process_group`/`kill(-pgid)` are unix-only. Decide the Windows story (job objects) or
  state the platform limit in `docs/stdlib.md`; do not leave it implied.

### Phases

| phase | deliverable | runnable proof |
|---|---|---|
| **M25a** | `spawn_shell` → `spawn()` + retained `Child` + `process_group(0)`; pipe draining on the handle | every existing `std.process` test unchanged, no behaviour change yet |
| **M25b** | group kill on the **ambient** scope cancel, TERM→grace→KILL | the three rows in the table above abort in ~ms instead of running to completion; `chezzi test --timeout` reports a real abort, not the `W7-57` post-hoc `TIMED-OUT` |
| **M25c** | explicit `Token` parameter, precedence over ambient | a `cancel.timeout(200)` threaded into a `cmd` kills it; a derived child token cascades (§6e) |
| **M25d** | docs + the `std.request` follow-on decision | `docs/stdlib.md`'s *Blocking calls cannot be interrupted* note narrows to `fs`/`io`/`request`; `std.cancel` §6e gains the effective-cancellation case |

### Verification bar

Same bar the `W7-47`→`W7-57` chain was held to, because this is the same subsystem:

- **The ancestor is the reference.** Write the Go program (`exec.CommandContext` + `signal.NotifyContext`)
  and cite its measured output for every semantic in question — not a description of it.
- **Grandchild proof, not PID proof.** Every kill test must use a *pipeline* (`… | cat`) so a
  PID-only implementation fails it. A test that only spawns a simple command will pass on a broken fix.
- **No zombies.** Assert the group is reaped, looped ≥30×.
- **False-kill fences.** A subprocess that finishes normally under an *uncancelled* token, a nursery
  that completes, and a `defer`-body subprocess must all be untouched — looped ≥30×, since this
  subsystem's bugs have historically been 2-in-30 rather than deterministic (`docs/gaps.md` `W7-12`).
- **Adversarial review** before landing, per the four fixes in this chain: it caught 12 defects a fully
  green 4000-test suite did not, including two hangs and a half-run `defer`.

### What this does NOT close

`std.fs` / `std.io` file seams stay uninterruptible, by the ancestor argument above. `std.request`
stays uninterruptible until its own follow-on. Both remain documented in `docs/stdlib.md`; neither is
a residual of this milestone.

---

## 7. Sendability

**The model — spawning a task copies its environment (fork-like).** A `spawn`ed task does not share the
parent's heap. It receives its **own isolated copy** of everything it captures — captured locals are
deep-copied, module globals are snapshot-copied per task (fresh at its `spawn`, [§2](#2-the-model)) — and a **closure**'s own references to its home module's globals are likewise snapshot-copied
onto the closure at the airlock (TICKET-016 / W8-25 — see the closures bullet below) — much like a
forked child copies the parent's address space. Two deliberate differences from a real `fork`:
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
stays on the isolated copy, invisible to the parent); a **module global** behaves the same way: a task
may reassign it or mutate it in place, and the write lands on that task's own copy,
invisible to the parent and to sibling tasks. (The earlier G1 checker rule — a compile error for both —
was retired when module globals started deep-copying per task.)

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
  home module index, never a by-reference heap handle. **A closure's references to its home module's
  `let`-bound globals are snapshot-copied at the airlock too (TICKET-016 / W8-25)**, alongside its
  captures: `Proto::global_free` names every such global the closure's body (or a closure nested inside
  it) reads and never writes, and the airlock copies exactly those slots' values onto the crossing
  closure. A global the closure itself **writes** is excluded from that set and stays a **late load**
  against whichever task's own module copy calls it — a module-level `let` binding is otherwise still a
  late load in-task (matching CPython), so a write to it AFTER a closure is created is visible to a
  later same-task call; only the value **crossing an airlock** is pinned. Top-level `fn`s, imports,
  `native fn`s and `extern` fns are unaffected and stay late loads always. So a `spawn f()`
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
  hash so a cyclic key is never re-hashed. **Byte-identical across runs.** For **data** the identity is
  **back-edge-only** (a node is popped off the serialize DFS stack on exit), so an acyclic **DAG alias**
  (the same node appearing twice off the cycle) is re-serialized as **two independent deep copies**, never
  collapsed into one shared node (mutating one copy in a task leaves the other untouched). The depth cap
  (`maximum structural depth …`) stays **only** as the backstop for a genuinely-unbounded **acyclic** nest.
- **A captured BINDING keeps its identity across the whole crossing — the one deliberate exception to the
  DAG rule above.** The `Cell` that backs a by-reference-captured local is memoized for the ENTIRE
  serialization, not just the current DFS stack, so **two sibling closures over one local still share one
  cell after crossing**:

  ```
  struct Ctr:
      inc: fn() -> nil
      get: fn() -> int
  fn make() -> Ctr:
      n := 0
      fn inc():
          n = n + 1
      fn get() -> int:
          return n
      return Ctr(inc, get)
  ch := Channel[Ctr]()
  ch.send(make())
  d := ch.recv()
  d.inc()
  print(d.get())          # 2 after two incs — ONE binding, not one per reference
  ```

  **Why the two rules differ:** a list is a *value*, a cell is a *binding's identity*. The language's own
  rule ([`syntax.md`](syntax.md)) is that a write through a capture is visible in the defining scope **and
  across sibling closures**; crossing the airlock snapshot-copies that binding into **one** independent
  per-task cell — one per **binding**, not one per reference — so the sharing rule survives inside the
  task (Go behaves the same). Data aliasing keeps the deep-copy-independence contract unchanged: only
  `Obj::Cell` uses the persistent memo, every container and the closure VALUES themselves still pop on DFS
  exit. One serialization spans everything that crosses together — a `spawn`'s callee/receiver + all args,
  a `spawn:` block's captures, and **the whole module snapshot** (every module, not one: W7-4a).

  **Known ceilings** — all of the shape "two *independent* serializations reach the same cell", which is
  exactly where identity stops. (A task's OWN two crossings — its captures and the module-global
  snapshot — are no longer one of them: `gaps.md` W7-4c, fixed 2026-08-06.)
  - **`RwShared` copy-out views.** `at`/`for_each`/`fold`/`get_key`/`has`/`for_each_entry`/
    `fold_entries` rebuild ONE piece of the stored container per step, so each piece is an independent
    copy of the binding — two `at()` calls are two crossings and can never share. A whole-container
    `get()`/`read()`, and `slice` (one call returning a container), are one crossing and DO share.
    (Mechanically: a value stored in a cross-heap box is serialized so every **depth-1 subtree** is
    self-contained — a cell reached from two of them carries its full definition in both, and the
    rebuild collapses the repeat back to one cell. That is what lets a piece be drained on its own
    without ever re-reading the box.)
    **One shape cannot be made self-contained: an element whose cycle closes through the ROOT
    container** (`a.back = xs; RwShared(xs).at(0)`) — the node its back-reference needs *is* the
    container. There, and only there, the view rebuilds the **whole** container under the same read
    guard and hands back the piece out of it, so the cycle survives: `at(0)`'s copy is reachable from
    its own back-reference, and mutating it is visible through that cycle. This matches CPython —
    `copy.deepcopy(xs[0])` on the same shape follows the cycle and copies the container too. The
    ceiling is that such a view costs O(container) *on cyclic data only* (CPython pays the same); a
    piece with no dangling back-reference never enters that path. Before this, the piece rebuild
    aborted the host process (`docs/gaps.md` W7-11).
  - **Handle-bearing cell.** A cell whose inner value carries a residual module/native/FFI handle falls
    to the snapshot's slow arm, which has no back-reference encoding.
- **A recursive *local* `fn` IS sendable (identity-preserving airlock).** A nested `fn` that calls itself
  captures its own name for recursion — the compiler's letrec gives it a self-cell, so the closure's
  capture graph is **cyclic** (`Closure → Cell → Closure`). The same `id` + `Backref` machinery (above)
  preserves that identity. So a recursive local `fn` — and a **mutually-recursive closure pair**
  (`Closure_f → Cell_g → Closure_g → Cell_f`) — crosses **any** airlock (`spawn:` block, `spawn f()`
  callee, `spawn f(g)` arg, `Channel[fn].send`) and computes correctly, **byte-identical**.
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
  identically on every run — not at construction. (A native/FFI *fn value* is pure code and now
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
  cleanly (a graceful `... cannot be sent across tasks` error, **never** a
  panic, **never** a silent mishandle): a suspension **with a pending `defer`** (`defer` is banned inside a
  generator) and a **multi-frame** suspension (`yield` fires only in the generator's own body frame). A
  generator held as a **module global** crosses **BY VALUE too** (backlog item B): a task that reaches it
  gets its own independent deep copy through the same `to_wire`/`from_wire` path, via the per-task
  module-global snapshot (`to_snap`) each task already takes. So two tasks reaching the same module-global
  generator each drive their **own** copy (and the parent keeps its own). Memory safety rests on `from_wire`
  rebuilding a fresh `GeneratorCore` on the worker heap (never a shared cross-heap `GcRef`). A **non-sendable**
  module-global generator (a non-sendable parked slot, a value cycle, a parked host handle) differs from the
  frame-local case in ONE way: `snapshot_modules` walks **every** global of the nursery's snapshot, reached
  or not, so it must NOT eager-fault on a generator the program merely *holds*. Instead `to_snap`'s slow arm
  snapshots such a generator as an inert **`Nil` placeholder** — a task that never touches it runs **clean**,
  and one that **reaches** it faults recoverably **at the use site** (`cannot iterate over nil`).
  (Fault only when reached; the frame-local crossing, by contrast, rejects eagerly at the
  `to_wire` serialize point because it crosses only the value actually sent.) (The earlier **Option-B
  reach-gate** model — which scanned each task for a *possible* reach and faulted it — is **retired**:
  by-value crossing removes the "why can a frame-local generator cross but not a module-global one?" drift.)
- **Captured locals AND module globals are isolated copies.** Reassigning — or mutating in place
  (`.push`/`.add`/`m[k]=v`/`s.field=x`) — a captured **local** or a **module global** inside a task is
  fine: the write lands on that task's own copy, invisible to the parent and to sibling tasks. To
  produce output visible to the parent, use a `Channel` or a `Shared`. (Reads are always
  fine, and a task reads the values current when its nursery opened — [§2](#2-the-model).) *(History: a
  G1 checker rule once made both a **compile error**, because the serial engine shared the globals while
  M:N snapshotted them. Deep-copying per task removed the divergence, and the rule — and
  its partially-covered indirect forms — was retired with it.)*
- **Cyclic sendables round-trip (identity-preserving copy).** The airlock copies a sendable by a
  structural deep walk (`spawn` arg / `Channel.send` / `Shared(...)` / worker return / module-global
  snapshot). A value that is sendable-by-type but contains a **reference cycle** (e.g. `a.next = b;
  b.next = a`, a list holding itself) is deep-copied **identity-preservingly**: every container +
  closure/cell node earns a per-serialization `id`, a back-edge becomes a `WireValue::Backref(id)`, and
  the receiver ties the knot — so the copy on the other side is an independent cyclic value with the same
  shape (like Python's `deepcopy`, which memoizes). This is the contract on the sole M:N engine. The
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

> **Status (shipped — EAGER):** `submit` **starts the job immediately** on the shared pool and
> `shutdown()` **waits** for the submitted work — the `ThreadPoolExecutor` / `ExecutorService` model.
> **The usage shape that keeps you out of a race: read or assert after `shutdown()`, never between it
> and the `submit`** — in that window the job is already running.
>
> (The older queue-at-`submit`, drain-at-`shutdown` behaviour — decision D3 — was the cooperative
> engine's, and it is gone with that engine as of 2026-08-16. `docs/gaps.md` **W7-13r(c)** recorded the
> resulting divergence: a job blocked on a full channel that another job closes faults `send on a closed
> channel` here, matching Go, where the lazy drain faulted `send on a full channel: deadlock`.)
>
> The fault contract is unchanged by eager execution: **every submitted job runs**, and the **first
> fault in submission order** (lowest index — not first-to-fail, which would be nondeterministic)
> propagates out of `shutdown()`. A faulting job costs its own result, never a sibling's work. This
> matches Python's `ThreadPoolExecutor` / Java's `ExecutorService` / Go's `errgroup`, none of which
> abort siblings by default.
>
> **The queue did not go away** — the shared pool has one, and a submitted job waits in it when every
> worker is busy. What changed is *who drains it and when*: continuously by pool workers, rather than
> only at the reap call. **Want the siblings to stop?** That is opt-in and lives in
> the caller: thread a `std.cancel.Token` through the closures and poll `tok.cancelled()` (§6e) — the
> same split Go uses (`errgroup` + `context`), and the reason there is no abort flag on the Executor.
> For structured first-fault-aborts-everything semantics, use a `parallel:` nursery, which is the
> primitive that means that. **The run-all guarantee is for an ORDINARY job fault** — a RESOURCE CAP
> (an over-memory/`--timeout` abort) is a separate, unconditional kill switch that trumps it, because a
> bound a sibling can outlive is not a bound: every job was dispatched at its `submit`, so the cap stops
> the ones that have not yet reached a cancellation point. **A dead stdout is NOT such a
> cap** (W7-5d, fixed 2026-08-05): a broken pipe raises an ordinary fault in the job that printed, and
> every sibling still runs to completion — matching CPython's `ThreadPoolExecutor`, the ancestor that
> owns `Executor` semantics, which runs every submitted job at every `max_workers`. The cost is
> deliberate and measured: under a GRACEFUL `shutdown()`, a submitted job that never prints and never
> returns (a bare `while true: j = j + 1`) now runs forever instead of dying with the queue, so
> `chezzi run x.chz | head -1` on that program hangs — CPython hangs on the same shape. Use
> `shutdown_now()` if you need it dead: it still cancels that job at its loop back-edge (measured
> 54 ms on every engine). (A nursery is unaffected: `parallel:` aborts siblings on any fault, this one
> included.) `shutdown_now` drops work
> that has not started
> and asks running jobs to stop — **cooperatively, at their next cancellation point**, exactly like
> Java's `shutdownNow`; a job with no such point still runs to completion, so on the default engine
> `shutdown_now` is not a guarantee that a submitted job did not run. `submit` after either is a fault.
> Reap with `defer ex.shutdown()` as shown.
>
> **A job sleeping, waiting a timer, or parked in a NESTED `Executor` join IS ended by
> `shutdown_now()`** (measured: 55 ms against a `time.sleep_ms(3000)`, and the code after the sleep
> never runs; **213 ms**, was 6 011 ms, against a job parked in an inner `shutdown()` whose own job
> runs `process.run("sleep 6")` — `W7-60`) — that deadline is ours, so it is a
> *continuous* checkpoint, not an entry-only one (§cancellation points; `docs/gaps.md` **W7-16**). This
> is a **deliberate divergence from CPython** and the one place Chezzi's executor does not follow it:
> `ThreadPoolExecutor.shutdown(cancel_futures=True)` does not interrupt a running `time.sleep(3)`
> (measured 3001 ms), and Go's `time.Sleep` is uninterruptible too. But Chezzi's own `sleep_ms` is a
> *fiber* wait, and its ancestor is the async sleep both languages DO cancel — `asyncio.sleep` under a
> `TaskGroup` (measured: cancelled at 50 ms), Go's `select { <-time.After; <-ctx.Done() }` (100 ms).
> Chezzi has one spelling for both, so it follows its own nursery: an executor that disagrees with the
> nursery beside it is the defect. A job blocked on a **channel** was already ended this way.
> **A NESTED executor's jobs are ended too** — W7-16's ruling applied one level down. An `Executor`
> created *by a running job* inherits that job's cancel flag — keyed on who **created** the executor,
> never on who calls `submit` (an `Executor` is a shareable value, so a job of an unrelated executor may
> submit to it; that must not hand over its cancel chain) — so an outer `shutdown_now()` reaches the
> inner executor's jobs at *their* checkpoints — the same structured-concurrency rule as "cancelling a
> scope cancels its nested scopes" (§cancellation points). Measured on the nested repro (an outer job
> creates an inner executor, submits a `sleep_ms(8000)`, then sleeps 8000 itself; `shutdown_now()` at
> 50 ms): **56 ms with no job line printed** — it was
> 8.005 s with the inner job's line printed. The ancestors are **split** here, so this is a decision,
> not a copy: CPython's nested `ThreadPoolExecutor` does **not** propagate — with
> `shutdown(wait=False, cancel_futures=True)` the call returns at 51 ms but the *process* still takes
> **8.04 s** and prints both jobs' lines (its non-daemon threads are joined at interpreter exit), and
> `wait=True` blocks for the full **8.00 s**; Go's derived `context.WithCancel(parent)` **does** — the
> child goroutine's `select { <-ctx.Done(); <-time.After(8s) }` reports cancelled at **50 ms**. What
> decides it is neither ancestor but Chezzi's own consistency: before this, the outer job's own
> `sleep_ms(8000)` died at 50 ms while the *identical* sleep one executor deeper ran to completion.
> One spelling of one wait cannot have two cancellation rules.
> **A non-terminating sibling still blocks `shutdown()`, by design.** Run-all deliberately drops the
> old fast-fail: if one submitted job faults but another never reaches a cancellation point (a tight
> loop, a blocking sleep with no polling), `shutdown()` now waits for it instead of
> killing it — the old abort-on-first-fault contract would have ended it at its next back-edge. This
> is accepted, matching Python/Java/Go's run-all default above; reach for a `std.cancel.Token` (§6e) if
> a submitted job needs to notice a sibling's fault and stop itself.
> **Program-exit join (A2):** an `Executor` is **detached** — it outlives the scope that created it —
> so an executor never explicitly `shutdown`/`shutdown_now`-ed is joined at a clean program exit, in
> creation order: the run waits for its in-flight work.
>
> **What "detached" means, precisely: detached from NURSERIES, not from an enclosing executor job.**
> An `Executor` created inside a `parallel:`/`spawn` task is not a child of that nursery — the nursery
> ending does not end it, and its work is joined at program exit instead (that is the A2 join, and the
> whole point of §"a task that outlives its scope" below). An `Executor` created inside a *running
> executor job* is detached in exactly the same LIFETIME sense — it is still joined at exit, not at its
> creating job's return — but it does inherit that job's **cancel** flag, so an outer `shutdown_now()`
> ends its jobs (above). Lifetime and cancellation are separate axes: nothing here shortens an
> executor's life, it only makes an explicit *stop* reach the whole subtree it started.
> Either way the submitted work completes instead of the program exiting out from under it. A hard
> `std.os.exit` skips it (consistent with how it skips `defer`); a faulting program is not joined (it is
> already erroring). This holds for an executor created **inside a task** too, which it did not before
> (`docs/gaps.md` **W7-5b**): the join walks a heap-independent registry of executor cores shared by
> every worker, rather than the per-`Vm` handle list that died with its task's heap.
> **Blocking inside a job, and the deadlock verdict (`future.md` §2d step 0, 2026-08-04).** An eager
> job has no scheduler to park a fiber into, and neither does the top-level `main` thread, so both
> BLOCK IN PLACE on an empty `recv` / full `send` / `wait:` — they no longer read "I have no scheduler"
> as "nobody can ever send", which was true only while every concurrent construct was scheduler-backed.
> A `deadlock` fault is now a **process-wide** verdict (`src/vm/quiesce.rs`): every counted party
> (`main`, plus each outstanding job) is registered as blocked AND none of their waits is already
> satisfiable. That is Go's `all goroutines are asleep` rule, so a genuinely stuck executor faults in
> milliseconds instead of hanging — two jobs deadlocked in one executor, two executors deadlocked on
> each other, and a blocked job with no `shutdown()` at all — while a job whose producer is still
> running keeps waiting, and `ex.submit(fn(): ch.send(42))` followed by `ch.recv()` in `main` simply
> works (it used to fault; Go and CPython both print the value). Under-reporting is deliberate wherever
> the verdict is unsure: an accepted hang is a missing answer, a false fault is a wrong one. Residuals
> — an all-joiner cycle, bounded-pool starvation, and PARTIAL deadlock (a subset stuck while the rest
> runs on, which Go cannot report either) — are in `docs/gaps.md` `W7-12r / W7-15`.
> **Captures cross by value:** `submit` wires the closure through the same by-value
> airlock (`wire_callable` → `to_wire`) that `spawn` uses, so its captures are deep-copied and isolated
> at submit time and the generator sendability enforcement runs — the same way regardless of worker
> count (the capture is a copy for every submitted closure). A mutation of a
> captured collection after the `submit` is NOT observed by the job.

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
| `submit(f)` | **start** a detached, side-effect-only job at once on the shared pool (results leave via a `Channel`, like `spawn`); returns immediately without waiting for it. **Faults** on an executor that no longer accepts work: after its own `shutdown()`/`shutdown_now()` (`submit on a shut-down Executor`), or — because the inherited cancel chain is sticky — after the job that CREATED it was cancelled (`submit on an Executor whose creating job was cancelled`); the alternative was accepting work that is immediately cancelled and silently vanishes |
| `shutdown()` | **graceful** — stop accepting new work, then **wait** for the submitted work (every job runs on an ordinary fault, per W7-5's fault contract above; a hard halt is a separate kill switch — see the engine-asymmetry note above and `docs/gaps.md` **W7-5d**) |
| `shutdown_now()` | **attempt to stop** — drop work that has not started and ask running jobs to stop at their next cancellation point, then wait for them (Java `shutdownNow`). **Cooperative, not pre-emptive:** a job with no cancellation point (a bare CPU loop with no back-edge, a syscall already in the kernel) still finishes, so on the default engine this is not a guarantee the job did not run. A job **sleeping, waiting a timer, or parked in a nested `Executor` join IS ended** — that wait's deadline is ours, so it is a continuous checkpoint (see §cancellation points; the join rung is `W7-60`). **It reaches jobs of a NESTED executor too** — an executor a job creates inherits that job's cancel, so the whole subtree stops at its checkpoints. Unaffected by the W7-5 run-all contract above |

- **`defer` is the lifetime knob.** A task "persists through scopes" because its *owner* — the
  `Executor` — does. Bind that owner's reaping to any scope with `defer ex.shutdown()` (a function, a
  `recover:` block, the module top level); `defer`'s all-exit-paths guarantee then reaps it on
  fall-through, `?`, `break`/`continue`, return, or panic. The task may outlive inner `parallel:`
  blocks, but it is **still deterministically reaped** — the leak becomes *your explicit, scoped
  decision*, never an accident.
- **Program exit ⇒ graceful shutdown** of any `Executor` not already shut down (the program waits for
  its submitted work; matches `defer`-at-top-level semantics). `std.os.exit` is still a hard halt and
  does **not** join (consistent with how it skips `defer`).
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
> which targeted the since-**removed** tree-walk interpreter. That engine no longer exists, and the
> serial-VM / M:N-VM split this note used to describe is also gone — the bytecode VM on its M:N
> scheduler is now the sole engine (`--parallel` is an accepted no-op alias). Read the `src/interp/*`
> references as planning history, not current paths.

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

### C4 — VM parity (historical)
Port C1–C3 to the bytecode engine (`src/vm`, `src/compiler`) — at the time, checked by matching the
tree-walk interpreter's output; the interpreter has since been removed.
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
- **Tests (historical):** every C1–C3 example ran identically under the VM (default) and the
  since-removed `--interp` flag; that differential assertion is gone with the interpreter.

### C5 — what's left, divided

C5 splits into **Group A** (small refinements that work on today's *sequential* executor — no engine
rewrite) and **Group B** (the real concurrency engine — a multi-session epic). Group A is independent
and shippable now; Group B is gated on **B1**. The surface of `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` is **unchanged** throughout.

**Group A — sequential refinements**

| # | Item | Status |
|---|------|--------|
| **A2** | `Executor` **program-exit join** — wait for any executor never explicitly `shutdown`-ed at a clean exit (creation order; `os.exit` skips it; a faulting program is not joined). Covers an executor created inside a task (W7-5b). | ✅ **done** (see [§8](#the-escape-hatch-c5-executor--a-separately-owned-work-queue)) |
| **A3a** | Reject a non-sendable **read through a nested closure** inside a `spawn:` block. | ✅ **enforced for a non-sendable local** — emergent from the persistent `capture_floors` + the `infer_ident` read gate. **Updated (B3.3 / Task 2a):** a plain **closure** read through a nested closure is now *accepted* (closures cross by value), so the pin is `read_captured_capturefree_closure_through_nested_closure_in_spawn_block_ok`. |
| **A1** | `Channel.try_recv() -> T?` — a **non-blocking poll** (`Some(v)`/`None`, never blocks/faults/suspends). Originally deferred (its motivating mid-flight-producer scenario needed the engine), un-deferred once B1/B2 landed. | ✅ **done** (it never suspends, so it is schedule-independent — see [§5](#5-channelt--a-mailbox-outside-every-heap)). |

> *Dropped from Group A, shipped in B3.6:* **A3b** (`Executor.submit` capture sendability gate). The
> submitted closure now crosses **by value** (`wire_callable` → `to_wire`), so a
> non-sendable capture (a live generator, a native handle) faults at submit, the same way regardless
> of worker count.

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
CPS rewrite of `eval`, a cost the oracle did not need. **The VM is the sole engine**, and since
2026-08-16 it has a single scheduler (M:N) — the cooperative `--serial` one was removed too
(`docs/future.md` §2b), so there is no cross-engine parity contract left at all. (Historically, while
the interp existed, a **blocking `recv` was VM-only** — under the old `--interp` it faulted
`deadlock`.)

**Landed (VM):** **B3** OS-thread multicore (the alternative bet, taken — B3.0–B3.6), **B4** real
`Shared`, **B5** real `Executor` pool (+ A3b) — then **Tier-D** rebuilt `--parallel` as an M:N
work-stealing scheduler, **complete through D6** (D0–D6 + owes #1/#2/#3; epoll/`std.net` netpoller
landed). Blocking `recv` inside a native callback (**D5 owe #3**) is **resolved** — see below.

**Cross-nursery wakeups — M:N RESOLVED.** A fiber in an outer nursery being woken
(and *run*) by an inner one (the circular outer-sibling case — `examples/parallel_cross_nursery_circular.chz`)
is **fixed under `--parallel`** (the M:N engine): one VM-global `MnSched` with a `Vec<JoinScope>` flat
scheduler (each nested nursery is a scope enlisted into the same global run queue, with a scope-scoped
owner stop), plus early-enlisting an outer nursery's siblings so a nested owner — draining the GLOBAL
queue — runs them. The fix also routes the inline outer-body's own `send`/`close` through the held sched
(so they wake an enlisted, parked sibling), runs a `spawn:` issued *after* the enlist, and makes the
enlist atomic — see §11 below and [`docs/cross-nursery-flat-scheduler.md`](cross-nursery-flat-scheduler.md).
(The cooperative engine serialized nested nursery levels and faulted `deadlock` on the same program; the
"cooperative flatten" that would have fixed it was never built and is now moot — that engine was removed
2026-08-16.)

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
    still cancels that inner nursery (silently — §2c1) while joining the function's implicit one.
    An uncaught **fault** propagating out of a body cancels the implicit nursery's tasks (abnormal
    exit, not a join). `defer`s run *after* the implicit join (tasks complete, then cleanup) — and
    identically for an **explicit** `parallel:` block: a `defer` directly inside the block flushes
    *after* the block's dedent join (its spawned children run to completion first, then the block's
    deferred cleanup), same order as the implicit body nursery. Each nursery on the unwind path is
    reclaimed separately, innermost-first.
    [**§2c1, 2026-08-14:** the cancel is now SILENT. This used to write one
    `"{n} pending task(s) cancelled on early exit from parallel:"` line per nursery; with eager start
    there are no unstarted tasks to count and any residual number would be racy, so the report is
    deleted — `trio` and `asyncio.TaskGroup` print nothing here either. Earlier history: resolved
    2026-06-12, when the VM dropped these reports while the tree-walk interp printed them.]
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
is nondeterministic, like Python/Go/Rust.**)** The last clause of this item read *"Serial is **never
deleted** — it stays permanently as the deterministic parity oracle + reproducible-debug engine"*;
**that was overturned and the serial engine was removed 2026-08-16** (`docs/future.md` §2b). Recorded
rather than edited away, because it is a decision this project reversed.

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

- **Cross-nursery wakeups** *(RESOLVED under `--parallel` (M:N) incl. multi-level nesting + late-spawn; the cooperative-engine flatten fix is moot now that engine is removed, but a few narrow limits still open — see below)*.
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
  `parallel:` with sibling and late `spawn:`s now runs correctly — every pending outer
  nursery enlists as its own scope, and a late `spawn:` into a middle nursery runs on the held flat
  sched as a fresh trailing scope via `register_scope_seeded` (atomic register+seed under one lock,
  un-latches a stale `terminate`) so the inline owner runs it — no clobber, panic, drop, or deadlock-veto
  race. Goldens: `examples/parallel_cross_nursery_multilevel.chz`, `..._late_spawn_parked.chz`.
  **Remaining narrow limits (revisit only if they bite real programs; full brief +
  reproductions in [`docs/cross-nursery-flat-scheduler.md`](cross-nursery-flat-scheduler.md)):**
  - **Contended shared channel across nested nurseries** — 2+ live receivers racing ONE channel across
    nested `parallel:` scopes is concurrent-divergent BY DESIGN: under `--parallel` delivery order is
    nondeterministic run to run, or it may deadlock-fault. It is NOT gated and NOT special-cased;
    it only must never PANIC and never HANG (completes or faults `deadlock` cleanly — see
    `parallel_cross_nursery_contended_never_panics`). The now-moot cooperative flatten (below) would
    have closed this same gap.
  - *(Historical: the cooperative `--serial` engine serialized nested nursery levels, so the same
    program faulted `deadlock` there. That engine was removed 2026-08-16, so the promised
    "cooperative flatten" is moot.)*
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

- **`Channel.close()` + closed-channel semantics** — **LANDED** (branch
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

- **Cooperative scheduler O(N²) in the per-nursery task count** *(historical — LANDED as Tier-D D0,
  then this scheduler was itself removed with `--serial` on 2026-08-16)*. The old
  `pick_runnable` linear-scan-per-turn (lowest-index runnable, O(N²); measured 1k→1.4 ms, 10k→51 ms,
  20k→246 ms, 50k→2.34 s) was replaced by a per-nursery **`ready: BTreeSet`** of runnable child indices
  (lowest-index pop, O(log N) per turn → whole nursery O(N·log N); `src/vm/mod.rs` — `run_scheduler` +
  `Nursery.ready`). Byte-identical scheduling order to the old scan (lowest-index is the contract), so
  all goldens stayed green. (Note: this was always purely the *cooperative default* engine — the piece
  since removed; `--parallel` uses the M:N `mn_worker_loop`, which is now the only path. D0 removed the
  quadratic wall but is orthogonal to the Tier-D per-task-cost work that makes fibers green-thread-cheap.)
