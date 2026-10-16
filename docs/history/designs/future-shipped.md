> Source: `docs/design/roadmap.md` (was `docs/future.md`) lines 1-976 (§1-§3), moved verbatim 2026-10-16. The open items now live in `docs/design/roadmap.md`; the record continues in `future-shipped-2.md`.

# Chezzi — Future Directions (brainstorm, NOT scheduled)

> **Status:** speculative design notes. Forward-looking and opinionated. Nothing here is committed
> work — [`PROGRESS.md`](../../PROGRESS.md) is the source of truth for what's actually scheduled and done.
> This doc captures *what would make Chezzi an effective scripting language* and *how to make it
> faster*, with verdicts and rough implementation shape. Most of §1–§3 has since **shipped** (noted
> inline); §4 (optimizations) is the live M19 backlog.

The language **core** is broadly implemented and still evolving (scalars, `list`/`map`/`set`/`tuple`, generic structs +
enums, `T!E`/`T?` + `?`, generics + structural protocols, exhaustive `match`, closures/HOF,
modules, GC, interpolation, pipe, panic recovery via `recover:`, the `Iterator[T]`
protocol bound). What follows is the gap between "core implemented" and "language you reach for to write
real scripts."

---

> **Promotion status:** §1 (`defer`) and the §3 scripting features have **shipped** (M15–M18); §2
> (concurrency) has **shipped through Tier-D**. They stay documented here for the design rationale.
> §4 (optimizations) is the live M19 backlog. See [`PROGRESS.md`](../../PROGRESS.md) for landing detail.

## 1. `defer` (cleanup on scope exit) — ✅ **SHIPPED (M17)**, **block-scoped since M18** — see `examples/defer.chz`

> **M18 update:** shipped frame-scoped in M17, then moved to **block/lexical scope** — a `defer` runs
> when its enclosing indented block exits (loop body, branch, `recover:`, `match` arm, function body,
> module top level), not just at function return. Realises the "cleanup on scope exit" intent below
> more literally. See the M18 entry in `PROGRESS.md` and the `defer` section of `docs/reference/syntax.md`.

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
> `Shared[T]`, sendability — lives in its own canonical doc **[`docs/reference/concurrency.md`](../reference/concurrency.md)**,
> with phase history in [`docs/history/designs/concurrency-tier-d.md`](../history/designs/concurrency-tier-d.md). Real OS-thread M:N
> engine via `--parallel`. **M-C implicit nurseries shipped** — bare `spawn` legal anywhere; every
> function body / module top level joins its tasks at `return`/end. Concurrency is feature-complete.

---

## 2b. Retire the serial engine + rebuild the oracle layer — ✅ **THE REMOVAL SHIPPED 2026-08-16**, the oracle layer is **partly built** (three of four)

> **Decision (2026-07-22), executed 2026-08-16.** The cooperative `--serial` engine was **not the model
> Chezzi ships** — the real runtime is M:N (and, post-JIT, a JIT'd M:N). `--serial` existed only as the
> byte-identical **parity oracle**, and it is now **deleted**: the flag, the `--check-parity` assertion,
> the cooperative scheduler, and every per-engine fork.
>
> **It shipped PRE-freeze, not post-freeze.** The original decision deferred it past the JIT freeze;
> that ordering was reversed once the removal turned out to be the *cheaper* of the two, and doing it
> first means the JIT is not designed against a byte-identity constraint that was already scheduled to
> die. Nothing about the JIT plan depended on the deferral.
>
> **This section stays in the file because two of the four replacement oracles are still UNBUILT.** Do
> not read the removal as "the oracle layer is done" — see the status column in the table below.

**Why it was removed (two independent reasons):**
1. **It can't truly test concurrency.** The serial engine is single-threaded and cooperative — it cannot
   preempt a running fiber (`docs/contributing/gaps.md` **N8**), so a whole class of concurrent programs either hangs
   (CPU-bound sibling) or takes a different schedule than M:N and diverges (**N9**, **N10**). As a
   concurrency oracle it only ever compared *one* cooperative schedule against *one* M:N schedule — a weak
   differential that misses interleaving-dependent races entirely.
2. **Keeping it byte-identical to M:N is accumulating technical debt.** Byte-identity forces the M:N engine
   to bend to what a single cooperative thread can reproduce. The code has already had to **split the two
   engines** at multiple sites (`op_wait_poll` timer-arm handling, the module-global snapshot path, the
   Executor drain) with per-engine branches whose *only* purpose is to keep serial matching M:N. Post-JIT,
   a JIT'd M:N can't be byte-identical to a tree-walking cooperative loop at all, so this debt only grows.

**The replacement oracle layer** (each covers a class serial did, better and without the byte-identity
tax). **Three of the four are built; one is not** — and that gap is why this section is still here:

| Bug class | Replacement oracle | Status |
|---|---|---|
| Sequential shared wrongness (both engines agree, both wrong) | **CPython differential** — `src/difftest/`, `tests/difftest.rs` gate + `src/bin/difffuzz` long-runner | ✅ **BUILT** (pre-dates the removal; untouched by it) |
| Accidental *schedule*-dependent divergence — the detector job serial actually did | **Two worker-count differential** — `tests/chezzi_threads_cli.rs` runs the whole `tests/chz` suite through the built binary at the default worker count and again at `CHEZZI_THREADS=2`, each in its own process/pool, and asserts the same verdicts | ✅ **BUILT 2026-08-16** (task 5). Not in the original plan; it is what actually replaced `serial == M:N` as the standing `cargo test` differential. Narrower than what serial covered: a *curated* corpus at *two* worker counts, not arbitrary programs. |
| Channel/select semantic wrongness (Chezzi's model vs the reference) | **Go paired-programs differential** — restricted to programs with a *deterministic outcome* despite nondeterministic scheduling (sort output lines where only order varies). Go's channels/`select`/close/backpressure map ~1:1 to `Channel[T]`/`wait:`. Catches *shared* wrongness serial couldn't (identical bytecode both engines). Sweet spot only — Go has **no** equivalent for the airlock or structured-nursery semantics. | ❌ **NOT BUILT.** Still planned. Nothing in the repo generates or runs Go programs. |
| Scheduler races / lost-wakeups | **Seeded / deterministic-interleaving mode for M:N** (the `loom`/`shuttle` pattern): explore many schedules from a seed, replay on failure. This — not an external language — is the real replacement for serial's race-finding job; a reference language's scheduler is *also* nondeterministic, so diffing two nondeterministic schedulers catches nothing. | ✅ **BUILT 2026-09-23 (TICKET-167)**. `CHEZZI_SCHED_SEED=<u64>` (`src/vm/sched_seed.rs`) seeds every scheduler free choice: which runnable fiber a pop returns (`LocalQ::pop`, the three `global.pop_front()` sites), the step-0/`GLOBAL_CHECK_INTERVAL` cadence, `try_steal`'s start, `handoff_wake`'s `runnext` coin flip, and the reds-refill count; at `worker_count() >= 2` it also drives random yield/spin/sleep perturbation at every sync point (`sched_seed_point`/`sched_seed_perturb`). **Covers:** T=1 replay AT A MEASURED RATE (not byte-for-byte — see below), T>=2 perturbation that widens real thread-timing races, a `src/schedfuzz/` + `src/bin/schedfuzz` sweep harness over `tests/chz` + concurrency `examples/*.chz`. **Does not cover:** byte-for-byte T=1 replay of a top-level `parallel:` fan-out (W15-2, the body racing its `chezzi-eager` drainer as two ungated CPU runners, is fixed 2026-09-23, TICKET-168; **W15-10**'s two residual races — the drainer's pick running before its width-permit acquire, and a woken body queuing only once its own thread runs — still block byte-for-byte replay); replay of timers, sockets, blocking natives or eager-nursery programs (only the flat/nested in-nursery fan-out fixtures are measured); exhaustive loom-style schedule exploration (random perturbation, not enumeration). Mutation-tested against 3 historical fixes: reliably catches W15-1 (TICKET-166) only at the DEFAULT worker count, not T=1/T=2 (0/256 seeded, matching 0/64 unseeded at those counts); reliably catches W14-39 (TICKET-135) at T=1 (217/256) and T=2 (78/256), though at both counts the SEEDED rate is below the raw UNSEEDED rate, so seeding replays a jitter-found failure rather than widening past it; did not catch the TICKET-128 mutation in 256 seeds at any count on release (found only on debug). Full mutation table, replay-rate data and the T=1 fan-out limit: `docs/contributing/bug-discovery.md`. The two-worker-count gate above stays as the accidental-divergence stand-in for what this mode does not reach (timers/sockets/eager nurseries). |
| Airlock / structured-concurrency semantics (no external equivalent) | Hand-written **known-answer** tests | ✅ the existing hand-written Rust + `tests/chz` tests already are this; no new mechanism was needed |

**Every oracle in that table compares RAW BYTES.** `String::from_utf8_lossy` is not injective (`ff`
and `fe` both become one U+FFFD), so a decoded compare reports agreement for a run whose two sides put
different bytes on fd 1 — and both Chezzi (`io.stdout().write_bytes`, byte-exact since W6-9) and the
reference languages (CPython `sys.stdout.buffer.write`, Go `os.Stdout.Write`, both measured `ff fe`)
can emit non-UTF-8. This has already been the live shape of three separate holes (W6-9, W6-9b, and the
CPython differential itself — `gaps.md` W7-30), so the Go paired-programs differential must be born
with it: capture `Vec<u8>`, keep the decode for *display and text heuristics only*, never for a verdict.

**Net, and stated honestly:** CPython (sequential) + two-worker-count (accidental schedule divergence)
+ known-answer (airlock) + seeded-M:N (races, TICKET-167) are live today. Only Go (channel/select
semantics) is **not**, so the *race-finding* half of what serial did is now covered by an explored
seed space over the curated corpus, not just two fixed schedules — the seeded oracle already turned
up one new race on `main` in its first sweep (W15-3, `docs/contributing/gaps.md`), and it needed the seed to find
it: 0/10 unseeded at the same worker count (`CHEZZI_THREADS=1`). Seeding buys a MEASURED-RATE replay
at T=1 only — never at T>=2, where the seed drives perturbation but real OS-thread timing still
decides the outcome, so a T>=2 seed is not byte-for-byte replayable either (see the table row above
and `docs/contributing/bug-discovery.md`). None of the live oracles constrain the M:N engine's design, which was
the point.

**Migration mechanics — DONE 2026-08-16, in this order:** every bare `assert_parity(src)` site was
first given a real, run-derived golden (so nothing lost its expectation when the second engine went
away); then every test helper collapsed onto M:N (`run_file_p`/`parallel_outcome`/`run_capture_parallel`
and friends deleted, `parity_entry_cfg_lines` reduced to a single run); then `--serial` and
`--check-parity` dropped from the CLI; then the cooperative scheduler itself, with every serial-only
fork, field and signature parameter. `src/vm/parity_tests.rs` and its `assert_parity*`/`parity_entry*`
helpers were renamed to `src/vm/golden_tests.rs` / `assert_golden_out`+`golden_entry*` once the doc
sweep that shipped this removal flagged the old names as a leftover — every helper in it runs the one
engine once and compares against a literal expectation. The `--threads=1` M:N mode (kernel-preempted,
safe single-thread) covers every legitimate user need `--serial` was ever mistaken for — **measured, not
assumed, as of 2026-08-18**: when this was written `--threads=1` silently ran two CPU runners (`W8-8`),
and on the binary that fixed it the N8 shape (a pure-CPU spinner beside a faulting sibling — the exact
program `--serial` hung on) was re-run 15× → **15/15 faulted in 4–6 ms, 0 hangs**, at one genuine
runner.

**One deliberate exception to "drop the per-engine forks":** `op_wait_poll`'s live-timer **inline-sleep**
block was **KEPT**. It is not serial-only — it is still reachable on M:N by the inline
outermost-`parallel:` builder (`mn == None`, `mn_enlist_sched == Some`; `src/vm/sched.rs:527`), which
owns no worker loop to drive a park. That surviving arm still sleeps to the deadline and takes the
timer arm **without re-polling siblings**. See `docs/contributing/gaps.md` **N10** for exactly what that does and
does not guarantee.

---

## 2c. `Executor` moves to eager execution — ✅ **SHIPPED 2026-08-03**

> **Reading note (added 2026-08-16).** This section is a **dated design record** of the 2026-08-03
> decision, kept as written. Its present-tense statements about `--serial` describe the engine as it
> was then; that engine was **removed 2026-08-16** (§2b), so decision **D3** (queue-at-`submit` /
> drain-at-`shutdown`) now has no execution path at all — every `submit` is eager. The `inner` queue
> field survives in the code but is provably always empty.

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
- **D2 — Concurrency: shared process pool, no size parameter.** Shipped; since TICKET-213 `Executor(n)` caps the jobs running at once (`Executor()` has no cap).
  **Accepted known limits:** the old parent-participation mitigation is gone with the batch join.
  TICKET-052 (2026-09) closed the starvation limit this row used to accept: a job blocking on a
  `Channel`, a `Shared`/`RwShared` guard, `time.sleep_ms`/a timer, or a nested `Executor` join now
  yields its pool thread to a freshly spawned replacement worker and retires when the job ends —
  including a job that calls `shutdown()` on an executor (its own, or another), so a self-join no
  longer hangs a starved pool either. The cost is one extra OS thread per job concurrently blocked
  this way; an OS-refused replacement degrades to the old in-place block rather than faulting the job.
  `Executor(max_workers)` stays an additive door if it bites.
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
  scheduler state is `MnSched::{runnable, inflight}` + `SchedCore::{parked_n, waiters}`, and the
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
`Executor` drain was their last caller); A2/C5 prose in `docs/reference/concurrency.md` and the module-snapshot
instant at §"module globals" — the airlock crossing point moves from drain time to **submit** time.

**Reframing:** the `Executor` is a bounded-concurrency nursery with a detached lifetime — the model the
ancestors (Python/Java) already have.

---

## 2c1. `spawn` moves to eager execution — ✅ **SHIPPED 2026-08-14**

> **Reading note (added 2026-08-16).** Same as §2c: a **dated design record**. The "documented engine
> split" it describes (eager on M:N, lazy on `--serial`) no longer exists — `--serial` was removed
> 2026-08-16 (§2b), so only the eager half remains, and `docs/reference/stdlib.md` / `docs/reference/concurrency.md` have
> been updated to state the single behaviour.

> **Numbering.** This is §2c's sequel and sits next to it deliberately. It is **not** `§2d`: that number
> is already the deadlock-detection milestone below and is cross-referenced from `src/vm/{sched,netio,mod,
> quiesce,tests}.rs`, `PROGRESS.md`, `docs/contributing/gaps.md`, `docs/reference/stdlib.md` and `docs/reference/concurrency.md`.
> Renumbering would break ~20 live pointers to buy nothing. `3a1` is the same house convention.

> **Why this was filed, and closed the same day (2026-08-14).** A spawned task did not start at its
> `spawn` — it started at its nursery's **join**. Go, which owns the concurrency seam, starts at the
> `go`. Nothing in the docs ever claimed the Chezzi behaviour: `docs/reference/concurrency.md`'s nursery-scoping
> bullet defined the implicit nursery **only** by where a bare `spawn` *joins* and said nothing about
> when a task *starts*. So it was a divergence from the owning ancestor with no documented decision
> behind it — a defect, not a note. `docs/contributing/gaps.md`'s `bare spawn: start time` row (`:82`) is closed and
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
shipped eager on M:N and lazy on `--serial` (decision D3), documented in `docs/reference/stdlib.md` as the one
place the two engines deliberately disagree. The same reasoning holds here, only harder: `--serial` is
single-threaded and cooperative, so Go's semantics are **unreachable on it by construction**, and it is
slated for removal in §2b. Therefore:

- **eager on M:N** (the engine Chezzi actually ships),
- **documented lazy on `--serial`**,
- **the parity harness splits for the affected programs** rather than the M:N engine bending to what one
  cooperative thread can reproduce. That is §2b's own stated debt argument — byte-identity is already
  forcing per-engine forks whose only purpose is keeping serial matching M:N; this adds one more rather
  than paying for it in wrong M:N behaviour.

`docs/reference/stdlib.md`'s "one place" is now **two**, reworded in the same commit; `docs/reference/concurrency.md` §4
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
   `run_mn_nursery_outermost` and build a *second* outermost sched beside the
   live one — reintroducing the `gaps.md` B5 cross-sched wake bug. (`parent_wake`, the single-ancestor
   pointer this point originally named, was replaced by the run-wide `MnSched::wake_run_wide` in
   TICKET-099 — the shape of the bug this point describes is unchanged.)
2. **`Vm::activate_eager_nursery` (`src/vm/sched.rs`) gained a no-parent form.** Publishing a sched with
   no ancestor to wake was correct at the top level (that sched *is* the outermost scheduler; there is
   nobody above it to wake) — TICKET-099's `wake_run_wide` walks the run's `Vm::sched_registry`
   directly rather than an ancestor pointer, so this point is now moot by construction, not by a `None`
   value, but the top-level sched still has nothing to wake beneath it.
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
   `send_wake` scanned its own sched and then `wake_parent_chain`, which was strictly *upward* — there
   was no sideways or downward path (`wake_parent_chain` was replaced by the run-wide
   `MnSched::wake_run_wide` in TICKET-099, which closed exactly this gap for the eager-private-sched
   case too). One sched with one scope per nursery is the cross-nursery flat
   scheduler that already existed for the lazy path; eager start simply has to keep using it.

   Two consequences that had to move with it: `poller::drain_scope` (a nested escape may only unpark
   ITS scope's socket-parked fibers — selecting by sched was scope-selective for free only while every
   nursery owned one), and the `worker_count() >= 2` clause of the `EnterNursery` gate, which is
   RESTORED for `mn.is_some()` — see item 8.

7. **The blocked body's own wait** (today a `fiber: false` `Waiter` in `SchedCore::waiters`, `src/vm/mod.rs`) — published on every
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
   - **It carries the WAIT, not the channel.** Reusing the then-existing demoted-channel peek — which
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
   also silently broke the user-facing flag. (**Re-measured 2026-08-18** on the post-`W8-8` binary, where
   `--threads=1` is genuinely one runner: the same shape peaks at **4 live OS threads**, two independent
   samplers, two runs — still bounded and flat, nowhere near 130. The original "3" has no surviving repro
   program in the tree, so the one-thread difference is methodology, not a regression; the bound this gate
   exists to hold is intact — but only at `N == 1`.) A top-level nursery has no outer worker to starve and
   creates exactly one drainer per thread, so it stays unconditional.

   **Correction (TICKET-073, 2026-09-08): this clause held the bound only at `N == 1`, not at every
   `N`.** The 2026-08-18 re-measurement sampled `--threads=1` alone, which is why the bound read as
   intact — at higher `N` it was not: the same depth-7, 128-leaf tree reaches **130 / 134 / 158** live
   threads at `CHEZZI_THREADS` **2 / 4 / 0 (default)**, because the eager drainer costs one OS thread
   per OPEN nested nursery, not per nesting level, and a depth-7 binary tree holds 127 interior
   nurseries open at once. The bound at every `N` now comes from the `NestedDrainerSlot` budget in
   `src/vm/sched.rs` (`worker_count().max(2)`), which the same tree measures at **4 / 6 / 12 / 20**
   live threads at `CHEZZI_THREADS` **1 / 2 / 4 / 8**. This `EnterNursery` clause is unchanged and still
   correct to keep — a nursery entered inside a spawned task still builds a scope on the enclosing
   worker's sched rather than run eagerly there when `worker_count() < 2` — but it is no longer the
   thing holding the nesting-depth bound.

   **Removed (TICKET-131, 2026-09-17).** The clause is gone: `EnterNursery` reads `let eager =
   self.mn.is_none();`. A nursery inside a spawned task never builds a private sched and never takes a
   `NestedDrainerSlot`; it registers a fiber-owned scope on the task's own sched, adding no thread, and
   its owner parks at the join at every worker count (TICKET-103's path). The clause's blocking private
   join was W13-6's hang: two recoverers fanning in to an open body. It could only go after TICKET-132
   parked the escape abort.

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
  enclosing one. Sibling scheds were mutually invisible (`send_wake` → own sched → `wake_parent_chain`,
  strictly upward — replaced by the run-wide `MnSched::wake_run_wide` in TICKET-099, which reaches a
  sibling directly), so a task in one nursery could not wake a receiver parked in another, and
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

**`src/vm/golden_tests.rs` (formerly `parity_tests.rs`) — CORRECTION: audit, but they probably survive.** The surveyed line numbers
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
`chz_suite_passes` (`tests/chz_suite.rs`) "the first thing to design, ahead of the VM
edits". It was **over-stated**: that gate compares per-test **verdicts**, never output, and no suite file
asserts inter-task print order. All 13 `spawn`-using suite files pass on both engines unchanged, and the
engine split needed no per-engine story in the native suite at all. (The files, for reference:
`spec/{static_witness,airlock_shared_binding,cancel_defer_recover,airlock_native,eq_func,module_global_freshness,opt_carrier,struct_name_resolution}_test.chz`,
`stdlib/{fs_bytes_roundtrip,sleep_cancel,process}_test.chz`,
`suites/{concurrent_collection,rwshared_readview}_test.chz`.)

### Docs that moved WITH the change

All of these shipped in the same commit:

- **`docs/reference/concurrency.md`** — the nursery-scoping bullet gained *when a task starts* and names the
  `--serial` exception.
- **`docs/reference/concurrency.md` §4** — the C1–C4-era "Staged executor" / "Documented consequence (sequential
  only)" passage was **rewritten, not patched**. It claimed tasks "*run to completion in FIFO order*" at
  the barrier and called the behaviour "*the deterministic sequential approximation of concurrency*"
  with real interleaving deferred to **C5** — written before C5 shipped and became the default engine,
  and never revisited. It now states eager start on M:N, lazy on `--serial`, the buffered-sink vs
  streaming-CLI ordering contract, and the join as a completion barrier.
- **`docs/reference/syntax.md`** — `print("dispatched")  # runs before the tasks` and the `parallel:` bullet's
  "tasks run at the barrier".
- **`docs/reference/stdlib.md`** — "the one place the two engines deliberately disagree" is now two places.
- **`docs/contributing/gaps.md`** — the `bare spawn: start time` row is struck FIXED, and the superseded observation
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
| decides by | live state: `running`/`runnable`/`inflight`/`waiters`/`parked_n`, plus veto terms | **nothing** — an unconditional `else` |
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
   owner being an unregistered live party). Overlaps `docs/history/designs/cross-nursery-flat-scheduler.md`.
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

**Sequencing note — RELAXED 2026-08-04, and step 0 confirmed it was right; MOOT since 2026-08-16.**
This previously said "do §2b (remove `--serial`) first, because a detector that must stay
byte-identical across two engines is much harder". That constraint was relaxed instead: correctness
outranks engine agreement (project `CLAUDE.md`), so the detector was built M:N-only while the serial
engine kept its crude arms. In the event the shared code degenerated correctly on the cooperative
engine anyway, and the only divergence step 0 introduced was that M:N RAN programs the cooperative
engine still faulted on — the direction that matters. **§2b has since shipped (2026-08-16) and there is
only one engine**, so nothing here constrains the remaining steps 1–4.

---

## 3. Missing features (ranked by leverage for scripting) → **mostly shipped (M12–M18)**

> Comprehensions, slicing, the iterator protocol, concat/merge, hex/bin/oct literals, optional
> chaining, tuple-destructuring `for`, match guards, `std.os.exit`, and runtime stack traces have all
> landed. Mutable closure capture **landed as uniform by-reference capture** (2026-07-09, reversing the
> earlier snapshot-by-value decision). Nothing in this list is still open — see
> [`PROGRESS.md`](../../PROGRESS.md).

1. **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set). A Python-feel language without
   these feels broken. Pure parse-time desugar to loop + push. Cheap, large UX win.
2. **Slicing — DONE, and since moved onto the `Range` value (TICKET-253).** Originally shipped as
   Rust-style `xs[1..3]`, then moved mid-M19 to Python colon syntax `xs[a:b:c]`, and now back to
   `xs[a..b]` with one difference: `a..b` is no longer syntax that only a slice, a `for` and a pattern
   accept. It builds a `Range` value (`std/prelude.chz`), and a slice takes any `Range`: `xs[a..b]`,
   `xs[a..=b]`, `xs[..b]`, `xs[a..]`, `xs[..]`, `xs[Range(0, 10, 2)]`. The colon slice and `[::-1]` are
   removed (`xs.reversed()` replaces the last). A negative bound counts from the end, an inclusive end
   adds one after that, both bounds clamp (Python's asymmetry stays: a plain index faults out of range).
   There is no `ExprKind::Slice`: a slice is `ExprKind::Index` with a `Range` index, and one function,
   `slice::range_indices` in `src/slice.rs`, resolves it for `List`, `str`, `bytes`, `bytearray` and
   `ConcurrentList`. The `Slice` protocol requires `slice(self, r: Range) -> R`. The `..` operator builds
   the value everywhere. See [`PROGRESS.md`](../../PROGRESS.md) "TICKET-253" and `docs/reference/syntax.md` §6b.
3. ~~**Iterator protocol + generators (`yield`)**~~ — **iterator DONE; generators removed.** The
   `Iterator[T]` parameterized protocol shipped (M13): user structs usable in `for`, generic
   `[S: Iterator[T], T]` bounds, and lazy `map`/`filter`/`take` written as **adapter structs** over it
   (Rust `std::iter` model — `examples/iter_adapters.chz`). **`yield`/generators have since landed as
   a complete, VM-only feature** (was a permanent non-goal): any `fn` that uses `yield` is a generator
   (the `-> Iterator[T]` annotation is optional — the element type is inferred from the first `yield`);
   the call returns a suspendable generator (one-shot cooperative coroutine, own private
   frame/stack swapped into the VM, resumed by an intrinsic `.next()`). A **live** generator held in a
   frame **local** is now
   sendable across a task airlock **BY VALUE** (F3 path C — deep-copied + rebuilt on the receiver, parked
   slots checked sendable at serialize time; mid-`recover:` / pending-`defer` / multi-frame suspensions
   reject cleanly). A **module-global** generator is serialized by value as well: the reach-gate +
   poison-snapshot model was **RETIRED 2026-07-21** (`docs/history/designs/concurrency-b3.md:314`), and
   `src/vm/golden_tests.rs:9260` pins the by-value behaviour. A generator crossing WHILE RUNNING
   is a clean fault (DEC-041); a module global holding one still snapshots as an inert `Nil` that
   faults only when a task reaches it. The adapter-struct pattern stays the default for lazy streaming.
4. ~~**List concat + map merge**~~ — **DONE.** Method-based: list `.concat`/`.extend`, map
   `.merge`/`.update` (concat/merge new, extend/update mutate). No new syntax; spread/unpack stays
   dropped. `examples/concat_merge.chz`.
5. ~~**Hex / binary / octal literals**~~ — **DONE.** `0xFF`/`0b1010`/`0o17`, lexer-only via
   `i64::from_str_radix`, `_` between digits. `examples/hex.chz`.
6. ~~**Optional chaining + null-coalescing**~~ — **DONE.** `x?.field`/`x?.method()` + right-assoc
   `a ?? b`. Originally lowered to a `match` by the desugar pass (zero checker/engine code); **W7-43
   (2026-08-11) moved that decision to the checker** — the carriers now survive desugar, the checker
   picks the lowering by operand type (`T?` → `match`; `T!E` → `?` then `.`, identical to the
   spaced `x? .f`) and records it in a `CarrierTable` the compiler reads. **TICKET-039 (2026-09-02)
   widened `??` to a `T!E`**: it discards the error via an `?v`/`!_` match, Rust's
   `unwrap_or`. Still zero new VM code: both paths lower to ops the engine already runs.
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
   [`PROGRESS.md`](../../PROGRESS.md) "Uniform by-reference capture".
9. **Match guards + range patterns** — `n if n>0:`, `1..10:`. Roadmap. Guards subsume the rest.
10. ~~**`std.os.exit(code)` + real exit codes**~~ — **DONE.** `std.os.exit(code)` is a hard, uncatchable
    halt (unwinds past `recover:`, bypasses `defer`), with the code threaded through both run drivers +
    the CLI; exit-wins precedence holds under `--parallel`. The status is the POSIX **low 8 bits** of
    the code (`-1` → 255, `300` → 44). `examples/exit.chz`.
11. ~~**Runtime stack traces**~~ — **DONE.** Error + call chain + line numbers
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
    Full surface + the decline list: `docs/reference/syntax.md §7a`. Running proof:
    `tests/chz/spec/static_witness_test.chz`, `examples/static_witness.chz`. The two rejected 2026-06-24
    attempts (`auto-task/protocol-static-req`, `…-v2`) are superseded and discardable.

14. **`cast[T](val: Any) -> T?` — ⛔ NO CONSUMER (2026-08-09).** The owner ruled against
    leaning on `Any` ("we vouch for statically typed; `Any` is Go's `interface{}`"), and `cast` only pays
    off through `Any` — so this is closed unless that reverses. Its erasure analysis stays valuable and
    is summarised in §3a1 "What stays impossible". Original entry: **a checked downcast off the `Any`
    top type — ⏸️ DEFERRED
    (DESIGN ONLY, no code).** The `Any` top type + variadics shipped (see `docs/reference/syntax.md`); `Any`
    lets a value of any type into a universal slot, but there is currently **no way back out** — you can
    hold and display an `Any` but not recover its concrete type. The companion is a **checked downcast**:
    ```chezzi
    cast[int](x)          # -> int?: n if x is really an int, else None
    match cast[Point](v):
        ?p: print(p.x)
        None:    print("not a Point")
    ```
    Returning `T?` (not a raw `T`) makes it fit `?` / `match` and keeps it total (no faulting
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
    (no implicit `int`→`float` widening — rule D3, TICKET-138). The extensible mechanism is
    the reserved `Convert[S]` protocol (there is still no `as`, no `Into`/`TryFrom`). Full current-state
    inventory in `docs/reference/spec.md` "Type conversions & casting". The intended direction, in leverage order:
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
      Self!E` — **no separate `TryFrom`** needed. **Skip `Into`** (needs expected-type threading;
      Chezzi infers bottom-up). **Multi-source (Phase 2) also DEFERRED** — needs argument-type overloading
      (banned invariant) for thin payoff; distinct-named static ctors cover it today.
    - **Cheap scalar fills — ✅ LANDED** (additive, low risk, landed independently ahead of the
      `From` protocol): `bool(x)` truthiness cast (int/float/bool/str, never faults on a scalar) +
      the `T!E`-returning `s.parse_int() -> int!str` / `s.parse_float() -> float!str`
      siblings of the `T?`-returning `to_int`/`to_float`.
    Variance/soundness note: a `from`-based conversion is a value-producing call, not a subtype
    relation — no covariance holes. This is a language feature (own milestone), not a perf lever.

**Ecosystem (Tier 4, separate track):** REPL (huge for scripting iteration), formatter, `assert` +
built-in test runner, LSP.

---

