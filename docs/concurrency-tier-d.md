# Tier-D — M:N scheduler + async I/O (phased breakdown)

> **Reading note (added 2026-08-16).** This file is a **dated design + landing record** for the Tier-D
> phase ladder, kept as written. It talks throughout about a cooperative `--serial` engine that was the
> "frozen parity oracle" and about `--parallel` as an opt-in. Both framings are historical: the M:N
> engine is now the **default and only** engine, `--parallel` is an accepted no-op alias, and `--serial`
> was **removed 2026-08-16** (`docs/future.md` §2b). Nothing in the phase ladder itself changed.

Companion to [`concurrency.md` §10](concurrency.md) (the design) the way
[`concurrency-b3.md`](concurrency-b3.md) is the companion to §9 for the B3 epic. §10 says *what*
Tier-D is and *why*; this file is the *how* — the phase ladder **D0…D6**, each independently
TDD-able, plus the **Go-vs-BEAM borrow ledger** so the split is not relitigated.

**Status: Tier-D COMPLETE through D6c.** All phases landed — D0 (O(N) ready-queue), D1 (lazy module
snapshot), D2 (D2a heap-into-`FiberCtx` + D2b M:N scheduler), D3 (reduction-counting preemption), D4
(D4a–D4e work-stealing + runnable-gated park), D5 (dirty/blocking pool + owe #1/#2/#3), and D6
(D6a netpoller + `std.net`, D6b drain-on-fault + timer-fold + non-blocking `connect`, D6c per-socket
`read`/`accept`/`write` `timeout_ms`). Per-connection `spawn` (eager injectable nursery) also landed.
The `--parallel` engine is a true Go-style GMP work-stealing M:N scheduler with BEAM-style preemption
and a netpoller; **1565 tests green**, clippy clean, `primes_parallel=148933` on both engines, all
`--parallel` goldens byte-identical, serial engine frozen as the parity oracle. **M-C implicit
nurseries shipped (2026-06-12)** — concurrency is feature-complete.

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
| **G/M/P split**, bounded P = `vm::worker_count()` (`--threads=N`/`CHEZZI_THREADS`, else `available_parallelism()`) | **Go** | Adopt (D2) | Cleanest decoupling of runnable work from threads; caps real parallelism; enables handoff + stealing |
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
| **G** goroutine | **Fiber** = `FiberCtx` (frames / stack / handlers / nurseries) **+ own `Heap` + Arc'd module snapshot** | `FiberCtx` `src/vm/mod.rs` |
| **M** OS thread | Pool worker thread (hosts a thin `Vm` shell = the execution engine) | `src/vm/pool.rs` |
| **P** processor | Per-worker **run queue** (bounded ring + `runnext`) + the run-license, count = `vm::worker_count()` (`--threads=N`/`CHEZZI_THREADS`, else `available_parallelism()`) | D2/D4 |
| LRQ / GRQ | Per-worker local queue + global overflow queue | D2/D4 |
| `runqsteal` | Steal half from a random victim | D4 |
| `gopark` / `goready` | Existing `self.suspend = Some(h)` park + `send`-side wake | `src/vm/mod.rs` recv arm |
| function-prologue safepoint | `run_until` loop-top (already checks GC + cancel) | `src/vm/mod.rs` |
| netpoller | epoll / kqueue pollset via the `polling` crate | D6 |
| sysmon / dirty schedulers | growable blocking pool for `native_reentry` blocking calls | D5 |

---

## Phase ladder

Each phase is independently TDD-able and (D1+) ships behind `--parallel`. **Serial parity goldens
stay byte-identical throughout.** Sequence: **D0 → D1 → D2 → D3 → D4 → D5 → D6.** D0 is standalone
(cooperative engine). D1 is the foundation everything else builds on.

### D0 — O(N²) → O(N) ready-queue *(cooperative engine; standalone; FIRST)* — ✅ LANDED

**Goal.** `run_scheduler` linear-scanned all live children for the lowest-index runnable one each
turn → O(N²) (50 k→2.34 s; practical ceiling ~10 k–20 k tasks). Replace with an explicit ready-queue.

**Landed:** shipped as O(N·logN) (a `BTreeSet`, lowest-index pop, preserving the old `pick_runnable`
scan order) with three verified deviations: (1) `blocked_on` is keyed by the **`ChannelCore` pointer**
(`Arc::as_ptr`), not `GcRef` — cooperative `spawn` deep-clones a channel handle aliasing one core, so a
`GcRef` key would lose every wakeup; (2) `BTreeSet` (not a FIFO `VecDeque`) keeps output order; (3)
`wake_on_send` drains every scheduler level to preserve cross-level wakeup. Deadlock still fires
byte-identically when `ready` empties with live blocked children. Cooperative-only — `--parallel`
never runs `run_scheduler`. See `src/vm/mod.rs` (`run_scheduler` / `run_child` / `channel_core_ptr` /
`wake_on_send`).

### D1 — Lightweight fiber: heap into the swappable context + lazy module snapshot *(foundation)* — ✅ LANDED

**Goal.** Kill the per-task `Vm`-rebuild cost. A fiber should be **`FiberCtx` + its own small `Heap`
+ an `Arc` to a read-only module snapshot**, faulted in lazily — not a full `Vm`.

**Landed (lazy-module-snapshot half):** the eager per-task `build_worker_modules` is replaced by a
heap-independent read-only `Arc<ModuleSnapshot>` (`snapshot_modules`/`to_snap`) pinned at each task's
`spawn` and faulted into each worker heap lazily, one module at a time on first global access
(`install_snapshot`/`fault_module`/`ensure_module_faulted`, gated by `module_faulted`). FIFO pool
unchanged → observably identical except faster. **The `Heap`-into-`FiberCtx` half was intentionally
deferred to D2a** — under the unchanged FIFO pool it has no observable effect and would risk the
cooperative engine's share-by-ref single heap; it lands with the M:N share-nothing fiber model.
Reuse: `swap_ctx`, `to_wire`/`from_wire`, `Arc<Program>`, `module_objs`.

### D2 — GMP run queues + unified M:N scheduler loop *(no stealing yet)* — ✅ LANDED (D2a + D2b)

**Goal.** Replace `run_parallel_nursery`'s "one `Vm` per task on a FIFO pool" with lightweight fibers
parking on `recv` instead of pinning threads.

**Landed (D2a — D1's deferred heap half):** `FiberCtx` now carries `heap: Option<Heap>`; `swap_ctx`
swaps it **only for M:N fibers** (`Some`), while cooperative fibers carry `None` and keep aliasing the
single `Vm::heap` (decision A — share-by-ref), so the cooperative engine is byte-identical **by
construction**. A `Fiber: Send` compile-time guard + `debug_assert!`s in `swap_ctx`/`collect` pin the
invariant; a parked M:N fiber's heap is share-nothing, quiescent, never traced cross-heap.

**Landed (D2b — the run-queue + park-on-`recv` half):** `run_mn_nursery` replaces
`run_parallel_nursery`. Each task is a lightweight **`Fiber`** (own heap + per-task
`out`/`stderr`/`module_objs`/`executors` in `FiberCtx`) seeded onto a single shared per-nursery run
queue (`MnSched`, one `Mutex<SchedCore>` + `Condvar`). Workers are thin host **shells**; an empty
`recv` **parks** the fiber (reusing the cooperative suspend/rewind-`ip` mechanism, filed into the
channel's wait set keyed by `ChannelCore` ptr) and the worker grabs the next fiber — no OS-thread
blocking. `send` (`send_wake`) enqueues the message AND re-queues parked waiters **atomically under
the sched lock**; `park` re-checks queue + cancel under that same lock to close the lost-wakeup gap
(lock order core-OUTER / channel-`q`-INNER → no ABBA). Verified deviations: (1) **one shared run
queue**, not per-worker rings — pointless without stealing, both deferred to D4; (2) the deadlock
predicate is the exact `running==0 && runq empty && parked>0 && done<total`; (3) the joining thread
runs an **inline shell that alone drains the run queue** (decision B), farmed helper shells are
fire-and-forget, so the join never depends on a bounded-pool resource (prevents the pool-exhaustion
join hang — a review Critical). Headline: 1000 consumers + 1000 producers finish in ~0.02 s.

### D3 — Reduction-counting preemption *(BEAM)* — ✅ LANDED

**Goal.** Fairness: a CPU-bound fiber must not hog a worker forever. Decrement a per-fiber **reduction
budget** at the existing safepoint; yield at exhaustion.

**Landed:** a fiber carries `reds: u32` (reset to `CONTEXT_REDS = 4000` on every schedule-in); the
`run_until` loop-top safepoint decrements it per dispatched op under the M:N engine and, at exhaustion
(with `native_reentry == 0`), sets `yield_now` and stops dispatch. `mn_worker_loop` maps that to
`Disp::Yield` → `MnSched::yield_fiber`, which requeues the fiber at the **tail** of the shared `runq`
(round-robin) under the sched lock — no `parked` bucket touched, so no false deadlock is possible.
One verified deviation: a yield reuses the recv-park suspend/rewind contract, so it must unwind
**every nested `run_until` level** without popping a result; the original gate only checked `suspend`,
so a yield deep in a call chain let `run_proto` pop a live operand-stack temp as a bogus return. Fixed
by a `paused()` helper (`suspend.is_some() || yield_now`) applied at every propagate-up gate.
Cooperative byte-identical by construction (`yield_now` gated on `mn.is_some()`).

### D4 — Work-stealing + `wakep` / runnable-gated park *(Go)* — ✅ LANDED (D4a–D4e)

**Goal.** Load-balance across P's and wake parked workers correctly (no lost wakeups, no busy-wait).

**Landed (D4a–D4d — work-stealing):** the D2b single shared run queue becomes a **per-worker `LocalQ`**
(a `runnext` slot + a ring, lock class B) + a shared `global` overflow queue (lock class A).
`take_runnable(wid, tick)` searches: a periodic global pull every 61 schedules → own local →
**work-steal** (`try_steal`: rotating-victim, ceil-half from the ring back, falling back to `runnext`)
→ a **capped global batch-grab** (Go `globrunqget`: `min(g/nworkers+1, g, CAP/2)`, run the first, leave
the rest — one core-lock acquisition amortized over the batch) → park. Verified deviations: (1) **only
the batch-grab populates locals** — a `yield` goes to **global** (Go-faithful; routing it to the
worker's own local would let a CPU hog re-pop itself forever, re-introducing D3 starvation),
`send_wake`/`park`-requeue/`cancel_drain` also stay on `global`; (2) the deadlock predicate reads a
`runnable: AtomicUsize` (count of fibers queued anywhere) instead of testing a single queue, maintained
under the core lock at every transition; (3) per-queue `Mutex`, not lock-free CAS (a `Fiber` is a large
move-only struct). Lock order strictly B-then-A and A-then-C → no ABBA.

**Landed (D4e — runnable-gated park; deviates from Go's barrier):** the poll is gone. Rather than port
Go's SeqCst StoreLoad publish/observe barrier + `nmspinning`, the park branch gates on the `runnable`
atomic chezzi already maintains: `runnable > 0` ⇒ work exists somewhere (a stealable local, or the
sub-µs in-hand `Vec` window of a concurrent grab/steal) ⇒ do **not** truly sleep, take a brief bounded
`cv.wait_timeout(SPIN_BACKOFF=500µs)` backoff any `notify_all` cuts short, then re-loop;
`runnable == 0` ⇒ a **true `cv.wait`** (no timeout), woken only by a sibling's `notify`. **Why not
Go's fence:** Go needs the lockless fence *because it has no global runnable counter* — `nmspinning`
reconstructs "is there work + is anyone hunting it" without a lock. Chezzi already pays for a precise
`runnable: AtomicUsize`, mutated and read under the core lock; the mutex *is* the StoreLoad barrier, so
the gate is lost-wakeup-free by the standard locked-condvar argument. Simpler and easier to prove than
the Go barrier for this codebase — no new atomics, no fence. Deferred (throughput-only, separable):
the conditioned single-wake that avoids the `notify_all` thundering herd.

### D5 — Dirty / blocking pool for opaque blocking native calls *(BEAM)* — ✅ LANDED

**Goal.** A blocking native call (`std.io.read_file`/`write_file`, `std.fs.*`, `std.time.sleep_ms`)
must not pin a core-pool worker (G3, live today). Route it to a **growable blocking pool** so the core
pool keeps scheduling.

**Landed (core):** `src/vm/blocking_pool.rs` (growable: spawn-on-stall, reap idle >10 s, cap 512) +
the offload path in `src/vm/mod.rs`. `invoke_native` intercepts an off-heap-safe blocking native
(one whose registry entry carries `native::Kind::Blocking`/`TimedWait`) under the M:N engine (gated `native_reentry == 0`), materializes args into
`Send` `NativeArg`s, suspends the fiber (`Vm::offload` + the `paused()` push-skip), and the worker
hands it (`Disp::Offload`) to the pool. The pool runs it with no `Vm`/heap (`OffloadHost`), stashes the
raw `NativeRet` on `Fiber.resume_native`, and `complete_offload`s the fiber back. `MnSched.inflight`
(a 4th fiber state) folds into the deadlock predicate so an in-flight call can't fire a false deadlock;
a panic in an offloaded native is caught and faulted (never a pinned hang). G3 starvation fixed
(`sleep_ms` ×N ≈ max not sum); serial `--serial` byte-identical.

**Landed (owe #1 + #2):** owe #1 — `std.request` (`get`/`post`) + `std.process` (`cmd`) classified
blocking-offloadable (verified off-heap-safe), guarded by a member-name-uniqueness test. Owe #2 — a
process-wide **timer** replaced the one-pool-thread-per-sleep model: `sleep_ms` parks on a deadline
min-heap, waking through the same `inflight`/`complete_offload` path; 10⁴ sleepers ≈ 1 thread. **D6b
later folded this timer deadline into the netpoller `poll()` timeout** (one blocking wait covers I/O +
timers); the dedicated timer thread is gone and `src/vm/timer.rs` is a shim over `poller::submit_timer`.

### D5 owe #3 — `recv` inside a native callback *(A + C landed; B rejected)* — ✅ RESOLVED

**The invariant.** `recv` can park **⟺** every frame between the fiber's entry and the `recv` is a
**VM frame** (snapshotted into `FiberCtx`). **One native Rust frame anywhere in that chain → fault** —
the suspend mechanism snapshots VM frames and returns out of `run_until`; it cannot snapshot a live
Rust stack (the `map` `for`-loop frame, `i`, the partial result). This is exactly the Go/BEAM split:
BEAM's `Enum.map` is bytecode (parks fine) and only NIFs (C) hit the wall; chezzi's only mistake was
implementing its HOFs in native Rust where BEAM implemented them in the language.

**Path A LANDED (primary fix).** The suspendable iteration HOFs `map`/`filter`/`fold`/`reduce` are now
chezzi source in `std/iter.chz`. Reached through `iter.map(xs, f)` the per-element callback runs through
**pure VM frames**, so a blocking `recv` (or `sleep`/socket op) inside `f` **parks** under `--parallel`
instead of faulting — zero Rust runtime change, generic return inferred from the closure alone. Native
`xs.map(f)` is **kept** as the faster non-blocking path (BEAM's NIF-vs-`Enum` split). `each` deferred (a
void fn-type param `fn(T)` doesn't parse yet — use a bare `for`).

**Path C LANDED (the intrinsically-native islands).** A blocking `recv`/`sleep_ms`/socket op reached
inside a native callback (`native_reentry > 0`) under `--parallel` no longer faults — the worker thread
**demotes**: accounts the op as a 5th fiber state (`blocked_native` for recv / `inflight` for
sleep+socket), spins up **one raw replacement OS thread** (`spawn_replacement_worker`, net-zero worker
count — Go's `handoffp`), and **blocks in place** (`ChannelCore.cv` for recv, `thread::sleep` for sleep,
`wait_fd_ready`/`libc::poll` for sockets), resuming in place on a sibling's `send`/readiness. The narrow
deadlock false-positive (#1) was resolved by registering each demoted fiber's channel and vetoing the
predicate if any has a queued value. **Path B (stackful fibers) rejected** — a substrate rewrite
(unsafe stack switching, memory-per-fiber, native re-entrancy re-audit); A + C deliver the same
observable behaviour at a fraction of the risk.

**`Shared.update` same-box hold-and-wait — WON'T FIX by design.** `update(f)` holds the box's lock
across `f`; if `f` blocks on a `recv` needing the **same** box, any such sender deadlocks. This is the
classic hold-and-wait-while-blocking deadlock *every* language with locks + blocking hits (Go detects
only the global case, golang/go#13759; Rust flags it statically via `clippy::await_holding_lock`; BEAM
avoids it structurally with no shared locks). chezzi's rule mirrors BEAM's: **don't block on a value
that needs the same `Shared` box** — `update` is a fast RMW, never park inside it. `update` is kept
deliberately: it is the only atomic read-modify-write, so removing it for bare `get`/`set` would
reintroduce a silent lost-update race (a worse, non-local footgun than this narrow same-box deadlock).
Future: this may be surfaced via a `share` binding modifier and/or a
lint/runtime fault to turn the silent hang loud.

### D6 — epoll / kqueue pollset + minimal `std.net` (TCP) *(Go netpoller)* — ✅ LANDED (D6a–D6c)

**Goal.** Cheap *massive* socket concurrency (10 k connections) without a thread per connection. Build
the pollset **and** the socket surface that justifies it (regular files stay on D5's blocking pool —
not epoll-able).

**Landed (D6a — netpoller + `std.net`):** the pollset (`src/vm/poller.rs`, `polling` crate: one poll
thread + an fd→parked-fiber registry) and the full non-blocking `std.net` surface
(`connect`/`listen`/`accept`/`read`/`write`/`close`/`addr`) ship. Sockets are `Channel`-shaped heap
handles (`Obj::Socket(Arc<SocketCore>)` / `Obj::Listener(Arc<ListenerCore>)`, a `WireValue` arm so a
handle crosses to a spawned fiber, GC-trace-nothing). A would-block op rewinds `ip`, sets
`Disp::PollPark`; `poll_park_offload` accounts it as **`inflight`** (reusing D5's counter → the deadlock
predicate is unchanged) and registers fd interest; the poll thread injects it back via `complete_offload`
on readiness. A per-`SocketCore` `in_flight` guard rejects a second concurrent op on a shared socket
(oneshot epoll) with a clean fault (review Critical). Off `--parallel`, a would-block op fails loud.
Headline: `examples/echo_server.chz` services **100 conns ≫ core workers** in one `parallel:`.

**Landed (D6b):** three follow-ups closed the D6a gaps. (1) **Drain-on-fault** (`poller::drain_sched`):
re-injects every fiber parked on the faulting nursery's sockets; the re-injected fiber unwinds at
`run_until`'s loop-top cancel check **before** its rewound op re-runs, so the fault propagates instead
of hanging the nursery. (2) **Timer fold:** the `sleep_ms` min-heap moved onto the netpoller poll thread
— one `wait()` bounded by the nearest deadline. (3) **Non-blocking `connect`** (`socket2`): an
`EINPROGRESS` handshake parks on writability (`pending_connect`, the connecting stream stashed non-heap
in `FiberCtx`), finished via `finish_connect` (`SO_ERROR`); loopback completes synchronously, top-level
fallback blocks with a 10 s cap, `connect` inside a native callback fails loud. A register-vs-cancel
race was closed by serializing register/deregister/`drain_sched`/fire under the registry lock.

**Landed (D6c — per-socket timeouts):** an optional trailing `timeout_ms` on `read`/`write`/`accept`
parks on the netpoller with a deadline (`Parked.deadline`); `fire_due_socket_timeouts` re-injects the
fiber with `poll_timed_out` set so the rewound op returns `Err("timeout")`; readiness wins ties
(`examples/socket_timeout.chz`).

**Per-connection `spawn` LANDED (eager injectable nursery, `--parallel` ≥2 cores).** A nested
`parallel:` entered inside a live fiber is now **eager**: `EnterNursery` builds the `MnSched`
immediately (`activate_eager_nursery`, `body_open=true`, spawns ONE dedicated **raw OS thread** as the
body drainer), a `spawn` **injects** a live fiber into the running sched (`MnSched::inject`, the
`complete_offload` twin), and `JoinNursery` closes the body + runs the inline join worker to drain +
join the drainer. A `body_open` flag holds termination open + vetoes the deadlock predicate while the
body may still inject (always `false` on the lazy/top-level path → D2b byte-identical). So the acceptor
`spawn`s `handle(conn)` per connection and keeps accepting while handlers run on the drainer. **Why a
raw thread, not the pool:** the eager body has no inline worker until the join, so the drainer is the
sole liveness guarantee during the body — a bounded-pool helper hangs on a 1-core box or nested eager
nurseries (a 2-agent panel reproduced exactly those hangs in the first pool-farmed cut). **v1 limits:**
(1) ≥2 hw threads (the eager inner join blocks the parent's outer worker — decision B); (2) bounded
accept loops only (graceful shutdown is future work); (3) a handler signalling the acceptor via a
Channel is a cross-nursery wakeup (handlers reach clients via sockets, which works).

---

## Cross-cutting invariants (all phases)

- **Serial engine frozen** — D1–D6 are `--parallel`-only; the serial `--serial` VM stays the parity oracle. Run
  the serial-vs-M:N sequential-subset parity suite every phase.
- **Decision F** — *(SUPERSEDED FOR THE CLI, 2026-07-13 — Interactive CLI milestone: `chezzi run` STREAMS,
  so cross-task output order is nondeterministic and a print is line-atomic. Everything below still
  describes the BUFFERED sink, which every test helper / golden / parity run uses — it remains the
  byte-identical serial-vs-M:N oracle. Archival design record; do not read it as a user guarantee.)*
  Output flushed in task order on join; deterministic transcript despite concurrent
  execution. All fault-free goldens stay byte-identical. The terminal (lowest-index propagating) fault
  ALSO flushes its buffered output at its task-order slot so a faulting task's partial output is not
  dropped; higher-index racy faults and `Cancelled` still drop (no deterministic slot position). This
  reaches byte-for-byte oracle parity **only when the faulting task is the nursery's sole
  output-producer** — with additional output-producing siblings the M:N transcript can still diverge
  from serial's strict stop-at-first-fault order (a sibling reaching `Done` before the cancel-trip
  keeps output serial never produced; `Fault`-vs-`Cancelled` classification is itself a race), a
  pre-existing nondeterminism the buffer-and-flush model cannot reconcile and does not assert. The
  **nursery deadlock-abort path is stronger**: `SchedCore::flag_deadlock` records each still-parked
  fiber with a DISTINCT `TaskOutcome::Deadlocked` outcome carrying that fiber's OWN buffered
  stdout/stderr, and `reduce_task_slots` flushes EVERY parked buffer at its task-order slot (not just
  the lowest-index one, as with a real `Fault`). So a deadlock with TWO-OR-MORE parked fibers preserves
  a higher-index printer's output too — full serial parity, not just the sole-producer case. The
  distinct outcome (never coexisting with a real `Fault`/`Exit`, which trip `terminate` first) is what
  lets the reduce flush all parked buffers without touching the real-fault multi-fault ordering above.
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

- ~~**`Channel.close()`**~~ — **LANDED** (branch `feat/channel-close`). Clean producer→consumer
  termination: `close()` (idempotent, wakes all receivers via `MnSched::close_wake`), `send`/`recv`
  after close fault, **`for v in ch:`** blocking iteration (drains then ends on close, Go's
  `for v := range ch`), and **`try_send(v) -> bool`**. `closed` folded into the queue mutex
  (`ChanState`) so the park-gap re-check is TOCTOU-free. See PROGRESS.md.
- **M-C implicit nurseries** — shipped (2026-06-12): bare `spawn` legal anywhere; every function body /
  module top level joins its tasks at `return`/end. See `docs/concurrency.md §10`.
- **Cross-nursery wakeups** ([§11](concurrency.md)) — **M:N (`--parallel`) RESOLVED, cooperative
  pending.** The circular outer-sibling case (`examples/parallel_cross_nursery_circular.chz`) now
  completes under `--parallel`: one VM-global `MnSched` holds a `Vec<JoinScope>` flat scheduler (each
  nested nursery = a scope enlisted into the same global run queue; the inline owner returns on a
  scope-scoped stop having drained the GLOBAL queue), and a nested builder EARLY-ENLISTS the outer
  nursery's still-pending siblings (so the nested owner runs them — the cross-nursery wake) while
  DEFERRING each enlisted scope's output flush to its OWN `JoinNursery` (per-nursery flush order →
  two-engine parity for non-blocking nested spawns). The cooperative `--serial` engine still
  serializes nested levels, so the same program **still faults `deadlock` on `--serial`**; the
  cooperative-engine flatten is a **separate, later commit**. Workaround on `run`: siblings in ONE
  nursery (doc case C). The fix also routes the inline outer-body's own `send`/`close` through the held
  sched (`..._inline_send.chz`/`..._inline_close.chz`), runs a `spawn:` issued *after* the enlist
  (`..._late_spawn.chz`), and makes the enlist atomic; genuine deadlocks still fault (the predicate
  vetoes only while every incomplete scope is *awaiting the builder's join*). **Wake-side only:** a
  *blocking* recv issued directly in the inline body (case B) still faults — put it in a `spawn:`. Eager
  (per-connection) nurseries run on a private sched; a wake OUT OF an eager body (child→parent) is now
  routed via `MnSched::parent_wake` (gaps.md B5, `..._nested_send_to_outer_recv.chz`), but a wake INTO an
  eager body (parent→child) + sibling-eager→sibling-eager remain a separate limit (timing-divergent).
  **Independent / normal multi-level nesting RUNS** (no "2+ enlisting levels" gate): any
  depth of nested `parallel:` with sibling + late `spawn:`s matches coop; a late `spawn:` into a middle
  nursery runs on the held sched as a fresh trailing scope (`register_scope_seeded`, atomic). The only
  residual M:N divergence is a genuinely-CONTENDED shared channel (2+ live receivers racing one channel
  across scopes) — concurrent-divergent by design, never panics/hangs.
- **Priority classes** (BEAM) — deferred; revisit if priority becomes a requirement.
- **Reduction constant tuning** (D3) — `CONTEXT_REDS` value + per-op vs per-back-edge accounting.
