## 3. Runtime types (concurrency & iteration)

These types come from the language/runtime; see [`concurrency.md`](../concurrency/01-model.md) for the full model.

> **`Shared` / `RwShared` / `Atomic` / `AtomicInt` / `Executor` are members of `std.concurrency`.** A
> program names them like any module member: `import Shared from std.concurrency` (or `import Shared as
> Box from std.concurrency`) binds the bare name, and `import std.concurrency` gives
> `concurrency.Shared(0)` / `concurrency.Shared[int]`. A whole-module import binds no bare name. Without
> an import, `Shared(0)` is `unknown type 'Shared'; import it from std.concurrency (`import Shared from
> std.concurrency`)`. `Channel` is global (no import). `timer(ms)` is an ordinary `std.time` function
> (`time.timer(ms)` or `import timer from std.time`). `net.Socket` / `net.Listener` and `std.ffi`'s
> `ptr`, `owned_str` and widths follow the same rule; they are type-only (`net.Socket(...)` is rejected:
> a value comes from a module function). A module that declares its own `struct Shared` has its own
> type of that name.

### `Channel[T]` — FIFO mailbox
Three shapes:

| Constructor | Shape | `send` |
|---|---|---|
| `Channel[T]()` | **unbounded** FIFO | never blocks |
| `Channel[T](0)` | **rendezvous** (Go's `make(chan T)`) | blocks until a receiver is already waiting |
| `Channel[T](cap)`, `cap > 0` | **bounded** FIFO (Go's buffered channel) | blocks/parks while `cap` messages are queued; resumes once a `recv` frees a slot |

A full/rendezvous `send` with no possible consumer is a deadlock fault, not an over-fill or a hang.

Methods: `send(x: T) -> None` · `try_send(x: T) -> bool` (`false` = closed, full, **or** rendezvous
with no waiting receiver; never blocks) · `recv() -> T` · `try_recv() -> T?` (`?v` if queued or
offered by a parked sender, `None` otherwise) · `close() -> None` · `trip() -> None` (permanent
level-trigger latch, **`Channel[bool]` only**, gated by `where T: bool`, since it always delivers
`true`; the primitive behind `std.cancel`'s `done()`) · `len() -> int` (buffered values only; a parked
sender's value is not counted, as in Go) · `cap() -> int` (`-1` for unbounded, `0` for rendezvous, the
bound otherwise). Iterate received values with `for v in ch:` (ends when closed and drained).
Backpressure changes only *which* task runs *when*, never the value sequence a consumer sees.

**Divergence from Go: `Channel[T]()` is NOT `make(chan T)`.** Go's no-argument channel is rendezvous;
Chezzi's is UNBOUNDED, because the standard library and existing programs depend on the no-arg form
never blocking. Port Go's `make(chan T)` to `Channel[T](0)`.

### `Shared[T]` — cross-task shared cell
`get() -> T` · `set(x: T) -> None` · `update(f: fn(T) -> T) -> None` · `read(f: fn(T) -> R) -> R`.

**Where the value lives.** A `Shared[T]` keeps its value in a private heap of its own.
`update(f)` runs `f` **inside the box, on the live value, in place**: `s.update(fn(t): hit(t))`
with `hit` doing `t.hits += 1` and returning `t` copies nothing, however large `t` is. The value `f`
returns is the new state. `Shared.read(f)` runs `f` inside the box too and copies only its **result**
out; it runs on the live value, so a mutation `f` makes **persists** (use `RwShared.read` for a
reader that works on its own copy). `get` is a **whole-value copy out**: mutating it
(`s.get().push(x)`) changes the caller's copy, not the box (the checker warns on that shape). The same
holds for `RwShared`/`Atomic` `get`; a plain in-task `struct` field differs: its reads alias the live
value, but it cannot cross a spawn.

**The closure rule.** A closure given to `update` / `read` runs like the body of a spawned task:

- Its captures are **copies**. A write to a captured value is the task-copy error or fault
  (`... is this task's copy: a write to it would be lost at the join ...`), never a silent lost write.
  A capture the closure stores into the state is the box's own value once the call ends.
- It sees **module globals as the caller has them at the call** (a binding made after the box, and a
  rebinding between two calls, are both visible). A write to a module global faults the same way.
- A module global's copy that the closure stores into the state stays a copy: a later in-place
  write to it faults, and the fault says so. Store `G.copy()` instead.
- A fault inside the closure is raised in the caller and `recover:` catches it; the box keeps
  serving, and the state is **what the closure left** (a `push` made before the fault stays).
- A slow closure blocks every other user of the box and holds its worker, as with Elixir's `Agent`:
  keep the closure short and do slow work outside it.

`update(f)` runs `f` **under the box's process-global update guard**, so the read-modify-write is
atomic against other tasks (a `get`-then-`set` races). `set` takes the same guard, so a cross-task
`set` racing an in-flight `update` **blocks** and lands instead of being overwritten.

**Reentrancy limit:** `f` must not touch the **same** box's guard. The result at every worker count:

| from inside `s.update(f)` / `rw.write(f)` | behaviour |
|---|---|
| `s.get()` / `s.read(g)` / `print(s)` — same `Shared` box | **faults**: the value lives in the box and the closure holds it, so no earlier copy exists to return |
| `rw.get()` / `rw.read(g)` / `print(rw)` — same `RwShared` box, inside its `write` | **faults** the same way; inside a `read` closure they are legal, because that closure runs on its own copy |
| `s.set(x)` — same box | **faults**: `"deadlock: this task already holds the update guard on this Shared box"` |
| `s.update(g)` / `rw.write(g)` — same box | **faults** the same way, instead of hanging undetected |

A cross-task `set`/`update`/`write` on the SAME box from a *different*, healthy task BLOCKS behind the
guard. Only a real wait-for cycle (same-box re-entry, or a longer AB-BA cycle across boxes) faults.
(`rw.write` nested inside `rw.read` is the one legal same-box crossing: `read` never takes the guard,
so the nested `write` persists.)

### `RwShared[T]` — cross-task read-write cell (many readers OR one writer)
`get() -> T` · `set(x: T) -> None` · `read(f: fn(T) -> R) -> R` (shared read guard; returns `f`'s
result, no write-back) · `write(f: fn(T) -> T) -> None` (exclusive write guard; `Shared.update` under
the write lock). Use it over `Shared` when reads dominate. Same reentrancy limit as `Shared.update`
(see the table above). `write` runs in place in the box heap; `read(f)` runs `f` on a copy in the
reader's heap. Constructed value-first: `RwShared(v)`; an optional turbofish pins (and is checked
against) the element type: `RwShared[T](v)` (a mismatch like `RwShared[str](0)` is a type error).

**`RwShared[T]` has exactly these four methods, for every `T`.** It has no container methods:
`r.len()`, `r.at(i)`, `r.fold(...)` on an `RwShared[List/Map/Set]` is a checker "no method" error whose
help line names the collection to use (`use ConcurrentList from std.concurrency.collection`). Share a
container through [`std.concurrency.collection`](08-pure-concurrency-utils.md#stdconcurrencycollection--thread-safe-collections-over-rwshared)
(`ConcurrentList`, `ConcurrentMap`, `ConcurrentSet`, `ConcurrentCounter`), which owns the per-entry
readers and the single-entry writers.

How the collections read: the container is a live `List` / `Map` / `Set` in the box's private heap.
- The single-entry readers (`len`, `at`, `slice`, `get`, `contains`) copy only what they return, under
  the shared read lock, and run a key's `hash` / `eq` in the caller after the lock is dropped.
- The walks (`for_each`, `fold`, `for_each_entry`, `fold_entries`) copy the collection **once**
  (`inner.get()`) and loop over the copy. A walk visits exactly the entries present when it starts,
  each once, whatever the callback or another task removes or adds meanwhile, and the callback may call
  any method of the collection it walks. Cost: one copy of the collection per walk.
- Each piece is copied out **independently**, so two sibling closures over one captured local do NOT
  share their binding when you pull them out one at a time (two `at()` calls are two crossings). A
  whole-container `get()`/`read()`, and `slice`, are one crossing and do share. See
  [`concurrency.md`](../concurrency/01-model.md) §airlock.

### `Atomic[T]` — cross-task atomic (numeric `T` for add/sub)
`load() -> T` · `store(x: T) -> None` · `exchange(x: T) -> T` · `cas(expected: T, new: T) -> bool` ·
`add(x: T) -> T` · `sub(x: T) -> T` (return the **new** value).
`cas` compares **structurally**, never through a user `eq`. A payload type that *reaches* a user `eq`
(its own, or one on any element/entry/field/payload the structural compare recurses into) is a
check-time error (use `Shared[T]`, which has no `cas`). The VM also switches the `eq` hook off for the
compare, so the guarantee holds where the checker cannot see the witness (a protocol existential
payload). A type parameter as the payload must carry the sealed bound **`StructuralEq`**
(`protocol StructuralEq`, exported by `std.concurrency`, import-gated like `Atomic`):
`fn mk[T: StructuralEq](v: T) -> Atomic[T]`. No type implements it; the checker grants it to every
type that reaches no `eq` hook. See [`concurrency.md`](../concurrency/01-model.md).

### `AtomicInt` — monomorphic **lock-free** int atomic
`load() -> int` · `store(x: int) -> None` · `exchange(x: int) -> int` · `cas(expected: int, new: int) -> bool` ·
`add(x: int) -> int` · `sub(x: int) -> int` (return the **new** value; overflow **faults**, like `+`/`-`).
The int sibling of `Atomic[T]`: it has no `[T]`, so a lock-free `std::sync::atomic::AtomicI64` backs it
(Rust `AtomicI64` / Java `AtomicInteger` / Go `atomic.Int64`) instead of a `Mutex`. Use it over
`Atomic(0)` for a hot int counter/flag under contention (measured ~2.7× faster than the Mutex-backed
`Atomic` on an 8-way counter; see [`benchmarks.md`](../../contributing/benchmarks.md)). Same import
gate as `Atomic`. Constructed `AtomicInt(v)` (one int arg; `AtomicInt(3.5)` is a type error).

### `concurrency.is_task_copy` — is this value an airlock copy?
`is_task_copy(v) -> bool` (after `import std.concurrency`). `true` when `v` is this task's airlock copy
of a value another task owns: a spawned task's argument or capture, or an Executor job's capture. A
write to such a value faults `this value is this task's copy` (D4, `concurrency.md`); `is_task_copy`
reads the same mark, so std code can choose between writing a value and reading shared state without
attempting the write. `false` for a value the calling task owns, a value received from a `Channel`,
and a scalar. Its std caller is `Task.get()` (`std.concurrency.task`). It is `Kind::Inline`: it reads
the calling task's own heap, so it never runs off-heap on the blocking pool.

```
fn child(xs: List[int]):
    print(concurrency.is_task_copy(xs))     # true: the spawned task holds a copy

xs := [1]
print(concurrency.is_task_copy(xs))         # false: the parent owns xs
parallel:
    spawn child(xs)
```

`spawn print(concurrency.is_task_copy(xs))` prints `false`: a spawn's call arguments run in the
parent (Go `go f(x)`), so the query runs before the crossing.

### `concurrency.task_copy_of` — this task's copy of a value
`task_copy_of[T](v: T) -> T` (after `import std.concurrency`). Returns `v` rebuilt as this task's
copy, so a later write to it faults `this value is this task's copy` (D4). It is the rebuild every
crossing runs: a function inside `v` leaves its module's globals unmarked, and a generator's
frame-local list the creating task never saw stays writable. It is for std code that reads a shared
core on behalf of a handle its owner aliases: `Task.get()` and a `memoize1` wrapper. A plain
`Shared.get()` or `ch.recv()` result is the receiver's own. The engine runs it
(`Kind::InterceptAirlock`).

### `concurrency.is_settled` — has a handle channel its outcome
`is_settled(ch) -> bool` (after `import std.concurrency`). True once the channel `ch` is sealed: a
`submit_result` channel whose job's outcome has been written. Never blocks, and never reads `false`
after `true`. `Task.done()` is this call. A non-channel argument faults. `Kind::Inline`. Its writer,
`_settle`, and `Executor._submit_settled` are std internals for `submit_result`: a `_` name a std
module declares is hidden from user code (the checker reports `has no member` / `has no method`).
User code calls `submit_result`.

### `Executor` — task pool
| Member | Behaviour |
|---|---|
| `Executor()` | A pool with no cap on the jobs running at once. |
| `Executor(n: int)` | **At most `n` jobs run at once** (Python `ThreadPoolExecutor(n)`); a job over the cap is held and starts when a running one finishes. `Executor(0)` (or any `n < 1`) faults `Executor(n) needs n >= 1`. A job parked in a blocking op still counts toward the cap, so `n` jobs that each wait for a held job are a deadlock, reported at the parked job's blocking op. |
| `submit(task: fn() -> _) -> None` | **Starts the job immediately** (detached, fire-and-forget), like Python's `ThreadPoolExecutor.submit`. `task` is any zero-arg callable: a fn, method path, closure, std native (`time.now_ms`) or extern fn. **A job is a spawned task**: it copies its globals at the submit, inherits its creator's cancel chain and parks like any `spawn`ed task. A fire-and-forget job's fault ends the run; no `recover:` catches it. |
| `shutdown() -> None` | **Wait** for the submitted work. It raises no job fault: a handle job's fault is its handle's `!e` (see `concurrency.md` §8). |
| `shutdown_now() -> None` | Ask running jobs to stop **cooperatively**, then wait (Java `shutdownNow`). A RUNNING job with no cancellation point still finishes, but a job **sleeping, waiting a timer, or parked in a nested `Executor` join is ended**. A job held by the `Executor(n)` cap never starts, and its handle settles `!e` (CPython `shutdown(cancel_futures=True)`). See `concurrency.md` §cancellation points. |
| `submit_result[T](f: fn() -> T) -> Channel[T!]` | Submit `f` and get a cap-1 channel carrying its outcome: `?value`, `!message` when the job faulted, or `!"task cancelled: shutdown_now() stopped it before it finished"` when `shutdown_now()`, a creator cancel or a fire-and-forget job fault dropped or cut it. The channel is **sealed**: every `recv` returns the outcome and takes nothing, so do not iterate it with `for`; to collect several outcomes keep a list of `submit_result` channels. `std.concurrency.task.submit_task` / `Task[T]` wraps this primitive. There is no variant that settles into a caller's channel: std seals only a channel std created. |

- No held job starts after an `os.exit`, a fire-and-forget job fault, `shutdown_now()` or a creator
  cancel (`concurrency.md` "Job states").
- `shutdown_now()` cancels held jobs only. A `submit` that returned without a fault and is under the cap
  always starts, and runs to its first cancellation point. The one exception is a run halt (an
  `os.exit`, a fire-and-forget job fault, a deadlock verdict), which drops it.
- A `submit` that reads the Executor as shut faults `submit on a shut-down Executor (it no longer
  accepts work)`. A `submit` racing a job's `shutdown_now()` can meet this fault.

**Jobs do not print in submission order, and `shutdown()` does not withhold their output.** Under
`chezzi run` a job's `print` reaches stdout the moment it runs, so concurrent jobs interleave in
completion order, as with `spawn` and `ThreadPoolExecutor`. Submission order fixes only which job's
fault wins (`shutdown()` raises the lowest-index one) and the byte order of the **buffered** sink that
embedders get. For ordered output, collect results with `submit_result` and print them after the join.
Full statement: `concurrency.md` §"Output ordering".

A job may `shutdown()`/`shutdown_now()` the executor it runs under: that join waits for the executor's
*other* jobs and not for itself, and the enclosing join (or the program-exit join) reduces the slots,
so no sibling's output or fault is lost.

An `Executor` is **detached**: it outlives the scope that made it, and the program waits for its
outstanding work at exit. Detached is about **lifetime**, not cancellation. An executor created inside
a *running job* of another executor is still joined at exit, but it inherits that job's cancel, so an
outer `shutdown_now()` stops the nested executor's jobs at their checkpoints too (`concurrency.md`
§cancellation points). That inheritance is **sticky** (a cancelled parent stays cancelled, as in Go),
so a later `submit` to such an executor **faults**: `submit on an Executor whose creating job was
cancelled`. **Read results after `shutdown()`, never between it and the `submit`**: in that window a
job is already running (eager `submit`, like CPython's `ThreadPoolExecutor`), so you observe a race.
The same holds for a `spawn`ed task, which also starts at the `spawn` (Go's `go f()`) and not at the
nursery's join (`concurrency.md` §4).

A blocking `recv`/`send`/`wait:` inside a job, or in `main` while jobs run, **blocks and waits**. A
`deadlock` fault comes only once the whole run is stuck: every party blocked, none of their waits
satisfiable. That is Go's rule (`all goroutines are asleep`), so `ex.submit(fn(): ch.recv())` with
nobody to send faults in milliseconds instead of hanging, while `ex.submit(fn(): ch.send(42))` then
`ch.recv()` in `main` works. Residual cases: [`gaps.md`](../../contributing/gaps.md).

### `Socket` / `Listener` — from `std.net` (see §4)
- `Socket`: `read(n: int, timeout_ms?: int) -> str!` · `write(s: str, timeout_ms?: int) -> int!` ·
  `read_bytes(n: int, timeout_ms?: int) -> bytes!` · `write_bytes(b: bytes, timeout_ms?: int) -> int!` ·
  `close() -> None`.
  `read` is a **`str`-only seam**: it decodes, and it never decodes lossily (no U+FFFD, ever):
  - `n` bounds the NEW bytes taken off the socket. If the previous read ended mid-codepoint, its ≤3-byte
    tail is carried on the socket and prepended here, so `read(n)` can return **up to `n + 3` bytes**,
    and reading valid text in a loop (even `read(1)`) reassembles it **byte-exactly**.
  - **A read blocks until it has at least one whole character.** A chunk that ends mid-codepoint cannot
    go to a `str` seam, so the read waits for the rest of it (the contract of Go's
    `bufio.Reader.ReadRune` and Python's text-mode socket file). The two escapes are `timeout_ms` and
    the peer closing (which errors, see below, and does not drop the tail). `timeout_ms` bounds the
    **whole call** on every path (a plain read, one that parks, and one reached inside a callback like
    `list.map`): the deadline is fixed when the call starts, so finishing a split codepoint never
    re-arms it. A timed-out read keeps the carried tail for the next read; no bytes are lost.
  - **Timeout vs. incomplete UTF-8:** on **every** timeout path, `!"timeout"` means *nothing arrived*.
    If the call took 1–3 bytes off the wire that did not complete a character, you get
    `!"incomplete utf-8: …"` instead, because those bytes stay on the socket; read again to finish the
    character. (`read_bytes`/`write`/`accept` never decode, so their timeouts are always `"timeout"`.)
  - **`read(n, 0)` polls once.** Same classification: `!"timeout"` if nothing arrived,
    `!"incomplete utf-8: …"` if the poll took a partial character. Both are "not ready yet" signals for a
    poll loop.
  - `read(0)` (or a negative / computed-zero `n`) is a **no-op** `?""`: it never touches the socket,
    never reports EOF, and leaves any carried tail for the next read. It still reports a closed socket
    (`!"read on a closed socket"`).
  - Two tasks may share one `Socket` (it crosses the airlock as a shared handle): each `read` takes its
    bytes off the socket and decodes them as ONE atomic step, so concurrent readers see wire order (they
    still must not both *block* on it: a second parked op on a shared socket is a fault).
  - Bytes that are not UTF-8 (a **binary payload**) → `!"invalid utf-8 on the socket: …"`.
    **Nothing is discarded, and the error is sticky:** valid text that arrived *before* the bad byte is
    delivered first (a normal `?v`), and the undecodable bytes stay on the socket, so every later
    `read` returns the same `!e`. Switch to `read_bytes` (below), which hands them over byte-exactly.
  - An incomplete codepoint left when the peer closes → `!"invalid utf-8 at eof: …"`.
  - `close()` returns `None` (no error channel): a still-carried tail at `close` is dropped. The EOF
    error surfaces on the `read` that sees the close, not on `close`.
  - `close()` from another task wakes a task parked in `accept`/`read`/`read_bytes`/`write`/`write_bytes`
    on the same handle; that call returns `!"<op> on a closed listener|socket"` (Go's `Close`
    cancelling a blocked `Accept`/`Read`).
  - `write`/`write_bytes` return `?len` only after every byte is sent (Go's `Conn.Write`). If the
    socket is closed, the `timeout_ms` deadline passes, or the OS reports an error first, the call
    returns `!e` and does not report the bytes already sent. `write(s, 0)` returns `!"timeout"`
    unless the whole payload fits at once.
  - **Binary payloads: use `read_bytes` / `write_bytes`.** They never decode, so any payload survives
    byte-exactly. Differences from the `str` `read`: `read_bytes(n)` returns **at most `n`** bytes;
    `?(b"")` is the EOF sentinel; `read_bytes(0)` is a no-op `?(b"")` that still errs on a closed
    socket; and it **drains any carried tail first**, including the undecodable bytes a str `read`'s
    sticky `!"invalid utf-8 …"` refused to deliver, so mixing the two on one socket is lossless.
    `write_bytes` takes a `bytes` (convert a `bytearray` with `bytes(ba)`). `timeout_ms` behaves as
    for `read`/`write`.
- `Listener`: `accept(timeout_ms?: int) -> Socket!` · `addr() -> str!` · `close() -> None`.
  `close()` from another task wakes a task parked in `accept`; that call returns the error `accept on a
  closed listener` (Go's `Close` cancelling a blocked `Accept`).
- `Socket`/`Listener` are std type names (a module may declare its own `struct Socket`, which shadows
  it). A bare annotation requires `import std.net` (whole-module, or `import Socket from std.net`):
  they are NOT global builtins, like `Shared`/`Executor` (std.concurrency) and `ptr` (std.ffi).

### Iterator cursors & generators
A `.iter()` cursor and a generator value both expose `next() -> T?` and `iter() -> Iterator[T]`
(idempotent: an iterator is its own iterable). See the `Iterator`/`Iterable` protocols and `yield`
in `syntax.md`.

**Re-entrancy.** You cannot resume a generator while it runs: a `.next()` (or a `for`) on the
generator that is *currently executing*, reached from inside its own body, is a recoverable
`generator already running` fault, catchable by `recover:` (Python raises `ValueError: generator
already executing`). It is a fault because a live, non-exhausted generator must never report itself
EXHAUSTED (`None`). A generator whose body **faulted** is *closed*, like Python's: a later `.next()`
answers `None`.
