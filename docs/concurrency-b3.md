# Chezzi — B3: Tier-C Shared-Nothing OS-Thread Multicore (phased plan + landing notes)

> **Status: B3 epic COMPLETE** (B3.0–B3.6 all landed). Superseded by **Tier-D**
> ([`concurrency-tier-d.md`](concurrency-tier-d.md)), which rebuilt `--parallel` as an M:N
> work-stealing scheduler on top of B3's share-nothing OS-thread foundation. This file is retained as
> the persistent source of truth for **B3** — the original phased plan plus condensed landing notes.
> The high-level A/B roadmap lives in [`concurrency.md` §9](concurrency.md#9-implementation-roadmap-c1c5);
> read `concurrency.md` §2 (tier table), §4 (staged executor), §5–§7 (Channel/Shared/sendability) for
> companion context. Line references were anchors at authoring time — re-grep by symbol name if drifted.
>
> **Global landing facts (true for every phase below, stated once):** each phase shipped TDD'd with
> `cargo test` + `cargo test conformance` + `cargo clippy` green, an S++ review panel with Criticals
> applied, all existing concurrency + GC-stress goldens byte-identical, and `PROGRESS.md` updated.
> ~1565 tests green now.

---

## 1. Goal & the load-bearing invariant

Make `parallel:` / `spawn` / `Channel[T]` / `Shared[T]` / `Executor` run on **real OS threads** —
true multicore — with the **surface completely unchanged**. This is Tier C from
[`concurrency.md` §2](concurrency.md#2-why-shared-nothing-and-why-not-the-go-memory-model):
shared-nothing threads + channels, **no `Rc`→`Arc` on `Value`, no concurrent GC**.

**The invariant that makes it cheap:** each worker thread owns its **own heap + its own mark-sweep
GC**. No value in one thread's heap ever references another's. A `Value::Obj(GcRef)` is a slot index
into *one specific* heap (`GcRef(pub u32)`), meaningless anywhere else — so GcRefs *cannot* cross
threads, and per-thread collection needs **no stop-the-world, no handshake, no atomic mark bits**.
Everything below exists to preserve this: values cross the thread boundary only as a **serialized,
`Send` wire form**, never as live heap references. **The VM is the sole engine** (its two schedulers —
serial `--serial` and default M:N — both preserve this). (A tree-walk interpreter formerly served as the
frozen sequential parity oracle; it has since been removed.)

---

## 2. Architecture (validated against source)

1. **Per-thread worker `Vm` + own heap/GC.** The compiled program (protos, bytecode, constants, function
   table) is **immutable after compile**, so `Rc<Program>` → `Arc<Program>` and every worker VM shares it
   read-only. A worker = `Arc<Program>` + a fresh `Heap` + its own `frames`/`stack`/`out`/`stderr`.

2. **Wire-format airlock — replaces `deep_clone`.** A `WireValue` enum mirrors the **sendable** value set
   (scalars, `List/Map/Set/Tuple`, `Struct`, `Enum`, plus **`Channel`/`Shared`/`Executor` handles** carried
   as the `Arc<…Core>` itself). `to_wire(v)` does the same deep copy the VM's `deep_clone` does but emits a `Send`
   value; `from_wire(w)` reconstructs in the *destination* heap. Only sendable values reach `to_wire`
   (checker-gated at `spawn`/`send`/`submit`); a non-sendable value is a **defensive runtime fault**, not
   `unreachable!()`.

3. **`Channel` / `Shared` cores move OUT of the heap** (the latter subsumes B4). Each shared object lives
   outside every heap as `Arc<…Core>` (`ChannelCore { Mutex<VecDeque<WireValue>> }` + condvar/close bits
   later; `SharedCore { Mutex<WireValue> }`); the heap holds only a **handle**. `send` locks +
   `push_back(to_wire(v))`, `recv` locks + `pop_front` + `from_wire` (+ `cv.wait` blocking at B3.3); `Shared`
   `get`/`set` lock + wire-convert, `update` `from_wire`s into the **calling** thread's heap, runs the user
   closure there, then `to_wire`s the result back under the lock.

4. **`parallel:` runs task bodies on a bounded pool** (decision B); `JoinNursery` joins all handles; the
   first child fault aborts siblings via cooperative cancel (decision C); `spawn` ships the proto by `Arc`
   and captures/args by wire. **`Executor` (B5) rides the same pool**, with **A3b** (submit-capture
   sendability gate) load-bearing because the closure's captures truly cross a thread.

---

## 3. Decisions (rationale recorded so future sessions don't relitigate)

### A. Determinism contract — the highest-priority decision

Real parallelism makes output ordering **nondeterministic**, breaking byte-identical goldens (most sharply
cooperative-interleave goldens like ping-pong's exact ordering) and the deterministic two-engine parity
goldens — you cannot keep those goldens *and* have real parallelism produce them. **Decision (historical):
keep the cooperative single-thread engine as the DEFAULT; gate OS-thread multicore behind a `--parallel`
flag** (mirroring the existing flag plumbing; "flags must precede the file"). Every existing
concurrency golden + GC-stress + parity test stays green untouched on the cooperative engine; the
parallel engine gets its own **deterministic-by-construction** suite (collect→drain→sort→print, or
order-insensitive set-of-lines assertions). **Byte-identical parity is suspended by definition under
`--parallel`** — real parallelism reorders output. This is also what lets B3.0–B3.2
ship behind unchanged behavior. (The default has since flipped to the M:N engine; `--serial` selects the
cooperative parity oracle.)

### B. Bounded work pool, not thread-per-task

`parallel:` with N spawns must **not** be N OS threads (nested `parallel:` would explode N×M). **Decision:
a bounded pool sized to `vm::worker_count()` (the `--threads=N` / `CHEZZI_THREADS` override, or
`available_parallelism()` when unset/`0`); the thread that hits `JoinNursery` participates as a
worker.** Known v1 hazard: bounded pool + blocking tasks can starve. v1 mitigation: parent-participates +
a documented "tasks should not out-block the pool" rule; work-stealing / grow-on-stall deferred.

### C. Cancellation / abort-siblings — cooperative cancel flag, no thread kill

Rust has no safe thread kill. **Decision: per-nursery `cancel: Arc<AtomicBool>`**, checked (Relaxed) at the
same back-edge / call / channel-op sites the dispatch loop already visits. First fault sets `cancel`;
running siblings observe it at their next check and unwind as a `Cancelled` sentinel the parent swallows. A
condvar-blocked `recv` must also wake on cancel (else siblings hang — risk #2). `std.os.exit` in a child is
a fault-that-cancels: the worker's own `pending_exit` halts it, the join propagates the code to the **parent
VM's** `pending_exit` (hard halt). (Implementation deviated to a `wait_timeout` re-check loop — see B3.4.)

### D. Deadlock detection — keep the nursery-local case, accept global hangs (Go-like)

With real condvar blocking the automatic "no runnable fiber" detector is gone. **Decision: preserve the
nursery-local all-blocked detector** (backs the `"deadlock: every task in this parallel: block is
blocked…"` golden) via a per-nursery counter of siblings-blocked-in-recv vs live-sibling-count: when
`blocked == live` and the awaited channels are empty, broadcast a deadlock fault. **Accept hangs** for
deadlocks spanning nurseries or involving `Executor` (document it, like Go) — a global cycle detector would
need cross-thread inspection that contradicts shared-nothing. The counter is race-prone — a false-positive
fault is the failure mode to test (risk #6).

### E. GC + Arc cores

The end-state goal is `children()` returning nothing for a core (tracing stops at the `Arc` boundary), but
that's only reachable once `WireValue` is fully `GcRef`-free — not until B3.3 (cores still embed
`Handle(GcRef)`s: a `Channel[str]` queues `Str` handles, an `Executor` queues `Handle(closure)`). So **at
B3.1 the `children()` arms are REWRITTEN, not dropped**: for `Obj::Channel/Shared/Executor(Arc<…Core>)`,
`children()` locks the core and walks its `WireValue`s via `collect_gcrefs`, yielding embedded `Handle`
GcRefs (stopping at a nested core, rooted via its own handle). B3.3 later lets `str` and closures cross by
value, after which the arms could be dropped. **Known leak — do NOT claim "no cycles":** reply-channel `Arc`
cycles are reachable (core A's queue holds `Arc<B>`, B's holds `Arc<A>`); drop both handles and the pair
leaks for the program's lifetime (`Arc` is refcounted, not cycle-collected; a cross-thread cycle collector
would contradict shared-nothing). **Documented limitation** matching Go/Rust `Arc` — an unbounded leak, not
a crash.

### F. Output — buffer-per-worker, flush-on-join

Each worker VM accumulates its own `out` / `stderr` `String`. **Decision: each worker returns its `out` on
join; the parent concatenates in join order.** A child's output appears all-at-once at its join point (this
changes what an interleave golden would show — fine, such goldens stay on the cooperative default engine
per decision A). Live shared-stdout interleave is rejected — this is the only option compatible with
keeping any golden green.

### G. Mutable module globals across threads — RESOLVED (Option A)

`do_spawn_block` passes `home` (module-globals object) by handle; under threads that `GcRef` points into
the *parent* heap and can't cross, and mutable globals can't be trivially wire-copied (mutations wouldn't
propagate back). Candidates: **(A) module globals immutable after init** vs **(B) per-worker snapshots**
(a worker's writes silently never propagate).

**Decision: Option A.** A module global is just a top-scope value; mutating it across a task is the same
move the checker already bans for `Ref[T]` at the `spawn` boundary. Chezzi's mutation ladder is already
`value` (copy) → `Ref[T]` (in-task box) → `Shared[T]` (cross-task box); Option A applies the existing top
rung to globals. Option B was rejected — a write that *looks* global going silently local-only is a footgun
with no precedent, and there's nowhere for it to propagate under shared-nothing.

Concretely (B3.3): under `--parallel` a module global is **read-only after the module's init prologue** — a
`SetGlobal` reachable from inside a `spawn`'d task (directly or transitively) is a **checker error**
(*"…use Shared[T]"*); reads of post-init-constant globals are fine. The worker gets `home` as a read-only
wire-copied snapshot, never copied back. Cross-task mutable state goes through `Shared[T]`:

```chezzi
counter := Shared(0)                       # not  counter := 0 (a plain global is read-only in a task)
fn bump(): counter.update(fn(n): n + 1)
parallel:
    spawn bump()
    spawn bump()
print(counter.get())                       # 2
```

The default (cooperative) engine is **unchanged** — global mutation stays legal there; the restriction is
scoped to `--parallel` task bodies. The B3.3b gate is **flow-scoped to `spawn`-reachability** (not
whole-program) to avoid over-restricting purely-sequential code.

---

## 4. Phased breakdown

Each phase is independently shippable. **B3.0–B3.2 ship behind unchanged behavior** (parity + GC-stress
goldens exercise them) so the serialization + worker-VM machinery is de-risked *before a single thread is
spawned*. `--parallel` (and any nondeterminism) appears only at **B3.3-threads**.

| Phase | Goal | Behavior |
|-------|------|----------|
| **B3.0** | Wire-format airlock, single-thread: define `WireValue`; replace the `deep_clone` call sites with a `to_wire`+`from_wire` round-trip into the *same* heap. | unchanged |
| **B3.1** | Move Channel/Shared/Executor cores out of the heap into `Arc<…Core>` holding `WireValue` (no `Condvar` yet). `to_wire`/`from_wire` gain `Arc<…Core>` arms. | unchanged |
| **B3.2** | `Arc<Program>` + worker-VM construction (no threads): a `spawn`'d task runs **synchronously** in a fresh worker `Vm`, args wire-copied in, result + `out` back. | unchanged |
| **B3.3a** | `str` crosses the airlock by value (owned-bytes `WireValue::Str` arm). | unchanged |
| **B3.3b** | G1 module-globals checker gate: a reassignment of a module global reachable from a `spawn` task is a checker error. | unchanged |
| **B3.3c** | Read-only `home` snapshot — worker module-graph reconstruction so a task can read post-init globals + call sibling/imported fns. | unchanged |
| **B3.3d** | Method tasks (`spawn obj.m()`): lower to `Lowered::Method` (recv + args by wire), dispatch against the reconstructed `module_objs`. | unchanged |
| **B3.3-threads** | Real OS threads behind `--parallel`: bounded pool (B), condvar `recv` (C), buffer-flush-on-join (F). Cooperative engine stays the **default**. | `--parallel` new |
| **B3.4** | Cancellation + cross-thread `os.exit`: per-nursery `cancel: Arc<AtomicBool>` (C), exit-code propagation up the join. | `--parallel` |
| **B3.5** | Nursery-local deadlock detection under threads (blocked-count vs live-count, D). | `--parallel` |
| **B3.6** | `Executor` / B5 on the pool + A3b submit-capture sendability gate. | `--parallel` |

**Status checklist:** all landed ✅ — B3.0, B3.1, B3.2, B3.3a, B3.3b, B3.3c, B3.3d, B3.3-threads, B3.4,
B3.5, B3.6. **B3 epic complete.**

### Landed notes (condensed — key files + deviations per phase)

**B3.0 / B3.1.** `WireValue` + `to_wire`/`from_wire` (`src/vm/wire.rs`) replace the `deep_clone` round-trip;
`to_wire` is total/statically-infallible at B3.0 (by-reference set crosses as `WireValue::Handle(GcRef)`,
same heap; the non-crossable `Err` arms arrive at B3.3). B3.1 moves the cores to `src/vm/core.rs`
(`ChannelCore`/`SharedCore`/`ExecutorCore`, each a `Mutex`, condvar/cancel deferred); the airlock now
serializes *at the core boundary* and `from_wire` allocs a **fresh** handle onto the **same** `Arc` (shared,
not copied). **`children()` arms REWRITTEN, not dropped** (decision E). Key gotcha: **never hold a
`MutexGuard` across `invoke_value`** (lock→read→drop-guard→call→relock); `shut` moved into the shared core so
a `from_wire`'d alias can't double-drain.

**B3.2.** `Rc<Program>` → `Arc<Program>` everywhere (`Program` is `Send + Sync`). `Vm::spawn_worker` +
`Vm::run_task_isolated(PendingCall) -> WorkerResult` lower a `Call` to `ProtoId` + wire'd captures/args
(callee never crossed as a parent `Handle`), run synchronously, cross result + output back; all
`#[allow(dead_code)]` until B3.3. **Cross-heap safety enforced:** `WireValue::has_handle()` +
`Vm::ensure_crossable` reject captures/args/**result** with a clean fault — a dangling `GcRef` is a
`RuntimeError`, not a dangling read.

**B3.3a/b/c/d.** (a) `str` crosses by value (owned-bytes `WireValue::Str`). (b)
`Checker::check_spawn_global_mutation` errors on a module-global reassignment reachable from a `spawn` task
(flow-scoped, scope-aware, transitive over the free-function call graph). (c) `Vm::build_worker_modules` +
`map_global_value` reconstruct the parent's initialized module graph into the worker heap — **snapshots,
never re-inits**; GcRef-safe (callables rebuilt explicitly, containers recurse so a `[fn …]` global can't
smuggle a parent `GcRef`). (d) Method tasks lower to `Lowered::Method` dispatched via `do_method_call`. A
`Closure` wire arm is deferred (no live path until `Executor.submit`, B3.6).

**B3.3-threads.** `Vm.parallel` selects the engine: `join_nursery` branches to `run_parallel_nursery`, which
`prepare_worker`s each task, farms `tasks[1..]` to the bounded pool (`src/vm/pool.rs` — one process-wide
`OnceLock<Pool>` of `vm::worker_count()` threads — `--threads=N` / `CHEZZI_THREADS`, else
`available_parallelism()`), runs `tasks[0]` inline (decision B), then flushes
output in **task order** (decision F) and propagates the lowest-index fault (whose own buffered output is
flushed at its slot before it propagates so it is not dropped; higher-index racy faults + `Cancelled`
still drop — byte-for-byte oracle parity only when the faulter is the nursery's sole output-producer,
else a residual pre-existing race, see Decision F in `concurrency-tier-d.md`). `run_task_isolated` split into
`prepare_worker` (parent-heap) + `ReadyWorker::run` (thread-side; `Vm` is `Send`); blocking `recv` waits on
`ChannelCore.cv`; `Shared.update` takes a per-core `update_lock` **only under `--parallel`** so concurrent
RMWs can't lose each other. **Deferred here:** no cancellation (first fault joins-then-reports), no deadlock
detection (an all-blocked `--parallel` nursery *hangs*, so every golden is deterministic-by-construction),
`Executor` not yet on the pool.

**B3.4 — cancellation + cross-thread `os.exit`.** Each worker carries `cancel: Option<Arc<AtomicBool>>` + a
`cancelled` latch. `run_outcome` classifies each task (`Done`/`Cancelled`/`Exit{code}`/`Fault`); the first
sibling to fault or `os.exit` trips `cancel`, and the join propagates the lowest-index `Exit` (→ parent
`pending_exit`, hard halt) **preferred over** any `Fault`. Running siblings observe the flag at the dispatch
back-edge and unwind as a `"cancelled"` sentinel. **The cancel unwind bypasses `recover:`** (a cancelled task
must die) while still running `defer`s — else a worker-internal `recover:` would catch the sentinel and
resume. **Decision-C deviation:** instead of a per-nursery cancel condvar, the blocking `recv` uses a **50ms
`wait_timeout` re-check loop**, eliminating the lost-wakeup hazard by construction (cost: ≤50ms abort
latency). **Known limitation:** cancellation is single-level — an outer nursery's cancel doesn't reach a
worker blocked in its child nursery's join.

**B3.5 — nursery-local deadlock detection under threads.** A per-nursery `DeadlockWatch` cloned into each
worker; a barrier-confirm detector in the blocking `recv` — a parked worker confirms its channel empty at
most once per `epoch`, and `confirms == live` ⇒ fault `deadlock` with the cooperative engine's byte-identical
message. `send`/`task_finished` bump `epoch`; watch and channel `q` locks are never held together. When the
detector fires (Tier-D `SchedCore::flag_deadlock`), each still-parked fiber's OWN buffered stdout/stderr is
moved into its `Fault` slot, so a parked task's partial output flushes at its task-order slot exactly like a
real fault (decision F) — the serial engine printed those lines live before returning the deadlock error.
**Residual hangs documented:** `Executor`-spanning / orphaned-message / saturated-pool cases. (The
**cross-nursery** circular case is now **RESOLVED under `--parallel`** by Tier-D's VM-global flat
scheduler — `examples/parallel_cross_nursery_circular.chz`; the cooperative engine's cross-nursery
flatten is a separate, later commit. See `docs/concurrency-tier-d.md` "Open / deferred".)

**B3.6 — `Executor` on the pool + A3b. B3 epic complete.** **Checker (A3b):** the `submit` arm pushes a
`capture_floor` so a submitted closure's captures are gated exactly like a `spawn:` block's. **VM —
`WireValue::Closure` (the arm B3.3 deferred):** a submitted closure crosses **by value**
(`{ proto, captured, home }`, all `GcRef`-free since `home` is a `module_objs` index). `Vm::wire_callable`
produces it, but `submit` calls it **only under `--parallel`**; on the cooperative default engine `submit`
crosses by handle so its same-heap drain shares captures by reference — matching the same-heap sequential semantics
(**Review C-01:** an unconditional `wire_callable` broke sequential-subset parity; fixed by
the engine gate). **Drain:** under `--parallel`, `shutdown` marks `shut`, drains under the core lock (guard
dropped before invoke), then farms tasks to the pool via **`run_workers_on_pool`** — the farm/inline/join/
flush core **extracted verbatim** from `run_parallel_nursery` (a pure refactor; that path stays byte-identical).
The cooperative engine keeps the inline FIFO drain, byte-identical. (`parallel_defer_runs_on_cancelled_sibling`
is a known low-rate timing flake under heavy full-suite parallel load; passes in isolation — not a regression.)

---

## 5. Risk register

1. **Module globals across threads** (G) — ✅ resolved: Option A. Checker gate (B3.3b) + read-only `home`
   snapshot (B3.3c) implemented. Known gate gaps (documented in the walker comment): a module-global
   *closure* used as a `spawn` target, and method-mediated call chains — indirect dispatch the static call
   graph can't follow. The *runtime* reconstruction handles these correctly; the gap is purely the
   *checker's* G1 reachability analysis.
2. **Condvar-blocked-recv cancellation** (G2) — lost wakeups; resolved via the `wait_timeout` re-check loop.
3. **Pool starvation** (G3) — parent-participates + documented "don't out-block the pool" rule for v1.
4. **Output contract** (G4) — settled (decision F) but pervasive; threaded through B3.2/B3.3.
5. **Arc-cycle leaks** (G5) — documented limitation, no fix.
6. **Deadlock counter races** (G6) — false-positive case tested explicitly (B3.5).

---

## 6. Critical files (by area)

- `src/vm/mod.rs` — `deep_clone`, `join_nursery`/`run_parallel_nursery`/`run_workers_on_pool`,
  `prepare_worker`/`run_task_isolated`, the `recv` suspend path + `*_method`, `build_worker_modules`/
  `map_global_value`, GC roots, the dispatch-loop check sites.
- `src/vm/wire.rs` (`WireValue`, `to_wire`/`from_wire`, `ensure_crossable`); `src/vm/core.rs`
  (`ChannelCore`/`SharedCore`/`ExecutorCore`); `src/vm/pool.rs` (the bounded process-wide pool).
- `src/vm/heap.rs` (`Obj` + `children()`); `src/vm/value.rs` (`GcRef(pub u32)` — source of `!Send`);
  `src/native/os.rs` (`request_exit`); `src/main.rs` (`--parallel` flag); `src/checker/mod.rs` (A3b
  `submit` gate + G1 `check_spawn_global_mutation`).

---

## 7. Out of scope for B3

B4 (real `Shared`) and B5 (real `Executor` pool) are **folded into** B3 (B3.4–B3.6) rather than sequenced
after it, because under shared-nothing threads they *are* the same machinery. The alternative bet —
Tier-A-only (richer cooperative scheduler, no real parallelism) — is **not pursued**: B1/B2 already shipped
the cooperative engine, and the remaining demand is genuine multicore. Items deliberately *not* in B3–B5
live in the [`concurrency.md` "Deferred / backlog"](concurrency.md#11-deferred--backlog-not-b3b5) section.

**I/O-bound concurrency is explicitly NOT a B3 goal.** B3 delivers CPU-bound multicore; a blocking-I/O task
pins its pool thread (risk G3). Smart I/O handling — an elastic blocking pool or a full **M:N scheduler +
async-I/O pollset** — is the **Tier-D post-B3 frontier**
([`concurrency.md` §10](concurrency.md#10-future-evolution); shipped in
[`concurrency-tier-d.md`](concurrency-tier-d.md)).
