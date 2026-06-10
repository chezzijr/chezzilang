# Tier-D — M:N scheduler + async I/O (phased breakdown)

Companion to [`concurrency.md` §10](concurrency.md) (the design) the way
[`concurrency-b3.md`](concurrency-b3.md) is the companion to §9 for the B3 epic. §10 says *what*
Tier-D is and *why*; this file is the *how* — the phase ladder **D0…D6**, each independently
TDD-able, plus the **Go-vs-BEAM borrow ledger** so the split is not relitigated.

**Status:** D0, D1, D2 (D2a + D2b), D3 landed — the `--parallel` engine is now a true M:N scheduler
(lightweight fibers parking on `recv`, not thread-per-task) **with BEAM-style reduction-counting
preemption** (a CPU-bound fiber yields its worker at budget exhaustion, so siblings make progress).
**D4 (D4a–D4e) has landed** — per-worker local run queues + a global overflow, a capped global
batch-grab (`globrunqget`), random-victim steal-half, a periodic global check, and **D4e — a
runnable-gated park** that removed the `cv.wait_timeout` poll: an idle worker now does a true
`cv.wait` (no timeout) when `runnable == 0` and re-steals after a brief bounded `wait_timeout` backoff
when `runnable > 0`. **D4e
deliberately did NOT adopt Go's `nmspinning` + SeqCst StoreLoad fence** — chezzi already maintains a
precise core-lock-serialized `runnable` atomic (the reachability oracle Go lacks, which is the whole
reason Go needs the lockless fence), so the core mutex *is* the StoreLoad barrier and a simpler,
easier-to-prove gate suffices (see the D4e section below). **D5 (dirty/blocking pool) has landed; D6
(epoll/kqueue pollset + `std.net`) is next on the ladder.**

## TL;DR

Replace the `--parallel` engine (one full worker `Vm` per task on a bounded FIFO pool) with a
**Go-style GMP work-stealing M:N scheduler**: lightweight fibers (own heap, share-nothing)
multiplexed over a core-sized worker pool, **parking on `recv`/I/O instead of pinning threads**.

*Go skeleton (GMP work-stealing run queues + netpoller), BEAM brain (reduction-counting preemption +
dirty pool for opaque blocking native calls) — because Chezzi is a share-nothing bytecode VM, which
is BEAM's world, so BEAM's hard answers are simpler than Go's.*

## Why now / what's broken

The cooperative (default / `--serial`) engine is M:1 and stays the frozen parity oracle. The
`--parallel` engine is true multicore but has three walls:

- **Per-task cost** — a full `Vm` + module-graph reconstruction per task (`prepare_worker` /
  `build_worker_modules`), ~2 s for the prime demo.
- **Thread-pinning** — a blocking `recv` parks the *whole OS thread* on a condvar; a blocking native
  call (`fs` / `io` / `time.sleep_ms`) pins its worker → **G3 starvation**, live today.
- **No green-thread scale** — tasks ≈ threads, so 10 k tasks ≈ 10 k `Vm`s.

**Why Chezzi is unusually ready** (the two worst M:N sub-problems are already gone):

1. **Bytecode VM ⇒ suspend is a data snapshot, not stack magic.** A fiber's state is `FiberCtx`
   (frames/stack/handlers); `swap_ctx` (`src/vm/mod.rs`) is 8 `mem::swap`s, O(1); resume via
   `run_until(0)` replays the saved IP with no Rust-stack rebuild. The cooperative engine is
   *already an M:1 scheduler* — M:N is "run several of those in parallel + work-steal + pollset."
2. **Share-nothing per-thread GC ⇒ no concurrent-GC coordination.** Each `Vm` owns its `Heap` and
   collects independently at the `run_until` loop-top safepoint. No cross-thread write barriers, no
   STW. This is also *why we can skip Go's signal-based preemption* (see the ledger).

## Borrow ledger — Go vs BEAM, and why

The throughline: **Chezzi already has BEAM's *memory model*** (private heap per task, message-copy
across the boundary, races unrepresentable — `concurrency.md §2`). What it lacks is BEAM/Go's
*scheduler*. **Memory model ≠ scheduler — two orthogonal axes.** So take Go's *scheduler mechanics*
(better-documented work-stealing) but BEAM's *preemption + native-call handling* (downstream of
share-nothing, strictly simpler for a bytecode VM).

| Mechanism | Source | Decision | Why this source (not the other) |
|---|---|---|---|
| **G/M/P split**, bounded P = `available_parallelism()` | **Go** | Adopt (D2) | Cleanest decoupling of runnable work from threads; caps real parallelism; enables handoff + stealing |
| **Per-P local run queue** (bounded ring + `runnext`) + global overflow | **Go** | Adopt (D2) | The scalability core — common case touches only per-worker state, no global lock |
| **Work-stealing** (random victim, steal half) + periodic global check (`schedtick % 61`) | **Go** | Adopt (D4) | Provably good load balance, low contention. BEAM's AMQL periodic-migration is heavier; Go's reactive steal is simpler to land first |
| **`wakep` / spinning-worker + StoreLoad barrier** | **Go** | Adopt (D4) | Avoids both idle cores and busy-wait; the barrier is the documented correctness lynchpin |
| **`gopark` / `goready`** channel park keeps the M | **Go** | Adopt (D2) | Cheap user-space switch; Chezzi's `self.suspend` is already exactly this |
| **Preemption** | **BEAM** (reduction counting) — **NOT** Go (SIGURG / sysmon) | Adopt BEAM (D3) | Go needs *signal-based async* preemption only because it runs **native machine code with a shared GC heap** — it must stop a goroutine at an arbitrary PC and find live pointers (per-instruction stack maps; the team measured a 7.8 % slowdown fighting this). A **bytecode VM** has a natural safepoint every dispatch (`run_until` loop-top, already there); a **share-nothing** heap means no STW-GC forcing preemption. Reduction counting gives BEAM's bounded soft-real-time fairness with *none* of Go's signal machinery. **The single biggest simplification vs Go.** |
| **Opaque blocking native calls** (`fs` / `io` / `sleep`) | **BEAM** (dirty / blocking pool) — **NOT** Go (syscall handoff) | Adopt BEAM (D5) | Go's `entersyscall` / `handoffp` works because **the Go runtime compiles & wraps every syscall** — it knows the boundary. For **opaque user / embedder native code** that may compute *or* block for unknown time, Go has no clean story (cf. cgo's poor P-handoff, golang/go#57103). BEAM's dirty schedulers are purpose-built for "about to call native code I can't introspect": route to a separate blockable pool so it *physically cannot* stall fairness-critical workers. Composes with reduction counting; Go's signals fight native code |
| **Async thread pool for blocking file I/O** | **BEAM** (`+A` async pool) — parallels Go `spawn_blocking` | Adopt (D5) | Regular files are **not** epoll-able (epoll always reports a regular file ready) — they *must* go to a blocking pool, in both runtimes. Same idea; BEAM names it explicitly |
| **Netpoller** (epoll / kqueue, non-blocking sockets, `injectglist`) | **Go** | Adopt (D6) | Go's netpoller is the cleanest documented design; turns a socket-block into a cheap fiber-park |
| **Per-task private heap + per-task GC, no STW** | **BEAM** (already Chezzi's model) | Already have | Removes cross-thread GC coordination *for free* — the reason the whole M:N is tractable and the reason we can skip Go's signal preemption |
| **Priority classes** (max / high / normal / low, schedule-count weighting) | **BEAM** | **Defer** | Real BEAM feature, not needed for v1; revisit if priority becomes a requirement |
| **Signal-based async preemption (SIGURG)** | **Go** | **Reject** | Exists to solve native-code + shared-GC; neither applies. Pure added complexity here |
| **P-handoff as the native-call story** | **Go** | **Reject** | Assumes the runtime wraps every syscall; wrong fit for opaque native extensions (see D5 row) |

## Go GMP → Chezzi mapping

| Go | Chezzi | Where |
|---|---|---|
| **G** goroutine | **Fiber** = `FiberCtx` (frames / stack / handlers / nurseries) **+ own `Heap` + Arc'd module snapshot** | `FiberCtx` `src/vm/mod.rs`; today `Heap` lives on `Vm`, D1 moves it into the swappable context |
| **M** OS thread | Pool worker thread (hosts a thin `Vm` shell = the execution engine) | `src/vm/pool.rs` |
| **P** processor | Per-worker **run queue** (bounded ring + `runnext`) + the run-license, count = `available_parallelism()` | new in D2 |
| LRQ / GRQ | Per-worker local queue + global overflow queue | new in D2 |
| `runqsteal` | Steal half from a random victim | D4 |
| `gopark` / `goready` | Existing `self.suspend = Some(h)` park + `send`-side wake | `src/vm/mod.rs` recv arm (extend to wake a worker, D2) |
| function-prologue safepoint | `run_until` loop-top (already checks GC + cancel) | `src/vm/mod.rs` |
| netpoller | epoll / kqueue pollset via the `polling` crate | D6 |
| sysmon / dirty schedulers | growable blocking pool for `native_reentry` blocking calls | D5 |

---

## Phase ladder

Each phase is independently TDD-able and (D1+) ships behind `--parallel`. **Serial parity goldens
must stay byte-identical throughout.** Sequence: **D0 → D1 → D2 → D3 → D4 → D5 → D6.** D0 is
standalone (cooperative engine). D1 is the foundation everything else builds on. D5 and D6 can swap
order (D5 has real surface today; D6 needs the new `net` surface).

### D0 — O(N²) → O(N) ready-queue *(cooperative engine; standalone; FIRST)* — ✅ LANDED

> **Shipped** as O(N·logN) (a `BTreeSet`, not a `VecDeque`) with **three deviations from the plan
> below, each verified against the code:** (1) `blocked_on` is keyed by the **`ChannelCore` pointer**
> (`Arc::as_ptr as usize`), not `GcRef` — cooperative `spawn` deep-clones a channel (`from_wire`
> allocs a fresh handle onto the same `Arc<ChannelCore>`), so siblings hold distinct handles aliasing
> one core and a `GcRef` key would lose every wakeup. (2) **`BTreeSet` (lowest-index pop)** preserves
> the old `pick_runnable` scan order; a FIFO `VecDeque` would re-queue a woken low-index fiber behind
> pending higher-index ones and reorder output. (3) `wake_on_send` **drains every scheduler level**,
> not just the innermost, to preserve cross-level wakeup. `FiberState::Blocked` dropped its now-unused
> `GcRef` payload (handle stays rooted on the fiber's operand stack). See `src/vm/mod.rs`
> (`run_scheduler` / `run_child` / `channel_core_ptr` / `wake_on_send`) + PROGRESS.md.

**Goal.** `run_scheduler` calls `pick_runnable` each turn, which **linear-scans all live children**
for the lowest-index runnable one → O(N²) (measured: 1 k→1.4 ms, 10 k→51 ms, 20 k→246 ms,
50 k→2.34 s; practical ceiling ~10 k–20 k tasks). Replace with an explicit FIFO **ready-queue** of
runnable child indices → O(1) amortized per turn, O(N) whole nursery.

**Changes** (all in `src/vm/mod.rs`, cooperative path only):
- Add `ready: VecDeque<usize>` to `Nursery`; seed with all child indices on `join_nursery`.
- `run_scheduler`: pop front of `ready` instead of `pick_runnable`. A finished child is not
  re-pushed; a child blocked on `recv` is not re-pushed until unblocked.
- **Unblock wiring**: when a fiber `send`s into a channel some sibling is `Blocked(h)` on, push those
  sibling indices onto `ready`. Cheapest correct form: a per-nursery `blocked_on: HashMap<GcRef,
  Vec<usize>>`; the `send` arm drains the matching bucket onto `ready`. (Alternative: after each turn
  scan only the *blocked* set — O(blocked) not O(N), acceptable v1. Prefer the map for true O(1).)
- `pick_runnable` / `all_children_done` collapse to: `ready` empty + any non-`Done` child ⇒ deadlock.
- **Preserve FIFO contract**: `ready` is seeded in index order and `send` pushes in index order, so
  first-runnable order is unchanged → goldens byte-identical.

**TDD.** (1) 20 k trivial fibers in one `parallel:` complete with the correct aggregate (sum via
`Shared`), optional wall-clock ceiling far under the old 246 ms. (2) Existing cooperative goldens
(producer/consumer, deadlock-fault, nested nurseries) stay green. (3) Deadlock test still faults
`deadlock` byte-identically.

**Risk.** Low; isolated. Main care: deadlock must still fire when `ready` empties with live blocked
children. **Reuse:** `Nursery`, `FiberState::Blocked(h)`, `DEADLOCK_MSG`, existing `send` arm.

> Note: D0 does *not* make cooperative tasks "green-thread cheap" — that is D1's per-task-cost work.
> It only removes the quadratic wall, and it is the *cooperative* engine only (`--parallel` farms to
> the pool and never runs `run_scheduler`).

### D1 — Lightweight fiber: heap into the swappable context + lazy module snapshot *(foundation)*

> **Status: lazy-module-snapshot half LANDED.** The per-task `build_worker_modules` eager
> reconstruction is replaced by a heap-independent, read-only `Arc<ModuleSnapshot>` built once
> (`snapshot_modules`/`to_snap`) and **faulted into each worker heap lazily, one module at a time, on
> first global access** (`install_snapshot`/`fault_module`/`replay_snap`/`ensure_module_faulted`,
> gated by `module_faulted`). FIFO pool unchanged → observably identical except faster; all
> `--parallel` goldens byte-identical, `primes_parallel` → `148933` on both engines, 1346 tests green.
> **The `Heap`-into-`FiberCtx` half is intentionally deferred to D2** — under the unchanged FIFO pool
> (one worker `Vm` per task) it has no observable effect and would risk the cooperative engine's
> share-by-ref single heap (decision A); it lands with the M:N share-nothing fiber model in D2.

**Goal.** Kill the per-task `Vm`-rebuild cost. Today `prepare_worker` builds a fresh `Vm` + eagerly
reconstructs the whole module graph (`build_worker_modules`) per task. A fiber should be **`FiberCtx`
+ its own small `Heap` + an `Arc` to a read-only module snapshot**, faulted in lazily — not a full
`Vm`.

**Changes** (`src/vm/mod.rs`):
- Make the **`Heap` part of the swappable context** so `swap_ctx` swaps `frames / stack / … / heap`
  together. A worker thread's `Vm` becomes a *host shell* running whichever fiber's context (incl.
  heap) is swapped in. Parked fibers carry their own heap in the `Fiber` struct — still share-nothing
  (no cross-fiber refs).
- **Lazy module snapshot**: replace eager `build_worker_modules` with an `Arc<ModuleSnapshot>`
  (read-only `module_objs` + parent module globals in wire form). On a module-global miss,
  reconstruct *that module* into the fiber's heap on first access and cache. `Program` is already
  `Arc<Program>` (free to share).
- GC `collect`: already roots parked fibers' contexts (Root 5); since each heap is independent, the
  running `Vm` collects only the *current* fiber's heap — parked heaps are untouched.

**TDD.** (1) Existing `--parallel` goldens (primes_parallel, shared, parallel_channel)
byte-identical. (2) Microbench: 10 k trivial `spawn` under `--parallel` in ms-range (ceiling assert);
the ~2 s prime-demo per-task overhead drops sharply. (3) A task that imports + calls a module fn
works (lazy reconstruction correctness).

**Risk.** Medium-high — the biggest refactor (heap ownership). **De-risk: land it with the existing
FIFO pool still farming one fiber per worker** (no new scheduler yet) so D1 is observably identical
except faster; the new loop is D2. **Reuse:** `swap_ctx`, `to_wire` / `from_wire` (`src/vm/wire.rs`),
`Arc<Program>`, `module_objs`, `map_global_value`.

### D2 — GMP run queues + unified M:N scheduler loop *(no stealing yet)* — ✅ LANDED (D2a + D2b)

> **Status: D2b LANDED.** The `--parallel` engine is now an M:N scheduler. `run_mn_nursery` replaces
> `run_parallel_nursery`: each task is prepared (`prepare_worker` → `ReadyWorker::into_fiber`) into a
> lightweight **`Fiber`** (own heap + per-task `out`/`stderr`/`module_objs`/`module_faulted`/`executors`
> in `FiberCtx`, the D2a foundation) and seeded onto a **single shared per-nursery run queue**
> (`MnSched`, one `Mutex<SchedCore>` + `Condvar`). Workers are thin host **shells** (`spawn_shell`);
> `mn_worker_loop` (the cross-thread generalization of cooperative `run_child`) swaps a fiber's ctx in,
> runs `start_task`/`run_until(0)`, and on an empty `recv` **parks** it (reusing the cooperative
> suspend/rewind-`ip` mechanism, filed into the channel's wait set keyed by `ChannelCore` ptr) then
> grabs the next fiber — no OS-thread blocking. `send` (`MnSched::send_wake`) enqueues the message AND
> re-queues parked waiters **atomically under the sched lock**; `park` re-checks the queue + cancel
> flag under that same lock to close the check-then-park **lost-wakeup gap** (lock order is
> core-OUTER / channel-`q`-INNER everywhere → no ABBA). **Deviations from the plan below, each
> verified:** (1) **one shared run queue**, not per-worker local rings + `runnext` + global overflow —
> rings without work-stealing are pointless and harmful, so both are deferred together to D4. (2) The
> deadlock detector is **not** B3.5's barrier-confirm `DeadlockWatch` (retired) but the exact predicate
> `running==0 && runq empty && parked>0 && done<total`, race-free under the single coordinator. (3) The
> joining thread runs an **inline shell that alone drains the whole run queue** (decision B), so
> farmed helper shells are **fire-and-forget — never joined**; the join's liveness never depends on a
> bounded pool resource, which is what prevents the nested/concurrent **pool-exhaustion join hang** (a
> review-panel Critical). Decision-F flush + `Exit`-over-`Fault` precedence are factored into
> `reduce_task_slots` (shared with the legacy `Executor`-drain `run_workers_on_pool`, which keeps the
> per-task-`Vm` model — its `recv`-on-empty faults immediately, as it always did). Headline: 1000
> consumers + 1000 producers finish in ~0.02 s (would starve on the old engine). **1361 tests** green
> (5× full-suite + 60× the defer/cancel race + 10× headline), `cargo clippy` clean,
> `primes_parallel=148933` both engines, all `--parallel` goldens byte-identical; 4-agent S++ review
> panel + cold pass, two Criticals fixed (the pool-exhaustion hang above + a `defer`-on-cancel test
> race: a sibling fault could trip cancel before a sibling registered its `defer`, so the test was
> synchronized with a start-token — matching the Go semantic that an unregistered defer doesn't run).
> **D3 is next.**
>
> **Status: D1's deferred heap-into-`FiberCtx` half LANDED (D2a).** `FiberCtx` now carries
> `heap: Option<Heap>`; `swap_ctx` swaps it **only for M:N fibers** (`Some`), while cooperative
> fibers carry `None` and keep aliasing the single `Vm::heap` (decision A — share-by-ref), so the
> cooperative engine is byte-identical **by construction** (every runtime `FiberCtx` is built via
> `FiberCtx::default()` → `None`; the `Some` arm is reached only by unit tests). A `Fiber: Send`
> compile-time guard + `debug_assert!`s in `swap_ctx`/`collect` pin the invariant; a parked M:N
> fiber's heap is share-nothing + quiescent and is **never traced cross-heap** (collect runs only on
> the swapped-in `self.heap`). **No runtime site sets `Some` yet** — the FIFO pool still runs one
> fiber per worker (D1), so D2a is observably identical except that a `Fiber` is now self-contained
> (owns its heap, `Send`), the prerequisite for parking it across worker threads. Tests:
> `swap_ctx_round_trips_an_mn_fiber_heap`, `swap_ctx_leaves_heap_untouched_for_cooperative_fiber`,
> `collect_under_swapped_in_fiber_heap_{preserves_parked_host_object,leaves_parked_host_heap_quiescent}`;
> all `--parallel` goldens byte-identical, `primes_parallel=148933` both engines, 1350 tests green.
> **The run queues + park-on-`recv` half (D2b below) is next** — that is where the share-nothing
> fiber model becomes observable.

**Goal.** Replace `run_parallel_nursery`'s "one `Vm` per task on a FIFO pool" with **per-worker run
queues of lightweight fibers**, parking on `recv` instead of pinning threads.

**Changes:**
- **`src/vm/pool.rs`**: introduce **P** — each worker gets a local run queue (bounded ring +
  `runnext`) + a shared **global** overflow queue. Worker loop: `runnext` → local → global.
- A **Fiber** (from D1) is the job unit. `join_nursery` under `--parallel` pushes children as fibers
  onto run queues; the **joining thread participates** (decision B — runs a fiber inline) so nesting
  can't explode the thread count.
- **Park on `recv`** (the core M:N win): in the channel `recv` arm, under the M:N engine an empty
  `recv` **parks the fiber** (`self.suspend = Some(h)`, context saved to the channel's wait set) and
  the **worker picks up the next fiber** — instead of `cv.wait_timeout` blocking the OS thread.
  `send` enqueues the unblocked waiter + **wakes a parked worker** (`wakep`; D2 can over-notify, the
  full barrier protocol is D4).
- **Join/flush**: keep decision-F task-ordered output flush + `Exit`-over-`Fault` precedence (the
  `run_workers_on_pool` reduce logic, ported to the fiber model).
- Keep B3.4 **cancellation** + B3.5 **deadlock detection**: a deadlock is now "all fibers parked, no
  runnable, run queues empty, no in-flight blocking-pool work."

**TDD.** (1) Producer/consumer with **#fibers ≫ #threads** (e.g. 1000 fibers, 4 threads) completes —
would deadlock/starve under today's thread-pinning (more blocked tasks than pool slots). (2) All
`--parallel` goldens byte-identical. (3) Deadlock + cancellation + `os.exit` precedence still pass.

**Risk.** High — the heart of the engine. Lost-wakeup hazard on `send`→`wakep` (mitigated in D4; D2
over-notifies safely). The `native_reentry` guard still forbids parking inside a native callback —
that fault path is **unchanged** here (fixed in D5). **Reuse:** `FiberCtx` / `Fiber` / `FiberState`,
`swap_ctx`, `run_until(0)`, `DeadlockWatch`, `cancel: Arc<AtomicBool>`, decision-F flush logic.

### D3 — Reduction-counting preemption *(BEAM)* — ✅ LANDED

> **Status: LANDED.** A fiber now carries a reduction budget `reds: u32` (reset to `CONTEXT_REDS =
> 4000` on every schedule-in in `run_one_fiber`); the `run_until` loop-top safepoint decrements it
> **per dispatched op** under the M:N engine (`self.mn.is_some()`) and, at exhaustion (with
> `native_reentry == 0`), sets `yield_now` and returns `Ok(())` to stop dispatch. `run_one_fiber`
> maps that to a new `Disp::Yield`; `mn_worker_loop` calls `MnSched::yield_fiber`, which requeues the
> fiber at the **tail** of the shared `runq` (round-robin) under the sched lock (`running--` +
> `push_back` + `notify_all`) — no `parked` bucket touched, so no park-gap re-check is needed and a
> false deadlock is impossible (`take_runnable` pops `runq` before testing `running==0 &&
> parked_n>0`). **One deviation from the plan below, verified:** a yield reuses the recv-park
> suspend/rewind contract, so it must unwind **every nested `run_until` level** without popping a
> result — the gate that did this only checked `suspend`, so a yield deep in a call chain
> (`main→worker→count_primes→is_prime`, the `primes_parallel` shape) let `run_proto` pop a live
> operand-stack temp as a bogus return → `expected bool, found int`. Fixed by a `paused()` helper
> (`suspend.is_some() || yield_now`) applied at every propagate-up gate (`run_proto`, `do_call`,
> `do_method_call`, `run_until` bottom, `start_task`). The cooperative engine is byte-identical by
> construction (`yield_now` is gated on `mn.is_some()`, always `None` there). **1365 tests** green
> (+4: the fairness hang-watchdog, the 10 k-fiber soundness churn, the nested-call unwind regression,
> the `yield_fiber` unit test), `cargo clippy` clean, `primes_parallel=148933` both engines, all
> `--parallel` goldens byte-identical; 4-agent S++ backend review panel (Godot Gameplay / Solidity /
> Incident Response / SRE) — zero real findings. **D4a–D4d landed (see the D4 status box below).**

**Goal.** Fairness: a CPU-bound fiber must not hog a worker forever. Decrement a per-fiber
**reduction budget** at the existing safepoint; yield at exhaustion.

**Changes** (`src/vm/mod.rs`):
- Add `reds: u32` to the fiber/Vm context; reset to `CONTEXT_REDS` (BEAM uses 4000 — tune) on
  schedule-in.
- At the `run_until` loop-top safepoint (beside the GC + cancel checks): decrement `reds` per
  dispatched op (or per back-edge/call for cheaper accounting); on `reds == 0`, **yield** — save
  context, push the fiber to the **tail** of its run queue, return to the worker loop. Reuses the
  exact suspend/resume machinery (a voluntary park with no channel handle — a new
  `FiberState::Yielded`, or `Ready` + a "requeue me" flag).
- The worker loop treats a yielded fiber as immediately-runnable (back of queue) → round-robin.

**TDD.** (1) One long CPU fiber + many short fibers: all short ones progress before the long one
finishes (observe via a `Shared` counter / monotonic timestamps). (2) 10 k CPU-bound fibers on 4
threads all complete; no starvation. (3) Goldens byte-identical (preemption changes *scheduling*, not
*results*; output stays task-ordered via flush-on-join).

**Risk.** Medium. Per-op accounting overhead — measure; coarsen to back-edges/calls if hot. Must not
yield while `native_reentry > 0` — gate the yield on `native_reentry == 0`, same guard as `recv`.
**Reuse:** the loop-top safepoint, suspend/resume, the D2 run queue.

### D4 — Work-stealing + `wakep` / spinning-worker protocol *(Go)*

> **Status: D4a–D4d LANDED** (`--parallel` engine; cooperative untouched). The D2b single shared run
> queue is now a **per-worker `LocalQ`** (a `runnext` slot + a ring, lock class **B**) + a shared
> `global` overflow queue (in `SchedCore`, lock class **A**). `take_runnable(wid, tick)` searches:
> a periodic global pull every `GLOBAL_CHECK_INTERVAL` (61) schedules → own local → **work-steal**
> (`try_steal`: rotating-victim, ceil-half from the ring back, falling back to the victim's
> `runnext`) → a **capped global batch-grab** (Go `globrunqget`: `min(g/nworkers+1, g, CAP/2)` into
> the own local, run the first, leave the rest for siblings — one core-lock acquisition amortized over
> the batch is the contention win) → park. **Deviations from the plan below, each verified:**
> (1) **Only `take_runnable`'s batch-grab populates locals**, not `yield`/`send_wake`: a time-slice
> `yield` goes to the **global** queue (Go-faithful — preserves cross-worker fairness; routing it to
> the worker's own local would let a CPU hog re-pop itself forever, re-introducing the D3 starvation),
> and `send_wake`/`park`-requeue/`cancel_drain` also stay on `global` (immediately balanced, no spill).
> So locals are fed from `global` and rebalanced by stealing. (2) **Bounded-poll wake, NOT the full
> `wakep` StoreLoad barrier** (→ D4e): a worker that finds no work parks on `cv.wait_timeout(2ms)` and
> re-steals on wake/timeout, plus a `notify_all` after the batch-grab surplus push. Once a fiber can
> land in a local *outside* the core lock, a plain `notify_all` is lost-wakeup-prone (a local push
> between a parker's "no work?" check and its `wait`); the bounded timeout caps that to ≤2ms latency,
> **never a hang** (mirrors B3.4's accepted 50ms recv-cancel re-check). Liveness still rests on the
> always-running inline shell (decision B): `try_steal` reaches any local it needs, so no fiber is
> stranded. (3) **Deadlock predicate reads a `runnable: AtomicUsize`** (D4a) — the count of fibers
> queued anywhere (all locals + `global`) — instead of `runq.is_empty()`, since there is no single
> queue to test under the split; maintained under the core lock at every transition (seed/pop/park/
> yield/wake/drain/grab; steal is net-zero), byte-identical `DEADLOCK_MSG`. (4) **Per-queue `Mutex`,
> not lock-free CAS** — a `Fiber` is a large move-only struct, not a word-sized CAS payload; per-worker
> mutexes are uncontended in the common case and keep the correctness/test story tractable (the doc
> allows this). **Lock order is strictly B-then-A (a local is taken alone and released before the core
> lock) and A-then-C** (`send`/`park` take core then `ChannelCore.q`) → no ABBA. **1372 tests** green
> (+8: `mnsched_runnable_tracks_single_queue`, `localq_runnext_then_ring_order`,
> `take_runnable_prefers_local_over_global`, `schedule_steals_half_from_victim`,
> `steal_skips_self_and_empty_victims`, `schedule_pulls_global_every_61st_tick`, the
> `d4_worksteal_cpu_and_channel_stress` watchdog), `cargo clippy -- -D warnings` clean,
> `primes_parallel=148933` both engines, all `--parallel` goldens byte-identical, full suite ×5 +
> stress ×15 + defer/cancel race ×40 stable. 2-agent S++ concurrency panel (SRE + invariant/VM):
> zero Critical; applied both Importants (the batch-grab `notify_all` to kill a 2ms steal-latency
> cliff; `try_steal` now drains `runnext` so the predicate stays sound if a future commit ever
> populates it). **D4e (below) is the remaining owe.**
>
> **D4e — runnable-gated park (LANDED; deviates from the originally-planned Go barrier).** The poll is
> gone. **Decision:** rather than port Go's SeqCst `store(work); fence; load(nmspinning/idle)` ⇄
> `store(idle); fence; load(work)` publish/observe barrier + a `nmspinning` spinning-worker, the park
> branch of `take_runnable` now gates on the **`runnable` atomic chezzi already maintains**:
> `runnable > 0` ⇒ work exists somewhere (a local — stealable — or the sub-µs in-hand `Vec` window of a
> concurrent grab/steal) ⇒ do NOT truly sleep; take a brief bounded `cv.wait_timeout(SPIN_BACKOFF=500µs)`
> backoff that any wake `notify_all` cuts short, then re-loop (re-steal/re-grab);
> `runnable == 0` ⇒ no fiber queued anywhere ⇒ a TRUE `cv.wait` (no timeout), woken only by a sibling's
> `notify`. **Why not Go's fence.** Go needs the lockless StoreLoad fence precisely *because it has no
> global runnable counter* — `nmspinning` reconstructs "is there work + is anyone hunting it" losslessly
> without a lock. Chezzi already paid for a precise `runnable: AtomicUsize` (for the deadlock predicate),
> mutated under the core lock at every enqueue and read under that same lock immediately before
> `cv.wait`. The mutex serializes publish-against-observe — *it is* the StoreLoad barrier — so the gate
> is lost-wakeup-free by the standard locked-condvar argument (a `runnable++`/`notify` either
> happens-before the read, so the worker sees `runnable > 0` and skips the park, or after the wait is
> registered, so the `notify` reaches it). The only counted-but-momentarily-unreachable state is the
> in-hand `Vec` window — a bounded handful of `VecDeque` pushes by a non-blocked worker — so the
> backoff re-loop is bounded, not a livelock. (The first cut busy-spun with `drop; yield_now; continue`;
> the S++ SRE review flagged that as a thundering-herd lock-hammer on `runnable > 0` and an
> oversubscription hazard where spinners starve the surplus-holder — hence the bounded `wait_timeout`
> backoff, which sleeps instead of spinning while the `notify_all` still wakes it the instant the work
> lands.) This is **simpler and easier to prove than the Go barrier
> for this codebase**, with no new atomics, no fence, no per-worker park primitives. **What was NOT
> done (deferred, optional, throughput-only):** the conditioned single-wake (`notify_one` when exactly
> one fiber became runnable + an idle-worker count) that avoids the `notify_all` thundering herd — a
> pure efficiency tweak the original plan folded in via `nmspinning`; it is correctness-irrelevant and
> separable, to be added only if a benchmark justifies it (that is where a `cfg(loom)` model would earn
> its keep — the runnable-gate's only race is the locked add-vs-locked-read, which stress tests +
> a `debug_assert!(runnable == 0)` before `cv.wait` cover without loom's dev-dep + atomic-abstraction
> cost). **TDD landed:** `d4e_pingpong_no_lost_wakeup_stress` (park-heavy consumer-first workload ×25
> rounds under per-round watchdog — the lost-wakeup guard) + `d4e_wake_parked_workers_from_true_sleep`
> (isolates wake-from-`runnable==0`-sleep: one CPU-burning producer drives every other worker to a real
> `cv.wait`, then a `send` burst must wake them). 1386 tests green, clippy clean, `primes_parallel=148933`
> both engines, all `--parallel` goldens byte-identical, release stress ×4 stable. **4-agent S++
> concurrency panel** (Godot Gameplay / Solidity / Incident Response / SRE): zero Critical, zero
> lost-wakeup/hang confirmed; applied SRE's two Importants (busy-spin → bounded `wait_timeout` backoff;
> corrected a stale `runnable` doc-comment the gate's soundness depends on).

**Goal.** Load-balance across P's and wake parked workers correctly (no lost wakeups, no busy-wait).

**Changes** (`src/vm/pool.rs` + scheduler):
- **Work-stealing**: when `runnext` + local + global are empty, pick a **random victim P** and
  **steal half** its local queue (Go `runqsteal` / `runqgrab`). 4 randomized steal passes, then park.
- **Periodic global check**: every Nth schedule (Go uses `schedtick % 61`), pull from the global
  queue first to prevent starvation.
- **`wakep` protocol** *(SUPERSEDED — see the D4e blockquote above for what actually landed)*: the
  original plan was Go's enqueue-time **StoreLoad full barrier** between "publish work" and "read
  `nmspinning` / idle-P", waking a worker **only if** an idle P exists *and* `nmspinning == 0`, with
  ~one spinning worker kept alive. **Not adopted** — chezzi's precise core-lock-serialized `runnable`
  atomic makes the lockless fence unnecessary; D4e gates the park on `runnable` under the core lock
  instead (the mutex *is* the StoreLoad barrier). Kept here for the GMP-reference contrast.
- **Park/unpark**: an idle worker releases its P, parks on a semaphore/condvar, woken by `wakep`.

**TDD.** (1) Load-balance: skewed initial distribution (all fibers spawned from one fiber) still
saturates all workers — assert via per-worker counters. (2) Stress: thousands of `send` / `recv`
ping-pongs across many fibers, repeated runs, must never hang (lost-wakeup regression guard). (3)
Goldens byte-identical.

**Risk.** High — concurrency correctness (the lost-wakeup race); consider a `loom` test for the
`wakep` barrier. Steal can use a per-queue mutex first, lock-free ring CAS later. **Reuse:** Go
`proc.go` `runqsteal` / `wakep` / `stopm` as the reference algorithm.

### D5 — Dirty / blocking pool for opaque blocking native calls *(BEAM; real surface today)*

> **D5 HAS LANDED (core).** `src/vm/blocking_pool.rs` (growable: spawn-on-stall, reap idle >10 s, cap
> 512) + the offload path in `src/vm/mod.rs`: `invoke_native` intercepts an off-heap-safe blocking
> native (`native::is_blocking` — `io.read_file`/`write_file`, all `fs.*`, `time.sleep_ms`) under the
> M:N engine (gated `native_reentry == 0`), materializes its args into `Send` `NativeArg`s, suspends
> the fiber (`Vm::offload` + the `paused()` push-skip), and the worker hands it (`Disp::Offload`) to
> the pool. The pool runs it with no `Vm`/heap (`OffloadHost`), stashes the raw `NativeRet` on
> `Fiber.resume_native`, and `complete_offload`s the fiber back (inflight→runnable + `notify_all`);
> the resuming worker lowers + pushes the result and continues past the `Call`. `MnSched.inflight`
> (a 4th fiber state) is folded into the deadlock predicate (`is_deadlocked`) so an in-flight blocking
> call can't fire a false deadlock. A panic in an offloaded native is caught in the pool job and
> faulted (never a pinned `inflight` hang). The G3 starvation is fixed (`sleep_ms` ×N ≈ max not sum);
> a blocking native inside a native callback (`native_reentry > 0`) still runs inline; cooperative/
> `--interp` byte-identical. **+12 tests, 2-agent S++ panel (1 Critical + 1 Important applied).**
> **D5 owes #1 + #2 HAVE LANDED.** Owe #1 — `std.request` (`get`/`post`) + `std.process` (`cmd`) are
> classified blocking-offloadable (verified off-heap-safe: primitive args/returns, no heap/stdio touch
> → they run on the `OffloadHost`), added to `native::is_blocking`, guarded by a member-name-uniqueness
> test (bare-name classification stays sound). Owe #2 — a process-wide **timer thread**
> (`src/vm/timer.rs`: deadline min-heap + one thread) replaces the one-pool-thread-per-sleep model:
> `sleep_ms` parks the fiber on the timer (`OffloadReq.timer_ms` branches `MnSched::offload`), waking
> it at the deadline through the same `inflight`/`complete_offload` path (deadlock predicate stays
> sound). 10⁴ sleepers ≈ 1 thread; `checked_add` saturates a pathological `ms`. D6 will fold this
> timer deadline into the pollset `poll()` timeout (one blocking wait covers I/O + timers).
> **Still deferred:** the `recv`-inside-native-callback unblock (a larger change — stackful fibers /
> CPS; an accepted v1 limit, untouched).

**Goal.** A blocking native call (`std.io.read_file` / `write_file`, `std.fs.*`, `std.time.sleep_ms`)
must not pin a core-pool worker (G3, live today). Route it to a **growable blocking pool** so the
core pool keeps scheduling. This is also the answer to the `native_reentry` "recv inside a native
callback can't park" blocker — the dirty-pool worker just *blocks*, no parking needed.

**Changes:**
- A **growable blocking pool** (separate from the core pool), à la Go `spawn_blocking` / BEAM async
  pool (`+A`): spawns a thread on stall, caps + reaps idle ones.
- **Classify** native fns — pure/fast (run inline) vs **blocking** (`fs` / `io` / `time.sleep_ms`,
  flagged in the `NativeFn` table, `src/native/mod.rs`). A blocking call: park the *fiber*, hand the
  work to the blocking pool, free the core worker; on completion re-enqueue the fiber + `wakep`.
- **`sleep_ms`**: a **timer-park** (park the fiber, wake after the duration) rather than
  `thread::sleep` (`src/native/time.rs`) — ideally via the D6 pollset timeout; interim via a timer
  thread.
- **`native_reentry` guard** becomes mode-conditional: under M:N a blocking native call is offloaded
  (no fault); the cooperative engine keeps the existing guard.

**TDD.** (1) N fibers each doing `read_file` / `sleep_ms` with **N > core pool size** all complete
concurrently (wall-clock ≈ max, not sum). (2) `sleep_ms(100)` ×100 fibers on 4 threads finishes in
~100 ms, not ~2.5 s. (3) Goldens byte-identical; cooperative native-reentry fault unchanged.

**Risk.** Medium. Classification correctness (mislabel CPU as I/O → starve). `io` stdout writes must
respect decision-F flush. **Reuse:** `NativeFn` table, `guarded` / `native_reentry`, the D4 wake path.

### D6 — epoll / kqueue pollset + minimal `std.net` (TCP) *(Go netpoller)*

**Goal.** Cheap *massive* socket concurrency (10 k connections) without a thread per connection.
Build the pollset **and** the socket surface that justifies it (regular files stay on D5's blocking
pool — they are not epoll-able).

**Changes:**
- **Pollset** via the `polling` crate (cross-platform epoll / kqueue / IOCP) on one poller thread (or
  the about-to-park worker as poller-of-last-resort, Go-style). A would-block socket op registers fd
  interest + parks the fiber on the `pollDesc`; the poller injects ready fibers back onto run queues
  (`injectglist` + `wakep`).
- **Minimal `std.net`**: `tcp.listen` / `accept` / `connect` / `read` / `write` / `close`, sockets
  **non-blocking**, integrated with the pollset. New native module (`src/native/net.rs`, registered
  in `src/native/mod.rs`).
- Fold **timers** into the poll timeout (D5's `sleep_ms` timer-park rides the pollset deadline — one
  blocking `poll()` covers I/O + timers).

**TDD.** (1) Echo server: a `parallel:` accept-loop spawning a fiber per connection; drive with
**#conns ≫ #threads** (e.g. 500 conns, 4 threads) — all serviced, worker count stays core-sized. (2)
`connect` to a slow peer parks the fiber, frees the worker; others progress. (3) Timeout on a socket
read wakes correctly.

**Risk.** High + new dependency (`polling`). Platform differences handled by the crate. Largest new
surface; gate behind the net module so non-net programs are unaffected. **Reuse:** `polling`, the
park/inject/`wakep` machinery from D2/D4, decision-F flush.

---

## Cross-cutting invariants (all phases)

- **Serial engine frozen** — D1–D6 are `--parallel`-only; cooperative stays the parity oracle. Run
  the VM==interp sequential-subset parity suite every phase.
- **Decision F** — output flushed in task order on join; deterministic transcript despite concurrent
  execution. All fault-free goldens stay byte-identical.
- **Decision C** — `os.exit` hard-halts, wins over sibling fault, uncatchable by an outer `recover:`.
- **Share-nothing** — every fiber owns its heap; no value crosses a heap boundary except via
  `WireValue` copy (`Channel.send` / `spawn` args) or `Arc`'d cores (`Channel` / `Shared` /
  `Executor`).
- **B3.4 cancellation + B3.5 deadlock detection** — preserved; deadlock = all fibers parked, none
  runnable, no in-flight blocking-pool / poll work.

## Verification (per phase)

```sh
cargo build && cargo clippy            # clean
cargo test                             # unit + guiding (affected modules)
cargo test conformance                 # grammar drift (cheap to confirm)
cargo run -- run examples/primes_parallel.chz   # D1+: faster, same output
cargo run -- run examples/parallel_channel.chz  # task-ordered output unchanged
cargo run -- run examples/parallel_deadlock.chz # still faults deadlock
# D0: examples/many_fibers.chz (20k fibers) — completes, correct aggregate
# D2: a #fibers≫#threads producer/consumer that pins-then-starves on the old engine
# D5: a sleep_ms/read_file fan-out — wall-clock ≈ max not sum
# D6: examples/echo_server.chz — #conns≫#threads
```

Plus the review gate per phase: fresh test/build/clippy output + a 2-agent review panel; apply
Critical + Important findings before the completion claim.

## Open / deferred

- **`Channel.close()`** ([§11](concurrency.md)) — clean producer→consumer termination; needs a
  surface decision; pairs naturally with D2's park model.
- **Cross-nursery wakeups** ([§11](concurrency.md)) — mooted by real-thread blocking; M:N's flat run
  queues may lift it for free — revisit after D2.
- **Priority classes** (BEAM) — deferred; revisit if priority becomes a requirement.
- **Reduction constant tuning** (D3) — `CONTEXT_REDS` value + per-op vs per-back-edge accounting,
  measured under D3.
