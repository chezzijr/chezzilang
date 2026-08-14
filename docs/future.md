# Chezzi — Future Directions (brainstorm, NOT scheduled)

> **Status:** speculative design notes. Forward-looking and opinionated. Nothing here is committed
> work — [`PROGRESS.md`](../PROGRESS.md) is the source of truth for what's actually scheduled and done.
> This doc captures *what would make Chezzi an effective scripting language* and *how to make it
> faster*, with verdicts and rough implementation shape. Most of §1–§3 has since **shipped** (noted
> inline); §4 (optimizations) is the live M19 backlog.

The language **core** is broadly implemented and still evolving (scalars, `list`/`map`/`set`/`tuple`, generic structs +
enums, `Result`/`Option` + `?`, generics + structural protocols, exhaustive `match`, closures/HOF,
modules, GC, interpolation, pipe, panic recovery via `recover:`, the `Iterator[T]`
protocol bound). What follows is the gap between "core implemented" and "language you reach for to write
real scripts."

---

> **Promotion status:** §1 (`defer`) and the §3 scripting features have **shipped** (M15–M18); §2
> (concurrency) has **shipped through Tier-D**. They stay documented here for the design rationale.
> §4 (optimizations) is the live M19 backlog. See [`PROGRESS.md`](../PROGRESS.md) for landing detail.

## 1. `defer` (cleanup on scope exit) — ✅ **SHIPPED (M17)**, **block-scoped since M18** — see `examples/defer.chz`

> **M18 update:** shipped frame-scoped in M17, then moved to **block/lexical scope** — a `defer` runs
> when its enclosing indented block exits (loop body, branch, `recover:`, `match` arm, function body,
> module top level), not just at function return. Realises the "cleanup on scope exit" intent below
> more literally. See the M18 entry in `PROGRESS.md` and the `defer` section of `docs/syntax.md`.

Before M11 this was weak: no panic meant nothing to clean up after. **Now there is unwinding** —
the `recover:` boundary, `?` propagation, and runtime faults all unwind. So `defer` earns its keep
by running on **all three exit paths**: normal return, `?` short-circuit, and panic unwind. That is
exactly Go's value proposition.

**Implementation shape**
- Per-frame deferred-call stack, drained LIFO on *every* frame exit including unwind.
  - VM: drain at `Return` **and** inside the handler-stack unwind (`PushHandler`/`PopHandler` already exist).
- **Arg-evaluation timing:** evaluate `defer` arguments *at the `defer` statement* (Go semantics),
  not at exit. Less surprising; the deferred call closes over already-evaluated values.

**Alternative considered:** Python-style `with` (context-manager protocol `enter`/`exit`). More
Python-feel, but needs a new protocol + an indentation block. `defer` is simpler, adds no protocol,
and composes cleanly with `recover:`. **Recommend `defer`.**

---

## 2. Concurrency + parallelism — the shared-nothing (BEAM) model

> **Shipped through Tier-D.** The full design — `spawn`/`parallel:` nursery, `Channel[T]`,
> `Shared[T]`, sendability — lives in its own canonical doc **[`docs/concurrency.md`](concurrency.md)**,
> with phase history in [`docs/concurrency-tier-d.md`](concurrency-tier-d.md). Real OS-thread M:N
> engine via `--parallel`. **M-C implicit nurseries shipped** — bare `spawn` legal anywhere; every
> function body / module top level joins its tasks at `return`/end. Concurrency is feature-complete.

---

## 2b. Post-freeze: retire the serial engine + rebuild the oracle layer (planned — NOT before the JIT freeze)

> **Decision (2026-07-22).** The cooperative `--serial` engine is **not the model Chezzi ships** — the
> real runtime is M:N (and, post-freeze, a JIT'd M:N). `--serial` exists today only as the byte-identical
> **parity oracle**. Post-freeze it will be **removed**, and its oracle job re-covered by a layered set of
> better-targeted oracles. This is deliberately deferred **past** the JIT freeze; nothing changes pre-freeze
> except documenting the plan and the one pre-freeze limit it exposes (`docs/gaps.md` **N10**).

**Why remove it (two independent reasons):**
1. **It can't truly test concurrency.** The serial engine is single-threaded and cooperative — it cannot
   preempt a running fiber (`docs/gaps.md` **N8**), so a whole class of concurrent programs either hangs
   (CPU-bound sibling) or takes a different schedule than M:N and diverges (**N9**, **N10**). As a
   concurrency oracle it only ever compared *one* cooperative schedule against *one* M:N schedule — a weak
   differential that misses interleaving-dependent races entirely.
2. **Keeping it byte-identical to M:N is accumulating technical debt.** Byte-identity forces the M:N engine
   to bend to what a single cooperative thread can reproduce. The code has already had to **split the two
   engines** at multiple sites (`op_wait_poll` timer-arm handling, the module-global snapshot path, the
   Executor drain) with per-engine branches whose *only* purpose is to keep serial matching M:N. Post-JIT,
   a JIT'd M:N can't be byte-identical to a tree-walking cooperative loop at all, so this debt only grows.

**The replacement oracle layer** (each covers a class serial did, better and without the byte-identity tax):

| Bug class | Replacement oracle |
|---|---|
| Sequential shared wrongness (both engines agree, both wrong) | **CPython differential** — `src/difftest/` (already built) |
| Channel/select semantic wrongness (Chezzi's model vs the reference) | **Go paired-programs differential** — new; restricted to programs with a *deterministic outcome* despite nondeterministic scheduling (sort output lines where only order varies). Go's channels/`select`/close/backpressure map ~1:1 to `Channel[T]`/`wait:`. Catches *shared* wrongness serial couldn't (identical bytecode both engines). Sweet spot only — Go has **no** equivalent for the airlock or structured-nursery semantics. |
| Scheduler races / lost-wakeups | **Seeded / deterministic-interleaving mode for M:N** (the `loom`/`shuttle` pattern): explore many schedules from a seed, replay on failure. This — not an external language — is the real replacement for serial's race-finding job; a reference language's scheduler is *also* nondeterministic, so diffing two nondeterministic schedulers catches nothing. Stronger than serial==M:N (one schedule pair). |
| Airlock / structured-concurrency semantics (no external equivalent) | Hand-written **known-answer** tests |

**Every oracle in that table compares RAW BYTES.** `String::from_utf8_lossy` is not injective (`ff`
and `fe` both become one U+FFFD), so a decoded compare reports agreement for a run whose two sides put
different bytes on fd 1 — and both Chezzi (`io.stdout().write_bytes`, byte-exact since W6-9) and the
reference languages (CPython `sys.stdout.buffer.write`, Go `os.Stdout.Write`, both measured `ff fe`)
can emit non-UTF-8. This has already been the live shape of three separate holes (W6-9, W6-9b, and the
CPython differential itself — `gaps.md` W7-30), so the Go paired-programs differential must be born
with it: capture `Vec<u8>`, keep the decode for *display and text heuristics only*, never for a verdict.

**Net:** CPython (sequential) + Go (channel/select semantics) + seeded-M:N (races) + known-answer (airlock)
together cover **more** than serial==M:N did, and none of them constrain the M:N engine's design. The
freeze is the natural cut point: post-JIT, serial byte-identity is impossible anyway.

**Migration mechanics (when it happens):** delete the `--serial`/`parallel=false` cooperative scheduler path
and the `run --check-parity` two-engine assertion; convert the `parity_entry`/`run_capture` parity tests to
single-engine (M:N) golden tests or move their intent into the layer above; drop the per-engine split
branches whose sole job was byte-identity (the `if self.mn.is_some()` forks in `op_wait_poll`, the serial
module-global snapshot in `join_nursery`, the serial Executor inline drain). The `--threads=1` M:N mode
(kernel-preempted, safe single-thread) already covers every legitimate user need `--serial` was ever
mistaken for.

---

## 2c. `Executor` moves to eager execution — ✅ **SHIPPED 2026-08-03**

`Executor.submit(f)` executes eagerly on the default M:N engine — the job starts immediately rather
than waiting for the drain — and `shutdown()` waits for in-flight work. This matches Python
`ThreadPoolExecutor` (`submit` schedules at once; `shutdown(wait=True)` blocks) and Java
`ExecutorService`. `--serial` keeps queue-at-`submit` / drain-at-`shutdown` (decision D3).

**The queue did not go away.** Python's `ThreadPoolExecutor` has a work queue too; the drift was never
queue-vs-no-queue but *who drains it and when* — continuously by pool workers, versus only at the reap
call. `src/vm/pool.rs` (a process-wide bounded pool with a FIFO queue and condvar-parked threads) was
already the right machinery and is what jobs are now dispatched onto.

### Settled decisions (project owner, 2026-08-01) — as shipped

- **D1 — Lifetime: detached, joined at program exit.** Shipped. A2 is reworded from "run the backlog
  nobody ran" to "wait for in-flight work".
- **D2 — Concurrency: shared process pool, no size parameter.** Shipped; `Executor()` stays zero-arg.
  **Accepted known limits:** executor jobs hold pool threads for their whole lifetime and can starve
  nurseries; the old parent-participation mitigation is gone with the batch join; and a job that calls
  `shutdown()` on an executor (its own, or another) blocks a pool thread while it waits — self-join
  hangs, exactly as Python's `shutdown(wait=True)` from inside a worker does. `Executor(max_workers)`
  stays an additive door if it bites.
- **D3 — `--serial` is unchanged.** Shipped. The divergence is observable only between `submit` and
  `shutdown()`; the test-shape rule is **assert after `shutdown()`, never between**.
- **D4 — `shutdown()` waits for queued and running.** Shipped. `shutdown_now()` drops work that has not
  started, trips the per-core cooperative cancel flag, **then waits** — the wait is a deliberate
  deviation from Java, whose `shutdownNow` returns the never-started tasks and expects a follow-up
  `awaitTermination` that Chezzi has no spelling for; without it a `shut` executor's still-running jobs
  would be skipped by the exit join and the program could exit mid-job.
- **D5 — The W7-5 fault contract is frozen.** Shipped unchanged: `shutdown()` hands its
  submission-ordered outcome slots to the same `reduce_task_slots`, so lowest-index-fault selection,
  hard-halt precedence and W7-5c's per-slot output flush are inherited rather than reimplemented.
  `tests/chz/stdlib/executor_drain_test.chz` passes untouched.

### What shipped, and the two corrections to the plan above

- **`W7-5b` is FIXED, not merely folded in.** The plan said eager execution would not dissolve it. That
  assumed the exit join stays list-based. It does not: the join now walks a heap-independent
  `ExecRegistry` (`Arc<Mutex<Vec<Arc<ExecutorCore>>>>`) that `spawn_worker` shares with every worker, so
  an executor created inside a task is visible no matter which heap made it. That needed **no** change
  to `swap_ctx`'s `ctx.heap`-only gate — the STOP condition the previous attempt halted at.
- **`exec_cores` / `exec_outstanding` never existed.** The rejected attempt's post-mortem blamed a
  predicate keyed on them; no such identifiers are in the repo — it *invented* that state. The real
  scheduler state is `MnSched::{runnable, inflight, blocked_native}` + `SchedCore::parked_n`, and the
  existing contract is that `Executor` work stays **outside** the detector (decision D,
  `src/vm/sched.rs`: "**No deadlock watch**"). **This milestone changed no deadlock predicate.**

**The real hazard, and what it actually was.** An eagerly dispatched job has no scheduler, so a blocking
op falls to the "no scheduler" arms of `chan_recv_step` / `send` / `wait:`, which declare a deadlock.
That verdict was TRUE while jobs ran only at the drain (the submitter was blocked inside `shutdown()`,
so nobody could send) and becomes a LIE once jobs start at `submit`. This is why the rejected attempt
regressed `ch.recv()`-in-a-job. Fixed by having such a job BLOCK (`Vm::block_recv`, a bounded
poll on the channel's own condvar, mirroring `demote_recv_block`'s settle order) rather than fault —
Python's behaviour. (Decision D — "a job blocked on a value that never arrives hangs" — was the
milestone's own answer and stood for one day; §2d **step 0**, landed 2026-08-04, replaced it with the
process-wide verdict, so such a job now faults once nothing in the run can move.)

**The second hazard, found by self-review and reproduced:** `submit` must NOT hold the executor's
`core.inner` lock while a dispatched job is running. The GC's `Obj::Executor` mark arm takes that same
lock, so a closure that captured its own executor (`ex.submit(fn(): … ex …)`) deadlocks the moment the
job's worker GCs — `std::sync::Mutex` is not reentrant. The first cut of this milestone held the lock
across the dispatch and was vulnerable; it now prepares the worker with no lock held and takes the lock
only for the allocation-free re-check-`shut`-and-reserve. Proved by restoring the bad order, which
hangs `eager_executor_self_capturing_closure_survives_gc_stress_parallel` (60s watchdog) — worth
knowing that the natural window is narrow, so this would have shipped as a rare mystery hang.

**Known limit disclosed by the work:** the top-level program itself is not an eager job, so a `recv` in
*main* on a channel only a job will fill still faults `deadlock` instead of waiting. The sanctioned
shape is unaffected (`submit_result` then `recv` after `shutdown()`), but a `Channel` handshake from
main into a running job is not expressible. Closing it would need main to know whether any eager job is
outstanding — not built on speculation.

**Shipped with:** `examples/executor.chz` and `examples/executor_autodrain.chz` rewritten (both had
"nothing has run yet" as their point) plus goldens; `tests/chz/spec/module_global_freshness_test.chz`'s
two drain-instant tests re-expressed; `run_workers_on_pool` + `TaskSlots` + `DoneSignal` deleted (the
`Executor` drain was their last caller); A2/C5 prose in `docs/concurrency.md` and the module-snapshot
instant at §"module globals" — the airlock crossing point moves from drain time to **submit** time.

**Reframing:** the `Executor` is a bounded-concurrency nursery with a detached lifetime — the model the
ancestors (Python/Java) already have.

---

## 2c1. `spawn` moves to eager execution — ✅ **SHIPPED 2026-08-14**

> **Numbering.** This is §2c's sequel and sits next to it deliberately. It is **not** `§2d`: that number
> is already the deadlock-detection milestone below and is cross-referenced from `src/vm/{sched,netio,mod,
> quiesce,tests}.rs`, `PROGRESS.md`, `docs/gaps.md`, `docs/stdlib.md` and `docs/concurrency.md`.
> Renumbering would break ~20 live pointers to buy nothing. `3a1` is the same house convention.

> **Why this was filed, and closed the same day (2026-08-14).** A spawned task did not start at its
> `spawn` — it started at its nursery's **join**. Go, which owns the concurrency seam, starts at the
> `go`. Nothing in the docs ever claimed the Chezzi behaviour: `docs/concurrency.md`'s nursery-scoping
> bullet defined the implicit nursery **only** by where a bare `spawn` *joins* and said nothing about
> when a task *starts*. So it was a divergence from the owning ancestor with no documented decision
> behind it — a defect, not a note. `docs/gaps.md`'s `bare spawn: start time` row (`:82`) is closed and
> points here.

**As shipped:** a `spawn` starts its task **at the `spawn`** on the M:N engine (the default `chezzi
run`); `--serial` keeps queue-at-`spawn` / run-at-join. The join keeps its own, orthogonal job — it
guarantees **completion** by the barrier, never that a task could not have started (and printed)
earlier.

### The defect, measured 2026-08-14 on the release binary — and the fix, measured the same way

```chezzi
print("A")
spawn:
    print("SPAWNED")
time.sleep_ms(300)
print("B")
```

| | output |
|---|---|
| Chezzi **pre-fix**, both engines | `A` `B` `SPAWNED` |
| Chezzi **post-fix**, `chezzi run` (M:N) | `A` `SPAWNED` `B` |
| Chezzi **post-fix**, `--serial` (documented split) | `A` `B` `SPAWNED` |
| Go (`go fmt.Println(...)`, go1.26.5) | `A` `SPAWNED` `B` |

The mid-flight case moved with it. `ch := Channel[int]()` / `spawn: ch.send(1)` / `print(ch.recv())`
**faulted** `recv on an empty channel: deadlock` pre-fix; it prints `1` now.

**It was not bare-`spawn`-only.** An explicit `parallel:` deferred identically — measured: a
`parallel:` whose body slept 300 ms printed `A B SPAWNED C`. The runtime told users so in two
fault-hint strings: `src/vm/netio.rs` `FULL_SEND_DEADLOCK` / `EMPTY_RECV_DEADLOCK` both read "*the
nursery body runs before its spawned tasks start*".

**The known user-visible harm — and the milestone's failing test.** A `spawn`-based TCP repro
self-deadlocked: `l.accept()` on the accepting frame waited for a dialer queued behind the end-of-main
join, so the accept could never be satisfied. It hung forever pre-fix; it completes now.

### Why it was structural, not a scheduling accident

The pre-fix diagnosis, read 2026-08-14. The line numbers are the ones the fix was written against.

- `Op::SpawnBlock` / `Op::SpawnCall` (`src/vm/exec.rs:2479-2481`) reach `Vm::register_task`
  (`src/vm/sched.rs:279-312`), which pushes a `QueuedTask` onto `self.nurseries` — a plain
  `Vec<Vec<QueuedTask>>` (`src/vm/mod.rs:767-772`). A `Vec` entry is inert; nothing polls it.
- **The eager/lazy choice is made ONCE, at `EnterNursery`** (`src/vm/exec.rs:2455-2470`):
  `let eager = self.parallel && self.mn.is_some() && worker_count() >= 2;` (`:2468`). A **top-level**
  nursery has `mn == None`, so it is *always* lazy — by construction, not by timing. The single
  immediate-start path in the whole VM is `sched.inject(fiber, 0)` (`src/vm/sched.rs:303`), reachable
  only from that eager arm.
- **No `MnSched`, no fiber and no worker thread exists for the task until the join.**
  `Vm::join_nursery` (`src/vm/sched.rs:367`) → `run_mn_nursery_outermost` (`:520`) builds the fibers,
  allocates the sched, seeds it, and only then farms workers. `main` becomes a worker at
  `shell.mn_worker_loop(&sched, 0, 0)` (`:593`, "owner of scope 0") — from the join onward, never before.
- **There is no yield point that could rescue it.** `time.sleep_ms` on `main` is `native::Kind::TimedWait`
  with `mn == None`, so it takes `Vm::block_until_deadline` (`src/vm/netio.rs:2206`) — it blocks the OS
  thread and never touches `self.nurseries`. Waiting longer cannot help; there is nothing to wake.

**Partial precedent for the fix already existed.** `Vm::early_enlist_outer` (`src/vm/sched.rs:654`) seeds
an *outer* nursery's pending tasks into a **live** sched when some nested nursery joins first — the same
"queued task, meet running scheduler" move this milestone needed at the top level.

### Semantics as shipped, and the engine split

**A `spawn` starts at the `spawn`, not at the join.** Go owns this; the join keeps its existing job —
it guarantees *completion* by the barrier, which is orthogonal to *start*.

**§2c's precedent applied directly, and settled the serial question.** `Executor.submit` already
shipped eager on M:N and lazy on `--serial` (decision D3), documented in `docs/stdlib.md` as the one
place the two engines deliberately disagree. The same reasoning holds here, only harder: `--serial` is
single-threaded and cooperative, so Go's semantics are **unreachable on it by construction**, and it is
slated for removal in §2b. Therefore:

- **eager on M:N** (the engine Chezzi actually ships),
- **documented lazy on `--serial`**,
- **the parity harness splits for the affected programs** rather than the M:N engine bending to what one
  cooperative thread can reproduce. That is §2b's own stated debt argument — byte-identity is already
  forcing per-engine forks whose only purpose is keeping serial matching M:N; this adds one more rather
  than paying for it in wrong M:N behaviour.

`docs/stdlib.md`'s "one place" is now **two**, reworded in the same commit; `docs/concurrency.md` §4
carries the user-facing statement.

**Collateral, decided here: the cancel report is deleted.** `"{n} pending task(s) cancelled on early
exit from parallel:"` is no longer printed when a `parallel:` body escapes early. A task starts at its
`spawn`, so there are no unstarted tasks to count, and any residual number would be racy; `trio` and
`asyncio.TaskGroup` print nothing in this situation either. Tasks are still cancelled — silently.

### Mechanism as shipped — **the plan's "three edits" were INCOMPLETE**

Items **1–3** are the edits the plan named, and they shipped as named. **The plan was wrong that they
were the whole job**: the deadlock verdict needed two additions of its own, **4** and **5**, and both
were mandatory — see the correction under them.

1. **The `EnterNursery` eager gate was widened** (`src/vm/exec.rs`) — from
   `self.parallel && self.mn.is_some() && worker_count() >= 2` to plain **`self.parallel`**. *Both*
   dropped clauses were re-derived rather than carried forward, as the plan demanded: `mn.is_some()`
   **was the defect itself** (a top-level nursery has no worker shell, so it was lazy by construction),
   and `worker_count() >= 2` guarded an eager *inner* join blocking its parent's *outer* worker — a
   premise that is gone now that the outer nursery is eager too and an eager nursery owns a **dedicated
   raw drainer thread**, not a bounded-pool slot. Keeping top-level and nested on the same path also
   matters: a lazy nested nursery under an eager outer one would route its join to
   `run_mn_nursery_outermost` and build a *second* outermost sched with `parent_wake: None` beside the
   live one — reintroducing the `gaps.md` B5 cross-sched wake bug.
2. **`Vm::activate_eager_nursery` (`src/vm/sched.rs`) gained a no-parent form.** `parent_wake: None` is
   correct at the top level (that sched *is* the outermost scheduler; there is nobody above it to wake),
   and `exec_registry` + `quiesce` stay wired. It returns `None` if the OS refuses the drainer thread,
   which falls back to the lazy queue-at-join path — a worker-less eager scope would hang a blocking
   body.
3. **`join_nursery` joins an already-running sched** instead of building one (`join_eager_nursery`),
   attaching the joining frame as the inline worker.
4. **`QuiesceState::eager_bodies` (`src/vm/quiesce.rs`)** — `verdict`'s `live` count gains **one per
   outermost eager nursery that still holds an undone task**
   (`live = 1 + outstanding_jobs + live_eager_bodies()`). Those fibers are *uncounted senders*: without
   this term, `spawn: ch.send(1)` beside a blocking `ch.recv()` on `main` **false-faults deadlock**. One
   per nursery, not per task — a single un-blocked sender is all it takes to veto, and `live` only has
   to exceed `parties.len()`. A joined nursery reports every scope complete and so contributes nothing,
   which is why no deregistration is needed.
5. **`JoinScope::body_blocked` + `SchedCore::any_body_injecting` (`src/vm/mod.rs`)** — the `body_open`
   veto ("the body may still inject work, so do not judge") no longer applies while the body's own
   thread is **blocked**. A blocked body cannot reach another `spawn`, so it is not live work. Without
   this, an eager top-level nursery — whose body spans essentially the whole program — vetoes forever,
   and both a genuine `main`-plus-sibling deadlock and a genuine *nested* deadlock **hang instead of
   faulting**.

   **A blocked body is not always a dead feeder, and the difference is the whole of wrong turn (d).**
   A body parked in a **nested nursery's join** WILL resume the instant that inner scope completes, and
   may then `send`/`close` to a parked sibling. That is precisely what the pre-existing
   `JoinScope::awaiting_builder` flag already means, so the nested-join guard raises it rather than
   inventing a second concept: `all_incomplete_awaiting_builder` then vetoes when the inner scope is
   DONE (the builder is about to resume and feed) and declines when the inner scope is itself
   incomplete-and-stuck (a genuine nested deadlock, which must fault). A body blocked on a **channel**
   leaves `awaiting_builder` false — it resumes only if somebody feeds it, so it promises nothing.

6. **Nesting shares ONE sched (`EagerScope::scope`, `MnSched::retire_last_scope`)** — this is the
   structural half, and the milestone does not work without it. A nested `parallel:` entered while an
   eager scope is already open on the same thread registers a new **scope** on that sched instead of
   building a private one, and retires it (pop the scope, truncate its slot tail) at its join so the
   enclosing scope is the last scope again and its later `inject`s stay contiguous. Legal because
   eager scopes on one thread nest strictly LIFO.

   The reason is wrong turn (c): **two sibling nurseries on two private scheds cannot wake each other.**
   `send_wake` scans its own sched and then `wake_parent_chain`, which is strictly *upward* — there is
   no sideways or downward path. One sched with one scope per nursery is the cross-nursery flat
   scheduler that already existed for the lazy path; eager start simply has to keep using it.

   Two consequences that had to move with it: `poller::drain_scope` (a nested escape may only unpark
   ITS scope's socket-parked fibers — selecting by sched was scope-selective for free only while every
   nursery owned one), and the `worker_count() >= 2` clause of the `EnterNursery` gate, which is
   RESTORED for `mn.is_some()` — see item 8.

7. **`SchedCore::body_waits` (`src/vm/mod.rs`)** — the blocked body's own wait, published on every
   eager sched of its thread, and vetoing `is_deadlocked_ignoring_jobs` while it is satisfiable.

   This is what makes item 5's relaxation SOUND, and without it §2c1 shipped a **false deadlock on a
   healthy program** — the one unacceptable direction. A `parallel:` body blocked on a channel is very
   often the RENDEZVOUS PARTNER of one of that sched's own parked fibers, and nothing in the
   counter-only predicate could see it:

   ```chezzi
   ch := Channel[int](1)
   parallel:
       spawn:
           ch.send(0)
           ch.send(1)
       print(ch.recv())
       print(ch.recv())
   ```
   Go prints `0 1`. This printed `0` and then `recv on an empty channel: deadlock`, **4 runs in 8**.

   Two details are load-bearing, each measured:
   - **It carries the WAIT, not the channel.** Reusing the pre-existing `demoted_chans` peek — which
     asks `!q.is_empty()`, the RECEIVER's question — is inverted for a body blocked on a full `send`:
     an empty queue means it can proceed. That killed a live consumer after one `recv` and faulted
     `send on a full channel`, **12 runs in 12**. `PartyWait::satisfiable` already answers both
     directions, so it is what is stored, and it is the SAME `Arc` the quiesce party holds so the two
     can never disagree.
   - **Publishing it and raising `body_blocked` happen under ONE `SchedCore` acquisition**
     (`MnSched::set_body_wait`). Two acquisitions leave a window where the veto is down and the wait
     is invisible; an idle worker sampling there reaps a satisfiable parked fiber, **5 runs in 6**.

8. **The `EnterNursery` gate keeps `worker_count() >= 2` for `mn.is_some()`** — a nursery entered
   inside a spawned task, the only shape that still builds a private sched with its own dedicated raw
   drainer thread, and so the only per-nursery THREAD source left. Dropping it broke `pool.rs`'s
   documented bound that live threads stay at `N + joiners` "*regardless of `parallel:` nesting
   depth*": measured, nesting depth 7 with 128 leaves at `--threads=1` went **3 threads → 130**, which
   also silently broke the user-facing flag. A top-level nursery has no outer worker to starve and
   creates exactly one drainer per thread, so it stays unconditional.

9. **`Op::EnterNursery` is `#[inline(never)]`** (`Vm::op_enter_nursery`). The arm grew from three
   lines to a page, and `run_until`'s hot loop pays for every arm's code size whether it is reached or
   not: `benches/loop.chz` executes no nursery opcode at all and still measured **+3.1%** inline
   (1437 → 1481 ms, medians of 80, minima non-overlapping) — the shape `W7-57` measured at +4.5% on
   the same bench. Outlined: **+0.9%**, inside the file's ±1.5% band.

10. **The three channel-deadlock hints are restored, scoped to `--serial`.** They read "*the nursery
    body runs before its spawned tasks start*" unqualified, which is false on M:N — but still TRUE on
    the cooperative engine, where they are also the only guidance a user gets. Deleting them outright
    regressed `--serial` diagnostics; naming the engine keeps one const, keeps the engines
    byte-identical, and keeps every word true on both.

**Correction to the plan's "do not touch a deadlock predicate".** The instinct was half right and half
wrong, and the wrong half cost three of the four measured wrong turns below. Right: no predicate was **replaced**, and none
should have been — the process-wide quiescence verdict (§2d step 0) is still the thing that keeps an
eagerly-started task from turning a fault into a hang. Wrong: the plan asserted that needing to touch it
at all signals a bad mechanism. It does not. Eager start **breaks the invariant the `live` count rested
on** — "a nursery fiber never coexists with a counted party" — so the accounting had to be **extended**
in the two places above. Extending a detector's accounting to cover a party class that did not exist
before is not the same act as inventing a new predicate.

**Five wrong turns, all measured before being abandoned. Four of the five were in the deadlock
verdict, which is where this milestone's real difficulty lived — not in starting the task early:**

- **(a) Registering the eager nursery as a `PartyWait` instead of counting it into `live`.** A party is
  a **blocked thread**. Registering a non-thread inflates `parties.len()` toward `live`, and the verdict
  then false-faulted `spawn: print(…)` beside `time.sleep_ms(300)` — a program with **no channel in
  it**. The `live` side is the correct side of the comparison for "an uncounted sender exists".
- **(b) That same party's `satisfiable()` read an all-scopes-DONE nursery as "can still move".** A
  `main` left blocked after its sibling had been reaped therefore hung forever, observed as **774
  verdict evaluations** while `main` waited. A satisfiability answer built from a scope set that is
  already complete is not a "maybe"; it is a "no".
- **(c) Giving every nursery its own private sched.** The natural reading of "make the top level eager
  too" is "run `activate_eager_nursery` there as well" — and that builds a *sibling* sched beside the
  enclosing one. Sibling scheds are mutually invisible (`send_wake` → own sched → `wake_parent_chain`,
  strictly upward), so a task in one nursery could not wake a receiver parked in another, and
  `examples/parallel_cross_nursery_{circular,fanout}.chz` — the goldens that exist for exactly this —
  **false-faulted** `deadlock: every task in this parallel: block is blocked…`. Fixed by making a
  nested nursery a SCOPE on the enclosing sched (mechanism 6). **Lesson: eager start is not a scheduling
  tweak, it makes every nursery a live scheduler, so the flat-scheduler invariant "one sched per thread"
  becomes load-bearing where it used to be an optimisation.**
- **(d) Treating "the body is blocked" as "the body cannot feed".** With nesting fixed, relaxing the
  `body_open` veto for a body parked in a nested join looked right — it genuinely cannot `spawn`. But it
  can still `send`, the moment the inner scope finishes. Three more healthy programs false-faulted
  (`golden_parallel_cross_nursery_inline_send`, `..._inline_close`,
  `parallel_cross_nursery_late_spawn_parked_matches_coop`). The fix was not a new flag but the existing
  `awaiting_builder`, which encodes exactly "a builder will return to this scope and feed it".
- **(e) The same mistake once more, for a body blocked on a CHANNEL — and this one shipped past a
  green suite.** A body parked on `ch.recv()` cannot inject, but it is the rendezvous partner of the
  sibling parked on `ch.send()`. See mechanism 7 for the two measured failures and their fix. **It was
  found by adversarial review, not by the 4 123-test suite, and not by any of the ten repro programs**
  — and the in-process harness could not have found it either: `run_capture_parallel` uses the
  BUFFERED stdout sink, under which both broken shapes pass 12 runs in 12. The regression test
  therefore lives in `tests/spawn_eager_rendezvous.rs`, driving the real binary, and states plainly in
  its own header that at debug speed it is a smoke check rather than a strong guard.
- **The counting error that sat underneath (b) and (c).** At one point `live_eager_bodies` counted
  merely-*incomplete* nurseries instead of *movable* ones, over-counting `live` forever and hanging
  three genuinely-stuck programs (`a_nursery_judge_re_asks_the_verdict_when_a_party_registers_later`,
  `a_stuck_executor_job_beside_a_stuck_nursery_faults_instead_of_hanging`,
  `two_stuck_nurseries_with_no_polling_party_still_fault` — all `Timeout`).

**None of (c), (d) or the counting error was caught by a repro.** All four hand-written repro programs
(start time, the channel idiom, the TCP accept, the genuine deadlock) were green through every one of
them. `cargo test --lib` found all of them, which is the case FOR the 4 100-test suite: a concurrency
change's blast radius is not reachable from the programs you thought to write.

### Test churn — the prediction was **largely wrong**, and this is why

**The forecast below was written pre-implementation and is kept as the record.** What actually
happened:

- **The implicit-nursery order pins SURVIVED.** Every one of them. The Rust harness prints into the
  **buffered** sink — a task's stdout is a private per-fiber buffer flushed at the join, in task order —
  so an earlier *start* does not move a single line of expected output. The forecast read the pins as
  order-of-execution assertions; they are order-of-flush assertions.
- **`examples/parallel.expected` is UNCHANGED**, for the same reason, despite being called "the sharpest
  artifact in the repo for this milestone".
- **Only the cancel-report group actually broke** — the one group the forecast correctly flagged as a
  design question rather than churn. It was answered by **deleting the report** (see "Collateral"
  above), which is what those tests now assert.

The lesson generalises: the buffered sink is the reason a start-time change is *cheap* in this test
suite, and it is the seam any future scheduling change should check first.

---

*The pre-implementation forecast, retained:*

**`src/vm/tests.rs` — implicit-nursery order pins** (all confirmed on the `fn` line):
`:161` `implicit_nursery_basic_vm` (`"a\nb\nw\n"` — the sharpest of the group) · `:170`
`implicit_nursery_return_joins_vm` · `:181` `implicit_nursery_toplevel_vm` · `:201`
`implicit_nursery_defer_orders_tasks_then_defers` · `:423` `implicit_nursery_try_joins_before_propagating`
· `:431` `implicit_nursery_respects_function_boundary` · `:438` `implicit_nursery_nested_functions` ·
`:447` `implicit_nursery_try_preserves_error_value` · `:456` `implicit_nursery_spawn_in_defer_block`.

**`src/vm/tests.rs` — cancel-report pins. These REQUIRE the task to be unstarted** (they assert
`N pending task(s) cancelled on early exit from parallel:` and, in several, a `print("should not run")`
that must not appear). This group is the hard design question of the milestone, not mechanical churn —
an eagerly-started task cannot be "cancelled before it ran", so decide what the report *means* under
eager start before editing any of them:
`:464` `implicit_nursery_fault_cancels_pending_tasks` · `:502` `uncaught_fault_reports_implicit_nursery` ·
`:513` `uncaught_fault_reports_explicit_parallel` · `:525`
`uncaught_fault_reports_each_nursery_separately` · `:535`
`uncaught_toplevel_fault_does_not_report_module_nursery` · `:544`
`recover_caught_fault_reports_each_nursery_separately` · `:555`
`uncaught_fault_reports_before_frame_defers` · `:565` `recover_caught_fault_reports_before_frame_defers` ·
`:577` `uncaught_fault_interleaves_report_and_defer_per_frame`.

**`src/vm/parity_tests.rs` — CORRECTION: audit, but they probably survive.** The surveyed line numbers
`:11578`, `:11602`, `:11616` are mid-body lines, not test starts; the enclosing tests are
`serial_module_global_direct_mutation_forms_isolate_parity` (`:11537`) and
`channel_park_keeps_module_snapshot_parity` (`:11605`). Both are **module-global isolation** assertions,
and in both the parent's `print` sits *after* the nursery's dedent — so an earlier start does not move
their output. Re-run them; expect green, and do not pre-emptively rewrite them.

**Goldens** (`examples/`):

- **`parallel.chz` + `.expected` — the sharpest artifact in the repo for this milestone.** The golden
  pins `queued, not yet run` **before** `second worker 10 ran`, and the source comment states the
  behaviour as intent: "*Tasks run at the dedent, so statements that lexically follow a spawn inside the
  block run before the spawned bodies (the deterministic sequential approximation)*". Both the program
  and its prose are being rewritten, not just its expected output.
- **`implicit_nursery.chz` + `.expected` — read its header comment before assuming churn.** It already
  documents the correct contract: the exact line order holds for the **buffered test sink** (every lib
  helper, both engines) and `--serial`; `chezzi run` **streams**, and "*a join point guarantees
  COMPLETION by the barrier, never that the task could not have started (and printed) earlier*". This is
  the seam the whole milestone should lean on — the buffered harness is where the real pins live.
- **`parallel_cross_nursery_late_spawn` — CORRECTION: 2 variants, 4 files**, not 5: `.chz`/`.expected`
  plus `_parked.chz`/`_parked.expected`.
- **`airlock_cycle.chz` + `.expected`** — its `spawn use_it(a)` / `spawn use_wide(xs)` sections are inside
  `recover:` blocks whose `match` prints follow the dedent; verify rather than assume they move.

**`tests/chz` — CORRECTION: the gate was never the blocker.** The forecast called
`chz_suite_passes_both_engines` (`src/test_runner.rs:1032`) "the first thing to design, ahead of the VM
edits". It was **over-stated**: that gate compares per-test **verdicts**, never output, and no suite file
asserts inter-task print order. All 13 `spawn`-using suite files pass on both engines unchanged, and the
engine split needed no per-engine story in the native suite at all. (The files, for reference:
`spec/{static_witness,airlock_shared_binding,cancel_defer_recover,airlock_native,eq_func,module_global_freshness,opt_carrier,struct_name_resolution}_test.chz`,
`stdlib/{fs_bytes_roundtrip,sleep_cancel,process}_test.chz`,
`suites/{concurrent_collection,rwshared_readview}_test.chz`.)

### Docs that moved WITH the change

All of these shipped in the same commit:

- **`docs/concurrency.md`** — the nursery-scoping bullet gained *when a task starts* and names the
  `--serial` exception.
- **`docs/concurrency.md` §4** — the C1–C4-era "Staged executor" / "Documented consequence (sequential
  only)" passage was **rewritten, not patched**. It claimed tasks "*run to completion in FIFO order*" at
  the barrier and called the behaviour "*the deterministic sequential approximation of concurrency*"
  with real interleaving deferred to **C5** — written before C5 shipped and became the default engine,
  and never revisited. It now states eager start on M:N, lazy on `--serial`, the buffered-sink vs
  streaming-CLI ordering contract, and the join as a completion barrier.
- **`docs/syntax.md`** — `print("dispatched")  # runs before the tasks` and the `parallel:` bullet's
  "tasks run at the barrier".
- **`docs/stdlib.md`** — "the one place the two engines deliberately disagree" is now two places.
- **`docs/gaps.md`** — the `bare spawn: start time` row is struck FIXED, and the superseded observation
  bullet is marked as such.
- **`src/vm/netio.rs`** — the `FULL_SEND_DEADLOCK` / `EMPTY_RECV_DEADLOCK` hints no longer advise around
  a cause that no longer exists.
- **`src/vm/op.rs`** — the `EnterNursery` / `JoinNursery` doc comments and their section header, which
  read `// ----- concurrency (C4: sequential, run-to-completion executor) -----`.

---

## 2d. Deadlock detection: from quiescence counting to a wait-for graph (planned — NOT started)

> **Why this is filed (2026-08-03).** Eager `Executor` execution (§2c) exposed that Chezzi has *two*
> unrelated things called "deadlock", and the weak one does most of the work. Read this before touching
> either.

### What exists today

| | `MnSched::is_deadlocked` (`src/vm/mod.rs`) | the fault arms in `src/vm/netio.rs` |
|---|---|---|
| decides by | live state: `running`/`runnable`/`inflight`/`blocked_native`/`parked_n`, plus veto terms | **nothing** — an unconditional `else` |
| the question it asks | "can anything in this nursery still move?" | "am I inside a scheduler? no → therefore nobody can ever send" |
| scope | one nursery | any blocked party with no scheduler: top-level `main`, an `Executor` job, a native callback |
| catches | total quiescence of a nursery | nothing; it *asserts* |

The second one is a **proxy that no longer tracks its own premise**. "I have no scheduler" meant "no
sender can exist" only while every concurrent construct was scheduler-backed. Eager execution put
running jobs outside any scheduler, and both of §2c's bugs are that one stale assumption, mirrored:

* a job blocking on a value `main` sends next line → arm faulted, wrongly (fixed in §2c by blocking);
* a job blocking on a value `main` can never send, because `main` is inside `shutdown()` waiting for
  that job → arm then blocked, wrongly (a program that faulted in 0s on both engines pre-§2c hung
  forever on M:N after it).

Neither is fixable *in that arm*, because the arm has no way to tell the two apart. Both need a real
answer to **"can anyone still send on this channel?"**

The second one shipped an INTERIM answer for one narrow shape (gaps.md `W7-12`): a job faults if its own
executor is being joined by an explicit `shutdown()` and every job that executor still owes is parked.
That was a local predicate over one executor — it observed neither `parallel:` tasks, other executors,
nor `main`, and gaps.md row `W7-12r` listed the programs it therefore still got wrong.

**Step 0 below landed 2026-08-04 and DELETED it**, replacing it with the process-wide count described
there (`src/vm/quiesce.rs`). Both arms above now block and let that verdict decide, so the stale proxy
is gone from the `recv`/`send`/`wait:` paths for every party that owns its own OS thread; only the
native-callback arm, which genuinely cannot block, still faults on the spot.

### The proposal (project owner, 2026-08-03): a wait-for graph

Give every blocked party an outgoing "waits-for" edge and look for a cycle. This is the classic
wait-for-graph (WFG) deadlock detection from OS/DBMS lock managers, and it is the right direction. One
adaptation is essential, and it is the whole design difficulty:

**A channel is not an owned resource.** With a mutex the edge is exact — `A → B` because B *holds* the
lock. A blocked `recv` does not wait for a specific fiber; it waits for **whoever sends next**, which is
any fiber that can reach that channel. So the edge is `A → {set of possible senders}`, and the graph is
an **AND-OR graph**, where deadlock is a **knot** (a set S from which every outgoing option leads back
into S), not a simple cycle. Knot detection is still polynomial; the point is that "find a cycle" will
silently give the wrong answer here.

**Node set — must include non-fibers, or it misses the reported bug.** Nodes are not just fibers:
* a fiber parked on `recv`/`send`/`wait:`,
* a worker DEMOTED in place (`demote_recv_block`) — blocked but not parked,
* **a joiner**: the thread inside `Vm::join_eager_jobs` or a nursery join. `main` blocked in
  `shutdown()` is exactly the node whose absence makes today's arms unable to see the bug.

**Edge kinds:**
| blocked on | edges to |
|---|---|
| empty `recv` on `ch` | every party that could `send` on `ch` |
| full `send` on `ch` | every party that could `recv` on `ch` |
| `wait:` over N arms | OR-edges, one per arm (ready on ANY arm ⇒ progress) |
| a join (`shutdown()`, nursery barrier) | every outstanding job/task it waits for |

**Approximate the sender set in the SAFE direction.** Computing "who could send on `ch`" exactly is
undecidable; approximate it by reachability (who holds a handle). Over-approximating ADDS edges, which
means fewer knots, which means the detector **under-reports** — it misses deadlocks rather than
inventing them. That is the correct failure direction and must be stated in the code, because the
opposite instinct is what sank the first eager-execution attempt (`§2c`): it invented a veto keyed on
`exec_outstanding > exec_cores.len()`, where `exec_cores` counted ancestor *memberships* rather than
live jobs, and six of ten upheld review charges traced to that one predicate.

A cheap sound special case worth landing FIRST, no graph required: if a blocked receiver holds the only
live handle to the channel's core (`Arc::strong_count` on the `ChannelCore`), no other party can ever
send — provable deadlock, O(1), zero false positives. It covers a large share of real mistakes and is a
useful stepping stone that the graph later subsumes.

**Keep every veto `is_deadlocked` already earned.** They encode real races that cost real bugs, and a
new detector that drops them re-opens them: pending IO/timers/blocking-pool work (`inflight`), a cancel
in flight that would wake a demoted fiber, a scope mid-teardown, and a value racing into a queue the
predicate is about to read. Each is an edge to the outside world — i.e. an escape from the knot.

**Cost control.** Run the analysis only when quiescence is *suspected* (a blocked count changed and
nothing is runnable), never per poll tick; O(V+E) every `DEMOTE_POLL_BACKOFF` would be a tax on every
blocking program.

### How the ancestors do it — and why Chezzi can do better

* **Go** — the only mainstream runtime that aborts: `fatal error: all goroutines are asleep -
  deadlock!`. It builds no graph. It counts: nothing runnable and nothing in a syscall/timer/netpoll ⇒
  everything is stuck. Sound and O(1), but it only ever reports **total** quiescence, and a goroutine in
  a syscall, a network read, or `time.Sleep` suppresses it entirely. Unrecoverable (fatal, not a panic).
* **Python** — no detector. `q.get()` blocks forever; a `ThreadPoolExecutor` worker waiting on something
  `main` never sends simply hangs. The answer is user-side timeouts (`q.get(timeout=…)`).
* **Java** — no auto-abort. Detection is *tooling*: `ThreadMXBean.findDeadlockedThreads()`, jstack,
  JConsole. The runtime answer is `awaitTermination(timeout)`.
* **Rust** — none in std; `parking_lot` has an opt-in `deadlock_detection` feature for lock cycles.
* **Erlang/BEAM** — none; `receive … after Timeout` bakes the timeout into the syntax.

Chezzi's `is_deadlocked` is already Go's rule, scoped per nursery. **The WFG's payoff over Go is
PARTIAL deadlock** — a subset stuck while the rest of the program runs happily, which Go structurally
cannot report. Both bugs found in §2c are partial deadlocks, so this is not a theoretical gain.

### Ordering — step 0 SHIPPED 2026-08-04; steps 1–4 remain

The original plan opened with the `Arc::strong_count` rule and treated the graph as the payoff. Working
W7-12 to a conclusion changed the ranking, for one reason: **every program that hung was TOTAL
quiescence, which is the cheap case, not the hard one.** `main` parked inside `shutdown()`, both jobs
parked, nothing runnable, nothing in flight — that is precisely Go's rule, and Go answers it by
COUNTING, with no graph at all. W7-12's local predicate went wrong three times (`W7-12r`) because it
asked "is this executor stuck?" — a question no per-executor counter can answer — when the answerable
question was the process-wide one all along.

0. **Process-wide quiescence — DONE 2026-08-04 (`src/vm/quiesce.rs`), and it closed `W7-12r`.**
   Landed NOT by lifting `MnSched::is_deadlocked` (which stays per-nursery, with every veto it earned
   intact) but as a second, independent layer over the parties that scheduler never accounted:
   `main` and each eager `Executor` job. `live = 1 + Σ ExecutorCore::outstanding`; a party registers a
   `PartyWait` while blocked; the verdict is "every counted party registered AND none satisfiable".
   It DELETED W7-12's interim predicate whole — `eager_join_deadlocked`, `join_has_no_live_siblings`,
   `ExecutorCore::joining`/`blocked`, the `eager_block_suspect` debounce and the registry sweep. It
   added the missing **joiner** node (`Vm::join_eager_jobs` registers `PartyWait::Join`), which is what
   makes `main`-in-`shutdown()` visible, and it also fixed a wrong answer the ledger had not recorded:
   `main` blocking on a channel an eager job was about to fill used to FAULT where Go and CPython both
   print the value.

   Three things are worth carrying forward to steps 1–4, each learned the hard way here:
   * **The verdict must be ONE observation.** A first cut snapshotted the party list, released the
     lock, then read the channels — and reported a producer and a consumer parked on empty channels
     that were never empty at the same instant. Holding the party lock across the channel reads fixed
     it. A wait-for graph is a bigger version of exactly this hazard.
   * **A party must not be registered across its own attempt.** `pop()` and un-registering are not one
     atomic step, so a party still registered while it consumes a value reads as parked at the instant
     it made progress. Registration is scoped to the wait, never to the retry.
   * **Satisfiability is what replaced the debounce.** "Is this wait already over?" is a direct
     question with a direct answer; "has nothing moved recently?" is a guess, and it is the guess that
     faulted a healthy cap-1 pipeline 6/40 runs.
1. `Arc::strong_count` sole-handle rule (sound, O(1), no graph) — still worth landing, but it is
   narrower than it looks: it fires only when the blocked receiver holds the ONLY handle, so it misses
   the common case where the channel is a module global `main` also holds.
2. Unify the blocked-party registry with the SCHEDULER's parties — nursery fibers and demoted workers,
   which step 0 deliberately left out (a live nursery is currently covered only indirectly, by its
   owner being an unregistered live party). Overlaps `docs/cross-nursery-flat-scheduler.md`.
3. AND-OR knot detection over that registry, keeping every existing veto, run only on suspicion. **This
   buys PARTIAL deadlock only** — a subset stuck while the rest of the program runs on, which Go
   structurally cannot report, and which step 0 also cannot. Real, but extra credit on top of step 0.
4. Retire the remaining `netio.rs` "no scheduler ⇒ no sender" arm. Step 0 already retired it for every
   party that owns an OS thread; what is left is the native-callback case, which faults because it
   genuinely cannot block, not because of the stale premise.

**THE RISK, stated first because it is the one that has already bitten three times.** The vetoes are the
whole correctness surface. A job sleeping on a `timer`, blocked on a socket, waiting on netpoll or
blocking-pool work, or racing a value into a queue is NOT deadlocked, and counting it as such is a false
alarm on a working program — the exact failure W7-12 shipped three times (see `gaps.md` W7-12 and the
memory `parked-is-not-stuck`). So: write the Go/CPython comparison programs and the LOOPING regression
tests BEFORE the detector, keep every veto `is_deadlocked` already earned, and put the whole thing
through `adversarial-review` — a full green gate had no opinion on any of the three false positives.
Step 0 followed that order and it paid: the Go/CPython table came first, and the two false positives it
did hit were caught by an existing looping fence (a 300-handoff pipeline), not by reasoning.

**Sequencing note — RELAXED 2026-08-04, and step 0 confirmed it was right.** This previously said "do
§2b (remove `--serial`) first, because a detector that must stay byte-identical across two engines is
much harder". That constraint is gone: correctness now outranks engine agreement (project `CLAUDE.md`),
and `--serial` is scheduled for deletion regardless, so **build the detector M:N-only and let the
serial engine keep its crude arms until it is removed.** In the event the shared code degenerated
correctly on `--serial` anyway (`live` is 1 there, since that engine queues at `submit` and has no
eager jobs, so a top-level block faults on its first halt check exactly as it always did) — the only
divergence step 0 introduced is that M:N now RUNS the programs `--serial` still faults on, which is the
direction that matters. Its acceptance tests are M:N-only and say so.

---

## 3. Missing features (ranked by leverage for scripting) → **mostly shipped (M12–M18)**

> Comprehensions, slicing, the iterator protocol, concat/merge, hex/bin/oct literals, optional
> chaining, tuple-destructuring `for`, match guards, `std.os.exit`, and runtime stack traces have all
> landed. Mutable closure capture **landed as uniform by-reference capture** (2026-07-09, reversing the
> earlier snapshot-by-value decision). Nothing in this list is still open — see
> [`PROGRESS.md`](../PROGRESS.md).

1. **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set). A Python-feel language without
   these feels broken. Pure parse-time desugar to loop + push. Cheap, large UX win.
2. **Slicing — DONE, and since upgraded to Python colon syntax.** Originally shipped as Rust-style
   `xs[1..3]` (half-open, bounds-clamped, reusing `..`). Mid-M19 (owner-requested language change) the
   subscript-slice form moved to **Python `xs[a:b:c]`** with the full surface: open bounds, step, reverse
   `[::-1]`, and negative indexing (plain index faults out of range, slice bounds clamp — Python's
   asymmetry). `ExprKind::Slice { obj, start, end, step }` (each `Option`); one shared resolver in
   `src/slice.rs` drives both engines. The `..` operator stays the for-loop/match range. See
   [`PROGRESS.md`](../PROGRESS.md) "Slice syntax → Python colon".
3. ~~**Iterator protocol + generators (`yield`)**~~ — **iterator DONE; generators removed.** The
   `Iterator[T]` parameterized protocol shipped (M13): user structs usable in `for`, generic
   `[S: Iterator[T], T]` bounds, and lazy `map`/`filter`/`take` written as **adapter structs** over it
   (Rust `std::iter` model — `examples/iter_adapters.chz`). **`yield`/generators have since landed as
   a complete, VM-only feature** (was a permanent non-goal): any `fn` that uses `yield` is a generator
   (the `-> Iterator[T]` annotation is optional — the element type is inferred from the first `yield`);
   the call returns a suspendable generator (one-shot cooperative coroutine, own private
   frame/stack swapped into the VM, resumed by an intrinsic `.next()`). Generators run on **both** VM
   engines (serial `--serial` and default M:N). A **live** generator held in a frame **local** is now
   sendable across a task airlock **BY VALUE** (F3 path C — deep-copied + rebuilt on the receiver, parked
   slots checked sendable at serialize time; mid-`recover:` / pending-`defer` / multi-frame suspensions
   reject cleanly). A **module-global** generator is not serialized by value — it stays reach-gated + a
   poison snapshot on M:N. The adapter-struct pattern stays the default for lazy streaming.
4. ~~**List concat + map merge**~~ — **DONE.** Method-based: list `.concat`/`.extend`, map
   `.merge`/`.update` (concat/merge new, extend/update mutate). No new syntax; spread/unpack stays
   dropped. `examples/concat_merge.chz`.
5. ~~**Hex / binary / octal literals**~~ — **DONE.** `0xFF`/`0b1010`/`0o17`, lexer-only via
   `i64::from_str_radix`, `_` between digits. `examples/hex.chz`.
6. ~~**Optional chaining + null-coalescing**~~ — **DONE.** `x?.field`/`x?.method()` + right-assoc
   `a ?? b`. Originally lowered to a `match` by the desugar pass (zero checker/engine code); **W7-43
   (2026-08-11) moved that decision to the checker** — the carriers now survive desugar, the checker
   picks the lowering by operand type (`Option` → `match`; `Result` → `?` then `.`, identical to the
   spaced `x? .f`) and records it in a `CarrierTable` the compiler reads. `??` stays Option-only.
   Still zero VM code: the `Result` path emits the `Op::Try` the spaced form already emitted.
   `examples/optchain.chz`.
7. ~~**Tuple-destructuring `for` (+ `enumerate` / `zip`)**~~ — **DONE.** `for a, b in List[(A,B)]`
   (N-var over `List[tupleN]`); VM splits map vs list-of-tuples at runtime on a new `Op::IsMap`.
   `enumerate`/`zip` shipped as pure-Chezzi `std/iter.chz`. `examples/for_tuple.chz`.
8. ~~**Mutable closure capture**~~ — **DONE (reversed 2026-07-09):** capture is now **uniformly by
   reference** (a closure shares & can edit the closest binding of a captured name), superseding the
   earlier snapshot-by-value decision. A counter / accumulator is now a raw captured local, mutated
   through a `defer:` block or a method call (closures are expression-only — `fn(): n = n + 1` is a
   parse error, so the write goes in a statement position). Cross-task mutation still requires
   `Shared[T]` et al. (a plain capture into a
   `spawn` is an isolated per-task copy — the one deliberate divergence from Go). See
   [`PROGRESS.md`](../PROGRESS.md) "Uniform by-reference capture".
9. **Match guards + range patterns** — `n if n>0:`, `1..10:`. Roadmap. Guards subsume the rest.
10. ~~**`std.os.exit(code)` + real exit codes**~~ — **DONE.** `std.os.exit(code)` is a hard, uncatchable
    halt (unwinds past `recover:`, bypasses `defer`), with the code threaded through both run drivers +
    the CLI; exit-wins precedence holds under `--parallel`. The status is the POSIX **low 8 bits** of
    the code (`-1` → 255, `300` → 44). `examples/exit.chz`.
11. ~~**Runtime stack traces**~~ — **DONE.** Error + call chain + line numbers, both engines
    (`37f374a`).

12. **`ref T` / `Ref[T]` — transparent reference bindings — ⛔ REMOVED (2026-07-19).**
    `ref T` (a binding modifier lowering to a `Ref[T]` box) and the reserved `Ref[T]` box were
    **removed entirely** on minimalism/coherence grounds: they added only **scalar** aliasing, a
    pointer-graft on Chezzi's Python object model (structs/`List`/`Map`/`Set` already share by
    reference on assignment; scalars copy). Nothing real depended on it — zero stdlib `.chz` imported
    `std.ref`. For an in-task mutable value to close over or pass by reference, use a plain one-field
    `struct` (a struct is a shared reference); for cross-task mutation use `Shared[T]`. The `ref`
    keyword, the `Ref[T]` reserved global, and `std.ref` no longer exist.

13. **Static / associated protocol requirements (typeclass-style `T.default()`) — ✅ SHIPPED as M24
    (2026-08-10), by witness passing.** A protocol may declare a *static* (no-`self`) requirement, and a
    generic bounded by it **constructs** through the type param — the one thing instance-only protocols
    can't express:
    ```chezzi
    protocol Default:
        fn default() -> Self
    struct Counter:
        n: int
        fn default() -> Counter: return Counter(7)
    fn reset[T: Default](old: T) -> T:
        return T.default()
    print(reset(Counter(1)).n)      # 7
    ```
    **Mechanism — §3a1's ruling, built.** A type param whose bounds carry a static requirement AND whose
    body needs it gets a **hidden trailing parameter** holding the concrete type's runtime identity key;
    `T.method(...)` lowers to `Op::CallStaticDyn`, which pops that key and runs the same
    `Vm::do_static_call` as an ordinary `Type.method(...)`. Generics stay **erased** — one body per
    generic fn, nothing monomorphized, no type argument reaches the VM. The witness is charged only to a
    body that uses one, so a generic that merely *has* such a bound keeps every position it had before.
    All six axes §3a1 named as the ones that broke the two earlier runs are covered: cross-module (every
    import spelling), `spawn:`/`parallel:` bodies, `defer:`, closures (including escaping ones) and
    nested `fn`s, inferred/turbofish/annotation-pinned `T`, non-leading bound params, and transitive +
    recursive + mutual **forwarding**. Axis 3 (first-class value / `defer f(...)`) is the one that did
    NOT close — it is a permanent wall, recorded in "What stays impossible" below.
    **The factory closure is still the right answer when the wall bites** — it needs no static
    requirement at all and works in value position:
    ```chezzi
    fn make[T](mk: fn() -> T) -> T: return mk()
    make(fn(): Counter(0))
    ```
    Full surface + the decline list: `docs/syntax.md §7a`. Running proof:
    `tests/chz/spec/static_witness_test.chz`, `examples/static_witness.chz`. The two rejected 2026-06-24
    attempts (`auto-task/protocol-static-req`, `…-v2`) are superseded and discardable.

14. **`cast[T](val: Any) -> Option[T]` — ⛔ NO CONSUMER (2026-08-09).** The owner ruled against
    leaning on `Any` ("we vouch for statically typed; `Any` is Go's `interface{}`"), and `cast` only pays
    off through `Any` — so this is closed unless that reverses. Its erasure analysis stays valuable and
    is summarised in §3a1 "What stays impossible". Original entry: **a checked downcast off the `Any`
    top type — ⏸️ DEFERRED
    (DESIGN ONLY, no code).** The `Any` top type + variadics shipped (see `docs/syntax.md`); `Any`
    lets a value of any type into a universal slot, but there is currently **no way back out** — you can
    hold and display an `Any` but not recover its concrete type. The companion is a **checked downcast**:
    ```chezzi
    cast[int](x)          # -> Option[int]: Some(n) if x is really an int, else None
    match cast[Point](v):
        Some(p): print(p.x)
        None:    print("not a Point")
    ```
    Returning `Option[T]` (not a raw `T`) makes it fit `?` / `match` and keeps it total (no faulting
    downcast). **Why deferred — the runtime ERASES generics, so `cast` can only *honestly* witness what
    a runtime `Value` still carries:**
    - `Value` is `Int`/`Float`/`Bool`/`Nil`/`Obj` (`src/vm/value.rs`) — scalars and `str` witness fine.
    - `Obj::List(Vec<Value>)` (`src/vm/heap.rs`) carries **no element type**; `Obj::Struct{tid,…}`
      carries only the **type id** (→ name via `struct_names`). So `cast` can witness a *bare container
      KIND* (is-it-a-list) and a *named struct/enum BY NAME*, but **not** a parameterized target.
    - Therefore `cast[List[int]]`, `cast[Map[str,int]]`, `cast[Box[int]]`, … are **unsound and must be
      REJECTED** at the checker: `List[int]` and `List[str]` are the same runtime shape, and an empty
      list is ambiguous for *any* element type. Only `cast[Scalar]`, `cast[str]`, `cast[List]`-kind, and
      `cast[NamedStructOrEnum]` (by name) are honest.
    Lifting the parameterized-target restriction needs **runtime type tags** on heap objects (element
    types on lists/maps, type args on structs) — its own milestone (also a prerequisite for reflection).
    Record this so a future `cast` implementation starts from the erasure contract, not a surprise.

15. **Type conversion protocol (`Convert[S]`) + scalar fills — ✅ LANDED (slices 1+2+3).** Today
    conversion is a fixed set of builtins (`int`/`float`/`str`/`ord`/`chr`, safe `to_int`/`to_float`)
    plus one-way `int`→`float` widening of an untyped CONSTANT, and one-way newtype wrap/unwrap. The extensible mechanism is
    the reserved `Convert[S]` protocol (there is still no `as`, no `Into`/`TryFrom`). Full current-state
    inventory in `docs/spec.md` "Type conversions & casting". The intended direction, in leverage order:
    - **`Convert[S]` structural protocol** (the big one) — a type witnesses it with a **static** method
      `fn convert(x: S) -> Self`, witnessed structurally like `Comparable`/`Add` (reuses `satisfies_args`,
      made `is_static`-aware; no nominal trait-impl machinery — fits Chezzi's structural model). **Slices
      1+2 LANDED** (2026-07-07): the protocol is reserved + binds as `[T: Convert[S]]`, sound static-slot
      witnessing (an instance `convert(self,…)` does NOT witness it), and **bound-only** enforcement
      (rejected as a value-annotation type — a value can't invoke a static ctor). **Slice 3 (`T.convert`
      through a bound) ✅ LANDED as M24** (2026-08-10). The 2026-07-07 spike had ruled it deferred on the
      premise that a "restricted construction" checker rewrite delivers nothing under erased,
      single-pass, non-monomorphizing generics — which was true of *that* model and is why the answer
      was **witness passing** instead: the concrete type's runtime identity key rides in as a hidden
      trailing argument, so `T.convert(n)` dispatches without `T` ever becoming concrete in the checker.
      It was never `Convert`-specific (the same gap hit *every* generic static call, e.g. `T.empty()`),
      and it did not ship as a `Convert` feature: item 13 is the mechanism and `Convert[S]` is one
      reserved consumer of it (`fn make[T: Convert[int]](seed: T, n: int) -> T: return T.convert(n)`).
      Direct `Type.convert(x)` still needs no protocol and is still the right spelling when the type is
      known. A fallible conversion is `convert(x: S) ->
      Result[Self, E]` — **no separate `TryFrom`** needed. **Skip `Into`** (needs expected-type threading;
      Chezzi infers bottom-up). **Multi-source (Phase 2) also DEFERRED** — needs argument-type overloading
      (banned invariant) for thin payoff; distinct-named static ctors cover it today.
    - **Cheap scalar fills — ✅ LANDED** (additive, low risk, landed independently ahead of the
      `From` protocol): `bool(x)` truthiness cast (int/float/bool/str, never faults on a scalar) +
      the `Result`-returning `s.parse_int() -> Result[int, str]` / `s.parse_float() -> Result[float,
      str]` siblings of the `Option`-returning `to_int`/`to_float`.
    Variance/soundness note: a `from`-based conversion is a value-producing call, not a subtype
    relation — no covariance holes. This is a language feature (own milestone), not a perf lever.

**Ecosystem (Tier 4, separate track):** REPL (huge for scripting iteration), formatter, `assert` +
built-in test runner, LSP.

---

## 3a1. Generics strategy — the live options, and the ruling (2026-08-09; **BUILT as M24, 2026-08-10**)

Items 13 (`T.default()`), 14 (`cast[T]`) and 15 (`Convert[S]` slice 3) above were all blocked by the
same wall, and §14's "erasure contract" is what the wall is made of. This section records the
**strategy decision that governs all three**, so they stop being re-litigated one at a time.

> **Status: the ruling shipped.** Witness passing is built (**M24**, 2026-08-10) and items 13 and 15
> are closed by it; item 14 (`cast[T]`) stays closed for its own reason (no consumer). The "what stays
> impossible" list below is still true and now has one more entry — the **fn-value wall** — which is
> the only one of the six axes the spike named that did not close. Everything below is kept as the
> reasoning of record; the "recommended next step is a SPIKE" note at the end is superseded.

### The question

Chezzi's generics are **erased**: `T` exists in the checker and is gone by the time the compiler
emits. Nothing at runtime knows a `Box[int]` from a `Box[str]`. Three ways forward:

| | monomorphize | witness passing | stay fully erased |
|---|---|---|---|
| `T.default()` / `T.convert()` through a bound | ✅ | ✅ | ❌ |
| `cast[List[int]]` / `match` on a `T` | ✅ | ❌ | ❌ |
| unboxed `List[int]` (`Vec<i64>` not `Vec<Value>`) | partial | ❌ | ❌ |
| one erased body per generic fn | ❌ | ✅ | ✅ |
| separate compilation / `import` | breaks | fine | fine |
| size of the change | whole-language | one milestone | none |

### Ruling: **witness passing.** Monomorphization is NOT the direction.

Reasons, in the order that decided it:

1. **Its biggest win is a feature we declined.** Monomorphization's headline gain is reified type
   args — `cast[List[int]]`, `match` on a `T`. But `cast` is dead: it only pays off through the `Any`
   top type, and **the owner ruled against leaning on `Any` (2026-08-09) — "we vouch for statically
   typed; `Any` is Go's `interface{}`"**. With `Any` out, §14 has no consumer.
2. **It does not deliver the perf win people assume.** `Value` is a uniform 8-byte pointer-tagged word
   (`src/vm/value.rs`), so a monomorphized `List[int]` is still `Vec<Value>`. Unboxing needs *typed
   containers* as a separate change. Monomorphization alone moves nothing on the CPython gap — do not
   justify it on perf without building the container work too.
3. **It contradicts the declared anchor.** `PROGRESS.md` pins the reference model as **Java (erased)**
   precisely because Java is the only mainstream *erased* model, and copying Rust/Swift ergonomics
   without their machinery is how generics go inconsistent. Monomorphizing is not an increment on that
   model; it replaces it.
4. **It breaks the module story.** Chezzi resolves modules at runtime (`resolver`), so there is no
   whole-program point at which every instantiation is known.

**The one condition that would reopen this: the Cranelift JIT.** A JIT wants monomorphized,
type-specialized code, so if that end-game is committed to, monomorphization is on its path. `CLAUDE.md`
scopes Cranelift as a late-stage endeavor, so this stays closed until then — but revisit it *with* the
JIT, not before, and revisit it as one decision rather than as three feature requests.

### What witness passing must satisfy before it ships

The mechanism is small — resolve the conforming type's static method at the call site, thread it in as
a hidden trailing argument, keep the single erased body. **The mechanism is not what failed twice.**
Both rejected runs (branches `auto-task/protocol-static-req`, `…-v2`, item 13 above) died the same way:
the checker's "accept" boundary drifted out of lockstep with the compiler's "can-lower" boundary, so
each run half-covered the surface and a prosecutor found the next axis.

A witness cannot be resolved once at the outermost call, because the caller is often generic itself:

```chezzi
fn make[T: Default]() -> T: return T.default()
fn g[T: Default]() -> T:    return make[T]()     # T is STILL abstract here
```

`g` must *forward* its witness. So the deliverable is **one checker gate that decides "this call can be
witnessed", proven against every axis at once** — not codegen. The six axes that broke the earlier runs,
which any future attempt must cover before writing a line of lowering:

1. cross-module call
2. `spawn:` / `parallel:` body
3. first-class value / `defer` (`g := make; g()`)
4. inferred `T` through a container (`xs: List[T]`)
5. non-leading bound param
6. generic-calls-generic witness forwarding (the shape above)

**M23 is evidence the contract is now expressible.** The `Eq` protocol needed exactly this shape and
landed it: `validate_eq_shape` (checker decides) → `binds_eq_hook` (compiler records) →
`Program::eq_struct` / `eq_enum` (VM dispatches from a validated table, never a name lookup). Checker-only
was tried and rejected there for the identical reason — `fn same[T](a, b): return a == b` erases `T`, so
a use-site gate never sees the concrete type. That is the first in-tree precedent for a
checker→compiler→VM contract over an erased boundary.

**The spike ran, and the contract closed — M24 (2026-08-10).** Five of the six axes are supported;
axis 3 (first-class value / `defer f(...)`) is a permanent wall and is listed below. The shape is
exactly M23's: the checker decides ([`Checker::witness_params_of`] answers "does this fn take hidden
witness params" ONCE and stores it on `FnSig::witness_params`; every consumer reads it from there),
the compiler records (`$w:T` locals, appended to nested bodies' capture entries), and the VM
dispatches from the recorded key (`Op::CallStaticDyn` → `Vm::do_static_call`). The lesson from item
13's shelf note held: **a filed residual's premise decays** — the note was written 2026-06-25, before
any of this machinery existed, and re-deriving it is what turned a "not worth the cost" into a
milestone.

**One trap this milestone paid for three times, worth stating generally: a span used as a cross-half
table key must not double as a diagnostic anchor.** The witness table is keyed by source position so
the checker's answer and the compiler's lookup agree. Three separate bugs on this branch were the
same mistake — `|>` desugars at PARSE time and gives every link of `a |> f() |> g()` the span of the
whole infix expression, so two witness calls aliased onto one key and the second silently took the
first's type: a **wrong value both engines agreed on**, so parity was blind to it and only a running
test with two different concrete types caught it. The third instance survived a fix that *claimed* to
have separated them: the fragment anchor was still written onto a cloned AST node's `span`, so a
fragment root that was itself a string carried the outer literal's span into the NESTED
interpolation's keys and both per-call tables missed. A FOURTH followed the third — a re-lexed
fragment carried an absolute line but no absolute COLUMN, so two sibling nested fragments restarted
at column 1, shared one key, and both took the last one's witness (that fix turned a loud fault into
a silent wrong value). The key is now the **callee token** (distinct per link) and the anchor is
**deleted**: a fragment is re-lexed against the enclosing literal's `PosMap`
(`lexer::tokenize_frag`), so every fragment token span is the char's REAL physical source position,
and with a real position there is nothing left to anchor — the checker points at the expression,
exactly where CPython carets inside an f-string (measured 3.14.6). The general lesson holds: any
future checker→compiler table keyed on position inherits this hazard, and the durable fix is to make
the position REAL rather than to correct it at a consumer. **M24-6 (2026-08-11) is that lesson
carried to its conclusion.** The intermediate fixes above each made the position *more* injective by
a chosen arithmetic — an absolute base line, then a base column that deliberately kept counting past
a newline (`Lexer::base_col`, since deleted, whose price was a column that could run off the end of
its physical line). The map replaces the arithmetic with the thing itself: keys are injective because
two distinct source chars are two distinct positions, a property of the FILE. Same move `W7-49` made
for module identity in the other axis.

### What stays impossible under witness passing, permanently

State these when the questions recur; they are not oversights:

- **A witness-taking generic read as a FUNCTION VALUE.** `g := reset`, a turbofish read as a value, a
  HOF argument and a cross-module read all reject with *"'reset' cannot be used as a function value:
  its bound on T requires a static protocol method, which needs the concrete type — a function value
  erases it."* A `fn` value carries a code pointer and captures; it does not carry which *declaration*
  it came from, so there is no site at which the hidden argument could be supplied. This is axis 3 of
  the six above — the one that did not close, and the reason to say so plainly is that it looks like a
  v1 gap and is not. Workaround: call it directly, or take a factory closure
  (`fn make[T](mk: fn() -> T) -> T`).
- **A type parameter of the enclosing TYPE** (`struct Bx[T: Default]` … `T.default()` in a method).
  The concrete type is erased the moment a `Bx` *value* exists, so only a value could carry the
  witness — and putting it there would make every generic struct pay for it. Declare the parameter on
  the **member** instead; its witness rides on the call.
- **`cast[List[int]]` / any parameterized downcast.** `Obj::List` carries no element type. Java refuses
  the same thing at compile time (`o instanceof List<String>` → *"Object cannot be safely cast to
  List<String>"*); Python refuses it at runtime (`isinstance(x, list[int])` → *"argument 2 cannot be a
  parameterized generic"*). Both measured 2026-08-09.
- **`match` on a bare `T`** — `cannot match on non-enum type T`.
- **A generic slot that coerces like a declared one.** `fn f(x: float)` accepts `f(2)` → `2.0`; a method
  param declared `T` on a `Box[float]` rejects `b.set(2)`. Same type, same value, different answer,
  because the backend sees `T` and not `float`. Documented at `docs/spec.md:513`; the erasure tax
  showing through to users, and the one item on this list that is arguably a bug rather than a wall.
- **Specialized numeric containers.** Needs typed containers, independent of this decision.

**Never on that list — the `spawn f(...)` / `defer f(...)` STATEMENT TARGET was a DEFERRAL, and it is
now FIXED** (2026-08-14, `docs/gaps.md` **M24-5**): the six emit lines push the hidden witness after
the declared args and widen `Op::SpawnCall`/`SpawnMethod`/`DeferCall`/`DeferMethod`'s `argc`, which
is all it ever needed — the VM was already argc-generic. It was arity plumbing, not erasure. The
receiver-less head found beside it (`defer Holder.build(3)`, which PANICKED the compiler) is fixed in
the same row: a static method is an ordinary call, so every static spelling now lowers through a
wrapper proto that replays it — still no new opcode, still no VM change.

### Not the same problem — do not fold it in

**Compiler type-blindness** (`docs/future.md §4`, struct-field slotting) looks like erasure and is not.
The compiler discards *all* static types, not just generic args — it knows a field's *name* but not the
receiver's struct type. The fix is to thread the checker's types into the compiler, and its payoff is
larger than perf: it is the `checker-superset-of-compiler` soundness class, where the checker accepts
what the compiler cannot lower. Keep the two tracked separately.

---

## 3b. Test system — planned improvements (M20 shipped; these are follow-ups)

M20 shipped: `assert`, free `test fn`, struct **suites** with lifecycle hooks
(`before_all`/`after_all`/`before_each`/`after_each`) + a typed shared fixture, and the `chezzi test`
runner (`src/test_runner.rs`). The native suite lives in **`tests/chz/`** (`spec/`/`stdlib/`/`suites/`),
run M:N-by-default (like `chezzi run`, `--serial` opt-out), with a `cargo test` dual-engine gate
`test_runner::chz_suite_passes_both_engines` asserting serial==M:N. These are the ranked follow-ups.

Current semantics: `assert true`=PASS, `assert false`=FAIL (with `file:line` + message; the message is
any `str` **expression**, not just a literal — variable/interpolation/concat all work, checker-enforced
`str`). **Any other runtime fault renders ERROR** (see item #1 below). File-level compile/type errors
render ERROR too (whole file, before any test runs), counted separately as `file error(s)`.

1. **FAIL vs ERROR split — DONE.** A `test fn`/method is **void**, so `assert` is the *only* intended
   failure signal → any other runtime fault (OOB, div-zero, overflow, missing key, native fault) is by
   definition **unexpected** and renders **ERROR**, not FAIL — pytest's FAILED-vs-ERROR distinction
   ("wrong assumption" vs "code crashed"). **Landed:** `RuntimeError` (`src/vm/mod.rs`) carries a
   `pub is_assert: bool` discriminator (default `false`; `Display` unchanged, so parity strings are
   byte-identical), set `true` ONLY by the `Op::Assert` arm (`src/vm/exec.rs`). The runner's per-test
   `Outcome` now holds an extensible `Verdict` enum `{ Pass, Fail{line,msg}, Error{line,msg} }`
   (`src/test_runner.rs`): a free-test / test-method body fault routes assert→`Fail`, else→`Error`, and
   every setup/teardown fault (suite construction, `before_all`/`before_each`/`after_each`) is
   `Error`-class regardless of `is_assert`. Summary is now `P passed, F failed, E errored` (+ optional
   `K file error(s)`); `report.passed` requires `F==E==file_errors==0`; exit non-zero if any. The
   `Verdict` enum is the extension point for the ergonomics wave's `TimedOut`/`OverMemory` buckets —
   `OverMemory` is now the first of these to land (item 1b below).

1b. **Per-test memory cap (`--max-heap=<bytes>`) — DONE.** A runaway-allocation guard for `chezzi test`,
   mirroring the (still-planned) per-test timeout but for memory. Opt-in `chezzi test --max-heap=<N>`
   (plain byte count; `0`/omitted = OFF, the default): when a single test's in-VM live heap exceeds `N`
   it is **hard-aborted** (bypassing `recover:` — a `for: recover: <alloc>` loop cannot defeat it) and
   bucketed in a new `Verdict::OverMemory` (rendered `OVER-MEMORY name (file) msg`, counts as failure,
   exit non-zero; summary appends `, M over-memory` only when `M>0` so cap-off output is byte-identical
   to before). **Deterministic-in-VM, NOT OS RSS:** the cap is checked against `Heap::live_bytes()` —
   the same value already computed once per `sweep()` for the peak probe — a per-heap high-water, so it
   is deterministic and the dual-engine gate `chz_suite_passes_both_engines` (which runs cap-OFF) is
   untouched. Mechanism mirrors the cancel bypass-recover unwind: `Heap` gained `mem_cap`/`over_cap`;
   `sweep()` sets `over_cap = mem_cap != 0 && lb > mem_cap`; `run_until`'s post-collect boundary
   hard-aborts via `unwind_deferred(base_level, false)`. The check is **re-observed at every GC boundary
   like a cancel checkpoint — no latch** — so a `defer` that itself allocates runaway during the abort's
   own cleanup unwind is bounded too (its nested `run_until` re-trips and aborts it; `should_collect()`
   resets after each collect, so a non-allocating defer runs to completion). The abort stamps an
   **`is_over_memory` marker onto the `RuntimeError`** (mirrors `is_assert`, excluded from `Display` so
   parity is unaffected) and forces it back on after every unwind, so it travels WITH the error across
   an enclosing **native-reentry** `run_until` (a HOF callback / operator overload / deferred call) AND a
   **`spawn`'d worker's** fault crossing back to the parent; the `run_until` Err funnel bypasses
   `recover:` whenever the marker is set, and `verdict_from_fault` reads `e.is_over_memory` first to
   bucket it. `spawn`/`parallel:` tasks are covered on M:N too — `spawn_worker` threads `mem_cap` onto
   the worker's own heap, so a **runaway** alloc in a task trips and buckets on both engines. VM + runner
   + `main.rs` flag only — no checker/compiler change. **Off-heap wire storage IS counted (gaps.md
   W6-10, fixed 2026-07-27).** A value moved across the airlock into a `Channel`/`Shared`/`RwShared`/
   `Atomic`/`Executor` core lives as a `WireValue` in an `Arc` **outside every `Heap`**, so `live_bytes`
   used to count it nowhere and a 195 MB channel backlog sailed past a 200 KB cap. Each core now caches
   its payload's approximate byte size (the same summary that makes the GC skip pure-data payloads —
   see gaps.md W6-7) and `live_bytes` adds it in, so the natural *concurrent* runaway — an unbounded
   backlog, or data parked in a `Shared` — trips like any other. A core's bytes are charged **once per
   core per heap** (by `Arc` pointer identity): `from_wire` mints a fresh `Obj::Shared`/`Obj::Channel`
   alias slot on every crossing, so charging per *slot* would multiply one payload by the number of live
   handles and fire OVER-MEMORY on a program using a fraction of the cap. Two things follow, and they
   sharpen the per-heap guarantee below rather than restate it: a core reachable from N M:N worker heaps
   is counted in **each** of them (the number is "bytes **reachable from** this heap", not an ownership
   split — the N heaps' totals do not sum to RSS), and a payload reachable only through a **nested** core
   whose last alias slot has been swept needs a cross-core recursion to be seen at all — that was
   `W6-10r`, **FIXED 2026-08-06**: `live_bytes` now walks into nested cores (`Arc`-de-duped against the
   same per-heap set, so a nested core with an alias slot of its own is still charged once), gated on
   `mem_cap != 0` so a cap-off run pays one branch and zero extra walks. The inline-scalar escape below
   is a different hole, and is **CLOSED 2026-08-07 by `W7-28`** (see the end of this item).
   **GC pacing is byte-aware WHEN A CAP IS SET (round 3, 2026-07-27).** Counting the off-heap bytes was
   not enough on its own: `over_cap` is evaluated only inside `sweep()`, and `sweep()` used to run
   purely on `Obj`-count growth, so a program pushing megabytes across the airlock while allocating ~2
   `Obj`s per iteration never swept, never sampled the cap and **passed** (304 MB against an 8 MB cap;
   a 200k-int sibling at 3369 MB). `should_collect()` now also fires on charged off-heap bytes —
   `mem_cap != 0 && since_gc_wire_bytes >= (mem_cap/4).max(64*1024)`, charged at `Vm::to_wire_crossable`
   (the one helper every cross-heap value store routes through) and reset in `sweep()` beside
   `since_gc`. It is a monotonic pacing HINT, never accounting (`live_bytes` stays the sole measure):
   a replacing store charges, a `recv` never decrements — net tracking would let a steady send/recv
   pipeline stall the trigger forever, i.e. fail open again. **Gated on `mem_cap != 0`**, so cap-off
   pacing (every `chezzi run`, every bench, the whole parity gate) is bit-for-bit unchanged; a capped
   run pays extra sweeps plus a second `wire_summary` walk per store (+11% measured, `docs/benchmarks.md`).
   Residual SAMPLING escapes were listed in gaps.md `W6-10s`, which is **CLOSED 2026-08-06** — its
   filed premise (uncharged by-hand airlock paths) did not survive re-derivation: the only one storing
   persistently off-heap through the QUEUE is `Executor.submit`, and only on `--serial`, which
   `--max-heap` refuses at the CLI. (That sentence read "the only one storing persistently off-heap"
   until `W7-26` measured its blind spot the same day: on M:N `submit` runs EAGERLY, so the same call
   stores persistently in the core's *other* half — `eager` — which nothing counted and nothing
   sampled. Both are fixed; `W7-26r` tracks what still is not sampled.) The reachable escape from
   this row was a worker heap **born big** — its task's payload arrives in ~7
   `Obj`s, so the object-count trigger never moves and nothing ever samples — now fixed by
   `Heap::request_collect` from `Vm::spawn_worker`. **Both residuals on that row are now CLOSED**: (b)
   by `W7-28` (next paragraph), and (a) — a task whose entire body is ONE native call, which pushes no
   frame and so never reaches `run_until`'s loop — by `W7-29` 2026-08-07. `Vm::start_task` samples the
   cap before dispatch with the pending call's operands rooted on the operand stack; the filed claim
   that that window had "no safe sample point" was wrong. `request_collect` stays for the OTHER task
   door, `ReadyWorker::invoke` (eager `Executor` jobs), which does not route through `start_task`.
   **THE TRIGGER COUNTS BYTES, NOT EVENTS (round 4, `W7-28`, 2026-08-07).** Every earlier trigger
   counted an event — allocations, wire crossings — and each event class has a shape that adds
   unbounded bytes without raising it. Measured against `--max-heap=8000000`, all PASS pre-fix:
   `xs.push(i)` × 80 M grows the `Vec` behind an existing `Obj::List` and moves NOTHING (**617.8 MB,
   77× the cap**); `big.extend(chunk)` × 150 does the same in ~1200 instructions (**~240 MB**);
   `s = s + s` × 22 (41 MB) and `"x".repeat(20000000)` (20 MB in ONE allocation) both stay under the
   256-object floor. `Map`/`Set` fail open like `List`. So `should_collect()`'s byte term is now fed by
   **all three** funnels through which a heap can gain bytes — `Heap::alloc` (new objects),
   `Heap::get_mut` (the sole `&mut Obj` door, charging a deferred before/after delta, so growth in
   place cannot escape whatever new container method is added later) and `Vm::to_wire_crossable`
   (off-heap, as before) — all reset in `sweep()`, all gated on `mem_cap != 0`. An INSTRUCTION TICK was
   tried first and rejected: `extend` adds N values in one instruction, so no instruction interval can
   bound it. A proxy that is only *nearly* proportional to bytes is not a byte counter.
   **v1 limits (deterministic, documented):** the
   trip fires only at a **GC boundary**; the check is a high-water on `live_bytes` which
   **undermeasures** true RSS and can overshoot ~2× `N` before firing (`next_gc = 2*live`). **The cap is
   PER-HEAP, so its guarantee is: any single execution context (the test's own heap; a `spawn`'d worker's
   heap on M:N) whose live heap — including the off-heap wire payloads it can REACH, which a worker that
   allocated nothing itself may still hold a handle to — exceeds `N` is aborted — a real runaway trips on whichever heap runs it,
   the SAME verdict for a real runaway.** **M:N ENGINE ONLY — `--max-heap` errors if combined with
   `--serial`**, which is what makes the cap sound-by-construction. The cooperative `--serial` engine
   shares ONE heap across the parent + every `spawn`/`parallel:` fiber (so its `live_bytes` is
   `parent-baseline + Σ live tasks`), while M:N isolates each worker on its own fresh heap (measured
   alone). So a *concurrent* test near the boundary — allocation *split* below `N` per-fiber but summing
   above it, or a single sub-`N` task plus a non-trivial parent baseline — would bucket `OverMemory` on
   `--serial` yet pass on M:N: a serial≠M:N divergence. The only cross-engine-identical aggregate would be
   a global RSS-style measure, which is non-deterministic (rejected — it would break the very gate the cap
   protects), and the M:N aggregate peak is itself non-deterministic (task interleaving). Rather than ship
   that divergence, the flag is **restricted to the default M:N engine**, where the cap is
   per-worker/per-context and fully deterministic — there is no second engine to disagree. `--serial` is
   the parity oracle, slated for post-freeze removal ([serial-engine post-freeze removal](§2b)), so the
   restriction costs nothing real. `chezzi run` never sets the cap (test-runner-scoped). k/m/g
   suffixes and `chezzi run --max-heap` are deliberately out of scope (later waves). (`--timeout` — the
   wall-clock sibling — has since landed; see #4 below.)

2. **Table-driven subtests (`t.Run`-style).** Today a `for case in cases:` loop inside a `test fn`
   aborts at the first bad case with no per-case verdict. A subtest construct that reports each case
   PASS/FAIL (Go `t.Run`, Rust `rstest`, pytest `parametrize`) is the single highest-value ergonomic
   add and fits the Go lineage. Design open: a `subtest "name":` block, or a `cases`-driven helper.

3. **skip / xfail.** Conditionally skip a test (Go `t.Skip`, pytest `skip`/`xfail`, Jest `.only/.skip`)
   — WIP + platform-gated tests. Cheap; needs a skip signal the runner counts separately (`S skipped`).

**Runner ergonomics + CLI options & output format** (independent of the semantics above; `cmd_test` is
`src/main.rs:541`, accepts `[path]` + `--serial`/`--parallel` + `--max-heap` + `--timeout`):

4. **Per-test timeout (`--timeout=<ms>`) — DONE.** The wall-clock sibling of `--max-heap` (#1b), same
   arc. Opt-in `chezzi test --timeout=<MS>` (ms; `0`/omitted = OFF, the default): a test running longer
   than `MS` is **hard-aborted** (bypassing `recover:`) and bucketed `Verdict::TimedOut` (rendered
   `TIMED-OUT name (file) msg`, counts as failure, exit non-zero; summary appends `, T timed out` only
   when `T>0` so cap-off output is byte-identical). The abort stamps an **`is_timed_out` marker onto the
   `RuntimeError`** — the exact machinery `is_over_memory` uses (excluded from `Display` so parity is
   unaffected; forced back on after every unwind so it crosses native-reentry `run_until` + the
   worker→parent boundary; the `run_until` Err funnel bypasses `recover:` whenever it is set;
   `verdict_from_fault` reads it first). **Observation site (the key difference from `--max-heap`):** the
   deadline is checked at the **loop back-edge** in `jump_checked` — the hottest engine-independent
   checkpoint, hit every iteration of any `while`/`for` — so it covers BOTH the top-level test body
   (`invoke_test → run_proto → run_until`, which runs OUTSIDE the fiber scheduler) AND a `spawn`ed task's
   loop (which routes through the same back-edge). A per-VM `deadline: Option<Instant>` is armed
   (`now + timeout_ms`) at each invoke entry and threaded onto M:N workers as the SAME absolute instant.
   **Zero overhead when off:** the check is `if let Some(dl) = self.deadline` FIRST, short-circuiting
   before any clock read, so a cap-off `chezzi run`/`chezzi test` does ZERO added `Instant::now()` calls
   on the hot path; when on, the read is throttled to one per 1024 back-edges (a wrapping counter).
   **M:N ENGINE ONLY — `--timeout` errors if combined with `--serial`** (a wall-clock trip is
   non-deterministic → no serial==M:N parity; the dual-engine gate runs timeout-OFF and is untouched).
   VM + runner + `main.rs` flag only — no checker/compiler change. **v1 limits (documented, watchdog-
   thread follow-up):** the trip is observed only at loop back-edges (+ the M:N reds checkpoint), so a
   test blocked in a **native call** (a blocking syscall, `Channel.recv` with no traffic) or spinning in
   **loop-free infinite recursion** (which hits the stack/recursion guard instead) is NOT caught by this
   v1 — a true watchdog thread that can interrupt a blocked native is the natural next seam. Ms
   granularity; the abort lands at the next back-edge after the deadline (sub-ms overshoot for a tight
   loop). k/m/g suffixes and `chezzi run --timeout` are out of scope.
5. **Name filter. SHIPPED (2026-07-24).** `chezzi test -k <substr>` / `--filter <pat>` runs only tests
   whose displayed name (free `fn_name`, suite `Suite::method`) contains the substring, like
   `cargo test <filter>` / pytest `-k` / `go test -run`. Filtered after discovery, at the invoke site
   (filtered tests genuinely don't run). The summary notes `(K filtered out)`; a zero-match run is a
   clear `— no tests matched '<pat>'` failure (not a silent "0 tests"). Substring, not regex (v1).
6. **stdout capture option. SHIPPED (2026-07-24).** `--show-output` surfaces a FAILING test's captured
   stdout, indented under its `FAIL`/`ERROR`/etc. line (pytest show-on-failure). Default still discards
   (assert-on-value is the intended path); a passing test's stdout is never shown. Kept in the `Outcome`
   only when the flag is on (bounded to the run).
7. **Better options + output format. SHIPPED (2026-07-24).**
   - Verbosity: `-q`/`--quiet` (dots `.`/`F`/`E`/`M`/`T` + summary only) vs default per-line vs
     `-v`/`--verbose` (per-line + per-test `(Nms)` timing + a total). `-q`/`-v` are mutually exclusive.
   - `--errors=json` machine output, mirroring `chezzi check --errors=json` (same flag; suppresses the
     human lines). Shape necessarily diverges (it carries totals): `{"tests":[{name,file,line?,status,
     duration_ms}…],"totals":{total,passed,failed,errored,over_memory,timed_out,filtered_out,file_errors}}`.
   - Color (`--color=auto|always|never`, default auto = isatty on stdout) on the verdict tag. Resolved
     to a bool in `cmd_test`; the runner never probes the tty, so the captured (non-tty) test harness +
     the byte-identity gate never see ANSI.
   - Per-test + total **timing** — `-v`/json ONLY, NEVER in default/quiet output (non-deterministic → it
     would break the byte-identical `chz_suite_passes_both_engines` gate).
   - `--fail-fast` (stop at the first non-pass verdict) for tight iteration loops.
   - Ordering is deterministic (sorted files → free tests → suites, all declaration order); documented in
     `chezzi test`'s USAGE + the `run_tests_opts` doc-comment. **Default (no-flag) output is unchanged.**

**Migration note (corrects an earlier claim):** fault-path tests **are** portable in-language via
`recover:` — `r := recover: <faulting expr>` yields `Err(e)` and `e.message()` gives the fault text, so
`assert e.message().contains(...)` tests a fault without Rust (proven on both engines). The runner keeps
its *own* fault tests in Rust only because IT needs the fault `span` for `file:line`. So the "stays in
Rust" set for the `tests/chz/` migration is just: gc-stress rooting (`run_capture_stress`), checker
`rejects/ok` (compile-time), parser/lexer/bytecode internals, and concurrency timing — **not**
fault-path. Fault-path is a future migration cluster.

---

## 3c. Native-registry hygiene: a native's PROPERTIES belong on its registry entry — **DONE 2026-08-05** (option B, plus the interception fold)

**Landed.** `pub enum Kind { Inline, Blocking, TimedWait, InterceptIo, InterceptNet }` is a field of
every `MEMBERS` tuple (`&[(&str, NativeFn, Kind)]`, 192 entries across 14 tables), copied onto
`Obj::Native` when the module binds (`vm/exec.rs`) and carried through `WireValue`/`SnapValue` and
`Callee::Native` into `Vm::invoke_native(func, name, kind, args, span)`. `is_blocking` is **deleted**,
along with its `strip_prefix('_')` patch and the `native_member_names_are_unique_across_modules` guard
whose whole premise was bare-name classification. **No native's behaviour is decided by a string
comparison anywhere in the VM**, and a `MEMBERS` entry without a kind is a compile error
(demonstrated: dropping `Kind::Inline` from `("now", now)` → `expected a tuple with 3 elements`).

Three corrections to the estimate below, all found by reading the code before writing it:

- **`Vm::invoke_native` has exactly ONE call site** and `Obj::Native` is built in 3 non-test places, so
  the kind rides the value to the dispatch site — **no name→kind map, no lookup, no per-call cost.**
  The "~10 tuple-destructuring consumers" were 2 real ones (`vm/exec.rs`, `compiler/mod.rs`) + tests.
- **Name-keying would have been unsound anyway.** `std.io::_append` (an intercepted opener) and
  `std.fs::_append` (a syscall) collide on the bare name; they were kept apart only by the ORDER of the
  checks plus an exemption list in a test. Distinct entries, distinct kinds, no ordering hazard.
- **The interception fold was worth doing in the same pass** (it was listed out of scope). `connect`/
  `listen` were matched by BARE NAME in `invoke_native` — a future `std.foo.connect` would have been
  hijacked by the net handler — and the `std.io` openers by `fn_addr_eq` identity. Both are now kind
  arms. This is still two properties on one enum, not a plugin registry: the "don't grow it" warning
  below stands.

**Found by the conversion, fixed the next day:** `fs._stat`/`fs._walk` were never in the `is_blocking`
list, so they ran inline and pinned an M:N core worker (`walk` recurses a whole tree) — exactly the
silent failure this section predicted, already in the tree. The refactor preserved them as
`Kind::Inline` (behaviour-identical) and filed `gaps.md` **W7-19**; both are `Kind::Blocking` as of
2026-08-05, after the off-heap-safety proof. **This is the section's own payoff, measured**: the
property was made impossible to omit, and the first thing writing it down produced was a live
starvation bug nothing else had detected.

**W7-5e did NOT fold in** — `Vm::stdout_writes` is a per-CALL runtime observation ("did this call emit
to stdout?"), not a static per-native property. It was **fixed separately 2026-08-05** by the same
move this section makes one level down: the property rides the only door that can produce it
(`stream::write_out` takes the writing `&mut Vm` and bumps the counter itself), so forgetting it is a
compile error rather than a silent hole.

<details>
<summary>Original filing (2026-08-05) — kept for the reasoning</summary>

**The registry itself is fine and is not what this is about.** A native ships as a per-module
`pub const MEMBERS: &[(&str, NativeFn)]` (`src/native/<mod>.rs`) plus a bodyless `native fn` decl in
`std/<mod>.chz` for the signature. Two edits, both obvious, both local. Keep that.

**The problem is that a native's BEHAVIOURAL PROPERTIES live in string matches far from the entry.**

| property | where it lives today | what it decides |
|---|---|---|
| "is this blocking?" | `src/native/mod.rs:520` — one ~40-name `matches!` | offload to the dirty pool vs run inline on the worker |
| "is this a timed wait?" | `src/vm/call.rs:291`, `:339`, `:359` — three `"sleep_ms"` string arms | ride the timer thread; be a continuous cancel + `--timeout` checkpoint (W7-16) |

`sleep_ms` is named in 4 files. **A new blocking native that forgets `is_blocking` fails SILENTLY** —
nothing errors, no test goes red, it just pins an M:N worker for the syscall's duration (the D5
starvation the set exists to prevent). That near-miss is already documented inside the function it
would break: `native/mod.rs:521` strips a `_` prefix specifically so the W7-8 rename did not silently
un-classify every `std.fs` syscall.

**The fix — the property moves onto the entry, so the table is the single source of truth:**

```rust
pub struct Native { pub name: &'static str, pub f: NativeFn, pub kind: Kind }
pub enum Kind { Inline, Blocking, TimedWait }   // TimedWait replaces all three `"sleep_ms"` arms

pub const MEMBERS: &[Native] = &[
    Native { name: "sleep_ms", f: sleep_ms, kind: Kind::TimedWait },
    ...
];
```

`is_blocking(name)` becomes a lookup; `match name { "sleep_ms" => … }` becomes `match kind { TimedWait
=> … }`. The payoff is not tidiness — it is that **omitting the property becomes a compile error** (a
missing struct field) instead of a silent behaviour loss.

**Two sizes, and they are genuinely different jobs:**

| | diff | gets you | when |
|---|---|---|---|
| **A. Collapse the scatter** — add `native::kind(name) -> Kind` beside `is_blocking`; `call.rs` matches on `Kind::TimedWait` | ~4 files, ~40 lines | 4 files → 1. Still a name match, so still silently forgettable | safe any time |
| **B. Property on the entry** (above) | **192 entries across 12 tables** (`ffi` 59, `math` 35, `io` 20, `fs` 17, `encoding` 13, `os` 12, `crypto` 10, `request` 8, `process`/`regex` 5, `rand`/`time` 4) + ~10 tuple-destructuring consumers (`native/mod.rs:824,851,873,890,976,1012,1055`; `ffi.rs` tests) | the compile-error guarantee | **before the JIT freeze** — a table-shape change wants that boundary, and it is a pure refactor best done outside a bug-hunt |

**Deliberately NOT in scope.** `timer(ms)` is an **opcode** (`vm/op.rs:462`), not a native, so it never
joins this table — its handling stays where it is either way. Do not grow this into a trait-object or
plugin registry: it is two properties and one special-cased name, and a `kind` field is the whole fix.

**Adjacent, same family, already filed:** `gaps.md` **W7-5e** — `Vm::stdout_writes` is a *third*
per-native property ("did this call write to stdout?") resting on an unenforced invariant. If B is
done, check whether it folds in as a fourth `Kind` / flag rather than staying a hand-maintained
assumption. *(Checked when B landed: it does not — see the header above.)*

</details>

---

## 4. Optimizations (ranked effort → payoff)

> **Live numbers:** `docs/benchmarks.md` tracks Chezzi vs CPython (reproducible via
> `benches/run.chz`). After the M19 phases (call-flatten + SSO incl.): **~1.3×–3.5× slower than
> CPython**, and a **standing startup win** (~11× faster cold). The gap scales with call density —
> `loop` (no calls) is 1.32×, `fib` (all calls) is 3.54×. The M19 levers below are marked landed;
> the **ranked not-started backlog is "Post-M19 next levers"** further down.

The original M5 baseline was ~4–6.5× over the then-existing (now-removed) tree-walker, near the safe-match-dispatch floor; the current live comparison is vs CPython (see `docs/benchmarks.md`). The two real costs are
**dispatch count** and **name lookup** — with **per-call allocation** a close third on call-heavy code.

**Cheap — do first:**
- ✅ **Peephole + constant folding (compiler)** — *landed M19 Phase 1* (`src/compiler/peephole.rs`):
  a jump-relocating pass that folds `ConstInt`/`ConstFloat` arith + `Neg`/`Not`, replicating the
  VM's checked semantics (overflow / div-by-zero stay unfolded so the runtime raises the same error).
- ✅ **Superinstructions** — *landed M19 Phase 1*: `BinLocalLocal` / `BinLocalConst` / `IncLocal`
  fuse the hot `GetLocal+GetLocal+BinOp`, `GetLocal+Const+BinOp`, and `i += k` windows (Int fast
  path inlined; non-Int falls back to the exact unfused op). Cut `loop` −36%, `primes` −25%.
  Remaining candidates: `GetLocal+GetField`, fuse compare+`AsBool`, the load-store accumulator.
- ✅ **Global-slotting (inline-cache equivalent for name lookup)** — *landed M19 Phase 2b*: the
  compiler assigns each module global a stable `u32` slot (`ModuleProto.global_slots`) and emits
  `GetGlobalSlot`/`SetGlobalSlot`/`DefineGlobalSlot`; `Obj::Module.globals` (a `HashMap<String,Value>`
  probed by name per read) became `{ slots: Vec<Value>, index }`, so a global read is a `Vec` index,
  no hash. The slot map lives in the shared `Arc<Program>`, so parent and faulted-worker agree on
  slot↔name by construction — removing the slot-order fragility rather than just guarding it.
  **Reality vs prediction:** it moved `fib` −9% (the call-heavy bench resolves its callee per call),
  but *not* `primes`/`loop` — their hot loops read locals, not globals, so the "moves `primes`" guess
  was wrong about where global-read density actually is. Still cheaper on every global read.
- ✅ **Struct-field caching (the other half of name-lookup ICs)** — *landed M19 Phase 4*: static
  slotting (P2b's model) is impossible for fields because the compiler is **type-erased** (it knows the
  field name but not the receiver's struct type at emit time), so `GetField`/`SetField` carry a
  per-call-site IC id into a per-`Vm` `field_ic: Vec<IcCell>` that caches the field index; a hit
  re-verifies `fields[idx].0 == name` and skips the name-probe. The cell holds an index, not a `GcRef`,
  so it touches no GC / snapshot / `swap_ctx` machinery; each access self-verifies, so it stays sound
  under any future polymorphism. **Reality vs prediction:** −13% on a field-access-bound bench
  (`struct`, 3.32×→2.89× CPython), but **~neutral to −3% on a method-bound shallow-field bench** — the
  cold `field_ic` indirection only pays off when field resolution is the actual bottleneck (wider /
  deeper structs), not when method dispatch dominates. **Follow-up — landed M19 Phase 5b (`bbdcb38`),
  measured neutral:** a struct **type-id guard** (stamp a numeric type id on `Obj::Struct`; guard on
  `obj.tid == cell.tid` — a pure-int compare with no name re-verify) replaced P4's name re-verify on a
  hit. It did *not* close the shallow-struct caveat (the cold-IC indirection, not the re-verify, is the
  cost), but was kept: cheaper hot path, VM-only ⇒ parity-clean.
- ✅ **Kill per-call clones in `invoke_value`** — *landed M19 Phase 1*: matches on `&Obj` (no whole-
  `Obj` / closure-`HashMap` clone) and drops the arity-check `name.clone()`. Cut `fib` −17%, `list`
  −22%.
- ✅ **Pass call args as a stack slice (no per-call `Vec`)** — *landed M19 Phase 2*: `do_call`'s
  `Func`/`Closure` fast path runs in place over the args already on the operand stack (`copy_within`
  drops the callee from beneath them), skipping the `split_off` `Vec` alloc + the re-push in
  `push_frame`. Native / non-callable callees keep the `Vec` path (`invoke_native` needs it). Cut
  `fib` −13%.

**Medium:**
- ✅ **Struct type-id guard for the field IC** — *landed M19 Phase 5b, measured **NEUTRAL***. Stamped a
  dense `tid` (layout id) on `Obj::Struct`; the field IC now guards on `obj.tid == cell.tid` (pure-int
  compare) instead of the `fields[idx].0 == name` string re-verify. **But it didn't move the benches**
  (struct 1.02×, method-bound 1.01× — noise): P4 had *already* collapsed the name-probe to a single
  verify-compare, and for short field names that string compare is already cheap, so swapping it for an
  int compare saves nothing measurable. Kept (correct, principled, no regression, future-proofs real
  polymorphic field sites), but **the field-IC lever is spent** — there is no cheaper guard to reach
  for. The "shallow-struct caveat" was a *prediction*, not a measured cost.
- ✅ **Small-string optimization (the real open `str` lever)** — *landed M19 (2026-06-12)*.
  `Obj::Str` now holds a `ChzStr` (`src/vm/chzstr.rs`): strings ≤ `INLINE_CAP` (22 UTF-8 bytes) are
  stored **inline** in the variant (no per-value `Box<str>` heap alloc), longer ones spill to a
  `Box<str>`. `Deref<str>` + `From` impls kept the ~100 `Obj::Str` sites compiling unchanged;
  `size_of::<Obj>()` stayed 88 B (pinned by a guard test). **`str` 217→174 ms, 2.62×→2.10× CPython
  (−20%)**; `list`/`loop`/`fib` neutral. **Note:** "concat / `split` / `+` builder/rope" is *not* a
  benched lever — the `str` bench is `BuildStr` + `,".join`, and `join` already buffers into one
  `String` (`mod.rs:4377`); `+`/`split` aren't exercised. A builder/rope only helps un-benched
  `s = s + x` loops. The pure-int `list` bench is at the **snapshot-parity floor** (both engines
  clone the iterand at `for`; ints are already unboxed) — no safe alloc lever there.
- ✅ **Faster `usize`/`u64` hasher** — *landed M19 Phase 5a*: `MapData`/`SetData`'s `index` (keyed by
  the cached content hash) and `str_intern` (pointer-keyed) swapped SipHash for an in-tree FxHash
  (`src/vm/fxhash.rs`, no dep). **`map` −7%** (3.04×→2.82× CPython; maps were unbenched, so a `map`
  bench was added). **Gotcha:** a naive multiply-only FxHash was **100× slower** — int keys store
  `f64::to_bits` (zero low bits), and FxHash mixes entropy only upward, collapsing hashbrown's low-bit
  bucket index; a splitmix64 finalizer in `finish()` fixed it. (The field/global IC paths don't use a
  HashMap — they're `Vec`-indexed already — so the lever reduced to the map/set + intern paths.)
- ✅ **`ConstStr` interning** — *landed M19 Phase 3*: a per-heap cache keyed by the literal's data
  pointer reuses the already-allocated handle, so a repeated `ConstStr` push is a pointer lookup,
  not a fresh box. (Cross-site compile-time dedup `Op::ConstStr(u32)` is marginal over this.)
- ✅ **Reduce string-op allocations (`stringify`-into-buffer)** — *landed M19 Phase 2*: `stringify`
  appends into a caller-owned buffer, so `BuildStr` reuses one `String` across all interpolation
  parts (cut `str` −5%).
- ✅ **Arithmetic specialization** — *largely shipped via P1 superinstructions*: `BinLocalLocal` /
  `BinLocalConst` / `IncLocal` inline the monomorphic int path for the hot `local op local` /
  `local op const` / `i += k` windows, so the int loops no longer re-dispatch per iteration. A
  general per-op type-guard cache is the only remaining slice, and it overlaps the superinstructions.
- ✗ **Frame pooling** — *low-ROI here*: `CallFrame`'s `deferred` / `defer_markers` are alloc-free
  `Vec::new()` and frames live in a capacity-reusing `Vec`, so there's no per-call frame alloc to pool
  (P2 already killed the per-call args `Vec`).

**Big (separate milestones):**
- ✅ **Flatten the call loop — LANDED (`634c6f5`); the `Arc::clone` warm-up — LANDED (2026-06-12).**
  `Op::Call` now pushes a frame and `continue`s the running `run_until` loop (no per-call Rust
  recursion / per-call `Arc::clone`); `run_proto_in_place` is kept only for native-initiated calls
  (HOFs). The stand-alone warm-up below — **hoist the per-entry `Arc::clone(&self.program)`** to a
  raw `*const Program` borrow (`mod.rs:2095`; sound because `self.program` is immutable + never
  reassigned) — also landed. Post-flatten the remaining entry is per top-level / native-reentry /
  fiber-resume, **not** per call, so it's neutral on the no-HOF standard suite but **1.05× on
  callback-heavy code** (`benches/chz/hof.chz`); see `benchmarks.md`. The original diagnosis follows.
- **Flatten the call loop (diagnosed 2026-06-12 — the top remaining lever for call-bound code).**
  Every Chezzi function call currently **recurses into a fresh Rust `run_until` loop**:
  `Op::Call` → `do_call` → `run_proto_in_place` (`mod.rs:1992`) → `run_until(base_level)`. Two costs
  ride every call as a result: (1) a **native Rust stack frame** per Chezzi call (push + the
  `frames.len() > base_level` bookkeeping + the `paused()`/result re-plumbing on unwind), and (2)
  **`Arc::clone(&self.program)` on every `run_until` entry** (`mod.rs:2115`) — a per-call atomic
  refcount bump+drop that exists purely as borrow-checker tax. fib(30) is ~2.7M calls ⇒ ~2.7M native
  recursions + ~2.7M atomic clones. **This is why `fib` is 3.85× CPython but `loop` is only 1.31×: the
  gap is the *call*, not the dispatch floor** (straight-line code is already near Python-par). `primes`
  (2.50×) is also call-bound and would move. The fix is what CPython 3.11 did for its jump ("zero-cost
  frames"): make the bytecode `Op::Call` **push a frame and `continue` the existing `run_until` loop**,
  and `Op::Return` **pop + push the result and continue** — no Rust recursion, one `Arc::clone` per
  whole `run_until` instead of per call. **Hard part / parity risk:** today pause/park (B1/D3),
  `recover:` unwind, and `defer` all lean on Rust-stack unwinding through the nested `do_call`/`?`
  chain. A flat loop must instead park by leaving `self.frames` intact and breaking the loop (the M:N
  engine already saves/restores frame state via `FiberCtx`, so the machinery exists). **Keep the
  re-entrant `run_proto_in_place` for native-initiated calls** (HOFs — `map`/`filter`/`sort` call
  `invoke_value` per element and need the callback's result *synchronously* mid-native-method); only
  the bytecode `Op::Call` path flattens. The two coexist: HOFs nest a sub-loop when they must, the
  common recursive/bytecode call no longer does. Cheap warm-up that stands alone even without
  flattening: **hoist the per-call `Arc::clone`** (raw-pointer/restructure the program borrow) — a free
  few-percent on every call-bound bench. Blast radius is **VM-only**;
  parity is testable against the existing fib / recover-in-recursion / defer-in-recursion / deep-
  recursion-overflow goldens. Bigger than a Medium item, smaller than the register-VM rewrite below.
- **NaN-boxing the `Value` — BLOCKED by full 64-bit ints (2026-06-12 reality-check).** The goal
  (16 B → 8 B, operand-stack cache density, moves `loop`/`list`/`fib`) is real, but `Value::Int` is a
  **full `i64`** (`src/vm/value.rs:18`). NaN-boxing packs every value into 8 bytes, and a full i64 +
  a type tag do **not** fit in 8 bytes alongside `f64` — the payload of a NaN-box is ~48–51 bits. To
  do it you must **box big ints** (small-int tagging): a branch + a heap alloc on every int outside
  the taggable range, plus a semantics-sensitive overflow path — i.e. *not* behavior-preserving for
  free, and an uncertain net win on the very int-heavy benches it targets. **Lua 5.4 made exactly
  this call** — it stayed at a 16-byte tagged union *because* it added 64-bit ints. Blast radius is
  **VM-only**, but it's still a milestone-sized design spike (box-big-ints scheme + measure), not a clean
  behavior-preserving session. Park until the int model is up for revisiting.
  - **UPDATE 2026-07-18 — 8B-`Value` is now its own milestone (int-favoring pointer-tag, not NaN-box).**
    Design/plan in `~/.claude/plans/2026-07-18-8b-value-pointer-tag-*.md`: tag `Int` inline (`(n<<1)|1`,
    ±2^62), box the rare wide int (`Obj::BigInt`) and every `f64` (`Obj::FloatBox`). **Phase 0 scaffolding
    landed** (parity-trivial, additive): `Heap::live_bytes()` + peak high-water probe behind
    `CHEZZI_HEAP_STATS=1` (baseline: `benches/run.chz` peak_live_bytes=24277 at size_of_value=16), and the
    two GC-leaf `Obj` variants (`BigInt`/`FloatBox`, unused by real programs yet — reachable only from a
    unit test; `size_of::<Obj>()` stays 88). Phase 1 (the `struct Value(u64)` swap) is gated on the
    measured memory drop vs this baseline.
  - **DONE 2026-07-18 — the 8B-`Value` swap LANDED (int-favoring pointer-tag).** `Value` is now
    `struct Value(u64)`. The measure gate passed comfortably: on `benches/run.chz` the dispatch-floor
    benches got **faster**, not slower — `loop` 1.13×→**1.03×** CPython (near parity), `fib` 3.29×→**2.95×**
    (first sub-3×), `map`→1.77×, `poly_method`→3.94×; only `primes` +2.7% (in-noise). The cache-density win
    beat the tag decode tax. Behavior-preserving (int `==`/order exact-i64; `1==1.0` preserved; overflow
    still faults at the i64 ceiling). This also surfaced + fixed a pre-existing soundness bug: int `==`
    was lossy `as_f64` above 2^53 (fixed `ccbd3c4`, now exact i64). Full numbers in `docs/benchmarks.md`.
    Float-constant interning (plan Task 5) deferred — no measured float regression on the int-heavy set.
    **So this lever is no longer "blocked" — NaN-box was the wrong scheme; pointer-tag was the right one.**
- **Register VM** instead of stack — fewer ops, less stack traffic. Effectively a VM rewrite; only
  if dispatch count is still the wall after superinstructions.
- **Generational / incremental GC** — current is stop-the-world full-heap (`next_gc = 2×live`).
  Generational cuts pause + rescan cost on allocation-heavy scripts.
- **Cranelift AOT/JIT** — already the stretch goal. Near-native, but a whole backend. Only after the
  language stops moving.

### Memory layout & access patterns (cache levers — diagnosed 2026-06-16)

> **Caveat first (measure, don't guess):** the bench bottleneck is **dispatch + calls + a few alloc
> paths, NOT the value/heap layout** — `loop` is at the match-dispatch floor, and the `struct` bench is
> *method-dispatch*-bound (the field IC already serves hot reads; the type-id guard was measured
> **neutral**). So these are **not** reliable standalone bench-movers. But #1 and #3 below double as
> **JIT-prep**: a method-JIT compiles a field/capture read to a **constant offset**, so it needs a
> canonical *positional* layout — landing them is groundwork for the JIT, not (necessarily) a speedup
> on its own.

**Compact aggregate representation — drop per-instance redundancy (the real layout lever):**

1. ✅ **Shared per-type struct layout (hidden-class / `__slots__`) — DONE (2026-06-16); type-name drop DONE (2026-07-25).**
   `Obj::Struct { tid, fields: Fields }` (`heap.rs`; `Fields` was a flat `Vec<Value>`
   until lever 3b inlined it) stores fields **positionally** (declaration-order offsets) — no per-instance field-name
   strings. Names live only in `StructDef { fields: Vec<String>, tid }` (`op.rs:378`), resolved on the
   cold path (Display/stringify/probe-miss/wire/snap) by `name`→`StructDef`. Killed the N per-field
   `Box<str>` allocs/instance. The single top-level `name: Box<str>` was **also dropped** (2026-07-25,
   memory lever #7): it duplicated `tid`, so it now resolves from `tid` via the dense reverse index
   `Program::struct_names` (`Vm::struct_name_of_tid`, O(1) — the struct analogue of enum `variants_by_id`),
   and struct `==` is now a pure-`tid` int compare (subsumes the old `na != nb` name compare, as enum
   equality compares `variant_id`). The synthetic native structs `Match`/`Response`/`ProcResult`/`FileInfo`
   are registered in `Program.structs` (`compiler/mod.rs` `Compiler::new`) so the runtime can recover
   their field names + type key. Perf: bench-neutral (struct bench reuses instances, dispatch/alloc-bound)
   but a 4-field struct-construction micro went **827 ms → 510 ms (−38%)**, and the name drop is ~28% of
   `many_struct` RSS (measured post-merge). Hands the JIT a constant field offset. Interp removed.
2. **✅ DONE — Enum variant id instead of names.** Was `Obj::Enum { ty: Box<str>, variant: Box<str>,
   payload }` — two `Box<str>` per instance, both global (`Program::variants`). Now `Obj::Enum {
   variant_id: u32, payload }` (the enum analogue of `tid`); the type + variant names resolve from the
   new `Program::variants_by_id` table on the cold path only (Display/stringify/error/wire/snap).
   Match-arm dispatch, equality, and `?` are pure-int compares (was variant-name string compares /
   `ty==ty && variant==variant`). Native `Ok`/`Err`/`Some`/`None` hold the **reserved** fixed ids
   `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3); user variants follow at `4..`, so the
   reserved range is **disjoint** from every user id. `?`/top-level-error gate on the constants, and the
   native construction path (`alloc_enum`) stamps the constant **directly** (never a `variants[name]`
   lookup) — so a user enum may shadow a native name (`enum Foo: Some(int)`) without a genuine native
   Option/Result ever being stamped with the user's id (was a parity bug: name-resolved native
   construction collapsed identity + broke `?`; fixed 2026-06-16). `Op::NewEnum`/`Op::MatchArm` carry the
   compile-time id; wire/snap carry the dense `variant_id` **directly** (shared `Arc<Program>` ⇒
   meaningful both sides; preserves identity under shadowing). **−20% (1.25×)** on an enum
   construct+match-dispatch micro (`benches/chz/enum.chz`),
   suite-neutral; `Obj::Enum` shrank 56→32 B (Module still caps `Obj` at 88 B). Hands the JIT a numeric
   variant id → constant/jump-table dispatch. See `docs/benchmarks.md`.
3. **✅ DONE — Closure captures: positional `Vec`, not a per-closure `HashMap`.** Was `Obj::Closure
   { captured: HashMap<String, Value>, .. }` (a `HashMap` ~48 B + string keys **per closure** + a
   string hash on every `GetCaptured`). Now `captured: Vec<Value>` indexed by a compile-time slot;
   `Op::GetCaptured(u32)` is a hash-free `captured[slot]` read; capture names live in
   `Proto.capture_names` (cold path: error fallback + wire/snap name carrying). Nested captures map by
   `CapSrc::Captured(parent_slot)`. **−45% (1.83×)** on a closure construct+capture-read micro
   (`benches/chz/closure.chz`), suite-neutral; `Obj::Closure` shrank 88→64 B (Module still caps `Obj`
   at 88 B). Hands the JIT a constant capture offset. See `docs/benchmarks.md`.
3b. **✅ DONE — Inline small struct `fields` (no per-struct second malloc).** Was `Obj::Struct {
   fields: Vec<Value> }` — a SEPARATE heap malloc per instance (2M structs = 2M small buffers, ~61MB
   RSS on `benches/chz/many_struct.chz`). Now `fields: Fields`, a hand-rolled enum in `heap.rs`:
   `Inline { len: u8, vals: [Value; 3] }` folds ≤3 fields (the vast majority) into the 64B `Obj` slot —
   **zero second malloc** — while `>3` spill to `Spill(Box<[Value]>)` (exact-length, no `Vec` capacity
   slack). Fields are FIXED at construction (positional hidden-class layout, item 1; no growth sites), so
   inline-or-spill with no capacity is safe. No new dep (no `smallvec`); `Obj` stays 64B
   (`size_of::<Fields>() == 32`, guard-pinned). `Fields` gives a `Vec`-compatible surface
   (`from_vec`/`len`/`as_slice`/`as_mut_slice`/`iter`/`get`/`get_mut`/`heap_bytes` + `Index`/`IndexMut`)
   so the ~16 touch sites + the field-IC hot paths + in-place `s.a = s.b` writes stay byte-identical.
   Mechanical VM-only (checker never names `Obj`); GC `children()` traces every field, `live_bytes`
   counts `Spill` backing only. RSS delta measured post-merge. See `docs/benchmarks.md`.

**Heap-slot layout (GC-side; principled, low priority — GC moves no bench):**

4. **✅ DONE — Separate the mark bit from the object.** Was `Slot { obj: Option<Obj>, mark: bool }` —
   the `mark: bool` padded the slot to **72 B** (`Option<Obj>` is already 64 B: `Obj`'s spare-discriminant
   niche makes `None` free), so the mark bit cost 8 B of padding on every slot. Now
   `Slot { obj: Option<Obj> }` (exactly 64 B, guard-pinned `size_of::<Slot>() == 64`) + a dense parallel
   `marks: Vec<u64>` bitset on `Heap` (bit `i&63` of word `i>>6`), grown in lockstep with `slots` at the
   new-slot alloc arm. Three one-line helpers `is_marked`/`set_mark`/`clear_mark`; `mark()`/`sweep()`
   rewired to the bitset, EXACT current mark-then-sweep-and-clear protocol (survivors cleared in the
   sweep pass, holes never marked → post-sweep all bits 0). VM/GC-internal, no observable/checker change;
   all GC-stress rooting + two-engine parity green. Saves the 8 B mark padding per slot (≈16 MB on the 2M
   `many_struct` bench); the mark test-and-set also touches a compact word rather than a scattered slot
   byte. (Sweep still scans every slot's `obj` — the bitset does not avoid that.) RSS delta measured
   post-merge. See `docs/benchmarks.md`.
5. **Shrink `Obj` below 64 B.** ✅ DONE for `Module`: boxed to `Box<ModuleData>`, so
   `size_of::<Obj>()` dropped 88→**64 B** (guard, `chzstr.rs:205` / `heap.rs`); `MapData`/`SetData`
   (56 B payload + 8 B discriminant) now cap it. To go below 64 B you'd have to box `MapData`/`SetData`
   too — **but SSO deliberately sized strings to fill the inline `Obj`**, so shrinking un-inlines them.
   Net is a trade-off → measure first.

**Hot-loop access (mostly already addressed or parity-blocked — cross-ref):**

6. **HOF borrow-release clone (new finding).** List `map`/`filter`/`fold` do `self.heap.get(h).clone()`
   to release the heap borrow before `invoke_value(&mut self, …)` — an N×16 B copy per HOF call. A `Vm`
   split (`&mut ExecState` + `&Heap`) lets the borrow coexist. Structural refactor, not a one-session lever.
7. **`for`-loop snapshot (`ListClone`) + per-char alloc** — mandated by the for-loop's observable
   snapshot semantics (identical on both engines); `alloc_char` (Phase 3) already halved the string case. Behavior-blocked.
8. **Operand-stack 16 B/Value traffic** → **DONE 2026-07-18: 8B pointer-tag Value shipped** (NaN-box was the wrong scheme; see the dedicated note above) / register VM (#8, low-ROI).

**Land order:** **#1 ✅ → #3 ✅ → #2 ✅ — sequence complete** as **JIT groundwork** (the positional
layouts the JIT codegen wants), each measured against `struct`/`hof`/`enum` (read suite-neutral — they're
dispatch-bound, see caveat — with strong micro deltas: #1 −38%, #3 −45%, #2 −20%).
#4 ✅ done (mark bit → parallel bitset, `Slot` 72→64 B). #5/#6 are principled cleanups, post-JIT. Same discipline throughout: failing-then-green parity test →
keep two-engine parity → measure (`benches/run.chz`) → record the delta in `docs/benchmarks.md`.

**Highest payoff-per-effort (original M19 batch, all landed):** superinstructions + inline caching +
peephole/const-fold. They attacked dispatch count and name lookup — the two actual costs — without
touching the value model or the GC.

### AtomicInt — lock-free primitive int atomic (LANDED 2026-07-22)

`Atomic[T]` is generic over ANY sendable T (`store`/`exchange`/`cas` hold arbitrary values; `add`/`sub`
numeric only). A lock-free fast path that picks `AtomicI64` backing from the **runtime** init value is
UNSOUND — `x: Any = 5; a := Atomic(x); a.store("hello")` type-checks (`str` <: `Any`) then faults on an
int-only cell, where the old Mutex cell held the string fine. (Tried + REJECTED 2026-07-22 after 2
remediation rounds, branch discarded — the `checker-superset-of-compiler` class: the VM is **type-blind**
at construction, so it can't see the static element type is wider than the runtime int.)

**The fix is a NEW dedicated type, not a patch to `Atomic[T]`.** Add **`AtomicInt`** — statically,
monomorphically int (no `[T]`, nothing to widen), so it can ALWAYS be a lock-free
`std::sync::atomic::AtomicI64`: no runtime type-sniffing, no wider-T hole, the `Atomic[Any]` trap can't
be written. This is the mainstream design — Rust `AtomicI64`, Java `AtomicInteger`, Go `atomic.Int64`,
each distinct from the generic reference cell (`AtomicReference<T>` / `atomic.Value`). `Atomic[T]` stays
exactly as-is (Mutex, general) — **zero regression, purely additive**.

Sketch (additive-native-type pattern, ~3 touchpoints — see the `native-types-first-class-additive-pattern`
memory / `regex.Match` as the template): new `Obj::AtomicInt(AtomicI64)` + ctor `AtomicInt(0)`; checker
registers `AtomicInt` as a reserved `std.concurrency` type with int-only `load/store/add/sub/cas/exchange`
and gates the bare name behind `import std.concurrency`; runtime method dispatch on the new Obj.
**CRITICAL:** `add`/`sub` MUST keep the i64-overflow FAULT via a `compare_exchange` CHECKED CAS-loop —
NOT raw `fetch_add`/`fetch_sub` (they wrap silently = behavior regression). `SeqCst` ordering everywhere
(matches the sequential consistency the Mutex gave → serial==M:N byte-identical). Tests on BOTH engines:
overflow still faults; a high-contention `parallel:` counter (N tasks × M `add(1)` == N*M); serial==M:N
and `--check-parity` green. Perf: the discarded generic attempt measured **1.85× under contention**
(uncontended ~flat); NOT on the M19 `fib`/`loop`/`primes` benches → gate on a **contention microbench**,
record in `docs/benchmarks.md`, and if no measurable win SAY SO.

**LANDED 2026-07-22** (additive, `Atomic[T]` untouched). Shipped exactly per the sketch: unit `Ty::AtomicInt`
+ `Obj::AtomicInt(Arc<AtomicIntCore{v: AtomicI64}>)` + `Op::NewAtomicInt`, mirroring the four reserved
`std.concurrency` names at every checker/VM site; `native struct AtomicInt` (no `[T]`) in
`std/concurrency.chz` harvests the concrete int method table. `add`/`sub` use a checked `compare_exchange`
CAS-loop (keeps the i64-overflow fault, byte-identical `"integer overflow in Add/Sub"`), `SeqCst` on every
op. **Perf: the contention win materialised — ~2.7× faster than Mutex-backed `Atomic` on an 8-way int
counter** (16M adds: 1.73s vs 4.73s median; uncontended a wash), recorded in `docs/benchmarks.md §AtomicInt`.
Tests (both engines): roundtrip, add/sub overflow fault, 8×10000 contention counter == 80000, `import
AtomicInt from std.concurrency` runs (reserved-name hole closed), bare-unlicensed = checker error.

### Post-M19 next levers (ranked — diagnosed 2026-06-12; **status updated 2026-06-13**)

> **Status (2026-06-13):** Tier 1 is DONE — #1 method-IC (Phase 6) and #2 inline-hot-ops (Phase 7)
> landed; #3 `Op::Call` spec was analyzed and **deferred (no-gain after the Phase 7 inline)**. Tier 2 is
> underway — #4 adaptive quickening **v1 (binops) landed**, #5 **index specialization landed**
> (`GetIndex`/`SetIndex` Int-key fast path), and the #4 *CallMethod* extension **landed (2026-06-13):
> N-way polymorphic method-call IC + sticky-deopt + clone-free megamorphic slow path, `poly_method`
> −33% (6.0× → 4.28× CPython)** — this unifies the field/method caches under one adaptive form.
> **Genuinely remaining:** the **denser int-keyed `map`** representation **also landed (2026-06-13,
> `map` 2.68× → 1.94× CPython, −26% on merged HEAD)**, so what's left is the Tier-3 milestones (#6 JIT / #7 8B-Value **DONE** (pointer-tag) / #8 register
> VM). Per-lever tags below; landed details + measured deltas in `PROGRESS.md` "Current focus" and
> `docs/benchmarks.md`.

The M19 cheap batch + call-flatten + SSO are spent. Latest gap tracks **call density**: `loop` (no
calls) **1.32×**, `primes` 2.53×, `str` 2.65×, `struct` 2.71×, `map` 2.83×, `list` 2.97×, `fib` (all
calls) **3.54×**; startup **0.094× (11× win)**. The bottleneck is **call overhead + per-op dispatch +
a few alloc paths — NOT the value model or the GC** (confirmed: `loop` is already at the match-dispatch
floor; ints are unboxed; GC is share-nothing per-thread and moves no bench). Target is **CPython 3.14**
(specializing adaptive interpreter + optional copy-and-patch JIT), so the interpreter can *narrow* the
gap but a JIT is the only path to *match/beat* it on tight compute.

**Tier 1 — interpreter, cheap→medium, behavior-preserving, each hits a measured bench:**

1. **✅ DONE (Phase 6, 2026-06-13).** **Method-call IC + flatten `do_method_call`** *(hit `struct` −9%)*. `do_method_call`
   (`mod.rs:~3868`) still string-looks-up `def.methods.get(method)` per call **and** still recurses into
   a fresh `run_until` — call-flatten only covered `do_call`'s plain-fn fast path (see its own follow-up
   note). Add a per-call-site monomorphic cache (`tid → proto`, the same shape as the landed `field_ic`)
   and push the method frame in place. Symmetric to the field IC; reuses that machinery.
2. **✅ DONE (Phase 7, 2026-06-13) — landed as "inline hot ops"** *(moved every op-bound bench: `loop` −15%, `list` −17%, `primes` −8%, `fib` −6%)*. The inline-the-hottest-ops sub-lever shipped; the other two below (lazy `span`, serial/MN loop split) were left **unshipped** (predictably-false cheap branches, low payoff vs the inline win — revisit only if a profile shows them). **Trim per-op overhead in `run_until`** — three things run
   **every instruction** that are pure overhead on the serial (default, benchmarked) engine:
   - `span = proto_ref.lines[ip]` (`mod.rs:2157`) is loaded every op but used **only on fault** → pass
     `(pid, ip)` to the error path and reconstruct the span lazily there.
   - the `if self.mn.is_some()` reduction-count branch (`mod.rs:2137`) + the cancel check (`mod.rs:2122`)
     are MN-only → split a lean serial loop body from the MN body (or hoist them off the serial back-edge).
   - `self.step(op, span)` is a **separate fn call per opcode** → inline the ~6 hottest ops (GetLocal, the
     superinstrs, Jump, Call, Return) directly in the loop, delegate the long tail to `step`.
3. **⏸️ DEFERRED — no-gain (Phase 8 analysis, 2026-06-13).** **Call-site specialization for `Op::Call`** *(was aimed at `fib`)*. After the Phase 7 inline, `do_call`'s happy path is already lean (the deref a call-IC skips is ~2–3 instrs); fib's residual is frame-setup in `finish_frame`, which a dispatch cache doesn't touch — and a correct call-IC can't avoid a heap-specific callee handle ⇒ `swap_ctx` hazard for ~0 gain. fib's real lever is #4/#6. Each call re-checks Func/Closure/
   Native, derefs the heap callee, and re-validates arity (CPython `CALL_PY_EXACT_ARGS`); full rationale in `docs/benchmarks.md`.

**Tier 2 — structural, medium→large:**

4. **✅ v1 + CallMethod extension LANDED (2026-06-13).** **Adaptive opcode quickening (PEP 659)** *(the single most CPython-like lever)*. v1 specializes the un-fused generic binop arms (`Add..GtEq`, `Eq`/`NotEq`) to an int/int fast path behind a per-`Vm`, per-site `(proto,ip)` deopt guard (side table `quicken`/`quicken_base`, mirrors `field_ic`/`method_ic` — no `Op`/compiler/interp change ⇒ parity by construction). Measured **`primes` −7–8%**. **CallMethod extension (done):** the method-call IC's single `MethodIcCell` is widened to an N-way (4-way) `MethodIcSite` carrying the *same* one-way sticky-deopt discipline — a bounded-megamorphic site (≤4 receiver types) HITS a way per type and flattens; a 5th distinct type latches `sticky` and goes slow (now clone-free: borrows `Arc<Program>.structs` instead of cloning the whole `StructDef`). This **unifies** the field+method caches under one adaptive form (`GetIndex` is already covered by #5). Measured **`poly_method` −33% (6.0× → 4.28× CPython)** on a new megamorphic bench; side table still int-only (no `GcRef`) ⇒ parity by construction. After an op runs once, rewrite-in-place to a type-specialized form behind a deopt guard. **Constraint
   (same one P2b/P4 hit):** bytecode is shared `Arc<Program>` read-only across `--parallel` workers, so
   quickened cells must live in a per-`Vm` side table keyed by site, not mutate the `Op`.
5. **✅ DONE (2026-06-12).** **map/list index specialization** *(`list` −4%; `map` neutral — it's FxHashMap-probe-bound, not dispatch-bound, so the predicted `map` win needs a **denser int-keyed map** representation, a separate lever, not this tweak)*. `GetIndex`/`SetIndex`
   got an Int-key fast path (skips `hash_key_rooted` rooting) + inline dispatch in the `run_until` hot arm; 7 `idxspec_*` parity guards.

**Tier 3 — big, separate milestones:**

6. **Cranelift method-JIT** — the only path to *match/beat* CPython 3.14 on compute. Counter-triggered,
   JIT the hot protos (Python's tier-2 model). End-game; only once the language is fully frozen. #4 is the
   lower-risk stepping stone toward it.
7. **8B `Value` — DONE 2026-07-18** via int-favoring pointer-tag (NOT NaN-box, which stays blocked by full i64). `loop`→1.03× CPython, `fib`→2.95×; see the dedicated note + `docs/benchmarks.md`.
8. **Register VM / generational+incremental GC — low ROI** (dispatch is already near the match floor; GC
   moves no bench). Deprioritized; revisit only if a real workload proves otherwise.

**Sequencing (updated 2026-06-13):** Tier 1 is **done** (#1, #2 landed; #3 deferred), and Tier 2 is
**done** — #4 (v1 binops **and** the `CallMethod` N-way extension) + #5 (index spec **and** the denser
int-keyed `map`) all landed. With both the `CallMethod` adaptive quickening and the denser `map`
shipped, the high-ceiling play left is **#6 (Cranelift method-JIT)** as the JIT end-game (#7 8B-Value shipped 2026-07-18 via pointer-tag; NaN-box stays
blocked; #8 register VM / gen-GC stays low-ROI). All steps: behavior-preserving, two-engine-parity-clean,
measure-first, each targeting a named bench.

**M19 Phase 1 done (2026-06-11):** peephole/const-fold + superinstructions + `invoke_value` clone
kill — all behavior-preserving (1516 tests + full two-engine parity green). Results in
`docs/benchmarks.md`.

**M19 Phase 2 done (2026-06-11):** in-place call args in `do_call` (per-call `Vec` gone, `fib` −13%)
+ `stringify`-into-buffer for `BuildStr` (`str` −5%) — both behavior-preserving (1518 tests + full
two-engine parity green, 4-agent S++ panel clean). Results in `docs/benchmarks.md`. Remaining `str`
lever is `ConstStr` interning; the next dispatch win is inline caching (Phase 2b, below).

### M19 Phase 2b — inline caching via global-slotting (✅ landed 2026-06-11)

Landed as designed below. Net: `fib` −9%; other microbenches flat (their hot loops are local-bound).
Implementation notes vs the plan: `Module` became `{ slots: Vec<Value>, index: HashMap<Box<str>,u32> }`
(the `index` is kept alongside the `Vec`, not discarded — it backs `module.member`/imports/native
population, and reverse-iterating it in slot order via `module_slot_pairs` is how the snapshot stays
deterministic); the old name-keyed ops were *replaced*, not kept. The historical design note follows.

The next dispatch win, deliberately split out from Phase 2 because it is **not** a local opcode
tweak. Today `Op::GetGlobal(String)` does a `HashMap<String,Value>` probe by name every read
(`mod.rs` `module_global`). A monomorphic IC would cache the resolved location, but two facts block
the naive "cache in the opcode" approach:

1. **Bytecode is shared read-only across threads.** Under `--parallel` the `Program` is an
   `Arc<Program>` and every worker fiber reads the *same* `Op` slices — so an opcode cannot carry a
   per-site mutable cache cell without synchronization.
2. **Globals are name-keyed, not slotted.** `Obj::Module { globals: HashMap<String,Value> }` has no
   stable index to cache.

**Plan:** resolve globals to slots at compile time. The compiler assigns each module global a stable
`u32` slot and emits `GetGlobalSlot(u32)` / `SetGlobalSlot(u32)` / `DefineGlobalSlot(u32)`;
`Module.globals` becomes a `Vec<Value>` (name→slot map kept only for `module.member` field reads and
error messages). The read becomes a bounds-checked `Vec` index — no hashing, no string.

**The concurrency constraint (the reason it is its own milestone):** the lazy module-fault path
(`ensure_module_faulted` / `fault_module` / the worker module snapshot) reconstructs a worker's home
module on first access. Slot order must be **identical** between the parent's compiled module and any
faulted worker copy, or a worker reads the wrong global. The snapshot (`to_snap`/`replay_snap`) and
`ModuleInline`/`ModuleAlias` replay must round-trip slots, not names. This needs its own two-engine
parity pass + the `--parallel` module-fault tests, so it is scheduled separately rather than bundled
with the Phase 2 allocation kills.
