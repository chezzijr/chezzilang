## 5. `Channel[T]` — a mailbox outside every heap

A `Channel[T]` is **not** an object in any task's heap. It is a separate runtime structure — a
**mailbox** with its own queue — that sits outside both heaps. What each task *holds* is a lightweight
**handle**; the handle is the only shared thing, and values flowing through are **moved/copied** at
the airlock, never live in two heaps at once.

```chezzi
ch := Channel[str]()       # construct; capitalized like Shared[T] / T?. Unbounded FIFO.
rch := Channel[int](0)     # RENDEZVOUS: send blocks until a receiver is already waiting (Go's make(chan T)),
                            # but a parked sender's value is visible to try_recv/wait: polls, not just recv
bch := Channel[int](2)     # BOUNDED: holds ≤2 queued messages; a 3rd `send` blocks until a `recv` frees a slot
ch.send(x)                 # x moved/copied OUT of the sender's heap → channel queue
v := ch.recv()             # value reconstructed IN the receiver's heap
opt := ch.try_recv()       # non-blocking poll: v if queued, None if empty
n := ch.len()              # current queued count
c := bch.cap()             # capacity: 2 here; 0 for a rendezvous Channel[T](0); -1 for an unbounded Channel[T]()
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `send` | `send(self, v: T) -> None` | enqueue (move/copy at the airlock); the sender MAY keep using the value — the crossing copies, so its later writes are not seen by the receiver. On a **bounded** channel a `send` **blocks/parks** while the queue is at capacity (backpressure), resuming once a `recv` frees a slot. On a **rendezvous** channel (`cap == 0`) a `send` blocks until a receiver is already waiting. A blocked sender (rendezvous or full bounded) publishes its value as an OFFER, so a poll can take it, and `send` returns only once a receiver took it; `len()` does not count it |
| `try_send` | `try_send(self, v: T) -> bool` | **non-blocking** send: `true` once queued, `false` if the send can't proceed — the channel is **closed**, a **bounded** channel is **full**, or a **rendezvous** channel has no receiver already waiting. Never blocks/parks |
| `recv` | `recv(self) -> T` | dequeue (FIFO); blocking surface (see below) |
| `try_recv` | `try_recv(self) -> T?` | **non-blocking** poll: `?v` if queued, `None` if empty (or closed), or a value handed over by a parked rendezvous sender — never blocks, never faults, never suspends a fiber. Drain a mailbox without guarding on `len()` |
| `len`  | `len(self) -> int` | queued count — use to guard a `recv` |
| `cap`  | `cap(self) -> int` | capacity: `-1` for an unbounded `Channel[T]()`, `0` for a rendezvous `Channel[T](0)`, or the bound passed to `Channel[T](cap)` |
| `close` | `close(self) -> None` | idempotent; wakes every parked/demoted receiver (see "Closing a channel") |
| `trip` | `trip(self) -> None where T: bool` | permanent level-trigger latch ([§6c'](#6c-channeltrip--the-manual-level-trigger-latch)) |

- **Three shapes: unbounded, rendezvous, bounded.** `Channel[T]()` is an **unbounded FIFO** — `send`
  never blocks. `Channel[T](0)` is a **rendezvous** channel — `send` blocks until a receiver is
  already waiting (Go's `make(chan T)`; `try_send` declines with no waiting receiver). `Channel[T](cap)`
  (`cap > 0`; a negative `cap` is a runtime fault) is a **bounded** FIFO: a `send` blocks/parks once
  `cap` messages are queued and resumes when a `recv` frees a slot (Go's buffered channel).
  Backpressure changes *which* task runs *when*, never the value sequence a consumer sees, so a
  bounded or rendezvous channel gives the same value sequence at every worker count. A
  full/rendezvous `send` with no possible consumer (top level, no nursery, or inside a native
  callback) is a **deadlock fault**, not a silent over-fill or hang. Under multi-sender contention,
  `try_send`'s full-vs-not decision is nondeterministic, the same class as `try_recv`'s
  `None`-vs-`?v` under contention.
- **One hand-off protocol (Go's `hchan` model).** A value is delivered at exactly one commit point,
  under the channel lock. A blocked sender (plain `send` or a `wait:` send arm, in any context, on a
  rendezvous or FULL bounded channel) publishes an OFFER; a receiver takes it (`recv`, `try_recv`,
  `for`, a `wait:` recv arm), and on a bounded channel a receiver that frees a buffer place moves the
  next parked offer into it in the same hold. A blocked rendezvous receiver publishes a SLOT; a sender
  (`send`, `try_send`, a `wait:` send-arm poll) fills it. `send` returns only once its value was taken,
  so a value is received exactly once and never by its own sender. `close()` closes every live offer
  (that sender faults `send on a closed channel`) but a value already taken stays delivered — a
  receiver that takes a parked sender's value and then closes lets that send complete, as Go prints
  `sent both`. A `wait:` publishes ONE pending record for all its arms, so the commit that moves a
  value also picks the arm. Pinned by `tests/channel_handoff_grid.rs`.
- **DIVERGENCE from Go: `Channel[T]()` is NOT `make(chan T)`.** Go's no-argument channel is the
  rendezvous shape; Chezzi's no-argument `Channel[T]()` is UNBOUNDED instead, and this is
  deliberate. A Go programmer porting `make(chan T)` should write `Channel[T](0)`, not
  `Channel[T]()`. Reach for `Channel[T](cap)` or `Channel[T](0)` by default: an unbounded channel lets
  a producer outrun a consumer with no backpressure at all.
- **`recv` on an empty channel BLOCKS** until a sibling sends. It faults `recv on an empty channel:
  deadlock` only when the run is provably stuck — every counted party blocked with no satisfiable
  wait — which is Go's own detector (`fatal error: all goroutines are asleep - deadlock!`). A
  `spawn`ed producer is already running when the body reaches its `recv`, so Go's plainest channel
  idiom works verbatim. This holds for a nested `parallel:` whose outer body is itself parked on a
  rendezvous `recv`.

  ```chezzi
  ch := Channel[int]()
  spawn:
      ch.send(1)
  print(ch.recv())     # 1
  ```

  **The deadlock fault is FATAL, like Go.** Go's `fatal error: all goroutines are asleep -
  deadlock!` cannot be recovered, and neither can Chezzi's: `recover:` is transparent to the
  verdict, and the program aborts at rc=1 with the `deadlock` message:

  ```chezzi
  ch := Channel[int]()
  r := recover:
      parallel:
          spawn:
              v := ch.recv()
          spawn:
              v := ch.recv()
  print("unreachable")   # the `recover:` never binds `r`; `runtime error (<file>:3:5): deadlock: ...`, rc=1
  ```

  | fault | recoverable? |
  |---|---|
  | value and operation faults (index, overflow, `panic`, IO, call depth, a `Shared` guard re-entry) | yes |
  | scheduler and whole-program faults (deadlock, `os.exit`, `--max-heap`/`--timeout`) | no |

  An `Executor` job's deadlock aborts at `shutdown()`, and `chezzi test` reports a deadlocking test as
  ERROR and runs the rest of the suite. No task runs its `defer`s on a fatal deadlock — not the parked
  siblings, and not the task that reports it: the frames are dropped where they stand, as Go's
  `fatal error` does ([05-cancel.md](05-cancel.md#6e-stdcancel--cooperative-cancellation--timeouts),
  "a genuine deadlock does not run `defer`s").

  **What the deadlock report names.** One verdict ends the run, and no party runs past it. The
  report is the first blocked party's own site. Main locking a `Shared` guard that a stuck job holds
  reports `update guard wait: deadlock — …` at its own `update`, as Go reports
  `goroutine 1 [sync.Mutex.Lock]`. A `parallel:` body that waits at `s.update` reports at that line;
  the `parallel:` line is reported only when the body waits at the join. Main at a join
  (`shutdown()`, the end of a `parallel:` block) reports the victims' sites. A cleanup that meets the
  verdict is fatal too: no further `defer` runs, no `recover:` catches the unwind it ran in (a `?`
  inside `recover:` included), and the cause it was cleaning up after keeps the report. How the
  verdict is judged without false faults:
  [`concurrency-runtime.md`](../../design/concurrency-runtime.md#the-deadlock-verdict).

  A deadlock message raised inside a native callback that cannot park at all is an ordinary
  recoverable fault and still runs `defer`s. A `defer` that blocks forever is not that case: it is a
  counted party, so it reaches the verdict itself, and that verdict is fatal like any other.
- **Move-on-send** = Go's send without Go's sharing. Nothing is *enforced* — there is no Rust-style
  move checker, and a sender that keeps using the value it sent is legal and safe: the crossing
  deep-copies, so the two sides stop being the same object. Measured: `ch.send(xs)` then
  `xs.push(3)` leaves the sender with `[1, 2, 3]` and hands the receiver `[1, 2]`.
- **Channels are themselves sendable** — pass a `Channel` over a `Channel` for reply channels.
- **`try_recv() -> T?`** is the non-blocking sibling of `recv`: it pops or returns `None` and never
  blocks, faults, or suspends a fiber. `recv -> T` stays primary; reach for `try_recv` to poll
  without guarding on `len()`. On a rendezvous channel, `try_recv` also takes a value handed over by a
  parked rendezvous sender — matching Go's `select`/`default` against a parked sender — and the same
  is true of a `wait:` recv arm.

### Closing a channel

- **`for v in ch:`** — blocking iteration; drains buffered + future values, ends cleanly when
  closed-and-drained (Go's `for v := range ch`).
- **`close()`** — idempotent, wakes every parked/demoted receiver.
- **`send` after close → faults** `"send on a closed channel"`; **`recv` on closed-and-empty →
  faults** `"receive on a closed channel"` (drains buffered first).
- `try_recv` reads a closed channel as `None`; `try_send` returns `false`.
- A `Channel[T]` is an `Iterable[T]`: `for v in ch`, a comprehension and `List(ch)` / `Set(ch)` drain
  it until it is closed and empty (Rust `rx.iter().collect()`, Go `for v := range ch`), so `List(ch)`
  blocks until a sender closes it.

### 5a. `std.concurrency.pmap` — scoped parallel map (the ergonomic wrapper)

The report-channel + one-`spawn`-per-element + join + reassemble pattern is common enough that
`std.concurrency.pmap` bakes it in (pure Chezzi over a `parallel:` nursery + `Channel`):

- `pmap[T, U](xs, f) -> List[U]` — spawn a task per element, run `f` in parallel, results in
  **submission order**.
- `pmap_limited[T, U](xs, f, limit) -> List[U]` — same, capping in-flight `f`-executions at `limit`
  via a channel-as-semaphore token bucket (also the standard **concurrency limiter**; `limit > 0`).

Both live in the **submodule** `std.concurrency.pmap`, not on `std.concurrency` itself, so the
spelling is `import std.concurrency.pmap` then `pmap.pmap(xs, f)` / `pmap.pmap_limited(xs, f, n)` —
`concurrency.pmap(...)` is a `module 'concurrency' has no member 'pmap'` type error.

Determinism comes from reassembling by submission INDEX (`sort_by_key`), never completion order — so
the result is byte-identical at every worker count. The nursery lives inside the helper and joins
before the collect (a task can't outlive the call); `f` crosses the airlock into each task by value.
See [`stdlib.md`](../stdlib/01-builtins.md) for signatures.

### 5b. `std.concurrency.task` — result handles for `Executor` work

Bare `Executor.submit(f)` is fire-and-forget — nothing comes back. The result-returning primitive is
`Executor.submit_result[T](f: fn() -> T) -> Channel[T!]`: submit `f` and get a cap-1 channel you
`.recv()` for its outcome (`?value` or `!message`). That channel is **sealed**: its outcome is written
once, and every `recv` returns a copy of it and takes nothing, so `len()` stays `1` and every reader
sees the same outcome. Do not iterate it with `for` (it never ends); to collect several outcomes keep
a list of `submit_result` channels. Two writers can seal it, and the first wins: the job body seals
the value of `recover: f()`, and the job transition seals
`!"task cancelled: shutdown_now() stopped it before it finished"` when it drops a held job or a job
ends cut (see [Job states](08-executor.md#job-states)). `std.concurrency.task` wraps that channel in a
future-style handle (every copy of the handle, in any task, reads the same channel):

- `submit_task[T](ex, f) -> Task[T]` — submit `f` detached, get a handle (builds over
  `ex.submit_result(f)`). The work starts at the `submit` and is waited for by `shutdown()` (or the
  program-exit join). Read the result AFTER that call.
- `Task.get() -> T!` — block until the outcome lands, then return it as a value; idempotent, and the
  same answer in every task. The task holding the original handle gets the same object back each
  call; a task holding a copy gets the value as of the crossing, or a fresh copy when the owner had
  not called `get()` before it. `get()` never faults: a job that returned `v` gives `?v`, and a job
  that faulted gives `!message` with the job's own fault text (CPython's `Future.result()`, with a
  `T!` in place of a raise) — handle it with `match`, `else e:` or `?`. A job run through
  `submit_task` keeps its fault inside the handle, so `shutdown()` returns normally for it (measured:
  `shutdown ok`, then `get()` gives `!boom`); only a fire-and-forget `submit` job's fault ends the
  run. A task `shutdown_now()` cancelled gives
  `!"task cancelled: shutdown_now() stopped it before it finished"` at once, never hangs (CPython:
  `CancelledError`), and `done()` is `true`. When `f` itself returns a `T!E`, `get()` is `(T!E)!`: the
  outer layer says whether the job faulted, the inner one is the job's own result.
- `Task.done() -> bool` — non-blocking readiness poll; `true` once the job has finished, faulted or
  not. It reads `concurrency.is_settled(ch)`, so it never reads `false` after `true`.

Canonical shape: submit all → `shutdown()` → `.get()` each. **Determinism rule:** a task's value is
deterministic (`f()`); only its *timing* varies at runtime, so `.get()` is byte-identical across runs
**as long as you await in a fixed (submission) order**. There is deliberately **no**
`join_next()`/select-on-completion — completion order is nondeterministic and would break that
determinism.

### 5c. `std.concurrency.broadcast` — one sender, many receivers

`Broadcast[T]` is a pub/sub channel with the tokio `broadcast` contract. It is written in Chezzi
over `Channel`, `Shared` and `AtomicInt`; the runtime has no broadcast code.
`import Broadcast, Receiver, Lagged from std.concurrency.broadcast`.

| Call | Signature | Behaviour |
|---|---|---|
| `Broadcast[T].new(cap)` | `new(cap: int) -> Broadcast[T]` | Each receiver holds at most `cap` values. A `cap` below 1 faults `Broadcast capacity must be at least 1, got <cap>` |
| `subscribe` | `subscribe(self) -> Receiver[T]` | A receiver for every value sent after this call, in send order. After `close` it is closed and empty |
| `send` | `send(self, v: T) -> None` | Does not wait for receivers; it waits only while another `send`, `subscribe` or `close` holds the hub guard. Every receiver gets its own copy of `v`. A full mailbox drops its oldest value. With no receiver the value is dropped. After `close` it faults `send on a closed Broadcast` |
| `close` | `close(self) -> None` | Idempotent. Every receiver's `for` ends after the values already sent |
| `Receiver.recv` | `recv(self) -> T!Lagged` | Blocks. `?v`, or `!Lagged(missed)` when the broadcast dropped `missed` values before the next one. On a closed and drained broadcast it faults `receive on a closed channel` |
| `Receiver.try_recv` | `try_recv(self) -> (T!Lagged)?` | Never blocks. `None` when no value waits, when another task is inside a receive on this receiver, or when the broadcast is closed and drained |
| `for r in rx:` | `r: T!Lagged` | What `recv` gives, until the broadcast is closed and drained. `Receiver[T]` is an `Iterable[T!Lagged]` |

A receiver that falls behind loses its oldest values. Its next receive reports the count, then
continues with the oldest value still held (tokio `RecvError::Lagged(n)`):

```chezzi
b := Broadcast[int].new(2)
rx := b.subscribe()
for i in 1..6:
    b.send(i)
print(rx.recv())     # !Lagged(missed=3)
print(rx.recv())     # 4
```

`Broadcast` and `Receiver` cross `spawn`, `parallel:` and an `Executor` job as shared handles, as a
`Channel` does. Two tasks that read one `Receiver` compete for its values: each value reaches
exactly one of them, as with a `Channel`, and a `Lagged` report counts only the values the broadcast
dropped. A receive holds the receiver for its whole step, so `try_recv` returns `None` while another
task is inside a receive. Give each reading task its own `subscribe()` to get every value. Nothing
unsubscribes a receiver; one that stops reading keeps at most `cap` values. A hand fan-out over a
list of channels blocks the publisher on the first full mailbox; `Broadcast.send` drops that
receiver's oldest value instead.

### 6c'. `Channel.trip()` — the manual level-trigger latch

`trip()` is the one native primitive `std.cancel` needs. It flips a permanent latch on a channel: the
channel then reports ready (`recv`/`try_recv`/`wait` → `true`) on **every** call thereafter, fanning
out to any number of receivers — like a passed `timer` deadline, but flipped on demand. (An ordinary
`Channel[bool]` can't be a fan-out `done()`: it is move-on-send, so a value reaches one receiver
once.) `trip()` is idempotent and reuses `close()`'s wake fan-out (minus the `closed` flag, so a
`wait:` arm stays *ready* rather than *skipped*). See `examples/channel_trip.chz`.
