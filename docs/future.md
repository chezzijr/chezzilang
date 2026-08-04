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
regressed `ch.recv()`-in-a-job. Fixed by having such a job BLOCK (`Vm::eager_block_recv`, a bounded
poll on the channel's own condvar, mirroring `demote_recv_block`'s settle order) rather than fault —
Python's behaviour. A job blocked on a value that never arrives hangs: decision D, unchanged.

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
That is a local predicate over one executor — it does not observe `parallel:` tasks, other executors, or
`main`, and gaps.md row `W7-12r` lists the four programs it therefore still gets wrong. It is a
placeholder for this section, not a down payment on it: the graph below subsumes it, and closing
`W7-12r` by growing the local predicate instead is explicitly the wrong move.

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

### Ordering — REVISED 2026-08-04, and this is where the next session starts

The original plan opened with the `Arc::strong_count` rule and treated the graph as the payoff. Working
W7-12 to a conclusion changed the ranking, for one reason: **every program that hangs today is TOTAL
quiescence, which is the cheap case, not the hard one.** `main` parked inside `shutdown()`, both jobs
parked, nothing runnable, nothing in flight — that is precisely Go's rule, and Go answers it by
COUNTING, with no graph at all. W7-12's local predicate went wrong three times (`W7-12r`) because it
asked "is this executor stuck?" — a question no per-executor counter can answer — when the answerable
question was the process-wide one all along.

0. **Lift `MnSched::is_deadlocked` from per-nursery to PROCESS-WIDE.** This is the whole fix for
   `W7-12r`, it is Go's exact rule, and it subsumes and DELETES W7-12's interim predicate
   (`eager_join_deadlocked`, `join_has_no_live_siblings`, `ExecutorCore::joining`/`blocked`, the
   `eager_block_suspect` debounce, and the registry sweep — all of it). The one thing today's rule
   cannot see is a **joiner**: a thread inside `Vm::join_eager_jobs` or a nursery barrier is blocked but
   is counted nowhere, so `main`-in-`shutdown()` is invisible. Add joiners as blocked parties and the
   rule reaches every W7-12 shape.
1. `Arc::strong_count` sole-handle rule (sound, O(1), no graph) — still worth landing, but it is
   narrower than it looks: it fires only when the blocked receiver holds the ONLY handle, so it misses
   the common case where the channel is a module global `main` also holds. Do it after step 0, not
   before.
2. Unify the blocked-party registry process-wide (fibers + demoted workers + joiners) — the data
   structure step 0 needs anyway. Overlaps `docs/cross-nursery-flat-scheduler.md`.
3. AND-OR knot detection over that registry, keeping every existing veto, run only on suspicion. **This
   buys PARTIAL deadlock only** — a subset stuck while the rest of the program runs on, which Go
   structurally cannot report. Real, but extra credit on top of step 0, not a prerequisite for it.
4. Retire the `netio.rs` "no scheduler ⇒ no sender" arms; they become unreachable.

**THE RISK, stated first because it is the one that has already bitten three times.** The vetoes are the
whole correctness surface. A job sleeping on a `timer`, blocked on a socket, waiting on netpoll or
blocking-pool work, or racing a value into a queue is NOT deadlocked, and counting it as such is a false
alarm on a working program — the exact failure W7-12 shipped three times (see `gaps.md` W7-12 and the
memory `parked-is-not-stuck`). So: write the Go/CPython comparison programs and the LOOPING regression
tests BEFORE the detector, keep every veto `is_deadlocked` already earned, and put the whole thing
through `adversarial-review` — a full green gate had no opinion on any of the three false positives.

**Sequencing note — RELAXED 2026-08-04.** This previously said "do §2b (remove `--serial`) first,
because a detector that must stay byte-identical across two engines is much harder". That constraint is
gone: correctness now outranks engine agreement (project `CLAUDE.md`), and `--serial` is scheduled for
deletion regardless, so **build the detector M:N-only and let the serial engine keep its crude arms
until it is removed.** A temporary, documented engine difference on a doomed engine is a far smaller
cost than either hanging or waiting on §2b.

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
   `a ?? b` on `Option`, lowered to a `match` by the desugar pass (zero checker/engine code).
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

13. **Static / associated protocol requirements (typeclass-style `T.default()`) — ⏸️ SHELVED
    (attempted twice 2026-06-24, both rejected; not worth the cost at the current model).**
    The goal: a protocol may declare a *static* (no-`self`) requirement, and a generic bounded by it can
    **construct** through the type param — the one thing instance-only protocols can't express:
    ```chezzi
    protocol Default:
        fn default() -> Self
    fn make[T: Default]() -> T:
        return T.default()      # T erased at runtime — needs dictionary passing
    ```
    **Direction (if ever revived): dictionary passing, NOT monomorphization** — Chezzi is a type-erased
    bytecode VM, so `T` has nothing to dispatch on; thread the conforming type's static-method
    dictionary in as a hidden trailing call arg (kept the one erased body + two-engine parity).
    **Why shelved:** two full auto-task runs both **rejected** with 5 criticals *each*, all the same
    class — the **checker's "accept" boundary keeps drifting out of lockstep with the compiler's
    "can-lower" boundary**, so every run half-covers the lowering surface and a prosecutor finds the next
    axis (cross-module call, `spawn:`/`parallel:` body, first-class value / `defer` (`g := make; g()`),
    inferred-T through a container `xs: List[T]`, non-leading bound param). Each shape either crashes the
    compiler or diverges the two VM engines. Making it sound needs a *complete* lowering contract enforced in one
    checker gate — a real design pass, not another blind run.
    **Current behavior on main (the sharp edge): a no-`self` protocol requirement is DECLARABLE but
    UNUSABLE.** Main does **not** reject the no-`self` rule — `protocol Default: fn default() -> Self`
    hoists fine, a struct's *static* `default()` satisfies the bound `[T: Default]` (an *instance*
    `default(self)` does not — `method 'default' has the wrong signature`), so you can declare and bound
    on it. But you can never **call** it: `T.default()` inside the body fails with `unknown name 'T'`
    (no dict-passing to dispatch the erased `T`). So such a bound is just a **dead marker** today — not
    unsound, only inert until the feature is revived. (Verified 2026-06-25 on main @ 503b6b8.)
    **Why it's low priority anyway:** the workaround already exists and is idiomatic — **pass a factory
    closure** (first-class-fn style instead of typeclass style), works today with zero new machinery:
    ```chezzi
    fn make[T](mk: fn() -> T) -> T: return mk()
    make(fn(): Counter(0))      # same power; dict-passing only buys the `make[Counter]()` sugar over this
    ```
    The two rejected attempts live unmerged as branches `auto-task/protocol-static-req` /
    `…-v2` (main is clean); discardable. Revisit only with a design-first pass + appetite for the sugar.

14. **`cast[T](val: Any) -> Option[T]` — a checked downcast off the `Any` top type — ⏸️ DEFERRED
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

15. **Type conversion protocol (`Convert[S]`) + scalar fills — 🚧 PARTIALLY LANDED (slices 1+2).** Today
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
      through a bound) ⛔ DEFERRED** (2026-07-07): a spike proved the "restricted construction" model
      delivers nothing under Chezzi's **erased, single-pass, non-monomorphizing** generics — `T` is never
      concrete while its body is checked (the same gap hits *every* generic static call, e.g. `T.empty()`,
      not just `convert`), and the only concrete static call is `Type.convert(x)` written directly (which
      already works with no protocol). `T.<static>()` on a type param now gives a **clear error** (Option A)
      instead of `unknown name 'T'`. Making it real needs the deferred **witness-passing** escape hatch
      (thread the concrete `convert` in as a hidden arg — the only erasure-compatible way); build it only
      when real code needs generic-over-Convert construction. A fallible conversion is `convert(x: S) ->
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
   whose last alias slot has been swept is counted **nowhere** (gaps.md `W6-10r`, still OPEN). This is a
   **different hole from the inline-scalar escape below, which also remains OPEN.**
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
   Residual SAMPLING escapes are listed in gaps.md `W6-10s` — notably the by-hand airlock paths (spawn
   args, closure captures, `Executor.submit`) which grow off-heap storage without charging it.
   **v1 limits (deterministic, documented):** the
   trip fires only at a **GC boundary**, and GC triggers on `Obj`-count growth (plus charged off-heap
   wire bytes when a cap is set) — a loop growing a single
   container of **inline scalars** (e.g. `xs.push(i)` for int `i`) allocates no `Obj`s **and charges no
   wire bytes**, so neither trigger fires: it never sweeps and
   so never trips (push a heap value to guard it) — **still open, and byte-aware pacing does not close
   it**; the check is a high-water on `live_bytes` which
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
