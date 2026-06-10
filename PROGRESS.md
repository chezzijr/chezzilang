# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

Core language is feature-complete through **M18** plus several gap-closing passes. Concurrency
**C1 + C2 + C3 + C4** have landed (both engines), plus the **`Executor` escape hatch** (C5's
sequential subset) with **program-exit auto-drain** (C5 / A2) and the C5 checker refinements. So
`spawn` / `parallel:` / `Channel[T]` / `Shared[T]` / `Executor` all run on **both** engines.
**Group B's B1 + B2 (cooperative fibers + blocking `recv`) have now landed on the VM engine**: a
`recv` on an empty channel suspends the running fiber and the scheduler resumes it when a sibling
`send`s, so mid-flight producer/consumer works (`examples/channel_block.chz`). **`Channel.try_recv()`
(A1) — the non-blocking poll — now ships on both engines** (`examples/try_recv.chz`). **B3.0 — the
wire-format airlock — has now landed** (VM): the task-airlock `deep_clone` is implemented as a
`WireValue` serialize → reconstruct round-trip (`src/vm/wire.rs` + `Vm::to_wire`/`from_wire`),
byte-identical to the old direct deep-copy. **B3.1 — cores out of the heap — has now landed** (VM):
`Channel`/`Shared`/`Executor` data moved out of the GC heap into `Arc<…Core>` holding `WireValue`
(`src/vm/core.rs`), so the heap keeps only an `Obj::X(Arc<…Core>)` handle and a crossed core is shared
(not copied). The airlock serializes at the core boundary now; `children()` was *rewritten* (not
dropped — single-thread cores still embed `Handle(GcRef)`s) to keep queued strings/closures rooted.
**B3.2 — `Arc<Program>` + isolated worker-VM construction — has now landed** (VM): `program: Rc<Program>`
→ `Arc<Program>` (read-only sharable across workers), plus `Vm::spawn_worker` / `Vm::run_task_isolated`
— build a fresh worker `Vm` with its **own heap**, wire-copy a `spawn`'d function/closure task's
args+captures IN (callee lowered to `ProtoId` + wire'd captures, never a parent-heap handle), run it
**synchronously** (no threads), and wire result + per-worker `out`/`stderr` back. Cross-heap safety is
**enforced** (`WireValue::has_handle` + `Vm::ensure_crossable`): a `str`/closure value crossing — which
would be a dangling `GcRef` in another heap — is a clean fault, not silent corruption; method tasks are
gated off (a worker's `module_objs` is empty). All `#[allow(dead_code)]` until B3.3's `--parallel`
wires it in (decision A keeps the cooperative engine the default through B3.2). Still single-thread,
behavior byte-identical. Latest suite: **1292 tests** green (1287 + 5 new B3.2 units: distinct-heap /
result+out / program-Arc-sharing / str-rejection / method-rejection; unit + parity + `cargo test
conformance`), `cargo clippy -- -D warnings` clean.

**B3.3a — `str` crosses the airlock by value — has now landed** (VM): an owned-bytes
`WireValue::Str(Box<str>)` arm replaces the by-reference `Handle` for `str` in `to_wire`/`from_wire`/
`display_wire`/`collect_core_gcrefs`, so a `str` (and any data containing it) now crosses a worker
boundary instead of being rejected as a dangling `GcRef`. Parity-safe — `str` is immutable, value-
compared, has no identity operator — so a fresh handle on reconstruction is unobservable; all goldens
byte-identical.

**B3.3b — the G1 module-globals checker gate — has now landed** (checker): a reassignment of a module
global reachable, directly or transitively through free-function calls, from a `spawn` task is a type
error (*"cannot mutate module global '…' from a parallel task; use Shared[T]"*). Flow-scoped to spawn
reachability; scope-aware name resolution (params/`let`/`for`/`match`/closure/comprehension binders, so
a local shadowing a free fn or global is never mis-flagged); descends `recover:` blocks. Direct in-
`spawn:`-block writes stay caught by the existing `is_captured` gate. Reviewed by a 4-agent S++ panel
+ a cold pass (caught and fixed a false-positive on shadowed spawn targets and a `recover:`-block
false-negative before they shipped). Two indirect-dispatch gaps documented (global-closure spawn
target, method chains) → B3.3-threads. Latest suite: **1306 tests** green (unit + parity + `cargo test
conformance`), `cargo clippy` clean.

**B3.3c + B3.3d — worker module-graph reconstruction — have now landed** (VM, single-thread, parity-
preserved): the two remaining B3.3 "owes". **B3.3c (read-only `home` snapshot):** `Vm::build_worker_modules`
snapshots the parent's initialized `module_objs` into the worker heap (two-pass — alloc module objs,
then map globals), so a spawned task can read post-init module globals and call sibling/imported free
functions. It **snapshots, never re-inits** (re-running a toplevel would duplicate prints/I/O). The map
is the load-bearing GcRef-safety boundary: `map_global_value` rebuilds every `Func`/`Closure`/`Module`/
`Native` explicitly over the worker's home and **recurses structurally through containers**, so a
`[fn …]` handler list or `{k: fn …}` dispatch map cannot smuggle a parent-heap `GcRef` into the worker
(pinned by `worker_calls_through_global_fn_container`); only pure data + `Channel`/`Shared`/`Executor`
cores take the exact wire round-trip. **B3.3d (method tasks):** `run_task_isolated` lowers `spawn obj.m()`
to `Lowered::Method` (recv + args by wire) and dispatches via `do_method_call` against the rebuilt
`module_objs`; a method that blocks on `recv` faults cleanly (no scheduler in a sync worker). Still
`#[allow(dead_code)]`/test-only until the `--parallel` flip wires it onto threads. Latest suite:
**1312 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean. Reviewed by a
2-agent panel (caught + fixed a container-of-callables GcRef-smuggle and a method-suspend pop underflow
before they shipped).

**B3.3-threads — real OS threads behind `--parallel` — has now landed** (VM): the thread-flip. A new
`--parallel` flag (`chezzi run --parallel`, VM-only; the cooperative single-thread engine stays the
**default** per decision A) sets `Vm.parallel`, switching `join_nursery` to `run_parallel_nursery`,
which runs a nursery's tasks on a **bounded OS-thread pool** (`src/vm/pool.rs` — one process-wide
pool of `available_parallelism()` threads, each with the 256 MiB VM stack). The joining thread runs
`tasks[0]` inline (decision B — **parent participates**, so nested `parallel:` never explodes the
thread count) and farms the rest to the pool; results join, each worker's `out`/`stderr` flushes in
**task order** (decision F — deterministic despite concurrency), and the first fault propagates.
`run_task_isolated` was split into `prepare_worker` (parent-heap half) + `ReadyWorker::run`
(thread-side half) — the prepared worker `Vm` **moves** onto a pool thread (`Vm` is `Send`: plain
data + `fn` pointers + `Arc<…Core>`, proven by a 2-thread unit test). A blocking `recv` under
`--parallel` waits on a real `ChannelCore` **condvar** (`send` wakes it) instead of parking a fiber;
**`Shared.update` now takes a per-core `update_lock` under `--parallel`** so concurrent
read-modify-writes can't lose each other (a lost-update race the first cross-thread golden caught —
`Shared[T]`'s whole contract is serialised writes). Worker `host` inherits the parent's read-only
args+env (stdin stays inert — a consumable stream isn't shared). Deterministic-by-construction
goldens: `examples/parallel_shared.chz` (N threads bump one `Shared` → exact count) and
`parallel_channel.chz` (a collector recv-blocks across threads, sorts → fixed order). Every existing
golden + the 3-way VM==interp parity stays on the default engine, **byte-identical green**. Latest
suite: **1319 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean.
**Still owed (later phases):** `Executor` doesn't yet ride the pool + the A3b `submit`-capture gate
(B3.6).

**B3.4 — cancellation + cross-thread `os.exit` — has now landed** (VM, `--parallel`). Each worker
`Vm` carries a per-nursery `cancel: Arc<AtomicBool>` (cloned in by `run_parallel_nursery`) plus a
`cancelled` latch. `ReadyWorker::run_outcome` classifies each task into a `TaskOutcome`
(`Done`/`Cancelled`/`Exit{code}`/`Fault`); the **first sibling to fault or `os.exit` trips the flag**
(`Vm::trip_cancel`), and the join scans outcomes in task order — flushing `Done`/`Exit` output and
propagating the lowest-index `Exit` (→ parent `pending_exit`, a hard halt with the child's code) or
`Fault` (normal unwind, so an outer `recover:` still catches it). Running siblings observe the flag
at the **dispatch back-edge** (`run_until` loop top, beside the `gc_stress` check, gated by
`!self.cancelled` so a cancelled task's `defer`s still run) and a **`recv` `wait_timeout`
re-checking loop** (50ms) — the latter chosen over a separate cancel condvar because the faulting
worker can't know which channel cores siblings park on; the bounded re-check eliminates the
lost-wakeup hazard (risk #2) at a ≤50ms abort-latency cost. So the first child fault now **aborts
running siblings** (a recv-blocked sibling whose producer faults no longer hangs the join), and a
child `std.os.exit(code)` halts the whole process cross-thread with the right code. `recover:` /
`defer` compose — crucially, the cancel sentinel **bypasses `recover:`** (a cancelled task must die,
not resume) while still running `defer`s via `unwind_deferred`, on *both* the back-edge and recv
paths. An `os.exit` **wins over** any sibling fault regardless of index (a hard halt is never demoted
to a catchable error). New: `examples/parallel_cancel.chz`. Reviewed by a 2-agent concurrency/safety
panel: caught + fixed three real defects before commit — (1) the cancel sentinel was catchable by a
worker-internal `recover:` and skipped `defer`s on the CPU path; (2) `os.exit`-vs-fault precedence;
(3) an `Arc::try_unwrap` join race (a finished pool thread still holding a `results` clone) → now
`mem::take` under the lock. Latest suite: **1328 tests** green (unit + parity + `cargo test
conformance`), `cargo clippy` clean. Single-level cancel only — nested-nursery cancel propagation is
a documented, deferred limitation (`docs/concurrency-b3.md`).

**B3.5 — nursery-local deadlock detection under threads — has now landed** (VM, `--parallel`). Under
B3.4 a genuinely all-blocked nursery *hung* (the `recv` re-check only aborted on *cancel*); now each
worker `Vm` also shares a per-nursery `DeadlockWatch` (`Mutex<{blocked, live, epoch, confirms,
dead}>`, cloned in by `run_parallel_nursery` like the cancel flag). A blocking `recv` runs a
**barrier-confirm** detector (decision D): a parked worker "confirms empty" only when every still-live
sibling is parked (`blocked == live`) and does so at most once per `epoch`; any progress — a `send`,
a successful pop, a park-count change, or a task finishing (`task_finished` decrements `live`) — bumps
`epoch` and resets `confirms`. When `confirms == live`, every live worker independently re-checked its
own channel empty in the *same* epoch with no intervening progress ⇒ no message exists and no sibling
can send ⇒ fault `deadlock` (the **byte-identical** message the cooperative scheduler uses, now a
shared `DEADLOCK_MSG` const). This is immune to the "message delivered, consumer hasn't popped yet"
false-positive a plain blocked-count detector hits: a worker holding a deliverable message pops it
instead of confirming. **Lock discipline:** the watch mutex and a channel `q` mutex are never held
simultaneously (each recv phase takes one lock at a time; `send` bumps the epoch then releases before
pushing) — no lock-order cycle. Soundness rests on a `--parallel` nursery being the only thing running
(the parent thread is inside `run_parallel_nursery`), so its own live tasks are the only possible
senders. Five new tests (the cooperative all-blocked golden ported to `--parallel`, a near-miss + a
3-task chained relay that must NOT false-positive, a finished-task-strands-sibling case, all behind a
5s watchdog so a regression fails loudly instead of hanging) + `examples/parallel_deadlock.chz`.
Reviewed by a 2-agent concurrency panel (Solidity + SRE): detector logic confirmed sound (lock
ordering, no false-positive across stress runs, no missed-wakeup, counter integrity, poison-tolerant);
documented the residual hangs (Go-like, decision D) — deadlocks spanning nurseries / involving
`Executor`, an orphaned message no live sibling reads, and the **G3 saturated-pool** case (a sibling
still *queued* counts toward `live` but never parks, so the nursery waits for a slot rather than
faulting — counting a queued task as live is the deliberate no-false-positive choice). Latest suite:
**1332 tests** green (unit + parity + `cargo test conformance`), `cargo clippy` clean.

**B3 is decomposed into a persistent, multi-session plan.** Tier-C OS-thread multicore (B3) — with
B4 (real `Shared`) and B5 (real `Executor` pool) folded in, since under shared-nothing threads they're
the same machinery — is broken into seven TDD phases **B3.0…B3.6** in
**[`docs/concurrency-b3.md`](docs/concurrency-b3.md)** (validated shared-nothing architecture,
decisions A–G, risk register, per-phase TDD focus). The surface of `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` stays **unchanged**.

**B3.6 — `Executor` on the pool + the A3b `submit`-capture sendability gate — has now landed** (VM +
checker, `--parallel`). **A3b (checker):** `Executor.submit`'s closure runs on a pool thread, so its
captures cross the airlock exactly like a `spawn` task's — the `Ty::Executor` `submit` arm now pushes a
`capture_floor` (at the current scope depth) around the argument check, so the pre-existing
`infer_ident` read gate flags a non-sendable captured binding (a `Ref`, a function-local closure) while
the closure's own params/locals stay task-local. **VM:** a new `WireValue::Closure { proto, captured,
home }` arm crosses a submitted closure **by value** (proto via the shared `Arc<Program>`, captures
wired recursively, `home` as a `module_objs` index — no heap-local `GcRef`); `Vm::wire_callable`
produces it at `submit` **only under `--parallel`** — the cooperative default engine keeps crossing the
closure **by handle** (`to_wire` → `Handle`) so its drain on the same heap shares captures by reference
(a mutation between `submit` and drain stays observable, matching the interp oracle — a by-value snapshot
would break `VM == interp` for the sequential subset, decision A; caught in review). `from_wire` rebuilds
the `Closure` over the worker's reconstructed home, and `collect_core_gcrefs`/`has_handle`/`display_wire`
gained matching arms. Under
`--parallel`, `shutdown` (and the program-exit autodrain, which calls it) drains the whole queue under
the core lock then farms the tasks to the bounded pool via a new engine-agnostic
`run_workers_on_pool` (extracted from `run_parallel_nursery` — the nursery and executor drains now
share one farm/join/flush core); each executor task gets a fresh per-drain cancel flag (first fault
aborts siblings, matching the cooperative inline `r?`) but **no** `DeadlockWatch` (decision D — an
`Executor`-spanning deadlock is an accepted hang). Cooperative drain stays inline and byte-identical
(decision A oracle). New: `examples/executor_pool.chz` (submit→pool-drain→sort, same output on both
engines); tests `golden_executor_pool_chz_matches_expected`, `executor_submitted_closure_captures_by_value`,
`executor_cooperative_submit_shares_captures_by_reference` (the decision-A regression pin), and six
checker A3b tests (`submit_{non_sendable_capture,captured_closure,captured_closure_through_nested_closure}_rejected`,
`submit_captured_{channel,int}_ok`, `top_level_closure_submitted_ok`). Latest suite: **1341 tests** green
(unit + parity + `cargo test conformance`), `cargo clippy` clean. Reviewed by a 2-agent panel
(concurrency/VM + checker); the C-01 cooperative-snapshot regression it caught is fixed + pinned.

**With B3.6 landed, the B3 epic (B3.0…B3.6) is complete** — `spawn` / `parallel:` / `Channel` /
`Shared` / `Executor` all run on real OS threads behind `--parallel`, surface unchanged. **Next
frontier:** **Tier-D** (M:N scheduler + async-I/O pollset), designed in
**[`docs/concurrency.md` §10](docs/concurrency.md)** and now **broken down into seven TDD phases
D0…D6** in **[`docs/concurrency-tier-d.md`](docs/concurrency-tier-d.md)** — Go-style GMP work-stealing
skeleton + BEAM-style reduction-counting preemption & dirty pool for opaque blocking native calls
(full Go-vs-BEAM borrow ledger in that file). **D0 has landed** — the cooperative scheduler's
O(N²) per-turn linear scan (`pick_runnable`) is replaced by an explicit per-nursery ready-set
(O(log N)/turn), so 50k cooperative fibers run in ~tens of ms instead of seconds. **D1's
lazy-module-snapshot half has landed** (see below). **D2a has landed** — D1's deferred other half:
`Heap` is now part of the swappable `FiberCtx` as `heap: Option<Heap>`, swapped only for M:N fibers
(`Some`); cooperative fibers carry `None` and keep aliasing the single `Vm::heap` (decision A —
share-by-ref), so the engine stays byte-identical by construction. D2a was the parity-preserving prerequisite that made
a `Fiber` self-contained + `Send` so D2b could park it across worker threads. **D2b has landed** —
the `--parallel` engine is now a true M:N scheduler: lightweight fibers (own heap, share-nothing)
multiplexed over the bounded pool, **parking on `recv` instead of pinning OS threads**, so a
`#fibers ≫ #threads` producer/consumer workload completes instead of starving (1000 consumers +
1000 producers in ~0.02 s). One shared per-nursery run queue + park set (`MnSched`); `send` enqueues
and re-queues parked waiters atomically (lost-wakeup-safe); deadlock is the exact predicate
`running==0 && runq empty && parked>0 && done<total`; the joining thread runs an inline shell that
alone guarantees completion (decision B), so the join never waits on a bounded pool resource (no
nested/concurrent pool-exhaustion deadlock). The legacy condvar `recv` + `DeadlockWatch`
barrier-confirm detector were retired. **1361 tests** green (5× full-suite + 60× the cancel/defer
race + 10× the headline, all clean), `cargo clippy` clean, `primes_parallel=148933` both engines;
reviewed by a 4-agent S++ panel + cold pass — two Criticals found (a defer-on-cancel test race and a
nested pool-exhaustion join hang) and both fixed. See the changelog bullet below.
**D3 is next** — reduction-counting preemption (BEAM-style `reds` budget at the `run_until`
safepoint, yield-to-tail) for fairness. Items *not* in B3–B5
(cross-nursery wakeups, recv-in-native-callback, `Channel.close()`) are documented in
**[`docs/concurrency.md` §11](docs/concurrency.md)**. Full A/B breakdown: §9.

> **DECISION — do NOT build interp B1/B2 (suspendable tree-walker). This is a deliberate non-goal,
> not a TODO.** The interpreter stays frozen at the **sequential concurrency subset** and serves as
> the **differential-testing parity oracle** for the non-blocking language surface (its real value:
> catching VM / GC / compiler bugs). Giving it suspendable execution would need stackful coroutines
> or a full CPS rewrite of `eval` — a large, risky cost to cover a narrow slice the oracle does not
> need. **The VM is the sole concurrent engine.** Future sessions: spend effort on B3/B4/B5, not on
> closing this gap. Revisit only if interp maintenance ever costs more than the bugs it catches.

**Parity contract (narrowed, intentional):** the engines agree on the **sequential subset** —
including all *non-blocking* `parallel:` / `spawn` / `Channel` / `Shared` / `Executor` programs
(C1–C5 goldens, byte-identical, parity-tested). **Suspendable concurrency (blocking `recv`) is
VM-only by design**: under `--interp` a blocking `recv` faults `deadlock`, pinned by
`interp::tests::channel_block_chz_faults_deadlock_on_interp` vs the VM golden
`golden_channel_block_chz_matches_expected`. This divergence is the stated contract, not a bug.

**Known VM v1 limits (acceptable; not parity issues):** a blocking `recv` cannot suspend inside a
native callback (list HOFs, `sort`, `compare`/`hash`/`str` hooks, `Shared.update`, the executor
drain, or a `defer`red call) — it faults `deadlock` (the callback's loop/recursion state lives on the
host stack, not in a fiber); and a fiber in an outer nursery cannot be woken by progress in an inner
one (structured-concurrency scoping).

**Group A status (sequential refinements, no engine rewrite):** **A2 (`Executor` program-exit
auto-drain) is done** (this session, both engines). **A3a** (reject a non-sendable read smuggled
through a *nested closure* in a `spawn:` block) was found **already enforced** — emergent from the
persistent `capture_floors` + the `infer_ident` read gate — and is now **pinned by a regression
test**. **A1** (`Channel.try_recv`) — originally dropped (its mid-flight-producer scenario needed the
engine), **now shipped on both engines** once B1/B2 unblocked it (a non-blocking poll runs identically
on the interp, so it stays parity-tested). **Still dropped:** **A3b** (`Executor.submit` capture gate)
— `submit` runs the closure in-heap at the drain, so gating it now would wrongly reject valid programs
(lands with Group B).

**Permanent non-goals:** **interp B1/B2 (suspendable tree-walker)** — see the DECISION box above;
`yield`/generators, variadic args, Level-3 dynamic `cdylib`/C-ABI FFI, bignum (`i64`-only — every
overflow is a recoverable fault; binary work → a future `bytes` *sequence*, no `byte`/`u8` scalar).

---

## Done (newest → oldest)

Each landed TDD, both engines in lockstep, with a golden + parity `examples/*.chz`. Git has the detail.

- ✅ **Concurrency D2b — M:N scheduler: park-on-`recv`, not thread-per-task** (`--parallel` engine).
  Old: one full worker `Vm` per task on a bounded FIFO pool; an empty `recv` **blocked the whole OS
  thread** on a condvar, so `#blocked-tasks > #pool-threads` starved/hung. Now: tasks are lightweight
  **fibers** (each owns its `Heap` + per-task `out`/`stderr`/`module_objs`/`module_faulted`/`executors`,
  all carried in `FiberCtx` and swapped via `swap_ctx` — the D2a foundation) multiplexed over the pool.
  A new `MnSched` (one `Mutex<SchedCore>` + `Condvar`) holds a shared run queue + a per-`ChannelCore`
  park set + task-order outcome slots. `mn_worker_loop` (the cross-thread generalization of the
  cooperative `run_child`): pop a fiber → `swap_ctx` in → `start_task`/`run_until(0)` → on empty
  `recv` PARK it (reuse the cooperative suspend/rewind-`ip` mechanism, file into the channel's wait
  set) and grab the next; `send` (`MnSched::send_wake`) enqueues the message **and** re-queues parked
  waiters **atomically under the sched lock** (core-OUTER / channel-`q`-INNER everywhere → no ABBA);
  `park` re-checks the queue **and** cancel flag under that same lock to close the check-then-park
  lost-wakeup gap. Deadlock is the exact predicate `running==0 && runq empty && parked>0 && done<total`
  (no barrier-confirm epoch dance — a single coordinator has global knowledge), reusing `DEADLOCK_MSG`.
  Decision F flush + `Exit`-over-`Fault` precedence factored into `reduce_task_slots` (shared with the
  legacy executor-drain path). The joining thread runs an **inline shell that alone drains the whole
  run queue** (decision B), so liveness never depends on a bounded pool resource — farmed helper shells
  are fire-and-forget, never joined, which kills the nested/concurrent pool-exhaustion join hang. The
  legacy condvar-`recv` branch + `DeadlockWatch`/`WatchState`/`task_finished` were retired. Headline:
  1000 consumers + 1000 producers on the core-sized pool finish in ~0.02 s (would starve on the old
  engine). **1361 tests** (incl. `mnsched_*` mechanics + park-gap regressions, `mn_many_blocked_consumers_complete_without_starving`,
  `mn_thousand_fiber_pipeline_completes`); 5× full-suite + 60× the defer/cancel race + 10× headline,
  all green; `cargo clippy` clean; `primes_parallel=148933` both engines; all `--parallel` goldens
  byte-identical. 4-agent S++ review panel + cold pass: **two Criticals found and fixed** — (1) a
  `parallel_defer_runs_on_cancelled_sibling` race (a sibling fault could trip cancel before the
  consumer registered its `defer`; fixed by synchronizing the test with a start-token, matching the
  Go semantic that an unregistered defer doesn't run), (2) the nested/concurrent pool-exhaustion join
  hang (fixed by the fire-and-forget farm + inline-shell liveness above). Per-worker local rings,
  work-stealing, the targeted-wake StoreLoad barrier, and cross-nursery wakeups remain D4+ (decision D
  cross-nursery/`Executor` hangs documented). Subsumes D1's deferred heap-into-`FiberCtx` half (D2a).

- ✅ **Concurrency D1 (lazy module snapshot) — kill the per-task module-graph rebuild** (`--parallel`
  engine). Old: `prepare_worker` / `prepare_worker_from_wire` called `build_worker_modules`, which
  **eagerly reconstructed the entire parent module graph into every worker heap, per task** (N tasks
  → N full rebuilds via `map_global_value`). Now: `snapshot_modules` builds a heap-independent,
  read-only `Arc<ModuleSnapshot>` **once** (memoized on the top-level VM in `snapshot_memo`; a nested
  worker reuses its installed `module_snapshot` Arc since `--parallel` globals are frozen, G1), shared
  by every worker via a cheap `Arc` clone. Each worker pre-allocs **empty** module objs
  (`install_snapshot`, index order preserved so home indices line up) and **faults a module's globals
  into its heap lazily on first access** (`fault_module` / `replay_snap`, gated by
  `module_faulted: Vec<bool>`) — a task that touches only its home module rebuilds only that module,
  one that touches none rebuilds nothing. `SnapValue` mirrors the deleted `map_global_value`
  structural recursion exactly (Func/Closure home → `module_objs` index, import-alias → `ModuleAlias`,
  `Native` fn-pointer, containers element-wise, value-derived map/set hashes carried). Lazy fault-in
  is hooked at the four module-global read sites — `Op::GetGlobal`, the `Op::GetCaptured` home
  fallback, `get_field` (module member), and the `module.fn(...)` dispatch — each preceded by
  `ensure_module_faulted` (a no-op on the top-level / cooperative VM, which never fault: their
  `module_objs` are the real populated modules, `module_snapshot` stays `None`). **The
  `Heap`-into-`FiberCtx` half of the literal §D1 spec is deferred to D2**, where the M:N share-nothing
  fiber model makes it observable; under the unchanged FIFO pool it buys nothing and would risk the
  cooperative share-by-ref single heap (decision A). 2 new characterization units (sibling-fn + global
  resolution under `--parallel`; 2 000-spawn correctness + loose wall-clock ceiling); all `worker_*`
  reconstruction units + `--parallel` goldens byte-identical; `primes_parallel` still prints
  `148933` on both engines. Two parallel review-panel reviewers returned clean (no Critical/Important);
  applied the comment-only `module_global` invariant note for future read sites. **1346 tests** green,
  clippy clean.
- ✅ **Concurrency D0 — O(N²)→O(N·logN) cooperative ready-queue** (VM cooperative engine only;
  `--parallel` unaffected). `run_scheduler` no longer linear-scans every live child per turn
  (`pick_runnable`, deleted); each `Nursery` carries a `ready: BTreeSet<usize>` (lowest-index pop —
  byte-identical scheduling order to the old scan) + a `blocked_on: HashMap<usize, Vec<usize>>` of
  parked indices. A `recv`-park registers its index; a sibling `send` (`wake_on_send`) drains the
  bucket back onto `ready`. 50k trivial fibers: ~7 s (debug, old) → tens of ms.
  **Three deliberate deviations from the literal `docs/concurrency-tier-d.md` §D0 spec, each verified
  against the code:** (1) **key `blocked_on` by `ChannelCore` pointer, not `GcRef`** — cooperative
  `spawn` deep-clones a channel (`from_wire` allocs a fresh handle onto the same `Arc<ChannelCore>`),
  so siblings hold distinct handles aliasing one core; a handle key would lose every wakeup. (2)
  **`BTreeSet` (lowest-index-first), not `VecDeque` (FIFO)** — FIFO would re-queue a woken low-index
  fiber behind pending higher-index ones, reordering output; the `BTreeSet` reproduces the old
  scan's order exactly (goldens byte-identical). (3) **`wake_on_send` drains every scheduler level,
  not just the innermost** — preserves the old re-scan's cross-level wakeup (an inner-nursery `send`
  waking an outer parked sibling). `FiberState::Blocked` dropped its now-redundant `GcRef` payload
  (the receiver handle stays GC-rooted on the fiber's operand stack). 3 new fiber units (50k-scale
  ceiling, many-producers/one-consumer over the core-ptr map, cross-level wakeup); review-panel
  finding applied (the core-ptr resolver fails loud via `unreachable!`, matching `channel_core`,
  rather than a silent sentinel key). **1344 tests** green, clippy clean.
- ✅ **Concurrency B3.3c/d — worker module-graph reconstruction** (VM, single-thread, parity-preserved).
  `Vm::build_worker_modules` + `map_global_value` snapshot the parent's initialized module graph into
  the worker heap (read-only `home`): tasks read post-init globals + call sibling/imported fns; method
  tasks (`spawn obj.m()`) dispatch via the rebuilt `module_objs`. Structural container recursion keeps a
  nested callable from smuggling a parent `GcRef` across the airlock. `run_task_isolated` is now
  functionally complete bar real threads (still test-only until `--parallel`). 7 new `worker_*` units
  incl. a GcRef-smuggle regression + a `gc_stress` reconstruction test. `docs/concurrency-b3.md`
  B3.3c/d rows + landed note. **1312 tests** green, clippy clean.
- ✅ **Concurrency B3.3b — G1 module-globals checker gate** (checker, parity-preserved). A
  reassignment (`=`/`+=`/`-=`) of a module global reachable — directly or transitively through
  free-function calls — from a `spawn` task is a type error (*"…use Shared[T]"*). New
  `Checker::check_spawn_global_mutation` + scope-aware AST walkers (`collect_spawn_roots` /
  `collect_free_calls_*` / `find_global_mutations` / `find_mutations_in_expr`): flow-scoped to spawn
  reachability, transitive over the free-fn call graph (cycle-guarded), and scope-aware down to
  closure-params/comprehension-vars so a shadowing binder is never mis-flagged; descends `recover:`
  blocks. Direct in-`spawn:`-block writes stay caught by the existing `is_captured` gate. 4-agent S++
  panel + cold pass caught a shadowed-spawn-target false positive and a `recover:` false negative
  pre-merge. Documented gaps (→ B3.3-threads): global-closure spawn targets, method chains. 16 new
  checker tests. `docs/concurrency-b3.md` B3.3b row.
- ✅ **Concurrency B3.3a — `str` crosses the airlock by value** (VM, single-thread, parity-preserved).
  Owned-bytes `WireValue::Str(Box<str>)` arm; `to_wire`/`from_wire`/`display_wire`/`collect_core_gcrefs`
  handle it; `str` is no longer a by-reference `Handle`, so `ensure_crossable` lets `str` (and data
  containing it) cross a worker boundary. Parity-safe (immutable, value-compared, no identity operator
  → fresh handle is unobservable; cached map/set hashes preserved). 3 new VM units (incl. str map-key
  round-trip); the B3.2 str-rejection test became `worker_crosses_str_by_value`. All concurrency +
  GC-stress goldens byte-identical. `docs/concurrency-b3.md` B3.3a row.
- ✅ **Concurrency B3.2 — `Arc<Program>` + isolated worker-VM construction** (VM, single-thread,
  parity-preserved). `program: Rc<Program>` → `Arc<Program>` (the compiled program is immutable after
  compile, so a worker shares it read-only — `Program` is plain owned data, `Send + Sync`). New
  `Vm::spawn_worker` builds a fresh worker `Vm` with its **own heap** sharing `Arc::clone(program)`;
  `Vm::run_task_isolated` lowers a `spawn`'d function/closure task to its `ProtoId` + wire'd
  captures/args (the callee is **never** crossed as a parent-heap `GcRef` — the proto lives in the
  shared `Arc<Program>`), `from_wire`s them into the worker heap, rebuilds the closure over a fresh
  empty `home`, runs it **synchronously** (no threads), and crosses the result + per-worker
  `out`/`stderr` back as a `WorkerResult` (decision F). **Cross-heap safety enforced** —
  `WireValue::has_handle` + `Vm::ensure_crossable` reject any `str`/closure value (a dangling `GcRef`
  in another heap) on captures, args, **and the returned result** with a clean fault instead of silent
  corruption; method tasks gated off (worker `module_objs` is empty). All `#[allow(dead_code)]` until
  B3.3's `--parallel` wires it in (decision A keeps the cooperative engine the default through B3.2).
  5 new units (distinct-heap / result+out / program-Arc-sharing / str-rejection / method-rejection);
  **1292 tests** green, `cargo test conformance` + `cargo clippy -- -D warnings` clean; all existing
  concurrency goldens + GC-stress byte-identical. Reviewed by 2 parallel S++ panels — the silent-
  dangling-handle risk they flagged is now the enforced `ensure_crossable` guard. `docs/concurrency-b3.md`
  §4 + B3.2 landed note.
- ✅ **Concurrency B3.0 — `WireValue` airlock** (VM, single-thread, parity-preserved). The task-airlock
  deep-copy `deep_clone` (`spawn` / `Channel.send` / `Shared` get-set) is now a **`WireValue`
  round-trip**: `Vm::to_wire` serializes a heap `Value` into an owned, `Send`-shaped `WireValue`
  (`src/vm/wire.rs`) and `Vm::from_wire` reconstructs it into the destination heap — **byte-identical**
  to the old direct deep-copy. Data (list/tuple/map/set/struct/enum) recurses; by-reference objects
  (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as `WireValue::Handle` (the same
  `GcRef`, same heap in B3.0); `Map`/`Set` carry their cached hashes so reconstruction never re-hashes
  (identical order + index). This de-risks the serialization layer before any thread is spawned: B3.1
  swaps the shared-core handle arms for `Arc<…Core>`, B3.3 makes `WireValue` the form that crosses a
  real OS thread. `to_wire` is total in B3.0 (statically infallible — the `Result` + `deep_clone`'s
  `.expect` are forward-plumbing; B3.3 *adds* the real `Err` arms for `Module`/`Func` that can't cross
  a thread). `from_wire` builds bottom-up and `Heap::alloc` never collects, so it inherits
  `deep_clone`'s GC-safety. 3 `wire_*` unit tests (round-trip value-equality over a nested mix; map
  hash/order preservation under a collision; by-handle identity for `Channel`/`Shared`/`Executor`/
  `Str`); all existing concurrency goldens + GC-stress stayed byte-identical green. Reviewed by 3
  parallel S++ reviewers — no correctness/byte-identity/GC findings; the one unanimous note (docs
  claimed a defensive fault arm that doesn't exist yet) was applied (comment-only). Surface unchanged.
- ✅ **Concurrency B3 — decomposition + documentation** (planning session, no engine code). Broke the
  Tier-C OS-thread multicore epic (B3, with B4/B5 folded in) into seven independently-shippable,
  TDD'd phases **B3.0…B3.6** in **[`docs/concurrency-b3.md`](docs/concurrency-b3.md)** — a persistent
  multi-session plan with the validated shared-nothing architecture (per-thread `Vm`+heap;
  `Arc<Program>`; a `WireValue` airlock replacing `deep_clone`; `Channel`/`Shared` cores as
  `Arc<…Core>` outside every heap; bounded pool; cooperative cancel), recorded decisions **A–G**
  (chief among them **A**: keep cooperative single-thread as the *default* and gate OS-thread
  multicore behind `--parallel`, so existing byte-identical goldens + VM==interp parity survive
  untouched), a risk register (top risk: **mutable module globals can't cross threads**), and a
  per-phase TDD focus. B3.0–B3.2 ship behind unchanged behavior; `--parallel` lands at B3.3.
  Also documented the non-B3–B5 backlog (cross-nursery wakeups, recv-in-native-callback,
  `Channel.close()`, A3b) in **[`docs/concurrency.md` §11](docs/concurrency.md)**. Docs-only — no
  `src/` changes; suite unchanged.
- ✅ **Concurrency A1 — `Channel.try_recv() -> T?`** (both engines). A **non-blocking** poll: `Some(v)`
  if the mailbox has a queued value, `None` if empty — it never blocks, faults, or suspends a fiber
  (the opposite of `recv`, which faults `deadlock` / parks under the scheduler on an empty channel).
  One mirrored arm per engine (checker `channel_method_sig` → `Ty::option(elem)`; interp
  `eval_channel_method`; VM `channel_method` via `alloc_enum` — and crucially the VM arm never touches
  `scheduler_stack`/`native_reentry`/`suspend`/`ip`, so it can't route through the `recv` park path).
  Originally **dropped** (its motivating mid-flight-producer scenario was unreachable under
  run-to-completion) and **un-deferred** once B1/B2 landed; because it's non-blocking it runs
  *identically* on the sequential interp and the VM, so it ships on both and stays parity-tested.
  `examples/try_recv.chz` golden (VM + interp byte-identical + GC-stress) + checker type/arity tests +
  per-engine empty/`Some`/in-`parallel`-no-suspend tests + a VM `try_recv`-drains-residue-after-a-
  blocking-`recv`-resumes test (pins the resume path leaves `suspend`/`ip` clean). Reviewed by two
  parallel S++ reviewers — no correctness findings. `docs/concurrency.md` §5/§9 + `docs/syntax.md`.
- ✅ **Concurrency C5 / Group B — B1 + B2 cooperative fibers + blocking `recv`** (VM engine). The
  bytecode VM gained *suspendable execution*: a `recv` on an empty channel under an active `parallel:`
  scheduler **parks** the running fiber (rewind-and-retry at the instruction boundary — push the
  receiver back, `ip -= 1`, set a suspend flag that breaks `run_until`/the re-entrant call path
  without unwinding defers) and the **nursery-local cooperative scheduler** runs a runnable sibling,
  resuming the parked fiber once its channel has data. A child that never blocks still runs to
  completion FIFO, so non-blocking programs are byte-for-byte unchanged. Each fiber owns its full
  execution context (`frames`/`stack`/`call_depth`/`cur_base`/`handlers`/`nurseries`/`fault_trace`),
  swapped in/out around scheduling; parked fibers are GC-rooted; nested `parallel:` recurses into a
  fresh scheduler level; a child fault or `std.os.exit` aborts its siblings. A wide native-reentry
  guard converts a `recv` that can't be parked (inside a HOF/sort/`compare`/`hash`/`str`/`update`/
  executor-drain/`defer`) into the deadlock fault. `examples/channel_block.chz` golden (VM + GC-stress)
  + ping-pong, deadlock-detection, guard, nested-`parallel:`, recover-in-child, and os.exit-in-child
  tests. **VM-only** — interp parity is a later milestone (gap pinned by an interp test). See the
  Current-focus parity-gap note and [`docs/concurrency.md`](docs/concurrency.md) §9.
- ✅ **Concurrency C5 / A2 — `Executor` program-exit auto-drain** (both engines). An executor
  submitted to but never explicitly `shutdown`/`shutdown_now`-ed is now gracefully drained at a clean
  program exit (FIFO, creation order) instead of silently dropping its queued work — mirrors a
  top-level `defer ex.shutdown()`. A per-engine **executor registry** (interp `Vec<Rc<RefCell<…>>>`;
  VM `Vec<GcRef>` that also joins the GC root set so un-shut work survives to the drain) drives it via
  the shipped `shutdown` path (first-fault-aborts-siblings). Hooked into every driver
  (`run_program_inner` / `run_with` stress / `run_file_inner`, both engines). A hard `std.os.exit`
  skips it (like `defer`); a faulting program is not drained. Also pinned **A3a** with a regression
  test — a non-sendable read smuggled through a *nested closure* in a `spawn:` block is rejected
  (already enforced, emergent from `capture_floors`). `examples/executor_autodrain.chz` golden + VM/
  interp parity + GC-stress + os.exit-suppression + fault-propagation tests. *Dropped: A1, A3b
  (see Current focus).*
- ✅ **Concurrency C5 (sequential subset) — `Executor` escape hatch** (both engines). `Executor()` +
  `submit(fn())` / `shutdown()` / `shutdown_now()`, reaped via `defer ex.shutdown()` (docs
  [`concurrency.md`](docs/concurrency.md) §8). New `Ty::Executor` (non-generic, sendable handle,
  reserved type name); interp `Value::Executor(Rc<RefCell<ExecState>>)`; VM `Obj::Executor { queue,
  shut }` + `Op::NewExecutor` + GC child-tracing. `submit` enqueues by handle (rejected once shut);
  `shutdown` drains the **live** queue FIFO one task at a time via the re-entrant call path (first
  fault aborts the rest + propagates, like a nursery; not-yet-run siblings stay for a later reap);
  `shutdown_now` discards pending. Both engines drain the live queue identically (a re-entrant
  `shutdown_now`/fault mid-drain behaves the same) — parity-pinned. `examples/executor.chz` golden +
  VM/interp parity + GC-stress + re-entrancy/fault-during-drain tests. *Deferred to real-C5:*
  program-exit auto-drain + closure-capture sendability gating (see Current focus).
- ✅ **Concurrency C5 refinement — `spawn:` block read sendability gate** (checker). A non-sendable
  *function-local* capture merely **read** inside a `spawn:` block (e.g. capturing a closure and
  calling it) is now a compile error, not just a *reassignment* (closes the C2-era gap). Module
  imports / top-level bindings are excluded (globals resolvable in every task, like free functions),
  so reading an imported module inside a task stays legal.
- ✅ **Concurrency C5 refinement — `StructInfo` origin flag** (checker). The `Ref[T]` non-sendability
  gate now keys on a `StructOrigin::{Builtin,User}` flag (threaded from `check_graph` via a
  `current_module_is_stdlib` flag set per module) instead of a bare struct-name string — so a *user*
  struct merely named `Ref` is sendable, while the builtin `std.ref` `Ref[T]` stays non-sendable.
- ✅ **Concurrency C4 — VM parity for `spawn`/`parallel:`/`Channel`/`Shared`** (bytecode VM +
  compiler). Ported C1–C3 off `--interp`-only onto the default engine: heap `Obj::Channel(VecDeque)`
  / `Obj::Shared(Value)` with GC child-tracing; ops `EnterNursery`/`JoinNursery`/`SpawnCall`/
  `SpawnMethod`/`SpawnBlock`/`NewChannel`/`NewShared`; a VM `deep_clone` (data deep-copied, str/func/
  closure/module/Channel/Shared by handle — mirrors interp). The `spawn:` block compiles to a
  synthetic zero-arg closure proto captured like any closure. Sequential executor: a `nurseries`
  stack drains FIFO at the join, first error aborts siblings; pending tasks are GC roots; a
  `recover:` boundary reclaims a fault-orphaned nursery via `Handler::nursery_len`; `Shared.update`
  re-roots the box across its re-entrant call. Differential parity goldens for all three examples +
  micro-tests + GC-stress tests. The four staging-error stubs are gone. *No checker changes* (it was
  already engine-agnostic). Reviewed by two parallel S++ reviewers — no Critical/Important findings.
- ✅ **Concurrency C3 — `Shared[T]` cross-task mutable box** (interp). `Shared(v)` (value-first — the
  element type is inferred from `v`, unlike `Channel[T]()`); methods `get()->T` (copies out), `set(T)`
  (copies in), `update(fn(T)->T)` (read-modify-write; releases the box borrow before calling the user
  fn so a re-entrant `get`/`set` can't panic). The handle is sendable and copied across the airlock —
  every task reaches the one box, whose single owner serialises writes (no locking under the sequential
  executor). The element type is *not* sendability-gated (only the handle crosses — the surprising
  asymmetry vs `Channel`, locked by a test). `Ref[T]` (the in-task box, `std/ref.chz`) is now forced
  **non-sendable** so passing it across a `spawn` is a compile error pointing at `Shared` (spec §7).
  *Known limit:* the `Ref` gate is a struct-name check (a user struct named `Ref` would also be
  non-sendable) — a `StructInfo` origin flag is the principled fix, deferred. `examples/shared.chz`.
- ✅ **Concurrency C2 — `Channel[T]` + sendability** (interp). `Channel[T]()` buffered/unbounded
  FIFO mailbox; methods `send` (move-on-send, deep-copied across the airlock), `recv` (FIFO; empty =
  deadlock-detect fault, not a hang), `len`. A `sendable(Ty)` predicate gates channel element types,
  `spawn` arguments, and `spawn:` capture reassignment — recursing into struct/enum fields (a closure
  smuggled inside a struct field is caught) with a cycle guard. `spawn`'s call target is restricted to
  a function/method like `defer`. `examples/channel.chz` (the canonical fan-out worker).
- ✅ **Concurrency C1 — `spawn` / `parallel:` nursery** (interp, sequential executor). `parallel:` is a
  structured-concurrency nursery; `spawn f(x)` (form 1) and `spawn:` block (form 2) register tasks that
  run to completion FIFO at the dedent (first error aborts siblings + propagates, composing with
  `recover:`/`defer`). `spawn` legal only inside a `parallel:` (checker `nursery_depth`, reset across fn
  boundaries). `deep_clone` isolates task data across the airlock; channels/functions pass by handle.
  Grammar + conformance updated. `examples/parallel.chz`.
- ✅ **Integer overflow policy** — every `i64` overflow is a recoverable fault (never wrap/crash);
  closed the last leak (`std.math.abs(i64::MIN)` → `checked_abs`). `examples/overflow.chz`.
- ✅ **Gaps pass II** — `Ref[T]` mutable box (pure-Chezzi `std/ref.chz`); `sort_by_key`; call fn-typed
  field `self.f(x)`; relax non-const defaults (no param/field refs); runtime stack traces (error line
  + call chain, identical on both engines).
- ✅ **Scripting-ergonomics gap pass** — hex/bin/oct literals; list `.concat`/`.extend` + map
  `.merge`/`.update`; tuple-destructuring `for` + `std/iter.chz` `enumerate`/`zip`; optional chaining
  `?.` + null-coalescing `??`; general tuple destructuring + match-on-tuple + guards.
- ✅ **Fix — loop variable is immutable** — checker rejects assignment to a `for`-loop var (was a
  VM/interp divergence); inner `:=` shadow stays mutable.
- ✅ **M18 — `defer` → block/lexical scope** — runs when its enclosing block exits on every path
  (fall-through / break / continue / return / `?` / panic), LIFO, inner-block-first. Supersedes M17.
- ✅ **M17 — `defer` (Go-style, frame-scoped)** — runs at frame exit, LIFO; receiver+args evaluated
  at the `defer` statement.
- ✅ **M16 — comprehensions + `std.os.exit(code)`** — `[e for x in it if g]` (+ set/map forms),
  first-class AST node; hard uncatchable cooperative exit threaded through both run drivers + CLI.
- ✅ **M15 — slicing + `Index`/`IndexSet`/`Slice` protocols** — `xs[1..3]` half-open/clamped;
  list/map/str conform intrinsically, user structs structurally.
- ✅ **M14 — method-level type params** · **user-defined parameterized protocols** (concrete-arg
  bounds, generalizing `Iterator[T]`) · **default + named args on methods** (desugar-pass).
- ✅ **Default + named arguments** — free fns + struct ctors; scope-aware desugar pass, both engines
  consume an already-normalized AST.
- ✅ **Tech-debt sweep** — reject dup generic param `[T, T]`; nested `set` equality parity; explicit
  call-site type args `name[T,…](…)`.
- ✅ **M11 — panic recovery + Go-style errors** — 2-param `Result[T, E]` (`T!`/`T!E`), `Error`
  protocol (`str` conforms), `recover:` boundary catching any transitive runtime fault.
- ✅ **M10 — type-system depth** — `Stringable`, `Hashable`, per-operator `Add`/`Sub`/`Mul` protocols,
  multi-bound `T: A + B`, transparent type aliases, generic enums; `map`/`set` reworked into real
  insertion-ordered hash tables (any `Hashable` key/element).
- ✅ **M9 — Tier-2 stdlib** — `std.regex` (the `regex` crate) + `std.request` (`ureq`+rustls, blocking).
  First runtime deps; language stays single-threaded/sync.
- ✅ **M8 — Tier-1 stdlib** — `s.chars()` + iterable strings; `std.json` (pure-Chezzi parse/stringify
  + type-directed `decode[T]`); native `std.process`/`std.fs`/`std.time`; `set` type.
- ✅ **M7 — generics + structural protocols** — type-erased generic fns/structs, Go-style `protocol`s,
  `Comparable`; stdlib `min`/`max`/`clamp` unified into pure-Chezzi `std.cmp`; `list.sort()` widened.
- ✅ **Round 2 gaps #10–#15** — `sort_by`, `ord`/`chr`, int+float math, map `for`, nested/tuple
  match, bitwise ops. Plus: iterator protocol (struct `next()`), `Iterator[T]` parameterized bound
  with element recovery + lazy adapters, match guards + half-open range patterns.
- ✅ **Tuples + multiple return + destructuring (gap #8)** — `(e1, e2, …)`, tuple types, `a, b := f()`,
  `.0`/`.1` access; immutable, fixed-arity, GC-traced.
- ✅ **M6a/b/c** — core-type str/list methods; pipe `|>` (parse-time desugar); stdlib via the Level-2
  native FFI seam (`NativeFn` + `Host`): `std.math`/`std.io`/`std.os` native, `std.str` pure Chezzi.
- ✅ **`map[K, V]` dictionary (gap #5)** — literals, keyed read/insert/update, six methods, GC-traced.
- ✅ **Index & field assignment** — `xs[i] = v`, `p.x = v`, `+=`/`-=` mutate in place (both engines).
- ✅ **M5a/b/c** — bytecode compiler + stack VM; hand-built mark-sweep GC; cross-engine parity +
  perf (~6.5× arith / ~4.3× fib over the interp) + CLI default flip to the VM (`--interp` for the
  tree-walker). Documented divergence: VM pre-parses `{expr}` chunks (malformed interpolation in dead
  code is a load error). `std.os.getcwd` not yet injectable via `HostConfig`; `read_file` capped at 64 MiB.
- ✅ **M4.5 — modules / imports + resolver** — multi-file, `chezzi.toml` root, run-once dep order,
  cross-module home-globals, cycle detection. Type names are program-global (collision-detected).
- ✅ **M4 — type checker (local inference)** — bidirectional, no unification; return-type inference,
  `T?`/`T!` sugar, expression-valued `match`/`if`, Go-style error accumulation.
- ✅ **M3 — tree-walk interpreter** — full expr/stmt set, `?` operator, string interpolation,
  256 MB-stack thread + `MAX_CALL_DEPTH` guard.
- ✅ **M2.5 — canonical grammar + conformance** — `docs/grammar.bnf` executed via the `bnf` crate
  (dev-dep only), differential-tested vs the parser over a corpus. Run `cargo test conformance`.
- ✅ **M2 — parser → AST** — recursive descent + Pratt; spans retrofitted; depth-capped.
- ✅ **M1 — lexer** — full `examples/hello.chz` incl. Indent/Dedent; string escapes, numeric underscores.
  Open follow-ups (anytime): scientific notation `1e3`, single-quote strings, unicode `\u{…}` escapes.

---

## Roadmap (later)

- 🟦 **Concurrency C5 — Group B (real engine, VM)** — **B1 + B2 (cooperative fibers + blocking
  `recv`) done on the VM**. Remaining **B3/B4/B5 now planned as a phased epic** in
  [`docs/concurrency-b3.md`](docs/concurrency-b3.md) (B3.0…B3.6; B4 real `Shared` + B5 real `Executor`
  pool + A3b are folded into B3.4–B3.6 since shared-nothing threads make them the same machinery).
  **B3.0 (wire-format airlock) is done**; next code step: **B3.1** (move `Channel`/`Shared`/`Executor`
  cores out of the heap into `Arc<…Core>`, single-thread, parity-preserved).
  **interp B1/B2 is a deliberate non-goal** (see the DECISION box in Current focus — the interp stays
  the sequential-subset parity oracle; the VM is the sole concurrent engine). Group A is done: C1–C4,
  the `Executor` sequential subset, **A2 auto-drain**, the C5 checker refinements, **A3a** (pinned),
  and **A1** (`Channel.try_recv`, both engines). Only A3b is left (lands with Group B). See the A/B
  breakdown in `docs/concurrency.md` §9.
- VM/GC optimizations (superinstructions, inline caching, NaN-boxing) — written up in
  **[`docs/future.md`](docs/future.md)**.

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in
  `docs/spec.md` → *Standard library* → "Future idea — native FFI". Default build stays zero
  third-party crates; dynamic `cdylib` plugins deferred. Do not start without an explicit decision.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **Collection literals must be single-line** — a newline inside `[`/`{` ends the expression.
- **`match` limits** — no multiple `Some(...)` arms, no nested nullary-variant patterns (nest a
  second `match`).
- **Float division by zero is a runtime fault**, not an IEEE `Inf`/`NaN`.
- **`std.os.getcwd`** not yet injectable via `HostConfig` (parity holds); **`read_file`** capped at 64 MiB.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists
  need only `Node?` child fields + a `match` per step, no special support.
