## 8. Daemon / background tasks

A "background" task is one spawned into a **longer-lived outer nursery**
([§3](01-model.md#nursery-scoping-rules)). A task that must **outlive its scope** uses an explicit
`Executor` — deliberately **not** "`spawn` without a nursery".

Both major structured-concurrency ecosystems **forbid truly-unscoped tasks**:

- **Java (Loom / `StructuredTaskScope`):** a subtask's lifetime must not exceed its scope; all subtask
  threads are terminated at scope close. If a task **outlives** the scope, use a **separate executor /
  work queue**.
- **Elixir / BEAM:** `Task.async` (linked + awaited = the nursery analog) is separated from
  `Task.start` / `Task.Supervisor.async_nolink` (unlinked, side-effect-only daemons) — and even those
  daemons live under an explicit **supervision tree**.

So a daemon in Chezzi is attached to an explicitly-owned scope that is reaped at program exit; Go's
float-free `go` is the model both ecosystems rejected. **The implicit / top-level scope JOINS, never
cancels**: cancel-at-exit would mean a still-pending `spawn`'d task silently never runs.

### The escape hatch: `Executor` — a separately-owned work queue

The sanctioned tool for "a task that **outlives its scope** / runs in the background" is a distinct,
**explicitly-owned `Executor`**: a long-lived task pool / work queue you create, submit detached work
to, and reap yourself (Java's "separate executor", Elixir's `Task.Supervisor`). It keeps `parallel:`
pure (always structured, always joins) and confines all "outlives-its-scope" work to one
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
| `submit(f)` | **start** a detached, side-effect-only job at once (results leave via a `Channel`, like `spawn`); returns immediately without waiting for it. **Faults** on an executor that no longer accepts work: after its own `shutdown()`/`shutdown_now()` (`submit on a shut-down Executor (it no longer accepts work)`), or — because the inherited cancel chain is sticky — after the job that CREATED it was cancelled (`submit on an Executor whose creating job was cancelled`) |
| `submit_result(f)` | like `submit`, and returns a sealed `Channel[T!]` with the outcome ([§5b](02-channel.md#5b-stdconcurrencytask--result-handles-for-executor-work)) |
| `shutdown()` | **graceful** — stop accepting new work, then **wait** for the submitted work (a handle job's fault does not stop its siblings; a fire-and-forget job's fault ends the run, see [Faults and the drain contract](#faults-and-the-drain-contract)) |
| `shutdown_now()` | **attempt to stop** — drop work that has not started and ask running jobs to stop at their next cancellation point, then wait for them (Java `shutdownNow`). **Cooperative, not pre-emptive:** a job with no cancellation point (a bare CPU loop with no back-edge, a syscall already in the kernel) still finishes. A job **sleeping, waiting a timer, or parked in a nested `Executor` join IS ended**, and so is a task of a nursery the job opened that is parked on a channel or in a socket `accept`/`read`/`write`/`connect`. It reaches jobs of a NESTED executor too |

- **`defer` is the lifetime knob.** Bind the owner's reaping to any scope with `defer ex.shutdown()` (a
  function, a `recover:` block, the module top level); `defer`'s all-exit-paths guarantee reaps it on
  fall-through, `?`, `break`/`continue`, return, or panic.
- **Still no floating tasks.** Fire-and-forget work has a definite owner and a definite reap point.
  Submission is gated on the same **sendability** rules as a `spawn` capture
  ([§7](07-sendability.md#7-sendability)).
- **Submitted work is unstructured *by design*.** Reach for `parallel:` first; use an `Executor` only
  when a task must outlive the block that starts it. An `Executor` runs tasks and reaps them; it does
  not restart them (no supervision policies).
- **Usage shape that keeps you out of a race: read or assert after `shutdown()`, never between it and
  the `submit`** — in that window the job is already running.

### How an `Executor` runs jobs

`submit` **starts the job immediately** and `shutdown()` **waits** for the submitted work — the
`ThreadPoolExecutor` / `ExecutorService` model.

- **A job is a spawned task.** It starts the way `spawn` does, copies its globals at the `submit`,
  inherits its creator's cancel chain, and parks like any fiber. **Captures cross by value**: `submit`
  wires the closure through the same airlock `spawn` uses, so a mutation of a captured collection after
  the `submit` is NOT observed by the job, and a non-sendable capture faults at submit.
- **The pool** is the Executor's own runner threads, started on demand up to the worker count; an idle
  runner waits 5 ms for a next job, then leaves, so an idle Executor holds no thread.
- **`Executor(n)` caps the jobs running at once at `n`** (`Executor()` has no cap; `n < 1` faults
  `Executor(n) needs n >= 1`). A job over the cap is held: it starts when a running job finishes, and a
  parked job still counts as running (CPython `ThreadPoolExecutor(n)`). `n` jobs that each wait for a
  held job are a deadlock, reported at the parked job's blocking op (CPython hangs there).
- **A blocked job does not pin its pool thread.** A job blocking on a `Channel`, a `Shared`/`RwShared`
  guard, `time.sleep_ms`/a timer wait, a nested `Executor`'s `shutdown()` join, or a nursery join whose
  child is blocked hands its pool thread to a replacement worker, so the live pool count stays at
  `worker_count()` plus the number of jobs blocked right now. A job whose nursery is busy keeps its
  slot, so `CHEZZI_THREADS=1` still runs one CPU runner.
- **A submit that returned without a fault and is under the cap always starts**, unless a run halt (an
  exit, a job fault, a deadlock verdict) drops it. `shutdown_now()` marks the Executor shut before it
  trips the cancel, so a `submit` racing a job's `shutdown_now()` either faults or starts its job.
  `shutdown_now()` cancels held jobs only; a started job runs to its first cancellation point.
- **A run end keeps what was printed.** A job that faults or calls `os.exit` ends the run. What it
  printed before that reaches stdout, and so does the output of every job that finished, wherever
  `main` is when the run ends: at a `recv`, in a join, or in the exit drain (Go: a goroutine's writes
  before `os.Exit` are on stdout). An `os.exit` still runs no `defer` in any party.

#### Job states

A job is held, running (or parked), or ended. Before it releases a held job, the runtime reads the run
halt and the Executor's cancel (its own `shutdown_now` flag or its creator's):

1. after an `os.exit`: drop every held job and settle no handle. No `defer` runs after an exit and no
   reader returns, so nothing runs or prints after it (CPython `os._exit`, Go `os.Exit`).
2. after a fire-and-forget job fault, or under a cancel: drop every held job and settle each handle
   job it drops or cuts with the error `task cancelled: shutdown_now() stopped it before it finished`
   (CPython `shutdown(cancel_futures=True)`). A `defer` still runs after a job fault, and it may read
   a handle.
3. else: release held jobs while fewer than `n` run.

So no held job starts after an exit, a job fault, `shutdown_now()` or a creator cancel. One shape stays
accepted: a non-`defer` reader already waiting on a handle when a job fault lands may take the cancel
`!e` and run to its next wait or back-edge. The deadlock verdict still reaps a parked job without
settling its handle. Grid: `tests/executor_stop_grid.rs`.

#### Faults and the drain contract

- **A fire-and-forget job's fault ends the run at once**: no `recover:` catches it, and the first fault
  is the report.
- **A handle job** (`submit_result`, `submit_task`) never faults by itself: its fault is its handle's
  `!e`, and `shutdown()` raises no job fault.
- **A handle job's fault costs only its own result.** Its siblings still run (Python's
  `ThreadPoolExecutor`, Java's `ExecutorService`, Go's `errgroup`, none of which abort siblings by
  default). A non-terminating sibling therefore still blocks `shutdown()`.
- **A fire-and-forget job's fault stops the siblings with the run.** A held sibling never starts:

  ```chezzi
  ex := Executor(1)
  ex.submit(bad)      # faults: index 5 out of bounds
  ex.submit(slow)     # never runs; "sibling ran" is not printed
  ex.shutdown()
  ```

  Output: `runtime error (...): index 5 out of bounds (len 1)`, exit 1. Use `submit_result` when a
  sibling must still run after a fault.
- **Want the siblings to stop?** Thread a `std.cancel.Token` through the closures and poll
  `tok.cancelled()` ([§6e](05-cancel.md#6e-stdcancel--cooperative-cancellation--timeouts)) — the
  same split Go uses (`errgroup` + `context`), and the reason there is no abort flag on the Executor.
  For first-fault-aborts-everything semantics, use a `parallel:` nursery.
- **A resource cap is a separate kill switch.** An over-memory/`--timeout` abort trumps run-all,
  because a bound a sibling can outlive is not a bound: it stops the jobs that have not yet reached a
  cancellation point.
- **A dead stdout is NOT such a cap.** A broken pipe raises an ordinary fault in the job that printed,
  and every sibling still runs to completion, matching CPython's `ThreadPoolExecutor`. Under a graceful
  `shutdown()`, a submitted job that never prints and never returns (`while true: j = j + 1`) runs
  forever, so `chezzi run x.chz | head -1` on that program hangs — CPython hangs on the same shape. Use
  `shutdown_now()`: it cancels that job at its loop back-edge (measured 54 ms). A `parallel:` nursery
  aborts siblings on any fault, this one included.

#### Cancelling jobs

- **A job sleeping, waiting a timer, or parked in a NESTED `Executor` join IS ended by
  `shutdown_now()`** (measured: 55 ms against a `time.sleep_ms(3000)`, and the code after the sleep
  never runs; 213 ms against a job parked in an inner `shutdown()` whose own job runs
  `process.run("sleep 6")`). That deadline is ours, so it is a continuous checkpoint
  ([§6e](05-cancel.md#cancellation-points)). This is a **deliberate divergence from CPython**:
  `ThreadPoolExecutor.shutdown(cancel_futures=True)` does not interrupt a running `time.sleep(3)`
  (measured 3001 ms), and Go's `time.Sleep` is uninterruptible too. But Chezzi's `sleep_ms` is a fiber
  wait, and its ancestor is the async sleep both languages DO cancel — `asyncio.sleep` under a
  `TaskGroup` (cancelled at 50 ms), Go's `select { <-time.After; <-ctx.Done() }` (100 ms). An executor
  must not disagree with the nursery beside it.
- **A NESTED executor's jobs are ended too.** An `Executor` created *by a running job* inherits that
  job's cancel flag — keyed on who **created** the executor, never on who calls `submit` (a job of an
  unrelated executor may submit to it without handing over its cancel chain). Measured: an outer job
  creates an inner executor, submits a `sleep_ms(8000)`, then sleeps 8000 itself; `shutdown_now()` at
  50 ms ends the run at 56 ms with no job line printed. The ancestors are split: CPython's nested
  `ThreadPoolExecutor` does not propagate (8.04 s, both lines printed); Go's derived
  `context.WithCancel(parent)` does (50 ms). Chezzi follows its own consistency: one spelling of one
  wait cannot have two cancellation rules.

#### Program exit and `chezzi test`

- **Program-exit join.** An `Executor` is **detached** — it outlives the scope that created it — so an
  executor never explicitly `shutdown`/`shutdown_now`-ed is joined at a clean program exit, in
  creation order: the run waits for its in-flight work. A hard `std.os.exit` skips it (consistent with
  how it skips `defer`); a faulting program is not joined. This holds for an executor created inside a
  task too.
- **Detached from NURSERIES, not from an enclosing executor job.** An `Executor` created inside a
  `parallel:`/`spawn` task is not a child of that nursery: the nursery ending does not end it. An
  `Executor` created inside a running executor job is detached in the same LIFETIME sense — it is
  joined at exit, not at its creating job's return — but it inherits that job's **cancel** flag.
  Lifetime and cancellation are separate axes.
- **Under `chezzi test`, the join is scoped per test.** An executor created inside a `test fn` (or a
  suite method) is joined at the end of THAT test, and a job fault there errors the test. An executor
  created at a `*_test.chz` module top level or in a suite's `before_all` belongs to no test, so it is
  joined only at the end of the FILE, unattributed and best-effort. A job a test leaks that calls
  `std.os.exit` errors THAT test (the run continues, rc=1), while a job leaked through an executor the
  test did not create is absorbed by whichever later test is running when the exit fires.
  `tests/exit_status.rs` pins both halves.

#### Blocking inside a job, and the deadlock verdict

A job and the top-level `main` thread have no scheduler to park a fiber into, so both BLOCK IN PLACE on
an empty `recv` / full `send` / `wait:`. A `deadlock` fault is a **process-wide** verdict: every
counted party (`main`, plus each outstanding job) is blocked AND none of their waits is already
satisfiable — Go's `all goroutines are asleep` rule. So a stuck executor faults in milliseconds instead
of hanging — two jobs deadlocked in one executor, two executors deadlocked on each other, and a blocked
job with no `shutdown()` at all — while a job whose producer is still running keeps waiting, and
`ex.submit(fn(): ch.send(42))` followed by `ch.recv()` in `main` works (Go and CPython both print the
value). Under-reporting is deliberate wherever the verdict is unsure: an accepted hang is a missing
answer, a false fault is a wrong one. Residuals — an all-joiner cycle, bounded-pool starvation, and
PARTIAL deadlock (a subset stuck while the rest runs on, which Go cannot report either) — are in
[`gaps.md`](../../contributing/gaps.md).
