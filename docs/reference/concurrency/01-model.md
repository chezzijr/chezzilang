# Chezzi — Concurrency & Parallelism (`spawn` / `parallel:`)

Chezzi's concurrency is a **shared-nothing actor model** in the lineage of the Erlang/Elixir **BEAM**
(shared-nothing tasks, message passing) **plus** two borrowings — **Go's
first-class channel** and **Rust's move-on-send**. The surface is the **goroutine feel** (`spawn`,
cheap, no `async` colouring) wrapped in a **structured-concurrency nursery** (`parallel:`) so tasks
can't leak. The design **never taxes the single-threaded fast path** (`Rc`, per-heap
stop-the-world GC stays untouched).

The engine is a real OS-thread M:N work-stealing scheduler with reduction-counting preemption, a
dirty/blocking pool, and an epoll/kqueue netpoller behind non-blocking `std.net`. Every function body
and the module top level is an implicit nursery, so a bare `spawn` is legal anywhere. Design records:
[`concurrency-tier-d.md`](../../history/designs/concurrency-tier-d.md),
[`concurrency-b3.md`](../../history/designs/concurrency-b3.md) and
[`concurrency-roadmap.md`](../../history/designs/concurrency-roadmap.md). Scheduler internals:
[`docs/design/concurrency-runtime.md`](../../design/concurrency-runtime.md).

Chapters:

| file | topic |
|---|---|
| 01-model.md (this file) | model, surface syntax, execution semantics |
| [02-channel.md](02-channel.md) | `Channel[T]`, `pmap`, `Task`, `std.concurrency.broadcast`, `trip()` |
| [03-shared.md](03-shared.md) | `Shared[T]`, `Atomic[T]`, `AtomicInt`, `RwShared[T]`, concurrent collections |
| [04-timer-wait.md](04-timer-wait.md) | `timer(ms)`, `wait:` |
| [05-cancel.md](05-cancel.md) | `std.cancel`, cancellation points, `defer` under cancel |
| [06-blocking.md](06-blocking.md) | blocking-context table, sockets, schedule-dependence checks |
| [07-sendability.md](07-sendability.md) | what crosses a task boundary, and how |
| [08-executor.md](08-executor.md) | daemon tasks and `Executor` |

---

## 1. Status & lineage

- **Model:** shared-nothing tasks. Each task owns its heap + GC; tasks share **nothing
  mutable**, communicating only by **move/copy** through channels.
- **Surface:** `spawn` (a statement, goroutine-feel) inside a nursery that joins all children at
  its end. No `async`/`await` colouring, no `WaitGroup`.
- **Primitives:** `Channel[T]` (the sync primitive — a mailbox outside every heap) and `Shared[T]`
  (the sanctioned cross-task mutable box — an owner serialises writes, Elixir's `Agent` trick).
- **Staging:** the surface, type system and semantics shipped first on a sequential executor; the
  M:N OS-thread engine came later behind the same syntax. Tasks start at their `spawn` and
  interleave ([§4](#4-execution-semantics)).

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

**Chosen = A + C composed.** The **Go model (Tier D)** would mean `Rc`→`Arc` across every value
(atomic refcount bumps ~10–30× a normal bump, taxing single-threaded code *forever*) plus a
thread-safe concurrent GC. That permanently taxes the common case and hands users the entire
data-race bug class. **Skipped.** (The branch `exp/go-shared-heap` tests one heap per run; M1 is done, see
[`go-shared-heap.md`](../../design/go-shared-heap.md).)

### Memory picture: one heap per run, channel between

```
 task 1 ──send──┐
 task 2 ──send──┼──►  main task          all tasks allocate in ONE heap per run
 task 3 ──send──┘   ┌──────────────────┐
                    │ results: [...]    │  the GC stops every thread that runs VM code
                    └──────────────────┘  a channel hands over the value itself (M3.2)
```

A run owns one heap. A collection stops every thread that runs VM code, marks every root (running
threads, detached threads, parked fibers, `Shared` boxes), then sweeps. A thread that blocks (a
channel wait, a sleep, a socket or file read, a subprocess wait, an `extern` C call) detaches first,
as Go's `entersyscall` does, so a blocked thread never delays a stop. M3.2: a channel, an `Atomic`
and a Task's outcome hold the value itself, so the tasks that pass it share the one object (Go).
M3.4: a `spawn`'s arguments and receiver, a `spawn:` block's captures and an `Executor` job's
closure pass by handle too, so a task shares the parent's values.

Known limits:

- A CPU-bound native fn or C loop that never reaches a safepoint delays a stop until it returns.
- An `extern` call keeps its runner permit at `CHEZZI_THREADS=1`. This is older than the one-heap change.

### Data races

The shared heap (branch `exp/go-shared-heap`) takes Go's rule. A data race is the user's bug, and
the heap stays memory-safe. Two threads that race on one list or one map can see:

- a wrong value (an old one, or one from another entry),
- a torn read of a multi-word entry (a map entry is three words; a `Value` and a map index slot
  are one aligned word each, so they never tear),
- a missed map hit,
- a task fault: the op's own out-of-range error (`index 3 out of bounds (len 2)`), which
  `recover:` catches, as Go's index panic is one `recover()` catches. A racy op reads one
  snapshot of the buffer and checks its index against that snapshot's length, so it never
  panics the VM; a probe whose entry another thread removed is a miss. Gate:
  `racy_container_ops_never_panic` (a grid of every container read and write op against a
  shrinking thread, 10,000 rounds, also under `CHEZZI_GC_STRESS=1`).

A race never reads freed memory. A list, map or set buffer that grows, or that a sort or a remove
replaces, retires; only the stop-the-world GC frees it, when no thread holds a pointer into it.
Gate: `two_vms_race_on_one_list_and_map_without_uaf` (two threads, 10,000 ops each, also under
`CHEZZI_GC_STRESS=1`).

A race never sees an object before its contents (the publish rule). A `Value` slot (a list or
map slot, a struct field, a cell, a module global) stores with Release and loads with Acquire, so
a thread that reads a `GcRef` also sees the object it names, as Go's `publicationBarrier` after a
malloc gives. A lock or a channel gives the same order. A byte or an int word stays Relaxed: no
pointer comes from it, so a race on it gives a wrong value only.

A race never sees a torn header (the mutation rule). An op that changes a live object writes
through atomic slots (a struct field, a cell, a cursor position, a buffer slot) or under a lock
(a module's name table), never through a `&mut` over the whole object, and never replaces the
whole object. Only an object that a call built and has not published yet takes a `&mut`. Gate:
`shared_object_mutation_grid` (one thread writes struct fields, a cursor, a cell and a module's
names; the other reads them; 10,000 rounds, also under `CHEZZI_GC_STRESS=1`).

Known limits:

- A racy map probe can give a wrong or a missing hit.
- A racy map upsert (`m[k] = v`, `m.insert(k, v)`, `m.merge(o)`, `m.update(o)`) can write over
  another key's value or insert a duplicate key: two threads probe, then one moves the entries
  before the other writes. The map stays memory-safe and gives no panic. Go's runtime reports
  "concurrent map writes" and stops; Chezzi gives a wrong map.
- Two threads that step one cursor can both get the same item or skip one.
- A stop of the world waits for a native loop or C call (the limit above).
- `CHEZZI_GC_STRESS=1` with two or more threads runs them one at a time: each thread asks for a
  stop at each instruction and waits out the other thread. Stress hides races; the race test asks
  for a stop from one thread only.
- Miri and TSan cannot run on the development box, so no sanitizer checks the race test.

The **process** is one process: a task inherits the parent's `std.os.args`
and env, and **stdin is ONE source every task shares** (Go's `os.Stdin` / Python's `sys.stdin`, not
entry-task-owned). Any task may `io.read_line()` / `io.input()`; a line goes to **exactly one** task
(never duplicated, never dropped); **which** task gets it is nondeterministic — order it
yourself (entry task reads, fans out over a `Channel[str]`), exactly as with concurrent `print`. `None`
means stdin is exhausted. A blocked read demotes its worker, so it pins no core worker; details in
[`stdlib.md`](../stdlib/01-builtins.md) §`std.io`.

### Tasks share the parent's values (the Go rule)

A spawned task holds the parent's values by handle, as a goroutine does (M3.1 for module globals,
M3.4 for the rest). A captured local is one cell that the task and the parent share; a loop
variable gets a cell per iteration (Go 1.22). A `spawn f(xs)` argument, a `spawn obj.m()` receiver
and an `Executor.submit(f)` job's captures are the parent's objects, not copies:

```chezzi
xs: Map[int, int] = {}
n := 0
done := Channel[int](1)
parallel:
    spawn:
        _ := done.recv()    # waits for fill's write: two writes to xs need an order
        xs[1] = 1
        n = 5
    spawn fill(xs, done)    # fn fill(m, done): m[2] = 2, then done.send(1)
print(xs.len(), n)          # 2 5
```

Go prints `2 5` for the same program (`go func() { <-done; xs[1] = 1; n = 5 }()`), and
`go run -race` reports no race. The `done` channel orders the two writes to `xs`. The join orders
the tasks' writes before the parent's read. Two tasks that write one value without a `Channel`, a
join or a box (`Shared[T]`, `RwShared[T]`) between them race, and a race is the program's bug:

```chezzi
counter := 0
parallel:
    for _i in 0..1000:
        spawn:
            counter = counter + 1   # a data race, as in Go: the result is not 1000
```

Share a counter through `Shared[T]` (`update` runs under the box's guard) or an `AtomicInt`.
`tests/chz/spec/spawn_handle_test.chz` pins the rule (Go goldens in its header).


**A task reads the live global — at every depth.** There is no per-task view: a global first
initialized *after* an earlier nursery is visible, a write by the root task between two spawns is
visible to the second task, and a task's own write (in-place, `q.push(x)`, `m[k] = v`, `p.x = 1`, or
by reassignment) is visible to the parent and to every other task once the two are ordered by a
`Channel`, a join or a box. An `Executor` job reads the same globals. To share mutable state that
two tasks write concurrently, use `Shared[T]` / `RwShared[T]` / `Atomic[T]`
([03-shared.md](03-shared.md)) or a `Channel[T]`.

**The trade in one line:** Tier D lets you share → fast sends, but races are your problem. A+C
forbids sharing → the race cannot be expressed. For a scripting language that is a *good*
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

A one-line `spawn f(args)` and the block form run the same way: the receiver and the arguments are
evaluated in the parent at the `spawn`, the call runs in the task, and a call that waits
(`spawn ch.send(v)`, `spawn ch.recv()`, `spawn l.accept()`) parks like any other.

An explicit `parallel:` nursery joins all children at the dedent:

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

The two channels differ, as they do in Go:

| what the spawned callee does | what the nursery does |
|---|---|
| **returns a value** — including an `!e` from a `-> T!` callee | **discarded.** `spawn f(x)` is a statement; there is no place for a return value to go, exactly like Go's `go f(x)`. |
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

**To observe what a task produced, collect it explicitly** — send it on a `Channel[T]`, or reduce it
into a `Shared[T]`/`AtomicInt`, then read after the join:

```chezzi
served := AtomicInt(0)
parallel:
    for _ in 0..n:
        spawn client(addr, served)     # a client bumps `served` only when it really succeeded
print(served.load())                   # the nursery has joined — the count is final and real
```

`examples/echo_server.chz` is the worked version. A `T!E` returned by a spawned callee and never
collected is silently gone, so **never conclude "it worked" from a nursery that merely finished** —
count what happened.

`recover:` + `defer` compose with the fault channel only.

### Why a nursery, not a `WaitGroup`

`WaitGroup` is manual (`Add`/`Done`/`Wait` — forget one and you leak or deadlock) and
**unstructured** (a goroutine's lifetime isn't tied to any scope). The nursery *is* the join, and a
task **cannot outlive its `parallel:` block**, so leaks are structurally impossible and a *fault* has
an obvious home (the join surfaces it). A **returned `!e`** does not: the nursery discards it, exactly
like Go. This is **structured concurrency**: Python `trio` / `asyncio.TaskGroup`, Kotlin
`coroutineScope`, Swift `TaskGroup`, Java `StructuredTaskScope`.

### Nursery scoping rules

- **`spawn` is legal anywhere in a function body or at the module top level.** Every function body
  and the module top level is an *implicit nursery*; a bare `spawn` binds to it and joins at the
  body's `return`/end (the module top level joins at program exit). An explicit `parallel:` is an
  inner sub-nursery that joins earlier, at its dedent.
- **A task *starts* at its `spawn`, not at that join** — Go's `go f()` ([§4](#4-execution-semantics)).
  The join is a **completion** barrier and nothing more: it guarantees the task is *done* by the
  dedent/`return`, never that it could not have started (and printed) earlier.
- **Function-boundary rule (the safety invariant).** A `spawn` binds to a nursery **within its own
  function** — it can **never** reach an enclosing function's or the module's nursery. A closure body
  counts as a function body for this rule, so a `spawn` in a closure never reaches the enclosing
  frame's nursery, even when the `spawn` sits inside a `recover:` nested in the closure. A task
  spawned in `fn worker()` cannot outlive `worker`'s return:

  ```chezzi
  fn worker():
      spawn helper()       # `worker` gets its OWN implicit nursery and joins helper at its
                           # return. It does NOT reach main()'s.
  ```
  Java `StructuredTaskScope` and Elixir give the same guarantee.

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
  For a task that must outlive its block, see [08-executor.md](08-executor.md#8-daemon--background-tasks).

---

## 4. Execution semantics

### When a task starts, and when it is done

Two separate guarantees; do not conflate them:

- **start** — a task starts **at its `spawn`**, running concurrently with the rest of the nursery
  body. This is Go's `go f()`.
- **completion** — the nursery's join (an explicit `parallel:`'s dedent, a function's `return`/end,
  program exit for the module top level) waits until every task of that nursery has finished. It
  guarantees the task is *done* by the barrier; it says nothing about when it *started*.

How a `parallel:` block runs (`chezzi run`):

1. Executing `spawn X` **evaluates the call's callee + arguments eagerly, at the spawn point** (Go's
   arg-evaluation timing), hands them to the task by handle
   ([§7](07-sendability.md#7-sendability), M3.4), and **starts** the task on the innermost nursery. **The
   parent then continues to the next statement immediately** — `spawn` does not block. A `parallel:`
   nursery entered **inside a spawned task** starts the same way, at every worker count: its tasks
   run on the existing workers without adding an OS thread, and the owner parks at its join,
   releasing its worker. A `return`, `?`, `break` or `continue` out of such a nursery parks the owner
   the same way until its cancelled tasks settle. How runners are claimed and reused:
   [`concurrency-runtime.md`](../../design/concurrency-runtime.md#runner-claims).
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
`examples/implicit_nursery.chz`'s header comment is the worked statement of this. A cancelled task's
already-printed lines are kept, not retracted.

**This covers `Executor` jobs exactly as it covers `spawn`ed tasks.** An `Executor`'s slots are
indexed by *submission* order, and that decides which fault wins and what order the **buffered** sink
emits; it is **not** a promise about live CLI output. Three jobs that each `print` once and return
usually come out in submission order because they are too short to overlap; give them real work and
they come out in completion order. CPython 3.14.6 `ThreadPoolExecutor` with `shutdown(wait=True)` over
three jobs doing a few million loop iterations each held submission order in **0 of 30** measured
runs. If you need ordered output, collect the results (`submit` and `Task.get`) and
print them yourself after the join.

### Early exit from a `parallel:` cancels silently

A `break`/`continue`/`return`/fault that leaves a `parallel:` body early **cancels** the nursery's
tasks, and prints **nothing** about it. A task starts at its `spawn`, so there are no unstarted tasks
to count. `trio` and `asyncio.TaskGroup` are silent here too.

The order is innermost-first: a `return`, `?`, `break` or `continue` runs the `parallel:` block's own
defers, then cancels its children and **waits** for them to finish unwinding (their defers run), and
only then runs the enclosing blocks' defers. A defer that flushes a log channel therefore sees what a
cancelled child's defer wrote. Go does the same (`cancel()` + `wg.Wait()` before the function returns)
and so does `asyncio.TaskGroup`.

An uncaught **fault** propagating out of a body cancels the implicit nursery's tasks (abnormal exit,
not a join). `defer`s run *after* the implicit join (tasks complete, then cleanup), and identically
for an **explicit** `parallel:` block: a `defer` directly inside the block runs after the block's
dedent join. Each nursery on the unwind path is reclaimed separately, innermost-first.
