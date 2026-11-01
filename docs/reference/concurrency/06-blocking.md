## Blocking-context table

Every blocking op asks ONE table how to block in the running context: `block::mode(ctx, spec)` in
`src/vm/block.rs`. `block::tests::every_cell_matches_the_table` pins every cell, and
`tests/blocking_context_grid.rs::grid_every_op_in_every_context` runs every op in every context at
T=1, T=2 and T=0 against the Go answer. Changing a cell means changing `block::mode`, its unit test and
this table in one commit.

The steps a wait takes come from one more table, `block::wait_steps(spec, ctx)`. `Vm::wait_begin`
takes steps 2 and 3 once at the start of the wait and `Vm::wait_end` undoes them at its end (an in-place channel or `wait:` loop returns to its caller between ticks, so it brackets each tick) (Go
`entersyscall` / `exitsyscall`); the one wait door, `Vm::wait_door`, takes step 1 on each tick:

| step | when |
|---|---|
| 1. detach the thread from the run's one heap | every wait |
| 2. hold no runner permit for the whole wait, and give a `parallel:` body's runner slot to its sched | every row of the table below but `extern`, when the thread holds one; the end takes a free permit first, else queues |
| 3. demote (hand the runner slot to a replacement) | the `D` cells below; a nursery join hands its slot over in its own joiner, in its `D` and `I` cells |

The same table gives each wait the counts it registers for the deadlock verdicts. `Vm::wait_end`
drops them before it takes the permit back; an in-place channel tick drops them after the permit
take, before its next pop:

| wait | counts |
|---|---|
| ends by itself (timer, sleep, offload, host fd, socket, connect, a timed `wait:`) | none; a demoted one is `inflight` |
| `recv`, `send`, an untimed `wait:`, a guard, an Executor join | a party on a judged thread (`main`, a `main` `defer:`), the eager bodies of its thread marked blocked on it; a demoted one is a waiter |
| a nursery join or abort | a party on a thread with no sched under it (`main`, the builder, `main` in a callback); a blocked owner on a fiber's thread; the eager bodies of its thread wait for it |
| an `extern` C call | none: a thread in C is running, so both verdicts decline while it lasts (Go: a goroutine in cgo is not asleep) |

A wait that ends by itself is a live party, so both verdicts decline while it lasts: a `wait:` whose
timer arm is live returns at the deadline, as Go's `select` with `time.After` does.
`tests/wait_counts_grid.rs` runs each wait kind in each context as a deadlock and as a live program.

A guard wait takes the permit back in the middle, before it takes the guard (a thread that took the
guard first would park the guard's owner behind permit holders), and gives it back again if it loses
that race. A thread in a loop of in-place channel ticks that a waker queued for a permit passes
that turn on before its next tick, so the queue never stalls behind it.

A short wait with no row (a regular-file syscall, a `stat`, a permit take) keeps its permit. The
garbage collector stops every attached thread and treats a detached thread as stopped. A blocking
call that keeps its attachment hangs the next collection. Known limit: a CPU-bound inline native
delays a stop until it returns (a C call runs detached, so it does not). The model is in
[`01-model.md`](01-model.md).

An `extern` C call (`WaitSpec::Extern`, `block::Permit::Watched`) keeps its permit and stamps its
thread's slot (Go `entersyscall`). The `chezzi-sysmon` thread (`src/vm/sysmon.rs`, Go `sysmon`)
starts at the first C call and reads every slot each tick: 20 us, doubled to 10 ms while nothing is
retaken. A call it sees on two ticks, while a thread waits for a permit or the caller's sched has a
queued fiber, is retaken: the watcher releases the permit for the thread, and for a worker it starts
the replacement worker a demote would start (armed once per worker). So at `CHEZZI_THREADS=1` a sibling
runs during a long C call, as Go 1.27 at `GOMAXPROCS=1` does. The call takes a permit back at its
return, and a C callback takes one back before its Chezzi code runs (Go `exitsyscall`). A halt that
arrives during the call (a cancel, a child's fault, `os.exit`) is delivered at the return, as for a
blocking native; no ancestor interrupts a running C call. The watcher runs under `CHEZZI_SCHED_SEED`
too: a C call that lasts longer than one watcher tick (20 us) is a wall-clock event, as a timer is,
so a seeded replay is exact only for C calls shorter than one tick.

Contexts:

| context | what it is |
|---|---|
| **Park** | an M:N fiber with no host frame under it |
| **Demote** | an M:N fiber inside a native callback, `defer:`, generator resume or `Shared.update` closure |
| **Builder** | the inline outermost-`parallel:` builder (`job` = it carries an eager `Executor` core) |
| **PoolJob** | an eager `Executor` job |
| **OwnThread** | `main`, a `main` `defer:`, `main` inside a callback |

Modes:

| mode | meaning |
|---|---|
| P | park the fiber on the op's own bucket |
| D | demote: block in place, hand the runner slot to a replacement worker |
| I | block this thread in place; it runs nothing else |
| H | help (a nursery join only): wait in place and run the joined scheduler's fibers on this thread |
| S | sleep to the op's own deadline |
| R | refuse (the op's documented error or fault) |

| wait (`WaitSpec`) | Park | Demote | Builder | Builder `job` | PoolJob | OwnThread | `will_return` |
|---|---|---|---|---|---|---|---|
| `recv` (empty) | P | D | R | I | I | I | no |
| `timer(ms).recv()` | P | D | S | S | S | S | yes |
| full `send` | P | D | R | I | I | I | no |
| `wait:` (recv arms) | P | D | R | I | I | I | no |
| `wait:` with a send arm | P | D | R | I | I | I | no |
| `wait:` with a timer arm | P | D | S | I | I | I | yes |
| `wait:` with timer + send arms | P | D | S | I | I | I | yes |
| `sleep_ms` | P | D | S | S | S | S | yes |
| blocking native (`fs`, `request`, `process`) | P | D | I | I | I | I | yes |
| stdin (`io.input`, …), a `Writer`/`Reader` on a FIFO, pipe or device | D | D | D | D | D | D | yes |
| socket `accept`/`read`/`write` | P | D | R | R | R | I | yes |
| `net.connect` handshake | P | I | I | R | R | I | yes |
| `Shared` update guard | D | D | D | D | D | D | no |
| `Executor.shutdown()` / join | D | D | I | I | I | I | yes |
| `parallel:` nursery join | P | D | H¹ | H¹ | I | H¹ | no |
| `extern` C call (the watcher hands over a long call's permit) | I | I | I | I | I | I | yes |

¹ I when a host frame is under the joiner (a native callback or a defer drain): the joiner only waits.
A fiber's nursery join in a Demote context hands its runner slot to a replacement worker and waits; it
runs no other fiber on its stack, because that fiber could wait for something only the buried frame
can give.

A Builder `R` cell for `recv`/`wait:` is the "deadlock — no runnable task can send" fault: a blocking
`recv`, `for v in ch:` or `wait:` issued directly in an inline `parallel:` body (not inside a
`spawn:`) faults there, and the diagnostic points at the fix: put blocking work in a `spawn:`.

How `will_return` drives the deadlock accounting, and the cells judged against Go 1.27:
[`concurrency-runtime.md`](../../design/concurrency-runtime.md#blocking-table-accounting).

**Deliberate differences from Go**, each a `R` or hang cell of the table:

- A socket op in an Executor job returns the "doesn't own its thread" `!e`; `connect` there too.
- An unjudged context (`main` inside a real callback, a job inside a callback) with nothing that can
  satisfy it HANGS, where Go faults: the verdict declines rather than risk a false fault
  ([`lessons.md`](../../contributing/lessons.md) §4). The grid pins it (`recv/main_cb/Nothing` hangs,
  `recv/main_defer/Nothing` faults).

---

### `std.net` — where a would-block socket op blocks

A `spawn`/`parallel:` fiber parks on the netpoller. A socket op reached where there is no fiber to
park **blocks its thread in place** in exactly two contexts:

1. a thread that owns itself: `main`, a `main` `defer:`, `main` inside a native callback
   (Go-identical: `ln.Accept()` on the main goroutine blocks until a client arrives; CPython and Go
   block and read in the callback and `defer:` shapes too);
2. an M:N worker inside a native callback (which spins a replacement worker first).

Everywhere else — an `Executor` job — it returns the error:

```
<op> would block: an Executor job doesn't own its thread — blocking here would starve every other job and `parallel:` nursery sharing the pool. Do this socket op inside `spawn:` or a `parallel:` nursery instead, where it parks rather than blocking a shared thread.
```

A blocked job hands its pool thread to a replacement, so the refusal no longer rests on starvation; it
stays because widening it would change what these ops RETURN on a would-block fd (a hang instead of
this `!e`). **The op set is `accept`/`read`/`read_bytes`/`write`; `connect` joins it ONLY inside an
`Executor` job.** Those four wait on a Chezzi peer fiber that can only run on the very thread they
would block, whereas a `connect` handshake is completed by the kernel and starves no Chezzi party, so
top-level `main` blocks it and succeeds, as CPython (0.1 ms) and Go (314 µs) both do. Inside a job,
`connect` refuses instead of spinning in place (an outer `cancel()` at 200 ms ends the run at
209 ms). Use `spawn`/`parallel:` for socket work, which parks instead of blocking.

- **On top-level `main`, an untimed op has NO escape but SIGINT.** The escapes are the op's own
  `timeout_ms`, the run's `--timeout`, and cancellation. Under `chezzi run` two of those three do not
  exist: `--timeout` is a **`chezzi test` flag only** (`chezzi run --timeout=500 f.chz` →
  `chezzi run: unknown flag '--timeout=500'`), and `main` has no scope, so there is no cancel flag to
  trip. `accept()`/`read()`/`write()` on `main` with no `timeout_ms` therefore blocks forever on an fd
  that never becomes ready — Go-identical. Pass a `timeout_ms` when you need a bound.
- **A socket-blocked `main` SUPPRESSES the process-wide deadlock verdict** while its op is
  outstanding — for a never-ready fd, permanently. `main` in `accept()` plus an `Executor` job blocked
  on a `Channel` nothing can send: Chezzi prints `listening` and hangs (rc=124). Go hangs identically —
  an open socket is a runnable party, so `all goroutines are asleep` never fires (measured, rc=124).
- **Closing a `Socket`/`Listener` from another task while a sibling is parked on it wakes the parked
  op with an `!e`** naming the closed resource (`"<op> on a closed listener|socket"`), Go's `Close`
  cancelling a blocked `Accept`/`Read` — it never hangs or crashes the netpoller.

A `Kind::HostWait` native (the `std.io` stdin readers) demotes the worker and gives its slot back for
the read, direct or inside a callback, so a runnable sibling runs while stdin is withheld, as under
Go's `sysmon`. An `io.create`/`io.append`/`io.open` handle on a FIFO, a pipe or a device does the same
for each open, write, flush, read and close that reaches the fd, so the task on the other end runs,
also at `--threads=1` (Go: a blocking syscall hands off its P). A handle on a regular file keeps the
fast path: its syscalls answer at once. A `print` or `str` of such a handle while another task holds
it in a blocked syscall waits the same way, so the task on the far end of the FIFO still runs at
`--threads=1`.

---

### 6g. Checking your own program for schedule-dependence

Run the program at more than one worker count and diff:

```sh
chezzi run examples/concurrent_jobs.chz > a.txt
CHEZZI_THREADS=2 chezzi run examples/concurrent_jobs.chz > b.txt
diff a.txt b.txt
```

The repo's own standing gate does this over `tests/chz` (`tests/chezzi_threads_cli.rs`).
`CHEZZI_THREADS=1` is one CPU runner (1.00 cores, vs 1.91 at `=2`), so the diff compares a serialized
run against a concurrent one. A difference is a **signal to investigate**, not automatically a bug: it
can be an order-dependence / race / scheduler fault, *or* a **non-deterministic cross-task print
order**, which `chezzi run` does not promise
([Output ordering](01-model.md#output-ordering-streaming-cli-vs-buffered-sink)). Compare a line SET, or
make the program deterministic by construction, before calling a diff a defect. A seeded scheduler
(`CHEZZI_SCHED_SEED`, `schedfuzz`) is in [`dev-commands.md`](../../contributing/dev-commands.md).
