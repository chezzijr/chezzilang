## 6e. `std.cancel` — cooperative cancellation & timeouts

`std.cancel` is a Go-`context`-inspired **cancellation token**, adapted to Chezzi. A `Token` is an
explicit handle you thread down a call tree (like `Channel`/`Shared`): poll `cancelled()` in
CPU loops, race `done()` in a `wait:` for IO loops, and `cancel()` it from anywhere. It is written
entirely in Chezzi (`std/cancel.chz`) over existing primitives, plus the one native `Channel.trip()`
latch ([§6c'](02-channel.md#6c-channeltrip--the-manual-level-trigger-latch)).

```chezzi
import std.cancel

c := cancel.manual()             # manually-cancellable token, no deadline
t := cancel.timeout(500)         # auto-cancels 500ms after creation; also manually cancellable
child := c.derive()              # a CHILD token — cancelled when c (or any ancestor) is
```

| Method | Returns | Notes |
|--------|---------|-------|
| `cancelled()` | `bool` | own `flag` (an ancestor cancel is *pushed* into it) OR own deadline passed OR the `parent` chain says so. Polls, never blocks. Cost grows faster than linear in depth (see below). |
| `reason()` | `str?` | `"cancelled"` (manual or cascaded) \| `"timeout"` (own/inherited deadline) \| `None` (live). NEAREST cause wins (own first, else the ancestor's), and the FIRST cause **latches**: a cascaded cancel beats a later own deadline, while an already-elapsed own deadline stays `"timeout"` even under an explicit `cancel()`. |
| `done()` | `Channel[bool]` | ready (recv → `true`) when done — for a `wait:` arm. Same handle every call. |
| `cancel()` | `None` | manual cancel, anytime, any task; idempotent; sets the cancel bit, then wakes `done()` waiters and **cascades down** to every transitive descendant (work-list, not recursion). |
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

### The token tree

- **Tree-structured, registered like Go's `context.WithCancel`.** `derive()` builds a **child** token
  linked to its parent: cancelling or timing out the parent cancels every transitively-derived child,
  root-to-leaves, while cancelling a child **never** touches the parent. A child is sent into its
  **immediate** parent's `kids: Channel[Token]` registry — **O(1)**, no ancestor walk — and `cancel()`
  cascades **down**, draining each node's registry and marking every descendant it reaches (an
  explicit work-list, so tree depth is not call-stack depth).
- **`done()` cascades transitively** through that same walk: a `cancel()` at *any* depth above trips a
  grandchild's `done()`, so a task parked in `wait: leaf.done()` wakes on a grandparent cancel.
- **The link is live across tasks.** A parent flip is observed by an already-derived child, *including
  a child that a `spawn`/`parallel:` task or a `Channel` receiver holds*, in **both** directions (a task on the
  far side of a `spawn` can `derive()` into a registry the canceller drains).
- **Deadlines.** A child inherits the **tightest** deadline (the soonest absolute deadline of itself
  and its ancestors), copied into its own `deadline` field at derive time, so a timeout needs no
  cascade. A derived child of an already-elapsed timeout is cancelled at once with reason `"timeout"`.
- **The first cause latches** (Go): the cascade does not overwrite a descendant whose own deadline has
  already elapsed, and neither does `cancel()` on the token itself (`cancel.timeout(10)`, sleep past
  it, then `cancel()` → `"timeout"`, matching Go's `context deadline exceeded`).
- **A child also keeps a `parent` link, as the cascade's safety net.** The cascade removes children
  from the registry as it walks, so a cancelling task torn down mid-cascade (a nursery sibling faults)
  would lose the already-popped subtree. The `parent` link lets `cancelled()` still see the ancestor.
  Measured on a 500-deep chain at `CHEZZI_THREADS=4`: without the link 387 of 501 tokens stayed
  permanently uncancelled; with it, 0 of 501.
- **Ordering.** Marking a descendant sets its cancel bit **before** it trips its `done()`, so a task
  woken by any node's `done()` always reads `cancelled() == true` (Go's `ctx.Done()`
  contract).
- **Retention.** A `cancel()` **drains** the registries it walks, so a cancelled subtree releases its
  child handles. An **uncancelled** long-lived parent retains them — there is no token-drop hook, so
  many short-lived children derived under one long-lived parent accumulate in its `kids` channel until
  it is itself cancelled or dropped. Tokens are request-scoped in practice.

Cost of the `parent` link:

| | with the parent link | without it |
|---|---|---|
| `cancelled()` poll, depth 0 / 1 / 3 / 5 | **0.30 / 0.58 / 1.15 / 1.74 µs** (~0.29 µs per ancestor) | 0.30 µs at any depth |
| deepest token a `spawn` task holds | any depth (M3.4: a spawn walks nothing; a 9 000-deep chain passes and polls) | any depth |
| 1 000-link chain of `derive()` + one root cancel | 85 ms | 19 ms |
| 8 000-wide fan-out + cancel | 81 ms | 88 ms (unaffected) |

At larger depth `cancelled()` is quadratic: 0.53 µs at depth 0, 178 µs at 100, 2360 µs at 400
(release). The ancestor walk is O(depth), and the GC mark pass re-walks the rooted chain. Go's
`ctx.Done()` check is flat O(1) at every depth. See [`benchmarks.md`](../../contributing/benchmarks.md).

**Deterministic deadline.** A timeout's deadline is checked via `monotonic()` *at poll time* — no
background canceller task — so a self-polling timeout loop stops on time identically across runs.
(`done()`'s deadline delivery rides the `timer(ms)` path, [§6c](04-timer-wait.md).)

**Cooperative contract.** A token *signals* cancellation; it cannot forcibly interrupt. The kernel
preempts a pure-CPU sibling on its OS thread and the cancel lands, but WHERE it lands is a scheduling
fact, so a *manual* cancel of a non-polling CPU sibling is **timing-dependent** (this is why
`examples/cancel_cpu.chz` and `examples/parallel_cancel.chz` carry no golden `.expected`); a
self-polling *timeout* does not. Guidance: **poll `cancelled()` in CPU loops; `wait:` on `done()` in
IO loops** — Go's `ctx.Done()` contract. The automatic cancel that structured concurrency issues when
a sibling faults has the same root: a task cancelled mid-loop emits a different **line set** from run
to run, because how far it got is a scheduling fact. This is not a bug.

---

### Cancellation points

A cancel (a sibling's fault, an `os.exit`, a scope teardown) is observed only at **cancellation
points**, never at every instruction:

- **loop back-edges**;
- **blocking / park ops** (`recv`, `wait:`, a socket op, a blocking native like `sleep_ms`);
- **a nursery join** (a `parallel:` dedent, a function's implicit-nursery join,
  `Executor.shutdown()` / `cancel()`). A cancelled owner never runs the code after its join —
  the join itself raises the cancel (asyncio never runs code after a cancelled `async with`). The
  join checks the cancel the owner holds from an ENCLOSING scope, never its own nursery's fault, and
  never inside a `defer`.

Precisions:

- **A blocking op is a cancellation point only when it is about to WAIT.** A send with room, a recv
  with a value ready, a ready `wait:` arm, an `else`, a `try_*` op, a socket op an `Executor` job
  refuses (`would block`), and a `Shared.update`/`RwShared.read`/`write` whose guard is free are never
  cut: the op completes and the task is cut at its next wait or back-edge. At a `recv`/`wait:`, a
  queued value, a tripped `done()` latch and a fired timer win over a cancel.
- A task that waited for a guard re-reads the halts before it takes the guard, so a guard freed by a
  cancel, a child fault or `os.exit` that cut its holder is not taken (Go's `os.Exit` runs nothing
  more, and CPython's cancelled `asyncio.Lock.acquire` raises `CancelledError`).
- A function that returned never loses its result to a cancel (Go and CPython keep a finished job's
  result too).
- An operator or protocol hook (`+`, `<`, `==`, `[]`, `in`, `hash`, `str`, `iter()`) and a generator
  `.next()` are calls, not checkpoints.
- A native loop that calls user code per element (`map`, `filter`, `fold`, `sort_by`, `sort_by_key`,
  `Shared`/`RwShared` each/fold) is checked once per element. A container walk that calls a hook per
  element (a sort of `compare` structs, `xs == ys` over `eq` structs, a `Map` build by `hash`, `str`
  of a list) is one op and is not checked inside; the hook body's own loops still are.
- **A run-wide halt also lands at every function entry.** An `os.exit` and a deadlock verdict end the run, so a task also receives them when it enters a
  Chezzi function. A loop-free recursion stops at its next call, as Go's `os.Exit` stops every
  goroutine. A cancel and a nursery child's fault are not delivered at a call.

Consequences (Trio-style structured concurrency; Go never preemptively kills a goroutine at all):

- **A STARTED task always runs its straight-line prologue**, so a `defer` it registers is registered
  *before* anything can kill it.
- **A long-running CPU loop is still cancelled promptly** — the loop back-edge is the checkpoint.
- **A child's fault reaches its owner at every wait.** A nursery owner is cut when its child faults
  while it is blocked in `peer.read(10)`, `ln.accept()`, `recv`, `sleep_ms` or `wait:` — in `main`,
  in a fn body, in a spawned task and in an Executor job. An owner parked at an inner join is freed
  too. A blocking host call (offload, stdin) is cut at its return: a value that completes together
  with the fault is dropped. At the two CPU checkpoints (loop back-edge, native-HOF re-entry) the
  owner checks on a 1-in-1024 sample, so a CPU-bound owner may run up to that many further
  iterations. A `defer` body is never truncated by it, and a `recover:` outside the `parallel:` still
  catches the child's fault.
- An `Executor` created outside the nursery is not cancelled: its owner is cut at `shutdown()` (by a
  child's fault or a cancel), and the job runs on until a later `shutdown()` or the exit drain reduces
  it, as CPython lets it finish; a `--timeout` still stops it.

**Two kinds of blocking checkpoint.** A wait whose deadline we own is CONTINUOUS; a syscall-blocking
native is ENTRY-only:

| wait | checkpoint | a cancel arriving *during* it |
|---|---|---|
| `time.sleep_ms(ms)`, `time.timer(ms).recv()`, a `wait:` timer arm | **continuous** (~5 ms) | ends the wait |
| `ch.recv()`, `ch.send()` on a full channel, `wait:`, a socket op | continuous | ends the wait |
| `fs.*`, `request*`, `process*`, `io.*` file I/O | **entry only** | observed after the syscall returns |

A timer deadline is ours to cut short; a `read(2)` already in the kernel is not. The split holds in a
`parallel:` nursery, in an `Executor` job and on the top-level `main` thread, so `cancel()` and
a sibling's fault reach a sleeping task the same way, and the task still unwinds through its
`defer`s. Killing a subprocess on cancel is an open proposal:
[`killable-subprocess.md`](../../design/killable-subprocess.md).

Resource caps, measured:

- **`chezzi test --timeout` reaches every blocking wait** — `sleep_ms` and `timer(ms).recv()`,
  top-level, in a nursery and in an `Executor`, blocked in place or parked, and inside a native
  callback (`[0].map(f)` whose `f` waits on a `recv`, a `send`, a `wait:`, a `time.sleep_ms` or a
  `time.timer(ms).recv()`). A parked `send` reads it when it wakes, as a parked `recv` does.
  A `recv`, `send` or `wait:` parked in a `parallel:` task wakes at the deadline even when no other task wakes it.
  A parked timer is armed
  for the *sooner* of its own deadline and the run's. A netpoller op (`accept`/`read`/`write`/
  `connect`) registers with the sooner of the op's own `timeout_ms` and the run deadline: the op's own
  deadline stays a catchable `!"timeout"`, the run's is a hard abort. A socket op's `timeout_ms` is
  unaffected when the cap is off or unexpired.
- **`--max-heap` caps the whole run.** Every task of a test allocates into one heap, and each
  collection, on any thread, measures that heap's live bytes. Three tasks that each hold 4.2 MB
  are OVER-MEMORY together under a 10 MB cap, though each one alone is under it.
- **`--max-heap` reaches a sleeper only through the cancel arm** — when the over-allocating task is a
  nursery/`Executor` sibling sharing its cancel scope (measured 365 ms, before the one-heap change).
  The task that runs the collection reads the verdict; a sleeping top-level `main` has no cancel
  flag, so it sleeps out (measured 3005 ms) before the OVER-MEMORY verdict lands.
- **`--max-heap` counts an `Executor` job's captured payload while the job is queued or runs.** The
  payload is objects of the run's one heap. Measured on the per-task heaps, 300 jobs capturing
  ~1 MB each under an 8 MB cap: peak RSS 18 MB at `CHEZZI_THREADS` 1, 2 and default, each run
  `OVER-MEMORY`. A test whose unfinished jobs together exceed the cap trips, sleeping jobs
  included.

**Where a cancel is NOT delivered — pure CPU with no back-edge.** **Deep recursion is not a
checkpoint**: a recursive function emits only `Call`/`Return`, never a backward jump, so a cancelled
task in a loop-free recursive computation (`fib(34)`) runs it to completion before it dies. This is
Trio's model; making a call a checkpoint would put a checkpoint *before the `defer` line* of any
prologue that calls a function. Go's `context` cancel is cooperative and Python's `TaskGroup` cancels
at an `await`, so neither stops a running recursion either. Bound a recursive computation yourself if
a task must tear down promptly. A run-wide halt (`os.exit`, a deadlock
verdict) does stop it, at its next function entry.

**Cancelling a scope cancels its nested scopes — at their CHECKPOINTS.** A `parallel:` entered from a
task that is then cancelled dies with it: its children observe the enclosing cancel at their own
checkpoints (a spinning grandchild cannot wedge the teardown). A nested nursery keeps its own cancel
token for its own faults: an inner fault never cancels an *outer sibling*. One limit: a grandchild
that is already **parked** (`recv`/`wait:`) when the *outer* scope is cancelled is not re-driven, so it
is torn down by the deadlock reap **without running its `defer`s** (the deadlock exception below;
`gaps.md` **N5**). A grandchild that is *running* (or parks *after* the cancel) unwinds normally.

An inner deadlock faults only the innermost stuck nursery that a parked owner is joining: every task
parked outside it stays parked until the program aborts. The verdict is fatal, so no `recover:`
returns to feed them. A deadlock nested under a spawned task's implicit nursery faults `deadlock: …`
at `--threads=1` too, byte-identically to every other worker count.

---

### `defer` under cancellation

A cancelled task unwinds through its `defer`s — cancelled while running (back-edge), while parked on a
`recv`/`wait:`, while parked on a socket, or while parked when a *sibling*'s fault tore the nursery
down. A cancelled task first cancels and joins every `parallel:` it is inside, so its own children
finish before its `defer`s run. `defer` is the language's only cleanup mechanism (no destructors, no
`with`), so this is the guarantee cleanup rests on.

**`std.os.exit` skips `defer`s.** `os.exit` runs no `defer` in any task, and cuts a `defer` body that
is running; buffered stdout is flushed (Go `os.Exit`, Python `os._exit`). The task that calls it, its
siblings and every Executor job stop where they are: a `defer` not yet started never starts, in a
frame's remaining `defer`s and in a fault unwind alike, and no `recover:` catches the unwind. (An
`os.exit` executed *by* a `defer` is honored — it beats the sibling's fault and sets the process exit
code.)

**Every spawned task starts — even into an already-cancelled scope.** A scope completes only when every
task is done, so a queued task is picked up even after a sibling has faulted. The task runs its
prologue, prints what it prints, registers its `defer`, and dies at its first checkpoint. A sibling of
a task that calls `std.os.exit` may still run part of its prologue, up to its next function entry,
where the exit reaches it (`print` is a call); it runs no `defer`.

**A `defer` is never itself cancelled** (by a cancel; an `os.exit` does cut it). No cancellation point fires *inside* a deferred call. Every registered `defer` of
a cancelled task runs, in LIFO order (loops, blocking ops and HOF callbacks inside a defer body
included), whether the task was cancelled at a checkpoint, returned normally, or faulted on its own
while a sibling had already tripped the scope cancel. A `spawn` / `parallel:` opened inside a `defer`
gets a **clean slate** — it does not inherit the already-tripped enclosing cancel, so its children run
to completion (they are still cancellable by their *own* nursery's faults).

**A cancelled task's own `defer` fault is a real fault.** A `defer` of a cancelled task (or of a nested
nursery it aborted) that panics does not vanish: the nursery join, or `Executor.cancel()`, raises
it (Go and asyncio surface it too). It ranks BELOW every ordinary fault and above a `deadlock` abort —
the cancel's root cause is the more useful report (asyncio `TaskGroup` lists
`['boom', 'cleanup failed']`, root cause first). A stuck cleanup's `deadlock` verdict stays swallowed
when a sibling's real fault is the cause. The same ranking holds for a nursery OWNER cut by its
child's fault: the owner reports the child's fault, and its own `defer` fault or stuck cleanup ranks
below it (Go prints `panic: boom`), unless a `--timeout` / `--max-heap` halt lands in that cleanup. A
party's own fault keeps Go's rule: a `defer` that faults while it unwinds supersedes it.

**A `recover:` INSIDE a defer body catches — even while the task is being torn down.** A fault raised
*beneath* a `recover:` that the defer body itself installed is caught there, and the rest of the
cleanup runs: a panic in cleanup step 1 does not skip cleanup step 2 (Go's rule — a deferred function
running during a panic completes normally and its own `recover()` works). It buys the **defer body**,
not the task's life: once the body finishes the pending cancel resumes travelling up — the task still
dies and the nursery still reports the original sibling fault. A `recover:` **outside** the defer
still cannot defeat a cancel. `chezzi test`'s `--timeout` / `--max-heap` hard aborts stay
**un-swallowable everywhere**, inside a defer included. That covers a `defer` that meets the halt
while an ordinary fault unwinds, or while a `recover:` block drains its own `defer`s after a
`panic` or a recover-scoped `?`: the halt supersedes the fault and no `recover:` catches it. A
`defer` fault that happens BEFORE the deadline is an ordinary superseding fault, and `recover:`
still catches it.

**Cleanup that blocks, blocks the teardown.** A `defer` that sleeps, waits on a socket or sends a last
message is *uninterruptible*: it delays the nursery join by exactly as long as it takes, with no cap
(`defer time.sleep_ms(10000)` in a cancelled task = a 10s join). That is Go's rule for a deferred
function during a panic. A `recv` in a cleanup demotes (the worker blocks in place on its own thread,
a replacement is spun) and the sibling's `send` reaches it, so the cleanup completes.

**Cleanup that can NEVER complete is REPORTED, never a silent hang.** A `defer` whose body waits for
something that can never arrive (`ch.recv()` no one will ever answer) leaves the program quiesced, and
the deadlock detector fires. If a sibling's fault cancelled the task, *that* fault is reported — the
stuck cleanup's own error is swallowed with its cancelled task, and likewise when the stuck cleanup
belongs to an owner whose child faulted. The same holds for a `defer` that runs on the `main` thread —
a `defer` in `main`, at module top level, or in a `parallel:` body (which runs AFTER the join): a
cleanup that can never complete faults `deadlock` (fatal) instead of hanging. A pending `os.exit` from
another party outranks that verdict (Go: the exit status). A cleanup a live task WILL feed is not
judged.

**Which fault is reported.** At the end of a run (and of each test in `chezzi test`) one ranking
picks the report:

1. `os.exit`;
2. an `Executor` job's fault that no join has reduced and that was recorded before the run's own
   cause reached the top;
3. the run's own cause (`--timeout`, `--max-heap`, an own fault, a deadlock).

Go's first panic ends the process, so the earlier job fault is reported with the job's frames; an own
fault that happened first stays the report. A job fault replacing a deadlock stays fatal, so
`recover:` cannot catch it. This applies only to an executor no `shutdown()` has started on: a
deadlock inside that executor's own join keeps its `deadlock` verdict. A job cut by the run's own
`--timeout` is that cause, not an earlier fault. A child's or a job's fault prints the faulting
party's frames, never the frames of the owner that received it. A run that ends in a deadlock verdict
still drains its executors, so a finished job's buffered output is not lost.

**Cross-task output order is NOT part of the contract.** One `print` = one locked write = line-atomic;
the *order* of prints from different tasks is nondeterministic (a cancelled task's already-printed
lines are kept, not retracted). Compare the **set of lines**, the **exit code** and **whether the
`defer` ran**, never byte-equal output.

**One deliberate exception: a genuine deadlock does not run `defer`s.** When every fiber is parked,
nothing is cancelled and nothing can arrive, the parked fibers are torn down where they stand and
their `defer`s do **not** run. This covers the task that REPORTS the verdict too, so a `defer` that
closes a file does not run on a deadlock. A deadlock is the runtime declaring the program cannot
proceed, which is not a cancellation, and the ancestors draw the line in the same place: Go's
`fatal error: all goroutines are asleep - deadlock!` skips its `defer`s (its `panic` path runs them),
and CPython does not reach the question — a `queue.Queue().get()` or an unset asyncio `Event` under a
`TaskGroup` hangs until something external kills it, `finally` unrun. Chezzi detects and reports in
milliseconds.

---

### `--threads=N` means N CPU runners

`--threads=N` caps the threads that run Chezzi code at once at N process-wide, main included, like
Go's `GOMAXPROCS`, at every N, including `N=1`. Measured on `examples/primes_parallel.chz` (12-core
Linux, cores = user/real): `--threads=1` **1.00**, `=2` 1.91, `=4` 3.02 — matching Go
`GOMAXPROCS=1` (1.00) and a 1-thread Rust fan-out (0.99). So `--threads=1` is the way to serialize a
flaky concurrency repro. Reduction-budget preemption fires per dispatched op regardless of worker
count, so one runner is enough to cancel a spinning task: a spinner plus a faulting sibling at
`--threads=1` faulted in 4–6 ms in 15 of 15 runs, with no hang. The budget also preempts a CPU loop
inside a native callback (`List.map`/`sort`/`Shared.update` closures) and an `Executor` job, so a
callback spin or a CPU-bound job no longer starves a sibling, a sleeper or a cancelling fault. How the
runner budget works: [`concurrency-runtime.md`](../../design/concurrency-runtime.md#one-runner-budget).
