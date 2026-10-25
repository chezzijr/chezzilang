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

**Where the value lives.** M3.3: a `Shared[T]` holds ONE value by handle, behind one guard (Go
`struct { mu sync.Mutex; v T }`). `update(f)` runs `f` in the caller's task **on the live value, in
place**, while the task holds the guard: `s.update(fn(t): hit(t))` with `hit` doing `t.hits += 1`
and returning `t` copies nothing, however large `t` is. The value `f` returns is the new value.
`Shared.read(f)` runs `f` the same way and returns its result; a mutation `f` makes **persists**.
**`get()` returns the live value; write it only inside `update`** (a write outside the guard races
another task's `update`, the user's bug as in Go). The same holds for `RwShared.get` and
`Atomic.load`.

**The closure rule.** A closure given to `update` / `read` is an ordinary call in the caller's task:

- Its captures are the caller's bindings: a write to one lands. A capture or a module global the
  closure stores into the state is that object itself; store `G.copy()` to keep the box's own value.
- It reads and writes the run's one copy of each module global (M3.1).
- A fault inside the closure is raised in the caller and `recover:` catches it; the box keeps
  serving, and the value is **what the closure left** (a `push` made before the fault stays).
- It may block (a channel wait, a sleep): the guard belongs to the task and stays with it. A slow
  closure blocks every other user of the box, as with Elixir's `Agent`: keep it short.

`update(f)` runs `f` **under the box's process-global guard**, so the read-modify-write is atomic
against other tasks (a `get`-then-`set` races). `set` and `get` take the same guard, so a cross-task
`set` racing an in-flight `update` **blocks** and lands instead of being overwritten.

**Reentrancy limit:** `f` must not take the **same** box's guard. The result at every worker count:

| from inside `s.update(f)` / `rw.write(f)` / `rw.read(f)` | behaviour |
|---|---|
| `s.get()` / `s.read(g)` / `s.set(x)` / `s.update(g)` — same `Shared` box | **faults**: `"deadlock: this task already holds the update guard on this Shared box ..."` (Go: `Lock` in `Lock` deadlocks) |
| `rw.get()` / `rw.read(g)` / `rw.set(x)` / `rw.write(g)` — same `RwShared`, inside its `write` | **faults** the same way |
| `rw.set(x)` / `rw.write(g)` — same `RwShared`, inside its `read` | **faults**: `"deadlock: this task already holds the read guard on this RwShared box ..."` (Go: `Lock` in `RLock` deadlocks) |
| `rw.get()` / `rw.read(g)` — same `RwShared`, inside its `read` | legal (Go's recursive `RLock`) |
| `print(s)` | legal: a print takes no guard and shows the value as it is |

A cross-task `set`/`update`/`write` on the SAME box from a *different*, healthy task BLOCKS behind the
guard. Only a real wait-for cycle (same-task re-entry, or a longer AB-BA cycle across boxes) faults.

### `RwShared[T]` — cross-task read-write cell (many readers OR one writer)
`get() -> T` · `set(x: T) -> None` · `read(f: fn(T) -> R) -> R` (shared read guard; returns `f`'s
result, no write-back) · `write(f: fn(T) -> T) -> None` (exclusive write guard; `Shared.update` under
the write lock). Use it over `Shared` when reads dominate. Same reentrancy limit as `Shared.update`
(see the table above). `write` and `read(f)` run `f` on the live value, under the write guard and
the read guard. Constructed value-first: `RwShared(v)`; an optional turbofish pins (and is checked
against) the element type: `RwShared[T](v)` (a mismatch like `RwShared[str](0)` is a type error).

**`RwShared[T]` has exactly these four methods, for every `T`.** It has no container methods:
`r.len()`, `r.at(i)`, `r.fold(...)` on an `RwShared[List/Map/Set]` is a checker "no method" error whose
help line names the collection to use (`use ConcurrentList from std.concurrency.collection`). Share a
container through [`std.concurrency.collection`](08-pure-concurrency-utils.md#stdconcurrencycollection--thread-safe-collections-over-rwshared)
(`ConcurrentList`, `ConcurrentMap`, `ConcurrentSet`, `ConcurrentCounter`), which owns the per-entry
readers and the single-entry writers.

How the collections read: the container is the box's one live `List` / `Map` / `Set`.
- The single-entry readers (`len`, `at`, `slice`, `get`, `contains`) take the read guard and return
  elements by handle; a key's `hash` runs before the guard, its `eq` under it.
- The walks (`for_each`, `fold`, `for_each_entry`, `fold_entries`) take a shallow copy of the
  collection under the read guard and loop over the copy with no guard held. A walk visits exactly
  the entries present when it starts, each once, whatever the callback or another task removes or adds
  meanwhile, and the callback may call any method of the collection it walks.

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

### `concurrency.is_settled` — has a handle channel its outcome
`is_settled(ch) -> bool` (after `import std.concurrency`). True once the channel `ch` is sealed: a
`submit` channel whose job's outcome has been written. Never blocks, and never reads `false`
after `true`. `Task.done()` is this call. A non-channel argument faults. `Kind::Inline`. Its writer,
`_settle`, the job body `_run_outcome`, and `Executor._submit_settled` are std internals for `submit`: a `_` name a std
module declares is hidden from user code (the checker reports `has no member` / `has no method`).
User code calls `submit`.

### `Executor` — task pool
| Member | Behaviour |
|---|---|
| `Executor()` | A pool with no cap on the jobs running at once. |
| `Executor(n: int)` | **At most `n` jobs run at once** (Python `ThreadPoolExecutor(n)`); a job over the cap is held and starts when a running one finishes. `Executor(0)` (or any `n < 1`) faults `Executor(n) needs n >= 1`. A job parked in a blocking op still counts toward the cap, so `n` jobs that each wait for a held job are a deadlock, reported at the parked job's blocking op. |
| `submit[T](f: fn() -> T) -> Task[T]` | **Starts the job immediately** (detached) and returns its `Task[T]`, like Python's `ThreadPoolExecutor.submit`. `task` is any zero-arg callable: a fn, method path, closure, std native (`time.now_ms`) or extern fn. **A job is a spawned task**: it copies its globals at the submit, inherits its creator's cancel chain and parks like any `spawn`ed task. A job fault is its Task's `get()` outcome; it ends no run and cancels no sibling. A refused submit returns an already-settled Task. An unused Task gets the discard warning. |
| `shutdown() -> None` | **Wait** for the submitted work. It raises no job fault: a job's fault is its Task's `!e` (see `concurrency.md` §8). |
| `cancel() -> None` | Ask running jobs to stop **cooperatively** and return at once; `shutdown()` after it waits (Java `shutdownNow`). A RUNNING job with no cancellation point still finishes, but a job **sleeping, waiting a timer, or parked in a nested `Executor` join is ended**. A job held by the `Executor(n)` cap never starts, and its handle settles `!e` (CPython `shutdown(cancel_futures=True)`). See `concurrency.md` §cancellation points. |
| `Task[T]` | `get() -> T!` returns `?value` or `!message` when the job faulted, or `!"task cancelled: cancel() stopped it before it finished"` when `cancel()` or a creator cancel dropped or cut it. `done() -> bool` and `cancel() -> None` (drop a held job, cut a running one). The outcome is **sealed**: every `get()` returns it. A `--max-heap` abort is never a Task outcome. |

- No held job starts after an `os.exit`, `cancel()` or a creator
  cancel (`concurrency.md` "Job states").
- `cancel()` cancels held jobs only. A `submit` that returned without a fault and is under the cap
  always starts, and runs to its first cancellation point. The one exception is a run halt (an
  `os.exit`, a deadlock verdict), which drops it.
- A `submit` that reads the Executor as shut returns an already-settled Task whose `get()` is
  `!"submit on a shut-down Executor (it no longer accepts work)"`. A `submit` racing a job's
  `cancel()` can meet this refusal.

**Jobs do not print in submission order, and `shutdown()` does not withhold their output.** Under
`chezzi run` a job's `print` reaches stdout the moment it runs, so concurrent jobs interleave in
completion order, as with `spawn` and `ThreadPoolExecutor`. Submission order fixes only which job's
fault wins (`shutdown()` raises the lowest-index one) and the byte order of the **buffered** sink that
embedders get. For ordered output, collect results with `submit` and print them after the join.
Full statement: `concurrency.md` §"Output ordering".

A job may `shutdown()`/`cancel()` the executor it runs under: that join waits for the executor's
*other* jobs and not for itself, and the enclosing join (or the program-exit join) reduces the slots,
so no sibling's output or fault is lost.

An `Executor` is **detached**: it outlives the scope that made it, and the program waits for its
outstanding work at exit. Detached is about **lifetime**, not cancellation. An executor created inside
a *running job* of another executor is still joined at exit, but it inherits that job's cancel, so an
outer `cancel()` stops the nested executor's jobs at their checkpoints too (`concurrency.md`
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
