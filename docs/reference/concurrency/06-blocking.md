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
| 3. demote (hand the runner slot to a replacement) | the `D` cells below; a nursery join hands its slot over in its own joiner, in its `D` cell |

The run's one deadlock check does not read this table. It reads one run-wide count of the tasks
that are not asleep, which changes only when a task starts, parks, is woken or ends (Go's
`checkdead`). A parked fiber is asleep: a channel, `wait:`, guard or join wait parks it. A task
that waits in place stays awake: a stdin read, a demoted wait (an FFI callback, a map/set probe
hook), a blocking native, a socket, a C call. A pending timer counts as awake while a task waits on it and it has not fired, so a
`wait:` whose timer arm is live returns at the deadline, as Go's `select` with `time.After` does.
When the count reaches 0 with tasks alive, the run ends with `deadlock: every task is blocked`
([02-channel.md](02-channel.md)). `tests/wait_counts_grid.rs` runs each wait kind in each context as
a deadlock and as a live program, and `tests/awake_count_grid.rs` pins the count itself.

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
| **Park** | a fiber with no host frame under it. Every party that runs Chezzi code is a fiber (`main` is fiber 1, a `spawn:` task, an `Executor` job), and every callback runs as a frame on it, so its waits are **Park** too: a hook, an operator, a nested `==`/`!=`/`in`/`str`/`<` walk (`contains`, `index_of`, `unique`, `dedup`, `sort`, `sort_by_key`, `min`/`max`, `x.eq(y)`, `x.str()`, `x.compare(y)`, `print`, an f-string, a failed `assert`'s operand text), a HOF callback (a native callee such as `xs.map(time.sleep_ms)` included), a sort comparator, a `T.from_iter(it)` or `ch.iter()` drain, a `json.decode` `default` thunk, a `Shared`/`RwShared` closure, a generator body and every `defer` drain, a hard-halt one (`--timeout`, `--max-heap`, a cancel) included |
| **Reentered** | a fiber with a host frame under it: an FFI callback (a C frame is under it, as under Go's cgo callback), a map/set probe hook (`hash`, or a key's `eq` during a lookup or set algebra), or a `defer` drain under one of those. An `Inline` native or a builtin (`ord`, `chr`, `panic`) still runs its callee nested, but none of them can wait or call user code |

Modes:

| mode | meaning |
|---|---|
| P | park the fiber on the op's own bucket |
| D | demote: block in place, hand the runner slot to a replacement worker |
| I | block this thread in place, keeping its runner slot |

| wait (`WaitSpec`) | Park | Reentered | `will_return` |
|---|---|---|---|
| `recv` (empty) | P | D | no |
| `timer(ms).recv()` | P | D | yes |
| full `send` | P | D | no |
| `wait:` (recv arms) | P | D | no |
| `wait:` with a send arm | P | D | no |
| `wait:` with a timer arm | P | D | yes |
| `wait:` with timer + send arms | P | D | yes |
| `sleep_ms` | P | D | yes |
| blocking native (`fs`, `request`, `process`) | P | D | yes |
| stdin (`io.input`, …), a `Writer`/`Reader` on a FIFO, pipe or device | D | D | yes |
| socket `accept`/`read`/`write` | P | D | yes |
| `net.connect` handshake | P | I | yes |
| `Shared` update guard | P | D | no |
| `Executor.shutdown()` / join | P | D | no |
| `parallel:` nursery join | P | D | no |
| `extern` C call (the watcher hands over a long call's permit) | I | I | yes |

A fiber's nursery join in the Reentered context hands its runner slot to a replacement worker and
waits; it runs no other fiber on its stack, because that fiber could wait for something only the
buried frame can give.

How `will_return` drives the deadlock accounting, and the cells judged against Go 1.27:
[`concurrency-runtime.md`](../../design/concurrency-runtime.md#blocking-table-accounting).

**Deliberate differences from Go**, each a hang cell of the table:

- A wait in the Reentered context (an FFI callback, a user `hash`/`eq` in a map/set probe) blocks in
  place and stays awake, so with nothing that can satisfy it the run HANGS, where Go faults (Go has
  no such case). Every other wait parks, and its deadlock is reported: `recv/main_cb/Nothing`
  (`[0].map(op)`), `recv/main_defer/Nothing`, a wait in a nested `==`/`str`/`<` hook, a
  `json.decode` default and a hard-halt `defer` drain alike (plan `2026-11-18-every-wait-parks`).
  What still hangs is `docs/contributing/gaps.md` W23-1, pinned by the `Residual` kind of
  `tests/callback_park_grid.rs` (`map_key_hash`).

---

### `std.net` — where a would-block socket op blocks

A fiber parks on the netpoller: `main`, a `spawn`/`parallel:` task and an `Executor` job alike. A
socket op **blocks its thread in place** only in the Reentered context (an FFI callback or a map/set
probe hook): `accept`/`read`/`read_bytes`/`write` demote the worker first (a replacement worker runs
the other fibers), and a `connect` handshake blocks in place and keeps its slot, since the kernel
completes it and it starves no Chezzi party. Use `spawn`/`parallel:` for socket work.

- **On top-level `main`, an untimed op has NO escape but SIGINT.** The escapes are the op's own
  `timeout_ms`, the run's `--timeout`, and cancellation. Under `chezzi run` two of those three do not
  exist: `--timeout` is a **`chezzi test` flag only** (`chezzi run --timeout=500 f.chz` →
  `chezzi run: unknown flag '--timeout=500'`), and `main` has no scope, so there is no cancel flag to
  trip. `accept()`/`read()`/`write()` on `main` with no `timeout_ms` therefore blocks forever on an fd
  that never becomes ready — Go-identical. Pass a `timeout_ms` when you need a bound.
- **A socket-blocked `main` is awake, so no deadlock is reported** while its op is
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
